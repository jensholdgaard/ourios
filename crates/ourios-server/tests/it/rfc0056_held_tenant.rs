//! RFC 0056 — audit-sink durability on permanent write failure.
//! See `docs/rfcs/0056-audit-durability.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC
//! is red; each names the slice that discharges it. RFC0056.1 and .2
//! are in `ourios-ingester` (`src/publish.rs`), beside the coordinator.
//! RFC0056.8 is in `ourios-wal`: the codec's tests in `src/checkpoint.rs`
//! and the version-3 successors in `tests/it/rfc0052_17_reclaim_record/`.
//! RFC0056.3 is in `ourios-ingester`'s `tests/it/rfc0056_3_held_admission.rs`,
//! over both transports.
//! RFC0056.11 is in `ourios-ingester` too: `src/audit_sink.rs`'s
//! `a_hold_logs_hold_started_once` and `tests/audit_sink_metrics.rs`.
//! RFC0056.4–.7 and .9 are in `ourios-ingester`'s
//! `tests/it/rfc0056_held_floors.rs`, on the barrier rig beside the
//! RFC 0052 barrier tests, restarting it over its roots.

/// Scenario RFC0056.10 — a restart into an unrepaired fault holds the tenant again.
#[test]
#[ignore = "RFC0056.10 stub — implemented with the bounded-recovery slice"]
fn rfc0056_10_a_restart_into_an_unrepaired_fault_holds_the_tenant_again() {
    todo!(
        "RFC0056.10 — A held again with floor X0; nothing of A's publishes without its \
         events; neither sink passes its bound during recovery"
    );
}
