//! RFC0052.17 — The reclaim record is the only startup witness, and it is
//! fail-closed.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! The §6 fixture matrix — the four record states (absent, empty,
//! valid with entries, corrupt) crossed with a restorable and an
//! undecodable snapshot — plus the crash points through the same
//! fault-injection hook the rotation tests use, the legacy-root
//! migration rows, the header-flag rows, the consumer-mode rows and
//! the uncertain-deletion rows. Each stub is one row or one closely
//! related group of rows of §5's `RFC0052.17` list; the matrix legs
//! come first.

/// Scenario RFC0052.17 — entry present, snapshot undecodable: halt naming the tenant.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (valid record with entries × undecodable snapshot)"]
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
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (empty record × undecodable snapshot)"]
fn rfc0052_17_no_entry_with_undecodable_snapshot_pins_the_tenant() {
    todo!(
        "RFC0052.17 — the record holds no entry for the tenant whose \
         snapshot is undecodable: recovery proceeds with that tenant \
         pinned at its oldest surviving frame rather than halting"
    );
}

/// Scenario RFC0052.17 — no record and no checkpoint: a pre-RFC root gains an empty record.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (absent record × never-checkpointed root)"]
fn rfc0052_17_absent_record_and_absent_checkpoint_opens_and_seeds_an_empty_record() {
    todo!(
        "RFC0052.17 — a root with no RECLAIM and no CHECKPOINT at all: \
         Wal::open succeeds, an empty record is durable before the first \
         housekeeping pass, and a tenant without a snapshot pins rather \
         than halts"
    );
}

/// Scenario RFC0052.17 — a corrupt record fails open as `Corrupt`, never as missing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (CRC32-C / version byte checked before any read)"]
fn rfc0052_17_corrupt_record_fails_open_naming_the_file() {
    todo!(
        "RFC0052.17 — a RECLAIM failing its checksum or version byte, \
         with either snapshot state: Wal::open fails as \
         OpenError::Corrupt naming the file, and the record is never \
         read as missing"
    );
}

/// Scenario RFC0052.17 — entries are monotone across passes.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (two-pass leg reading the record back)"]
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
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (record-then-unlink ordering; restart satisfies every entry)"]
fn rfc0052_17_crash_after_record_write_before_first_unlink_restarts_cleanly() {
    todo!(
        "RFC0052.17 — a crash injected between the record's write and \
         the first unlink leaves a record whose entries every restorable \
         snapshot satisfies, so the restart proceeds"
    );
}

/// Scenario RFC0052.17 — a torn slot leaves the previous slot live.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (two slots, generation-wins, CRC per slot)"]
fn rfc0052_17_crash_inside_the_inactive_slot_write_keeps_the_previous_slot() {
    todo!(
        "RFC0052.17 — a crash injected inside the inactive slot's write, \
         or between that write and its fsync: the torn slot fails its \
         CRC or carries the lower generation, the reader takes the other \
         one, and the next pass re-plans from the record the previous \
         pass left"
    );
}

/// Scenario RFC0052.17 — a deleted record on a post-RFC root fails open.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (CHECKPOINT version 2 marks the root post-RFC)"]
fn rfc0052_17_deleted_record_on_a_post_rfc_root_fails_open() {
    todo!(
        "RFC0052.17 — a root that has reclaimed and whose RECLAIM is then \
         deleted: its CHECKPOINT is version 2, so open fails naming the \
         missing record rather than recreating an empty one and pinning"
    );
}

/// Scenario RFC0052.17 — a replayed tenant key over 128 bytes fails open.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (128 B dictionary record; frame codec amended per RFC 0048 §3.1)"]
fn rfc0052_17_overlong_tenant_key_in_replay_fails_open_naming_the_frame() {
    todo!(
        "RFC0052.17 — a frame written before the codec was amended \
         carrying a tenant longer than 128 bytes: replay fails open \
         naming the frame offset and the length, rather than truncating \
         the key or dropping the frame"
    );
}

/// Scenario RFC0052.17 — both sidecars missing: version-2 segments are the witness.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (SEGMENT_VERSION 2 header gates the fail-closed branch)"]
fn rfc0052_17_no_sidecars_fails_closed_only_beside_version_2_segments() {
    todo!(
        "RFC0052.17 — a root holding segments and neither sidecar fails \
         open naming both when any segment header carries version 2; a \
         root whose segments are all version 1 opens on the legacy \
         branch; an empty directory with neither sidecar opens as a \
         fresh root; a NoConsumer root behaves the same, its version-2 \
         segments being its only witness"
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

/// Scenario RFC0052.17 — geometry growth rebuilds through `RECLAIM.new`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (write, fsync, rename, parent fsync; equal-or-smaller opens in place)"]
fn rfc0052_17_larger_geometry_rebuilds_atomically_smaller_opens_in_place() {
    todo!(
        "RFC0052.17 — a configured max_tenants or max_unlinks_per_pass \
         above the stored capacity rebuilds the file at the larger \
         geometry through RECLAIM.new, a crash at any point leaving \
         either the old or the new file complete with the entries \
         intact; a value at or below the stored capacity opens without \
         rewriting"
    );
}

/// Scenario RFC0052.17 — a full volume fails open, never a rotation state; passes never allocate.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (preallocated at open; slot_len fixed for the file's life)"]
fn rfc0052_17_full_volume_fails_open_and_passes_write_without_allocating() {
    todo!(
        "RFC0052.17 — a full volume at Wal::open fails the allocation as \
         OpenError::Io naming the file and the node starts normally once \
         space is freed; every pass writes its slot without allocating \
         whatever the tenant set does, so a node whose volume filled \
         after open still reclaims its way out"
    );
}

/// Scenario RFC0052.17 — the empty record is durable before the initial segment.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (fresh-root open ordering)"]
fn rfc0052_17_fresh_open_writes_the_record_before_the_initial_segment() {
    todo!(
        "RFC0052.17 — a restart immediately after a fresh Wal::open, with \
         or without a crash between the record write and the initial \
         segment's creation, finds the record and opens normally"
    );
}

/// Scenario RFC0052.17 — the legacy-root migration window.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (version-1 CHECKPOINT fixture; upgrade on the first checkpoint)"]
fn rfc0052_17_version_1_checkpoint_opens_legacy_and_upgrades_on_first_checkpoint() {
    todo!(
        "RFC0052.17 — a root holding a version-1 CHECKPOINT (an \
         RFC0008.7 fixture) and no RECLAIM opens as pre-RFC without \
         creating a record; its first checkpoint rewrites the sidecar at \
         version 2 and creates the record armed on the same path, even \
         when the mark equals the one on disk, while an equal mark on an \
         already-version-2 sidecar takes the no-write path; a restart \
         in the window takes the legacy branch again; a tenant whose \
         snapshot is missing and whose oldest surviving frame is above \
         its last recorded horizon fails closed naming the tenant"
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

/// Scenario RFC0052.17 — the `checkpoint_armed` / `checkpoint_seen` flag rows.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (header_flags bits crossed with CHECKPOINT presence and segments)"]
fn rfc0052_17_checkpoint_flags_beside_a_missing_or_stale_checkpoint() {
    todo!(
        "RFC0052.17 — checkpoint_seen with CHECKPOINT missing fails open \
         naming both files; checkpoint_armed without seen and CHECKPOINT \
         absent opens as a fresh root with an empty record when no \
         segments exist (as does a record with neither flag), and with \
         segments present retains the record with its arming and mode \
         and opens on the legacy branch; an armed record beside a \
         present version-2 CHECKPOINT opens normally and is promoted to \
         seen durably at open; an armed record beside a version-1 \
         CHECKPOINT opens legacy, reclaims nothing until the next \
         checkpoint retries the upgrade, and that retry succeeds"
    );
}

/// Scenario RFC0052.17 — slot ids and the `published_seeded_*` flag rows.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (dictionary union at open; next_slot_id = max of both headers)"]
fn rfc0052_17_slot_ids_survive_a_published_only_write_and_seeding_flags_resolve() {
    todo!(
        "RFC0052.17 — a tenant introduced by a PUBLISHED-only write keeps \
         its slot id across a restart; published_seeded_armed without \
         confirmed and PUBLISHED absent leaves the next start free to \
         seed again, while the same record with PUBLISHED present is \
         promoted to confirmed durably at open, never read as a fault"
    );
}

/// Scenario RFC0052.17 — consumer mode is recorded, refused on disagreement, adopted on first use.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the reclaim-record green slice A (consumer_mode header bit; ReclaimError names both modes)"]
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
