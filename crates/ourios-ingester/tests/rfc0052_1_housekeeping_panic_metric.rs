//! RFC 0052 §3.2: a housekeeping tick that unwinds is counted on the
//! existing cadence counter, `ourios.sink.flush.errors` with
//! `error.type = cadence_panic` — the same dimension the age sweep's
//! panic rides (#791) — and not on a metric of its own.
//!
//! Its own test binary, like `cadence_panic_metric.rs`: the sink's
//! instruments resolve through the **global** meter, and two
//! global-installing tests in one binary would race.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint,
};
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::Barrier;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::{CommitCoordinator, Journal, ReceiveError};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::store::Store;
use ourios_semconv as semconv;
use ourios_wal::{ReclaimError, ReclaimPlan, SnapshotHorizons, WalOffset};

/// A journal whose housekeeping pass unwinds — the one seam a
/// housekeeping tick reaches that no batch guard covers.
struct PanickingHousekeeping;

impl Journal for PanickingHousekeeping {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<WalOffset, ReceiveError> {
        unreachable!("the housekeeping tick appends nothing")
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        unreachable!("the housekeeping tick syncs nothing")
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }

    fn housekeeping_prepare(
        &mut self,
        _horizons: &SnapshotHorizons,
        _max_unlinks: usize,
    ) -> Result<ReclaimPlan, ReclaimError> {
        panic!("injected housekeeping panic")
    }
}

/// A housekeeper over [`PanickingHousekeeping`], counting through a real
/// `PublishCoordinator` — what the receiver hands it.
fn panicking_housekeeper(root: &std::path::Path) -> Housekeeper {
    for leaf in ["records", "audit", "snapshots"] {
        std::fs::create_dir_all(root.join(leaf)).expect("store dir");
    }
    let records = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(root.join("records")).expect("record store"),
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        },
    ));
    let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
        Store::local(root.join("audit")).expect("audit store"),
        1_024,
    ));
    let publish = PublishCoordinator::new(records, audit);
    let journal = CommitCoordinator::new(
        Box::new(PanickingHousekeeping),
        Duration::from_millis(20),
        u64::MAX,
    );
    let barrier = Arc::new(Barrier::new(
        publish.clone(),
        Arc::clone(&journal),
        root.join("snapshots"),
        usize::MAX,
    ));
    Housekeeper::new(journal, barrier, publish, 8)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_housekeeping_panic_is_counted_as_cadence_panic() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let root = tempfile::TempDir::new().expect("root");
    let housekeeper = panicking_housekeeper(root.path());

    let tick = housekeeper.tick();
    assert!(matches!(tick, HousekeepingTick::Panicked), "{tick:?}");
    guard.force_flush().expect("force_flush");

    // Through `Housekeeper::tick` itself, so a tick that caught the panic
    // without counting it fails here.
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let tagged: u64 = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .filter(|m| m.name() == semconv::OURIOS_SINK_FLUSH_ERRORS)
        .filter_map(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum),
            _ => None,
        })
        .flat_map(opentelemetry_sdk::metrics::data::Sum::data_points)
        .filter(|dp| {
            dp.attributes()
                .any(|kv| kv.key.as_str() == "error.type" && kv.value.as_str() == "cadence_panic")
        })
        .map(SumDataPoint::value)
        .sum();
    assert_eq!(tagged, 1, "the housekeeping panic is one cadence_panic");
}
