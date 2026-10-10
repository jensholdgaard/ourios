//! The one-time bootstrap of the high-water (RFC 0059 §3.5): a provable
//! floor, the highest id any data row, audit event or restored snapshot
//! carries.
//!
//! The scan is bound by the store's per-request latency, not by CPU: one
//! suffix GET per file, tens of thousands of files. So one thread walks
//! the listing and feeds a bounded channel, and a fixed number of reader
//! threads fetch footers from it. The floor is a maximum, so the order in
//! which reads complete does not matter.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ourios_miner::cluster::MAX_TEMPLATE_ID;
use ourios_parquet::{IdColumns, IdMaxError, Store, object_max_id};

use super::{SEATED_MARKER, Seated, TemplateIdsError, Written, create, names};

/// Files between two count-driven progress events.
const PROGRESS_EVERY: u64 = 10_000;
/// Footer reads in flight at once, by default.
pub const DEFAULT_SCAN_CONCURRENCY: usize = 16;
/// The longest gap between two progress events, by default.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// The prefixes the scan walks, and the id columns their files hold.
const PREFIXES: [(&str, IdColumns); 2] = [("data", IdColumns::Data), ("audit", IdColumns::Audit)];

/// How the bootstrap scan runs.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Footer reads in flight at once; 0 counts as 1.
    pub concurrency: usize,
    /// The longest gap between two progress events.
    pub progress_interval: Duration,
    /// Set by the process's shutdown signal: the scan stops between reads
    /// and fails with [`TemplateIdsError::Interrupted`], writing nothing.
    pub shutdown: Arc<AtomicBool>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_SCAN_CONCURRENCY,
            progress_interval: PROGRESS_INTERVAL,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// What the bootstrap scan read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapScan {
    pub data_max: Option<u64>,
    pub audit_max: Option<u64>,
    pub files_scanned: u64,
}

impl BootstrapScan {
    /// [`Self::run_with`] under the default [`ScanOptions`].
    ///
    /// # Errors
    ///
    /// As [`Self::run_with`].
    pub fn run(store: &Store) -> Result<Self, TemplateIdsError> {
        Self::run_with(store, &ScanOptions::default())
    }

    /// Scan every data and audit file in `store`, reading up to
    /// `options.concurrency` footers at once, and report its start,
    /// progress and end on stderr as well as through the progress event:
    /// the scan runs before any listener opens, and an operator waiting on
    /// it must see it whatever the logs exporter is.
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError::Scan`] when any step fails (a listing, a
    /// ranged read, a footer parse, the full-file fallback, or an id
    /// column's decode): a floor over a partial scan proves nothing.
    /// [`TemplateIdsError::Interrupted`] when `options.shutdown` was set
    /// before every listed file was read.
    pub fn run_with(store: &Store, options: &ScanOptions) -> Result<Self, TemplateIdsError> {
        let started = Instant::now();
        eprintln!(
            "template-id bootstrap: reading every data and audit footer, {} at a time \
             (RFC 0059 §3.5); the listeners open once it completes",
            options.concurrency.max(1),
        );
        let outcome = scan(store, options, &mut |files| {
            tracing::info!(
                name: names::BOOTSTRAP_PROGRESS,
                { { names::FILES_SCANNED } = files },
                "template-id bootstrap: {files} data and audit footers read",
            );
            eprintln!(
                "template-id bootstrap: {files} data and audit footers read in {:.0?}",
                started.elapsed(),
            );
        });
        let elapsed = started.elapsed();
        match &outcome {
            Ok(scan) => eprintln!(
                "template-id bootstrap: scan complete, {} footers read in {elapsed:.1?}",
                scan.files_scanned,
            ),
            Err(e) => eprintln!("template-id bootstrap: stopped after {elapsed:.1?}: {e}"),
        }
        outcome
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

/// What the scan's threads share.
#[derive(Default)]
struct Tally {
    /// The highest data id plus one, 0 while none is seen: every id is at
    /// most `i64::MAX` once [`in_domain`] admits it, so the sum fits.
    data_max: AtomicU64,
    /// As `data_max`, for audit files.
    audit_max: AtomicU64,
    files_read: AtomicU64,
    files_listed: AtomicU64,
    listing_complete: AtomicBool,
    failed: AtomicBool,
    failure: Mutex<Option<TemplateIdsError>>,
}

impl Tally {
    fn stopping(&self, shutdown: &AtomicBool) -> bool {
        self.failed.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
    }

    /// Keep the first failure and stop every thread.
    fn fail(&self, error: TemplateIdsError) {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(error);
        self.failed.store(true, Ordering::Release);
    }

    /// Count a read file; returns the files read so far.
    fn record(&self, columns: IdColumns, max: Option<u64>) -> u64 {
        let slot = match columns {
            IdColumns::Data => &self.data_max,
            IdColumns::Audit => &self.audit_max,
        };
        if let Some(id) = max {
            slot.fetch_max(id + 1, Ordering::AcqRel);
        }
        self.files_read.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn files_read(&self) -> u64 {
        self.files_read.load(Ordering::Acquire)
    }

    /// The scan's result once every thread has exited: a failure, or a
    /// floor only when the whole listing was walked, every listed file was
    /// read, and no shutdown arrived meanwhile, even during the last read.
    fn outcome(self, shutdown: &AtomicBool) -> Result<BootstrapScan, TemplateIdsError> {
        let files_scanned = self.files_read.into_inner();
        let every_file_read =
            self.listing_complete.into_inner() && files_scanned == self.files_listed.into_inner();
        let stopped = shutdown.load(Ordering::Acquire) || !every_file_read;
        let failure = self
            .failure
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        match failure {
            Some(error) => Err(error),
            None if stopped => Err(TemplateIdsError::Interrupted { files_scanned }),
            None => {
                let max = |slot: AtomicU64| slot.into_inner().checked_sub(1);
                Ok(BootstrapScan {
                    data_max: max(self.data_max),
                    audit_max: max(self.audit_max),
                    files_scanned,
                })
            }
        }
    }
}

type Keys = Mutex<Receiver<(String, IdColumns)>>;

/// What every reader thread works from.
struct Reading<'a> {
    store: &'a Store,
    tally: &'a Tally,
    shutdown: &'a AtomicBool,
    keys: &'a Keys,
}

/// The scan itself; `progress` is called with each multiple of
/// [`PROGRESS_EVERY`] files read, and with the files read so far whenever
/// `options.progress_interval` passes without a call.
fn scan(
    store: &Store,
    options: &ScanOptions,
    progress: &mut dyn FnMut(u64),
) -> Result<BootstrapScan, TemplateIdsError> {
    let readers = options.concurrency.max(1);
    let shutdown = options.shutdown.as_ref();
    let tally = Tally::default();
    let (keys_tx, keys_rx) = mpsc::sync_channel(readers);
    let keys: Keys = Mutex::new(keys_rx);
    // Readers send each multiple of PROGRESS_EVERY they complete; the
    // channel disconnects once every scan thread has exited.
    let (milestones, reached) = mpsc::channel::<u64>();
    std::thread::scope(|scope| {
        let mut started = 0;
        for _ in 0..readers {
            let milestones = milestones.clone();
            let reading = Reading {
                store,
                tally: &tally,
                shutdown,
                keys: &keys,
            };
            let reader = std::thread::Builder::new()
                .name("template-id-scan".to_owned())
                .spawn_scoped(scope, move || read_footers(&reading, &milestones));
            match reader {
                Ok(_) => started += 1,
                // Fewer readers only lowers the concurrency.
                Err(_) if started > 0 => break,
                Err(e) => return tally.fail(TemplateIdsError::ScanThread(e)),
            }
        }
        // Readers first: a lister with no reader would block on a full
        // channel forever.
        let (lister_running, lister_tally) = (milestones.clone(), &tally);
        let lister = std::thread::Builder::new()
            .name("template-id-list".to_owned())
            .spawn_scoped(scope, move || {
                let _running = lister_running;
                list_keys(store, lister_tally, shutdown, &keys_tx);
            });
        if let Err(e) = lister {
            return tally.fail(TemplateIdsError::ScanThread(e));
        }
        drop(milestones);
        watch(&tally, options.progress_interval, &reached, progress);
    });
    tally.outcome(shutdown)
}

/// Feed every `*.parquet` key under each prefix to the readers, until the
/// listing ends, a listing fails or the scan stops.
fn list_keys(
    store: &Store,
    tally: &Tally,
    shutdown: &AtomicBool,
    keys: &SyncSender<(String, IdColumns)>,
) {
    for (prefix, columns) in PREFIXES {
        let walked = walk(store, prefix, &mut |key| {
            if tally.stopping(shutdown) || keys.send((key.to_owned(), columns)).is_err() {
                return false;
            }
            tally.files_listed.fetch_add(1, Ordering::AcqRel);
            true
        });
        match walked {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => return tally.fail(e),
        }
    }
    tally.listing_complete.store(true, Ordering::Release);
}

/// Read footers from `keys` until the lister hangs up. Once the scan stops,
/// keys are still taken but no longer read, so a lister blocked on a full
/// channel always gets to see the stop.
fn read_footers(reading: &Reading<'_>, milestones: &mpsc::Sender<u64>) {
    let Reading {
        store,
        tally,
        shutdown,
        keys,
    } = *reading;
    loop {
        let next = keys.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok((key, columns)) = next else {
            return;
        };
        if tally.stopping(shutdown) {
            continue;
        }
        match in_domain(&key, object_max_id(store, &key, columns)) {
            Ok(max) => {
                let files = tally.record(columns, max);
                if files.is_multiple_of(PROGRESS_EVERY) {
                    // The receiver outlives every reader: it is dropped only
                    // after the scope joins them.
                    let _ = milestones.send(files);
                }
            }
            Err(e) => tally.fail(e),
        }
    }
}

/// Report progress from the calling thread until every scan thread exits:
/// each multiple of [`PROGRESS_EVERY`] a reader completes, and the files
/// read so far whenever `interval` passes without a report.
fn watch(
    tally: &Tally,
    interval: Duration,
    milestones: &Receiver<u64>,
    progress: &mut dyn FnMut(u64),
) {
    let mut reported_at = Instant::now();
    loop {
        let files = match milestones.recv_timeout(interval.saturating_sub(reported_at.elapsed())) {
            Ok(files) => files,
            Err(RecvTimeoutError::Timeout) => tally.files_read(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        progress(files);
        reported_at = Instant::now();
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
/// time, so no more than one directory's listing is held at once. Returns
/// `false` as soon as `visit` does, without listing further.
fn walk(
    store: &Store,
    prefix: &str,
    visit: &mut impl FnMut(&str) -> bool,
) -> Result<bool, TemplateIdsError> {
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
            if !visit(key) {
                return Ok(false);
            }
        }
        pending.extend(listing.common_prefixes);
    }
    Ok(true)
}

/// Bootstrap the high-water (RFC 0059 §3.5): scan, then create the object
/// at the floor. Nothing is written until the scan completes, so a crash
/// or a shutdown mid-scan leaves no object and the next start scans again.
///
/// # Errors
///
/// [`TemplateIdsError`] when the scan or the write fails, or the scan is
/// interrupted, and [`TemplateIdsError::BootstrapRaceLost`] when another
/// start created the object first: this one has restored snapshots the
/// winner's floor may not cover, and its restart discards them.
pub fn bootstrap(
    store: &Store,
    restored: u64,
    options: &ScanOptions,
) -> Result<Seated, TemplateIdsError> {
    let scan = BootstrapScan::run_with(store, options)?;
    // A shutdown after the scan's last check still writes nothing.
    if options.shutdown.load(Ordering::Acquire) {
        return Err(TemplateIdsError::Interrupted {
            files_scanned: scan.files_scanned,
        });
    }
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
            eprintln!(
                "template-id bootstrap: high-water created at {floor}; this replica writes \
                 {SEATED_MARKER} under its snapshots root once startup recovery completes",
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
mod scan_tests;

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
