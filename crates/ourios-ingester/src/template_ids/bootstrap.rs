//! The one-time bootstrap of the high-water (RFC 0059 §3.5): a provable
//! floor, the highest id any data row, audit event or restored snapshot
//! carries.

use ourios_miner::cluster::MAX_TEMPLATE_ID;
use ourios_parquet::{IdColumns, IdMaxError, Store, object_max_id};

use super::{Seated, TemplateIdsError, Written, create, names};

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
    /// [`TemplateIdsError::Scan`] when any step fails (a listing, a
    /// ranged read, a footer parse, the full-file fallback, or an id
    /// column's decode): a floor over a partial scan proves nothing.
    pub fn run(store: &Store) -> Result<Self, TemplateIdsError> {
        let mut scan = Self::default();
        walk(store, "data", &mut |key| {
            let max = in_domain(key, object_max_id(store, key, IdColumns::Data))?;
            scan.data_max = scan.data_max.max(max);
            scan.counted();
            Ok(())
        })?;
        walk(store, "audit", &mut |key| {
            let max = in_domain(key, object_max_id(store, key, IdColumns::Audit))?;
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

/// A file's highest id, refused when it lies past the template-id
/// domain: no allocator issued it, so no floor can be built on it.
fn in_domain(
    key: &str,
    max: Result<Option<u64>, IdMaxError>,
) -> Result<Option<u64>, TemplateIdsError> {
    match max.map_err(|e| TemplateIdsError::Scan(Box::new(e)))? {
        Some(id) if id > MAX_TEMPLATE_ID => Err(TemplateIdsError::Malformed {
            key: key.to_owned(),
            detail: format!("carries template id {id}, above the template-id domain's i64::MAX"),
        }),
        max => Ok(max),
    }
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
            .map_err(|source| {
                TemplateIdsError::Scan(Box::new(IdMaxError::Store {
                    key: dir.clone(),
                    source: Box::new(source),
                }))
            })?;
        for key in listing.objects.iter().filter(|k| k.ends_with(".parquet")) {
            visit(key)?;
        }
        pending.extend(listing.common_prefixes);
    }
    Ok(())
}

/// Bootstrap the high-water (RFC 0059 §3.5): scan, then create the object
/// at the floor. Nothing is written until the scan completes, so a crash
/// mid-scan leaves no object and the next start scans again.
///
/// # Errors
///
/// [`TemplateIdsError`] when the scan or the write fails, and
/// [`TemplateIdsError::BootstrapRaceLost`] when another start created the
/// object first: this one has restored snapshots the winner's floor may
/// not cover, and its restart discards them.
pub fn bootstrap(store: &Store, restored: u64) -> Result<Seated, TemplateIdsError> {
    let scan = BootstrapScan::run(store)?;
    let floor = scan.floor(restored);
    match create(store, floor)? {
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
        Written::Lost => Err(TemplateIdsError::BootstrapRaceLost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_id_past_i64_max_fails_the_scan_closed() {
        let err = in_domain("data/x.parquet", Ok(Some(MAX_TEMPLATE_ID + 1)))
            .expect_err("past the domain");
        assert!(
            matches!(&err, TemplateIdsError::Malformed { key, .. } if key == "data/x.parquet"),
            "{err}"
        );
        assert!(in_domain("data/x.parquet", Ok(Some(u64::MAX))).is_err());
    }

    #[test]
    fn a_file_id_at_i64_max_is_a_floor() {
        assert_eq!(
            in_domain("data/x.parquet", Ok(Some(MAX_TEMPLATE_ID))).expect("in the domain"),
            Some(MAX_TEMPLATE_ID)
        );
        assert_eq!(in_domain("data/x.parquet", Ok(None)).expect("no id"), None);
    }
}
