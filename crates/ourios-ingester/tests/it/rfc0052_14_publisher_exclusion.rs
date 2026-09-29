//! RFC0052.14 — The exclusion is held across encodes, never a PUT.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The legs where the publisher thread (§3.1) is what keeps store I/O
//! out of the ingest exclusion: a pre-cut batch's detached partitions
//! are written after the capture releases it, and neither their PUTs nor
//! an audit write is waited on while it is held. The cut-ordering legs of
//! the same criterion are [`crate::rfc0052_14_timer_exclusion`].

use std::sync::Arc;
use std::time::Duration;

use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{BarrierRig, Gate};

/// 2026-04-02T10:58:00Z, and an hour — two records an hour apart fall in
/// two partitions.
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;

/// Scenario RFC0052.14 — a pre-cut batch that detached mid-batch is fully in the cut.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_pre_cut_batch_detaching_mid_batch_is_covered_without_waiting_on_the_put() {
    // Given an acknowledged batch whose records fall in two partitions
    // (two hours), each detached by the size trigger as the worker
    // reaches it: the first partition's PUT goes through, the second's is
    // held — so the batch is past its encode phase with one detached
    // partition durable and one not.
    let tmp = tempfile::TempDir::new().expect("temp");
    let put = Gate::letting_through(1);
    let rig = Arc::new(BarrierRig::with_held_puts(tmp.path(), &put));
    let _release = put.opened_on_drop();
    let mut group = resource_logs("checkout", &["user 1 logged in", "user 2 logged in"]);
    for (hour, record) in group.scope_logs[0].log_records.iter_mut().enumerate() {
        record.time_unix_nano = TS0 + u64::try_from(hour).expect("hour") * HOUR_NS;
    }
    rig.pipeline
        .ingest(request(vec![group]), TenantId::new("checkout"))
        .await
        .expect("the batch acks");
    let mark = rig.pipeline.last_durable().expect("a durable mark");
    put.await_entered(2);

    // When a tick starts, and has opened its cut: the next turn must be
    // admitted after the capture, or its frame would be in this cut.
    let before = rig.epochs.current();
    let tick = {
        let rig = Arc::clone(&rig);
        tokio::task::spawn_blocking(move || rig.barrier.tick(&rig.pipeline, false))
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        while rig.epochs.current() == before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the tick opened its cut");

    // Then the capture does not wait for the held PUT: the quiesce
    // covers the batch's encode phase only, so the exclusion is released
    // and the next turn is admitted while the tick is still waiting...
    tokio::time::timeout(
        Duration::from_secs(30),
        rig.ingest("checkout", &["user 3 logged in"]),
    )
    .await
    .expect("ingest is not stalled behind the held PUT");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    // ...while the cut itself waits, because the batch's one shared
    // completion is held until its *last* detached partition finishes —
    // the first being durable does not release it.
    assert!(
        !tick.is_finished(),
        "the cut waits for the batch's last partition"
    );
    assert_eq!(rig.data_files().len(), 1, "one partition is durable");
    assert_eq!(
        rig.commits.last_checkpoint(),
        None,
        "and nothing is stamped"
    );

    // And once the held PUT lands, the cut stamps at the pre-cut batch's
    // mark with both of its partitions under it.
    put.open();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), tick)
            .await
            .expect("the tick finished once the PUT landed")
            .expect("the tick did not panic"),
        CutOutcome::Stamped,
    );
    assert_eq!(rig.commits.last_checkpoint(), Some(mark));
    assert!(rig.data_files().len() >= 2, "both partitions are durable");
}

/// Scenario RFC0052.14 — no audit-store PUT is waited on inside the exclusion.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// A partition's template events must be durable before its records are
/// written, and making them durable is a store write. Done on the encode
/// worker's inline barrier, a capture's `quiesce_encodes` would wait out
/// that write while holding the exclusion; the receiver's barrier
/// (`settled`) only reads the ledger, and the write happens in the cut's
/// own flush, outside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_a_held_audit_put_does_not_stall_admission() {
    // Given an acknowledged batch, wired as the receiver wires it, whose
    // template event is still buffered — so its partition waits over the
    // size target — and an audit store whose writes are held.
    let tmp = tempfile::TempDir::new().expect("temp");
    let put = Gate::default();
    let rig = Arc::new(BarrierRig::with_held_audit_puts(tmp.path(), &put));
    let _release = put.opened_on_drop();
    let mark = rig.ingest("checkout", &["user 1 logged in"]).await;

    // When a tick starts and opens its cut, whose own flush then holds on
    // the audit write.
    let before = rig.epochs.current();
    let tick = {
        let rig = Arc::clone(&rig);
        tokio::task::spawn_blocking(move || rig.barrier.tick(&rig.pipeline, false))
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        while rig.epochs.current() == before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the tick opened its cut");

    // Then the next turn is admitted while the audit write is still held:
    // the capture waited on the encode phase, which does no store I/O.
    tokio::time::timeout(
        Duration::from_secs(30),
        rig.ingest("checkout", &["user 2 logged in"]),
    )
    .await
    .expect("ingest is not stalled behind the held audit PUT");
    tokio::task::spawn_blocking({
        let put = put.clone();
        move || put.await_entered(1)
    })
    .await
    .expect("the cut's audit write reached the held store");
    assert!(!tick.is_finished(), "the cut still waits for the publish");
    assert_eq!(
        rig.commits.last_checkpoint(),
        None,
        "and nothing is stamped"
    );
    assert!(rig.data_files().is_empty(), "no record ahead of its event");

    // And once the audit write lands, the cut stamps at the batch's mark.
    put.open();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), tick)
            .await
            .expect("the tick finished once the audit PUT landed")
            .expect("the tick did not panic"),
        CutOutcome::Stamped,
    );
    assert_eq!(rig.commits.last_checkpoint(), Some(mark));
    assert!(
        !rig.data_files().is_empty(),
        "the record followed its event"
    );
}
