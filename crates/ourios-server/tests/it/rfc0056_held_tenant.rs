//! RFC 0056 — audit-sink durability on permanent write failure.
//! See `docs/rfcs/0056-audit-durability.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC
//! is red; each names the slice that discharges it. RFC0056.1 and .2
//! land beside the coordinator in `ourios-ingester`, RFC0056.8 beside
//! the `CHECKPOINT` codec in `ourios-wal`, and the rest here.

/// Scenario RFC0056.1 — a permanent audit write failure holds the tenant.
#[test]
#[ignore = "RFC0056.1 stub — implemented with the held-tenant slice (coordinator and sinks)"]
fn rfc0056_1_a_permanent_audit_write_failure_holds_the_tenant() {
    todo!(
        "RFC0056.1 — B publishes, A is held and its buffered records and events are \
         dropped; an inline size or ceiling flush holds it too; a hold at the ceiling \
         frees A's bytes; A's retained events drop with the hold"
    );
}

/// Scenario RFC0056.2 — a derive failure takes the same path.
#[test]
#[ignore = "RFC0056.2 stub — implemented with the held-tenant slice (coordinator and sinks)"]
fn rfc0056_2_a_derive_failure_takes_the_same_path() {
    todo!("RFC0056.2 — an event that does not derive a partition holds its tenant");
}

/// Scenario RFC0056.3 — a held tenant's appends are refused, retryably.
#[test]
#[ignore = "RFC0056.3 stub — implemented with the receiver's admission slice"]
fn rfc0056_3_a_held_tenants_appends_are_refused_retryably() {
    todo!("RFC0056.3 — UNAVAILABLE and 503 naming A, before the WAL append; B accepted");
}

/// Scenario RFC0056.4 — a snapshot installed by a cut that did not stamp.
#[test]
#[ignore = "RFC0056.4 stub — implemented with the held-floor slice (CHECKPOINT v3 and recovery)"]
fn rfc0056_4_a_snapshot_installed_by_a_cut_that_did_not_stamp() {
    todo!("RFC0056.4 — A's floor is its installed snapshot, not the earlier stamp");
}

/// Scenario RFC0056.5 — the checkpoint advances and records the floor.
#[test]
#[ignore = "RFC0056.5 stub — implemented with the held-floor slice (CHECKPOINT v3 and recovery)"]
fn rfc0056_5_the_checkpoint_advances_and_records_the_floor() {
    todo!(
        "RFC0056.5 — the sidecar holds X1 and A's floor X0; A's frames above X0 are kept \
         and a B-only segment is reclaimed"
    );
}

/// Scenario RFC0056.6 — a restart after repair publishes the held records.
#[test]
#[ignore = "RFC0056.6 stub — implemented with the held-floor slice (CHECKPOINT v3 and recovery)"]
fn rfc0056_6_a_restart_after_repair_publishes_the_held_records() {
    todo!("RFC0056.6 — A's records above X0 publish under regenerated events; B's do not again");
}

/// Scenario RFC0056.7 — a tenant held before the first checkpoint.
#[test]
#[ignore = "RFC0056.7 stub — implemented with the held-floor slice (CHECKPOINT v3 and recovery)"]
fn rfc0056_7_a_tenant_held_before_the_first_checkpoint() {
    todo!("RFC0056.7 — an absent floor replays all of A's frames and reclaims none of them");
}

/// Scenario RFC0056.8 — a version-2 root upgrades.
#[test]
#[ignore = "RFC0056.8 stub — implemented with the CHECKPOINT v3 codec in ourios-wal"]
fn rfc0056_8_a_version_2_root_upgrades() {
    todo!("RFC0056.8 — v2 reads as floorless, the next stamp writes v3, the witness holds");
}

/// Scenario RFC0056.9 — a snapshot past the floor after a repaired restart.
#[test]
#[ignore = "RFC0056.9 stub — implemented with the held-floor slice (CHECKPOINT v3 and recovery)"]
fn rfc0056_9_a_snapshot_past_the_floor_after_a_repaired_restart() {
    todo!("RFC0056.9 — recovery routes A by its snapshot; the next stamp drops the floor");
}

/// Scenario RFC0056.10 — a restart into an unrepaired fault holds the tenant again.
#[test]
#[ignore = "RFC0056.10 stub — implemented with the bounded-recovery slice"]
fn rfc0056_10_a_restart_into_an_unrepaired_fault_holds_the_tenant_again() {
    todo!(
        "RFC0056.10 — A held again with floor X0; nothing of A's publishes without its \
         events; neither sink passes its bound during recovery"
    );
}
