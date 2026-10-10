//! The concurrent bootstrap scan against an object store with injected
//! per-GET latency (#932).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, PartitionKey, Writer};

use super::*;
use crate::template_ids::read;

const HOUR_NS: u64 = 3_600_000_000_000;
/// 2026-01-01T00:00:00Z.
const EPOCH_2026: u64 = 1_767_225_600 * 1_000_000_000;

type BoxStream<T> = futures::stream::BoxStream<'static, T>;

/// What the latency store does to each footer read.
#[derive(Default)]
struct Plan {
    /// The latency range, as `(min, spread)`; each key's latency is fixed
    /// by a hash of the key and `seed`, so completion order is shuffled
    /// relative to listing order but reproducible.
    latency: Option<(Duration, Duration)>,
    seed: u64,
    /// This key's read fails.
    failing: Option<String>,
    /// Set `shutdown` once this many reads have started.
    stop_after: Option<(usize, Arc<AtomicBool>)>,
}

impl Plan {
    fn latency_of(&self, key: &str) -> Duration {
        let Some((min, spread)) = self.latency else {
            return Duration::ZERO;
        };
        let mut hasher = DefaultHasher::new();
        (key, self.seed).hash(&mut hasher);
        let nanos = u64::try_from(spread.as_nanos()).expect("spread fits") + 1;
        min + Duration::from_nanos(hasher.finish() % nanos)
    }
}

/// What the latency store saw.
#[derive(Default)]
struct Seen {
    started: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    completed: Mutex<Vec<String>>,
}

struct LatencyStore {
    inner: Arc<dyn ObjectStore>,
    plan: Arc<Plan>,
    seen: Arc<Seen>,
}

impl std::fmt::Debug for LatencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LatencyStore({})", self.inner)
    }
}

impl std::fmt::Display for LatencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LatencyStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for LatencyStore {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let key = location.as_ref();
        if !key.ends_with(".parquet") {
            return self.inner.get_opts(location, options).await;
        }
        let started = self.seen.started.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some((after, shutdown)) = &self.plan.stop_after
            && started >= *after
        {
            shutdown.store(true, Ordering::Release);
        }
        let now = self.seen.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.seen.max_in_flight.fetch_max(now, Ordering::AcqRel);
        tokio::time::sleep(self.plan.latency_of(key)).await;
        self.seen.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.seen
            .completed
            .lock()
            .expect("completed")
            .push(key.to_owned());
        if self.plan.failing.as_deref() == Some(key) {
            return Err(object_store::Error::Generic {
                store: "latency",
                source: "the footer read fails".into(),
            });
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<object_store::Result<Key>>,
    ) -> BoxStream<object_store::Result<Key>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

fn record(template_id: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("t"),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: EPOCH_2026 + template_id * HOUR_NS,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new()],
        body: Some(format!("line {template_id}")),
        confidence: 1.0,
        lossy_flag: true,
    }
}

fn created(template_id: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new("t"),
        timestamp: UNIX_EPOCH + Duration::from_nanos(EPOCH_2026 + template_id * 24 * HOUR_NS),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: format!("line {template_id}"),
            },
        },
    }
}

/// One real data file per id in `data_ids` and one audit file per id in
/// `audit_ids`, as `(prefix, bytes)` templates to copy.
fn templates(data_ids: &[u64], audit_ids: &[u64]) -> Vec<(&'static str, Vec<u8>)> {
    let scratch = Store::in_memory();
    for id in data_ids {
        let record = record(*id);
        let partition = PartitionKey::derive(&record).expect("partition");
        let mut writer = Writer::open_in(&scratch, partition).expect("writer");
        writer.append_records(&[record]).expect("append");
        writer.close().expect("close");
    }
    let mut audit = ParquetAuditSink::new(scratch.clone());
    for id in audit_ids {
        audit.emit(created(*id));
    }
    assert_eq!(audit.write_failures(), 0);
    PREFIXES
        .iter()
        .flat_map(|(prefix, _)| {
            let keys = scratch.list_blocking(Some(prefix)).expect("list");
            keys.into_iter()
                .filter(|key| key.ends_with(".parquet"))
                .map(|key| (*prefix, scratch.get_blocking(&key).expect("get")))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A store of `files` copies of the template files, spread over nested
/// directories `width` wide, plus non-Parquet objects the scan skips.
fn history(files: usize, width: usize, data_ids: &[u64], audit_ids: &[u64]) -> Store {
    let templates = templates(data_ids, audit_ids);
    let store = Store::in_memory();
    for i in 0..files {
        let (prefix, bytes) = &templates[i % templates.len()];
        let (day, hour, file) = (i / (width * width), (i / width) % width, i % width);
        let dir = format!("{prefix}/tenant_id=t/day={day}/hour={hour}");
        store
            .put_blocking(&format!("{dir}/f{file}.parquet"), bytes.clone())
            .expect("put");
        if file == 0 {
            store
                .put_blocking(&format!("{dir}/manifest.json"), b"{}".to_vec())
                .expect("put");
        }
    }
    store
}

fn behind(store: &Store, plan: Plan) -> (Store, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let (plan, observed) = (Arc::new(plan), Arc::clone(&seen));
    let wrapped = store.clone().wrap_backend(move |inner| {
        Arc::new(LatencyStore {
            inner,
            plan,
            seen: observed,
        })
    });
    (wrapped, seen)
}

fn options(concurrency: usize) -> ScanOptions {
    ScanOptions {
        concurrency,
        ..ScanOptions::default()
    }
}

fn latency(min_ms: u64, spread_ms: u64, seed: u64) -> Plan {
    Plan {
        latency: Some((
            Duration::from_millis(min_ms),
            Duration::from_millis(spread_ms),
        )),
        seed,
        ..Plan::default()
    }
}

const DATA_IDS: [u64; 4] = [3, 41, 17, 29];
const AUDIT_IDS: [u64; 3] = [52, 8, 44];

/// The sequential scan cannot finish before the sum of its reads'
/// latencies, so that sum is a floor under its wall time. The concurrent
/// scan over the same latencies finishes in under a tenth of it, with the
/// identical result.
#[test]
fn concurrent_reads_are_ten_times_faster_than_sequential_with_the_same_floor() {
    let base = history(400, 10, &DATA_IDS, &AUDIT_IDS);
    let sequential = BootstrapScan::run_with(&base, &options(1)).expect("sequential");
    let plan = latency(20, 60, 7);
    let sequential_floor_ns: u128 = base
        .list_blocking(None)
        .expect("list")
        .iter()
        .filter(|key| key.ends_with(".parquet"))
        .map(|key| plan.latency_of(key).as_nanos())
        .sum();
    let (slow, seen) = behind(&base, plan);

    let started = Instant::now();
    let concurrent = BootstrapScan::run(&slow).expect("concurrent");
    let elapsed = started.elapsed();

    assert_eq!(concurrent, sequential);
    assert_eq!(
        (
            concurrent.data_max,
            concurrent.audit_max,
            concurrent.files_scanned
        ),
        (Some(41), Some(52), 400)
    );
    assert!(
        elapsed.as_nanos() * 10 <= sequential_floor_ns,
        "{elapsed:?} is not a tenth of the sequential {:?}",
        Duration::from_nanos(u64::try_from(sequential_floor_ns).expect("fits")),
    );
    assert!(
        seen.max_in_flight.load(Ordering::Acquire) <= DEFAULT_SCAN_CONCURRENCY,
        "reads in flight stay bounded"
    );
}

/// Reads complete out of listing order under shuffled latencies, and
/// every order gives the sequential floor.
#[test]
fn completion_order_does_not_change_the_floor() {
    let base = history(300, 6, &DATA_IDS, &AUDIT_IDS);
    let sequential = BootstrapScan::run_with(&base, &options(1)).expect("sequential");
    let (unshuffled, seen) = behind(&base, Plan::default());
    BootstrapScan::run_with(&unshuffled, &options(1)).expect("listing order");
    let listing_order = seen.completed.lock().expect("completed").clone();
    for seed in 0..3 {
        let (store, seen) = behind(&base, latency(0, 6, seed));
        let scan = BootstrapScan::run_with(&store, &options(8)).expect("shuffled");
        let completed = seen.completed.lock().expect("completed").clone();
        assert_ne!(
            completed, listing_order,
            "seed {seed} completed out of order"
        );
        assert_eq!(scan, sequential, "seed {seed}");
    }
}

/// One failing footer read fails the scan closed, and the bootstrap
/// writes no high-water.
#[test]
fn a_single_failing_read_fails_the_scan_and_nothing_is_written() {
    let base = history(400, 8, &DATA_IDS, &AUDIT_IDS);
    let failing = "audit/tenant_id=t/day=3/hour=1/f2.parquet".to_owned();
    assert!(
        base.get_blocking(&failing).is_ok(),
        "the failing key exists"
    );
    let (store, _) = behind(
        &base,
        Plan {
            failing: Some(failing.clone()),
            ..latency(0, 2, 1)
        },
    );

    let err = bootstrap(&store, 0, &ScanOptions::default()).expect_err("fails closed");

    assert!(
        matches!(&err, TemplateIdsError::Scan(e) if e.to_string().contains(&failing)),
        "{err}"
    );
    assert_eq!(read(&base).expect("read"), None, "nothing is written");
}

/// A shutdown mid-scan stops the scan within the reads already in
/// flight, writes nothing, and the next start scans the whole store.
#[test]
fn a_shutdown_mid_scan_returns_promptly_and_the_next_start_scans_again() {
    let base = history(2_000, 10, &DATA_IDS, &AUDIT_IDS);
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop_after = 40;
    let (store, seen) = behind(
        &base,
        Plan {
            stop_after: Some((stop_after, Arc::clone(&shutdown))),
            ..latency(5, 5, 3)
        },
    );
    let options = ScanOptions {
        shutdown,
        ..ScanOptions::default()
    };

    let started = Instant::now();
    let err = bootstrap(&store, 0, &options).expect_err("interrupted");
    let elapsed = started.elapsed();

    let TemplateIdsError::Interrupted { files_scanned } = err else {
        panic!("{err}");
    };
    let reads = seen.started.load(Ordering::Acquire);
    assert!(
        reads <= stop_after + DEFAULT_SCAN_CONCURRENCY,
        "{reads} reads started after a stop at {stop_after}"
    );
    assert!(files_scanned < 2_000);
    assert!(elapsed < Duration::from_secs(2), "stopped in {elapsed:?}");
    assert_eq!(read(&base).expect("read"), None, "nothing is written");

    let seated = bootstrap(&base, 0, &ScanOptions::default()).expect("the next start");
    assert_eq!(seated.high_water, 52);
}

/// Progress is reported by time, long before the 10,000-file mark.
#[test]
fn progress_is_reported_by_time() {
    let base = history(120, 6, &DATA_IDS, &AUDIT_IDS);
    let (store, _) = behind(&base, latency(10, 0, 0));
    let options = ScanOptions {
        concurrency: 2,
        progress_interval: Duration::from_millis(50),
        ..ScanOptions::default()
    };
    let mut reports = Vec::new();

    let scan = scan(&store, &options, &mut |files| reports.push(files)).expect("scan");

    assert_eq!(scan.files_scanned, 120);
    assert!(reports.len() >= 5, "{reports:?}");
    assert!(reports.windows(2).all(|w| w[0] <= w[1]), "{reports:?}");
}
