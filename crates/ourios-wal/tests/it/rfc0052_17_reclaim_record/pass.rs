//! RFC0052.17 — the rows a housekeeping *pass* decides, and the rows
//! that read a tenant's snapshot.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it. All of
//! these need `SnapshotHorizons`, which §3.7 puts on
//! `housekeeping_prepare` — or, for the `PUBLISHED` rows, a file whose
//! writer and format are RFC 0053's.

/// Scenario RFC0052.17 — entry present, snapshot undecodable: halt naming the tenant.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (halt-or-pin reads SnapshotHorizons, which arrives with housekeeping_prepare)"]
fn rfc0052_17_entry_with_undecodable_snapshot_halts_naming_the_tenant() {
    todo!(
        "RFC0052.17 — a root housekeeping has reclaimed from under \
         per-tenant horizons, RECLAIM holding an entry per reclaimed \
         tenant; restart with one tenant's snapshot undecodable: \
         recovery halts naming that tenant; with a restorable snapshot \
         instead, recovery proceeds"
    );
}
/// Scenario RFC0052.17 — no entry, snapshot undecodable: pin at the oldest surviving frame.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (the pin is a RetainFloor case, which the pass derives)"]
fn rfc0052_17_no_entry_with_undecodable_snapshot_pins_the_tenant() {
    todo!(
        "RFC0052.17 — the record holds no entry for the tenant whose \
         snapshot is undecodable: recovery proceeds with that tenant \
         pinned at its oldest surviving frame rather than halting"
    );
}
/// Scenario RFC0052.17 — entries are monotone across passes.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (two passes under per-tenant horizons need housekeeping_prepare/commit)"]
fn rfc0052_17_a_pass_never_lowers_an_entry_or_touches_another_tenant() {
    todo!(
        "RFC0052.17 — two passes, the second under a higher horizon for \
         one tenant: that tenant's entry rises, every other entry is \
         unchanged, and no pass ever lowers an entry"
    );
}
/// Scenario RFC0052.17 — crash between the record write and the first unlink.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (the record-then-unlink ordering is the pass's, and the restart leg reads snapshots)"]
fn rfc0052_17_crash_after_record_write_before_first_unlink_restarts_cleanly() {
    todo!(
        "RFC0052.17 — a crash injected between the record's write and \
         the first unlink leaves a record whose entries every restorable \
         snapshot satisfies, so the restart proceeds"
    );
}
/// Scenario RFC0052.17 — a pass inside the migration window is a skipped pass that still sweeps.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (skipped pass with reason; partial sweep independent of the ledger)"]
fn rfc0052_17_pass_in_the_migration_window_is_skipped_but_sweeps_partials() {
    todo!(
        "RFC0052.17 — a housekeeping pass on a still-version-1 root \
         plans no segment, writes no record and is counted as a skipped \
         pass with its reason, while still sweeping stale .wal.partial \
         files; the pass after the first checkpoint reclaims normally"
    );
}
/// Scenario RFC0052.17 — consumer mode is recorded, refused on disagreement, adopted on first use.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (the mode arrives with SnapshotHorizons at housekeeping_prepare and is adopted by the first pass)"]
fn rfc0052_17_consumer_mode_is_persisted_and_disagreement_is_refused() {
    todo!(
        "RFC0052.17 — a WAL used with no miner state reclaims under \
         SnapshotHorizons::NoConsumer and restarts without a snapshot \
         without halting; a pass whose mode disagrees with the recorded \
         one is refused as a ReclaimError naming both modes before \
         anything is planned, including on a root that checkpointed but \
         never reclaimed; a header carrying no mode adopts the first \
         pass's mode durably before that pass unlinks anything"
    );
}
/// Scenario RFC0052.17 — a legacy root rotating before its first checkpoint stays openable.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the rotation green slice C (record written and fsynced before the version-2 segment is created)"]
fn rfc0052_17_legacy_root_rotation_writes_the_record_before_the_v2_segment() {
    todo!(
        "RFC0052.17 — a legacy root rotates before its first checkpoint \
         with a crash injected between the record write and the \
         version-2 segment's creation: no restart finds a version-2 \
         segment beside no record, and open succeeds"
    );
}
/// Scenario RFC0052.17 — a failed unlink or uncertain deletion never raises `reclaimed_through`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (ReclaimOutcome::Unlinked{removed,failed,fsync_failed}; reconciliation at open)"]
fn rfc0052_17_failed_unlink_and_uncertain_deletion_are_reconciled() {
    todo!(
        "RFC0052.17 — a segment whose unlink fails after the record was \
         written stays on disk with reclaimed_through behind it and a \
         restart with that tenant's snapshot undecodable pins rather \
         than halts; a segment whose parent fsync fails after its unlink \
         keeps reclaimed_through behind it and its bytes counted, the \
         next pass re-verifies presence, and a restart reconciles it \
         (present ⇒ retained and re-planned, absent ⇒ reclaimed_through \
         raised, no halt) under both outcomes of the injected failure; \
         a crash between the record write and the commit is reconciled \
         the same way with the reconciled record durable before the \
         first pass"
    );
}
/// Scenario RFC0052.17 — a failed record write unlinks nothing and loses nothing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (popped segments return to the eligible queue on a failed write)"]
fn rfc0052_17_failed_record_write_unlinks_nothing_and_segments_are_reclaimed_later() {
    todo!(
        "RFC0052.17 — a record write or fsync that fails unlinks \
         nothing, leaves the WAL's byte and segment accounting \
         unchanged, and the segments that pass popped are reclaimed by a \
         later pass once the write succeeds — never lost to the ledger"
    );
}
