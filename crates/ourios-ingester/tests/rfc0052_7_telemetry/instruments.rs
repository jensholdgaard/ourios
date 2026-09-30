//! Every WAL and barrier instrument is exported under its registry name.

use std::collections::BTreeMap;
use std::sync::Arc;

use opentelemetry_sdk::metrics::data::ResourceMetrics;
use ourios_ingester::barrier::CutOutcome;
use ourios_semconv as semconv;

use crate::drivers::{cut, housekeeper_of, tick};
use crate::harness::{harness, serial};
use crate::metric_read::{Value, counted, points, states, value};
use crate::rfc0052_barrier_support::BarrierRig;

/// Every RFC0052.7 instrument: the WAL state and the barrier's.
const INSTRUMENTS: [&str; 19] = [
    semconv::OURIOS_WAL_SIZE,
    semconv::OURIOS_WAL_UNFLUSHED_SIZE,
    semconv::OURIOS_WAL_SEGMENT_COUNT,
    semconv::OURIOS_WAL_UNRECLAIMED_SIZE,
    semconv::OURIOS_WAL_UNRECLAIMED_AGE,
    semconv::OURIOS_WAL_RETAIN_FLOOR_STATUS,
    semconv::OURIOS_WAL_RETAIN_FLOOR_PINNED_TENANT_COUNT,
    semconv::OURIOS_WAL_RETAIN_FLOOR_LAG_SIZE,
    semconv::OURIOS_WAL_RETAIN_FLOOR_LAG_SEGMENT_COUNT,
    semconv::OURIOS_WAL_ROTATION_STATUS,
    semconv::OURIOS_WAL_ROTATION_CONSECUTIVE_FAILURES,
    semconv::OURIOS_WAL_HOUSEKEEPING_HORIZON_REMAINING,
    semconv::OURIOS_WAL_HOUSEKEEPING_UNLINK_REMAINING,
    semconv::OURIOS_INGEST_BARRIER_CUTS,
    semconv::OURIOS_INGEST_BARRIER_CAPTURES,
    semconv::OURIOS_INGEST_BARRIER_CHECKPOINT_WRITES,
    semconv::OURIOS_INGEST_BARRIER_SNAPSHOT_WRITES,
    semconv::OURIOS_INGEST_BARRIER_EPOCH,
    semconv::OURIOS_INGEST_BARRIER_FAILED_EPOCH,
];

/// The barrier counters carry their outcomes, and successes carry no
/// error.type. Counters are cumulative for the process, so one cut is
/// read as the difference between the collections either side of it.
fn assert_one_cut_counted(before: &[ResourceMetrics], after: &[ResourceMetrics]) {
    for (name, key, want) in [
        (
            semconv::OURIOS_INGEST_BARRIER_CUTS,
            semconv::OURIOS_INGEST_BARRIER_CUT_OUTCOME,
            Some("stamped"),
        ),
        (
            semconv::OURIOS_INGEST_BARRIER_CAPTURES,
            semconv::OURIOS_INGEST_BARRIER_CAPTURE_OUTCOME,
            Some("filled"),
        ),
        (
            semconv::OURIOS_INGEST_BARRIER_CHECKPOINT_WRITES,
            "error.type",
            None,
        ),
        (
            semconv::OURIOS_INGEST_BARRIER_SNAPSHOT_WRITES,
            "error.type",
            None,
        ),
    ] {
        assert_eq!(
            counted(after, name, key, want) - counted(before, name, key, want),
            1,
            "{name} {key}={want:?} counts the one cut",
        );
    }
}

/// The unreclaimed bytes still grow with no checkpoint: the figure is
/// every unreclaimed frame, not the below-checkpoint one, which would read
/// flat through exactly this outage.
fn assert_unreclaimed_grows(first: &[ResourceMetrics], second: &[ResourceMetrics]) {
    let (Value::I(before), Value::I(after)) = (
        value(first, semconv::OURIOS_WAL_UNRECLAIMED_SIZE),
        value(second, semconv::OURIOS_WAL_UNRECLAIMED_SIZE),
    ) else {
        panic!("ourios.wal.unreclaimed.size is an i64 UpDownCounter");
    };
    assert!(
        after > before && before > 0,
        "unreclaimed bytes grow with no checkpoint: {before} -> {after}"
    );
    let Value::F(age) = value(second, semconv::OURIOS_WAL_UNRECLAIMED_AGE) else {
        panic!("ourios.wal.unreclaimed.age is an f64 gauge");
    };
    assert!(age >= 0.0, "the oldest unreclaimed frame has an age");
}

/// The state metrics report every member, 1 for the current state and 0
/// for the others, and the latch is the failed epoch beside the barrier
/// epoch.
fn assert_state_metrics(rms: &[ResourceMetrics]) {
    assert_eq!(
        states(
            rms,
            semconv::OURIOS_WAL_RETAIN_FLOOR_STATUS,
            semconv::OURIOS_WAL_RETAIN_FLOOR_STATE
        ),
        BTreeMap::from([
            ("min".to_owned(), 1),
            ("none".to_owned(), 0),
            ("pinned".to_owned(), 0),
            ("unknown".to_owned(), 0),
        ]),
        "the cut installed the tenant's snapshot, so the floor is its minimum",
    );
    assert_eq!(
        states(
            rms,
            semconv::OURIOS_WAL_ROTATION_STATUS,
            semconv::OURIOS_WAL_ROTATION_STATE
        ),
        BTreeMap::from([
            ("healthy".to_owned(), 1),
            ("retrying".to_owned(), 0),
            ("terminal".to_owned(), 0),
        ]),
    );

    // The latch is exported as the failed epoch beside the barrier epoch.
    let (Value::U(failed), Value::U(epoch)) = (
        value(rms, semconv::OURIOS_INGEST_BARRIER_FAILED_EPOCH),
        value(rms, semconv::OURIOS_INGEST_BARRIER_EPOCH),
    ) else {
        panic!("the epoch gauges are u64");
    };
    assert_eq!(
        failed, epoch,
        "the latch was reported at the current epoch just before collection"
    );
}

/// Scenario RFC0052.7 — every registry name is in the exported stream.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_every_wal_instrument_is_exported_under_its_registry_name() {
    let _serial = serial().await;
    let harness = harness();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let housekeeper = housekeeper_of(&rig);

    // Given a running node whose checkpoint never advances: frames land,
    // housekeeping passes, and no cut ever stamps.
    rig.ingest("checkout", &["user 1 logged in"]).await;
    tick(&housekeeper).await;
    let first = harness.collect();
    rig.ingest("checkout", &["user 2 logged in", "user 3 logged in"])
        .await;
    tick(&housekeeper).await;
    let second = harness.collect();

    assert_eq!(rig.commits.last_checkpoint(), None, "nothing stamped");
    assert_unreclaimed_grows(&first, &second);

    // When a cut stamps and the latch is set, and housekeeping passes.
    assert_eq!(cut(&rig, false).await, CutOutcome::Stamped);
    rig.epochs.report(rig.epochs.current());
    tick(&housekeeper).await;
    let rms = harness.collect();

    // Then every WAL and barrier instrument is under its registry name.
    for name in INSTRUMENTS {
        assert!(
            !points(&rms, name).is_empty(),
            "{name} is missing from the exported stream"
        );
    }

    assert_state_metrics(&rms);

    assert_one_cut_counted(&second, &rms);
}

/// Scenario RFC0052.7 — a cut whose checkpoint write fails is not counted
/// as `stamped`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.5.
///
/// The cut still decides `Stamped` (RFC0052.1: the WAL keeps a usable
/// mark), but the registry's `stamped` means the checkpoint advanced, so
/// the cut is recorded as `checkpoint_failed` and the write carries its
/// `error.type`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_a_failed_checkpoint_write_is_not_counted_as_stamped() {
    let _serial = serial().await;
    let harness = harness();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.sabotage_checkpoint();
    let before = harness.collect();

    assert_eq!(cut(&rig, false).await, CutOutcome::Stamped);
    let after = harness.collect();

    let delta =
        |name, key, want| counted(&after, name, key, want) - counted(&before, name, key, want);
    let outcome = semconv::OURIOS_INGEST_BARRIER_CUT_OUTCOME;
    assert_eq!(
        delta(
            semconv::OURIOS_INGEST_BARRIER_CUTS,
            outcome,
            Some("checkpoint_failed")
        ),
        1
    );
    assert_eq!(
        delta(
            semconv::OURIOS_INGEST_BARRIER_CUTS,
            outcome,
            Some("stamped")
        ),
        0,
        "a failed checkpoint write never reads as a stamp"
    );
    assert_eq!(
        delta(
            semconv::OURIOS_INGEST_BARRIER_CHECKPOINT_WRITES,
            "error.type",
            Some("io")
        ),
        1
    );
}
