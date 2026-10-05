//! The one-time bootstrap of the high-water (RFC 0059 §3.5): a provable
//! floor, the highest id any data row, audit event or restored snapshot
//! carries.

use ourios_parquet::{IdColumns, Store, object_max_id};

use super::{Seated, TemplateIdsError, Written, names, read, store_err, write};

/// Files between two progress events.
const PROGRESS_EVERY: u64 = 10_000;

/// What the bootstrap scan read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapScan {
    pub data_max: Option<u64>,
    pub audit_max: Option<u64>,
    pub files_scanned: u64,
}

impl BootstrapScan {
    /// Scan every data and audit file in `store`, one footer at a time.
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError`] when a listing or a file read fails: a floor
    /// over a partial scan proves nothing.
    pub fn run(store: &Store) -> Result<Self, TemplateIdsError> {
        let mut scan = Self::default();
        walk(store, "data", &mut |key| {
            let max = object_max_id(store, key, IdColumns::Data).map_err(scan_err)?;
            scan.data_max = scan.data_max.max(max);
            scan.counted();
            Ok(())
        })?;
        walk(store, "audit", &mut |key| {
            let max = object_max_id(store, key, IdColumns::Audit).map_err(scan_err)?;
            scan.audit_max = scan.audit_max.max(max);
            scan.counted();
            Ok(())
        })?;
        Ok(scan)
    }

    fn counted(&mut self) {
        self.files_scanned += 1;
        if self.files_scanned.is_multiple_of(PROGRESS_EVERY) {
            tracing::info!(
                name: names::BOOTSTRAP_PROGRESS,
                { { names::FILES_SCANNED } = self.files_scanned },
                "template-id bootstrap: {} data and audit footers read",
                self.files_scanned,
            );
        }
    }

    /// The provable floor: the highest id the scan or `restored` saw.
    #[must_use]
    pub fn floor(self, restored: u64) -> u64 {
        self.data_max
            .unwrap_or(0)
            .max(self.audit_max.unwrap_or(0))
            .max(restored)
    }
}

fn scan_err(e: ourios_parquet::IdMaxError) -> TemplateIdsError {
    TemplateIdsError::Scan(Box::new(e))
}

/// Visit every `*.parquet` key under `prefix`, one delimited level at a
/// time, so no more than one directory's listing is held at once.
fn walk(
    store: &Store,
    prefix: &str,
    visit: &mut impl FnMut(&str) -> Result<(), TemplateIdsError>,
) -> Result<(), TemplateIdsError> {
    let mut pending = vec![prefix.to_owned()];
    while let Some(dir) = pending.pop() {
        let listing = store
            .list_delimited_blocking(Some(&dir))
            .map_err(store_err("list", &dir))?;
        for key in listing.objects.iter().filter(|k| k.ends_with(".parquet")) {
            visit(key)?;
        }
        pending.extend(listing.common_prefixes);
    }
    Ok(())
}

/// Bootstrap the high-water (RFC 0059 §3.5): scan, then create the object
/// at the floor. Nothing is written until the scan completes, so a crash
/// mid-scan leaves no object and the next start scans again. A start that
/// loses the create to another reads the winner's object.
///
/// # Errors
///
/// [`TemplateIdsError`] when the scan or the write fails.
pub fn bootstrap(store: &Store, restored: u64) -> Result<Seated, TemplateIdsError> {
    let scan = BootstrapScan::run(store)?;
    let floor = scan.floor(restored);
    match write(store, None, floor)? {
        Written::Landed => {
            tracing::info!(
                name: names::BOOTSTRAPPED,
                {
                    { names::FLOOR } = floor,
                    { names::DATA_MAX } = scan.data_max,
                    { names::AUDIT_MAX } = scan.audit_max,
                    { names::FILES_SCANNED } = scan.files_scanned,
                },
                "template-id high-water bootstrapped at {floor} from {} data and audit footers",
                scan.files_scanned,
            );
            Ok(Seated {
                high_water: floor,
                bootstrapped: true,
            })
        }
        Written::Lost => match read(store)? {
            Some(winner) => Ok(Seated {
                high_water: winner.reserved_through,
                bootstrapped: false,
            }),
            None => Err(TemplateIdsError::Missing),
        },
    }
}
