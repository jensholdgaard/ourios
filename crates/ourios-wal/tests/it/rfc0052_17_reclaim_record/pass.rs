//! RFC0052.17 — the rows a housekeeping *pass* decides, and the rows
//! that read a tenant's snapshot.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! All of these need `SnapshotHorizons`, which §3.7 puts on
//! `housekeeping_prepare`. The one stub left is the legacy-root
//! rotation row, whose crash injection belongs to the rotation slice.

use std::path::PathBuf;

use ourios_wal::{PassOutcome, ReclaimOutcome, SkipReason, SnapshotHorizons, unlink_planned};

use crate::rfc0052_record::{PlannedRow, planned_unlinks, reclaimed_through};
use crate::rfc0052_support::{
    CHECKPOINT, MODE_KNOWN, MODE_NO_CONSUMER, RECLAIM, build_closed_segment, build_tenant_segment,
    checkpoint_version, downgrade_segments, known, live_slot, open, segment_files,
    write_legacy_checkpoint, write_partial,
};

const CAP: usize = 64;

/// Scenario RFC0052.17 — entry present, snapshot undecodable: halt naming the tenant.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_entry_with_undecodable_snapshot_halts_naming_the_tenant() {
    // Given: a root housekeeping has reclaimed from under per-tenant
    // horizons, so `RECLAIM` holds an entry per reclaimed tenant.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1"), ("beta", b"b1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[1]).expect("checkpoint past the first");
    let horizons = known(&[("alpha", first[0]), ("beta", first[1])]);
    assert_eq!(
        wal.housekeeping_pass(&horizons, CAP)
            .expect("housekeeping")
            .removed_segments,
        1,
    );
    drop(wal);

    // When: the node restarts with one tenant's snapshot undecodable —
    // which reaches the WAL as that tenant having no restorable
    // horizon at all. The restart is also where the `planned` list the
    // pass made durable becomes `reclaimed_through`.
    let mut restarted = open(root);
    restarted.rebuild_ledger().expect("ledger");
    assert_eq!(
        reclaimed_through(root).keys().collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "the record holds an entry per reclaimed tenant",
    );
    let failure = restarted
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect_err("beta's entry cannot be satisfied");

    // Then: it halts naming that tenant rather than pinning over
    // frames that are already gone.
    assert!(
        format!("{failure}").contains("beta"),
        "the halt names the tenant: {failure}",
    );

    // And: with a restorable snapshot instead, recovery proceeds.
    restarted
        .housekeeping_prepare(&horizons, CAP)
        .expect("a restorable horizon at or above the entry proceeds");
}

/// Scenario RFC0052.17 — no entry, snapshot undecodable: pin at the oldest surviving frame.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_no_entry_with_undecodable_snapshot_pins_the_tenant() {
    // Given: a pass that reclaimed `alpha`'s segment and never touched
    // `gamma`, so the record holds no entry for `gamma`.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    let second = build_tenant_segment(root, &[("gamma", b"g1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(second[0]).expect("checkpoint past both");
    wal.housekeeping_pass(&known(&[("alpha", first[0])]), CAP)
        .expect("housekeeping");
    drop(wal);

    // When: the node restarts with `gamma`'s snapshot undecodable.
    let mut restarted = open(root);
    restarted.rebuild_ledger().expect("ledger");
    let entries = reclaimed_through(root);
    assert!(entries.contains_key("alpha") && !entries.contains_key("gamma"));
    let plan = restarted
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect("a tenant with no entry has lost nothing");

    // Then: recovery proceeds with that tenant pinned at its oldest
    // surviving frame rather than halting. The reported offset is the
    // minimum over horizons *and* pins, so here it is `alpha`'s
    // horizon; what `Pinned` adds is that a tenant is holding it.
    assert_eq!(
        plan.progress.floor,
        ourios_wal::RetainFloor::Pinned {
            offset: first[0],
            tenants: 1,
        },
    );
    assert!(
        second[0] > first[0],
        "fixture: the pin itself sits above the reported minimum",
    );
    assert!(
        plan.segments.is_empty(),
        "and the pin holds its own segment",
    );
}

/// Scenario RFC0052.17 — entries are monotone across passes.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_a_pass_never_lowers_an_entry_or_touches_another_tenant() {
    // Given: two tenants sharing the oldest segment and one of them
    // alone in the next.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1"), ("beta", b"b1")]);
    let second = build_tenant_segment(root, &[("alpha", b"a2")]);
    build_tenant_segment(root, &[("alpha", b"a3")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(second[0]).expect("checkpoint past both");

    // When: the first pass reclaims only the shared segment.
    wal.housekeeping_pass(&known(&[("alpha", first[0]), ("beta", first[1])]), CAP)
        .expect("housekeeping");
    drop(wal);

    // The restart is what turns the durable `planned` list into
    // `reclaimed_through`; the commit raised it in memory only.
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    let after_first = reclaimed_through(root);
    assert_eq!(
        after_first,
        [
            ("alpha".to_owned(), first[0]),
            ("beta".to_owned(), first[1])
        ]
        .into_iter()
        .collect(),
    );

    // And: a second pass under a higher horizon for one tenant.
    wal.housekeeping_pass(&known(&[("alpha", second[0]), ("beta", first[1])]), CAP)
        .expect("housekeeping");
    drop(wal);

    // Then: that tenant's entry rises and every other entry is
    // unchanged.
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    let after_second = reclaimed_through(root);
    assert_eq!(after_second["alpha"], second[0], "alpha's entry rises");
    assert_eq!(
        after_second["beta"], after_first["beta"],
        "beta's entry is untouched by a pass that reclaimed none of its frames",
    );

    // And: no pass ever lowers an entry — a third pass that reclaims
    // nothing leaves both exactly where they are.
    wal.housekeeping_pass(&known(&[("alpha", second[0]), ("beta", first[1])]), CAP)
        .expect("housekeeping");
    drop(wal);
    drop(open(root));
    assert_eq!(reclaimed_through(root), after_second);
}

/// Scenario RFC0052.17 — crash between the record write and the first unlink.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_crash_after_record_write_before_first_unlink_restarts_cleanly() {
    // Given: a pass whose record write has landed.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let horizons = known(&[("alpha", first[0])]);
    let plan = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    wal.write_plan_record(&plan).expect("record");
    assert_eq!(
        planned_unlinks(root).len(),
        1,
        "the record is durable before any segment is gone",
    );

    // When: the process dies there — before the first unlink.
    drop(wal);
    assert_eq!(segment_files(root).len(), 2, "nothing was unlinked");

    // Then: the restart finds a record whose entries every restorable
    // snapshot satisfies, so it proceeds — the planned segment is
    // still present, so it is retained and its entry dropped rather
    // than read as proof of loss.
    let mut restarted = open(root);
    assert!(planned_unlinks(root).is_empty(), "reconciled at open");
    assert!(
        reclaimed_through(root).is_empty(),
        "a present planned segment raises nothing",
    );
    restarted.rebuild_ledger().expect("ledger");
    let progress = restarted
        .housekeeping_pass(&horizons, CAP)
        .expect("the restart proceeds");
    assert_eq!(
        progress.removed_segments, 1,
        "and the next pass re-plans it"
    );
}

/// Scenario RFC0052.17 — a pass inside the migration window is a skipped pass that still sweeps.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_pass_in_the_migration_window_is_skipped_but_sweeps_partials() {
    // Given: a still-version-1 root — a legitimate pre-RFC layout —
    // with rotation debris beside it.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"a1"]);
    let second = build_closed_segment(root, &[b"b1"]);
    build_closed_segment(root, &[b"c1"]);
    std::fs::remove_file(root.join(RECLAIM)).expect("a pre-RFC root has no record");
    downgrade_segments(root);
    let mark = *second.last().expect("segment two's offsets");
    write_legacy_checkpoint(root, mark);
    let debris = write_partial(root);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");

    // When: a housekeeping pass fires in that window.
    let plan = wal.housekeeping_prepare(&known(&[]), CAP).expect("prepare");

    // Then: it plans no segment, writes no record, and is counted as a
    // skipped pass with its reason.
    assert_eq!(
        plan.progress.outcome,
        PassOutcome::Skipped(SkipReason::MigrationWindow),
    );
    assert_eq!(SkipReason::MigrationWindow.as_str(), "migration_window");
    assert!(plan.segments.is_empty() && !plan.records);
    assert!(
        !root.join(RECLAIM).exists(),
        "no record is created under a version-1 checkpoint",
    );

    // And: it still sweeps stale `.wal.partial` files, so debris from
    // before the first checkpoint does not survive the window.
    let progress = wal
        .housekeeping_commit(unlink_planned(&plan))
        .expect("commit");
    assert_eq!(progress.removed_partials, 1);
    assert!(!debris.exists());
    assert_eq!(segment_files(root).len(), 3, "and no segment was touched");

    // And: the pass after the first checkpoint reclaims normally.
    wal.checkpoint(mark).expect("the upgrade");
    assert_eq!(checkpoint_version(root), 2);
    let after = wal
        .housekeeping_pass(&known(&[]), CAP)
        .expect("housekeeping");
    assert_eq!(after.outcome, PassOutcome::Planned);
    assert_eq!(after.removed_segments, 2);
}

/// Scenario RFC0052.17 — consumer mode is recorded, refused on disagreement, adopted on first use.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_consumer_mode_is_persisted_and_disagreement_is_refused() {
    a_no_miner_wal_reclaims_and_restarts_without_a_snapshot();
    a_pass_that_disagrees_with_the_recorded_mode_is_refused();
    a_root_that_never_reclaimed_still_refuses_a_disagreeing_pass();
    the_first_pass_adopts_its_mode_before_it_unlinks_anything();
}

/// A WAL used with no miner state reclaims under
/// `SnapshotHorizons::NoConsumer` and restarts without a snapshot
/// without halting: its entries carry that mode and are
/// checkpoint-covered.
fn a_no_miner_wal_reclaims_and_restarts_without_a_snapshot() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    assert_eq!(
        wal.housekeeping_pass(&SnapshotHorizons::NoConsumer, CAP)
            .expect("housekeeping")
            .removed_segments,
        1,
    );
    drop(wal);
    assert_eq!(
        live_slot(&std::fs::read(root.join(RECLAIM)).expect("read RECLAIM")).2,
        MODE_NO_CONSUMER,
        "the root records the mode every pass on it ran under",
    );

    let mut restarted = open(root);
    restarted.rebuild_ledger().expect("ledger");
    restarted
        .housekeeping_prepare(&SnapshotHorizons::NoConsumer, CAP)
        .expect("a no-miner root restarts without a snapshot and without halting");
}

/// A pass whose mode disagrees with the recorded one is refused as a
/// `ReclaimError` naming both modes, before anything is planned.
fn a_pass_that_disagrees_with_the_recorded_mode_is_refused() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    wal.housekeeping_pass(&SnapshotHorizons::NoConsumer, CAP)
        .expect("housekeeping");

    let before = segment_files(root);
    let failure = wal
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect_err("a Known pass on a NoConsumer root must be refused");
    let text = format!("{failure}");
    assert!(
        text.contains("NoConsumer") && text.contains("Known"),
        "the refusal names both modes: {text}",
    );
    assert_eq!(segment_files(root), before, "and nothing was planned");
}

/// The header does not depend on an entry existing: a miner-bearing
/// root that has checkpointed but never reclaimed still refuses the
/// first mistaken `NoConsumer` pass.
fn a_root_that_never_reclaimed_still_refuses_a_disagreeing_pass() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = open(root);
    let mark = wal
        .append(
            ourios_wal::FrameKind::TenantOtlpBatch,
            &ourios_wal::TenantBatch::encode("alpha", b"a1").expect("encode"),
        )
        .expect("append");
    wal.sync().expect("sync");
    wal.checkpoint(mark).expect("checkpoint");
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", mark)]), CAP)
        .expect("housekeeping");
    assert_eq!(
        progress.removed_segments, 0,
        "the one segment is the append target, so nothing is reclaimed",
    );
    assert!(
        reclaimed_through(root).is_empty(),
        "and no entry exists to infer a mode from",
    );

    let failure = wal
        .housekeeping_prepare(&SnapshotHorizons::NoConsumer, CAP)
        .expect_err("the header alone refuses it");
    assert!(format!("{failure}").contains("Known"));
}

/// A root whose header carries no mode adopts the first pass's mode
/// durably **before** that pass unlinks anything.
fn the_first_pass_adopts_its_mode_before_it_unlinks_anything() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let plan = wal
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect("prepare");
    wal.write_plan_record(&plan).expect("record");

    assert_eq!(
        live_slot(&std::fs::read(root.join(RECLAIM)).expect("read RECLAIM")).2,
        MODE_KNOWN,
        "the mode is durable while every segment is still on disk",
    );
    assert_eq!(segment_files(root).len(), 2, "nothing unlinked yet");
    wal.housekeeping_commit(unlink_planned(&plan))
        .expect("commit");
    assert_eq!(segment_files(root).len(), 1);
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
fn rfc0052_17_failed_unlink_and_uncertain_deletion_are_reconciled() {
    a_failed_unlink_keeps_the_entry_behind_and_pins_on_restart();
    an_uncertain_deletion_is_reverified_and_reconciled(true);
    an_uncertain_deletion_is_reverified_and_reconciled(false);
    a_renamed_planned_segment_is_not_counted_as_reclaimed();
    a_crash_between_the_record_write_and_the_commit_reconciles_the_same_way();
}

/// A segment whose unlink fails after the record was written stays on
/// disk with `reclaimed_through` behind it, and a restart with that
/// tenant's snapshot undecodable pins rather than halts.
fn a_failed_unlink_keeps_the_entry_behind_and_pins_on_restart() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let plan = wal
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect("prepare");
    wal.write_plan_record(&plan).expect("record");

    let path = plan.segments[0].path.clone();
    let progress = wal
        .housekeeping_commit(ReclaimOutcome::Unlinked {
            removed: Vec::new(),
            failed: vec![(path.clone(), std::io::Error::other("injected"))],
            fsync_failed: false,
        })
        .expect("commit");
    assert_eq!(progress.removed_segments, 0);
    assert!(path.exists(), "the segment stays on disk");
    drop(wal);
    assert!(
        reclaimed_through(root).is_empty(),
        "reclaimed_through stays behind the segment",
    );

    // So a restart with that tenant's snapshot gone pins rather than
    // halts: no entry, nothing lost.
    let mut restarted = open(root);
    restarted.rebuild_ledger().expect("ledger");
    restarted
        .housekeeping_prepare(&known(&[]), CAP)
        .expect("a root with no entry pins");
}

/// A segment whose parent fsync fails after its unlink keeps
/// `reclaimed_through` behind it and its bytes counted; the next pass
/// re-verifies presence, and a restart reconciles it under **both**
/// outcomes of the injected failure — present ⇒ retained and
/// re-planned, absent ⇒ `reclaimed_through` raised, no halt.
fn an_uncertain_deletion_is_reverified_and_reconciled(really_removed: bool) {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let before = wal.reclaim_state().unreclaimed_bytes;
    let plan = wal
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect("prepare");
    wal.write_plan_record(&plan).expect("record");

    let segment = plan.segments[0].segment;
    if really_removed {
        std::fs::remove_file(&plan.segments[0].path).expect("the unlink itself succeeded");
    }
    let progress = wal
        .housekeeping_commit(ReclaimOutcome::Unlinked {
            removed: vec![plan.segments[0].path.clone()],
            failed: Vec::new(),
            fsync_failed: true,
        })
        .expect("commit");

    assert_eq!(
        progress.removed_segments, 0,
        "an uncertain deletion completes nothing",
    );
    assert_eq!(
        wal.reclaim_state().unreclaimed_bytes,
        before,
        "and its bytes stay counted",
    );
    assert!(
        reclaimed_through(root).is_empty(),
        "reclaimed_through stays behind the whole removed set",
    );
    assert_eq!(
        planned_unlinks(root),
        vec![PlannedRow {
            segment,
            uncertain: false,
        }],
        "the durable entry is still the one the record write left: the \
         uncertain mark follows reclaimed_through and lands with the \
         next record write",
    );

    // The next pass re-verifies presence, and re-plans the entry as
    // the uncertain deletion it is.
    let replan = wal
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect("re-plan");
    assert_eq!(
        replan
            .segments
            .iter()
            .map(|s| (s.segment, s.uncertain))
            .collect::<Vec<_>>(),
        vec![(segment, true)],
    );
    drop(wal);

    the_restart_reconciles_an_uncertain_deletion(root, first[0], really_removed);
}

/// Present ⇒ retained and re-planned; absent ⇒ `reclaimed_through`
/// raised, no halt. Either way the reconciled record is durable before
/// the first pass.
fn the_restart_reconciles_an_uncertain_deletion(
    root: &std::path::Path,
    horizon: ourios_wal::WalOffset,
    really_removed: bool,
) {
    let mut restarted = open(root);
    assert!(
        planned_unlinks(root).is_empty(),
        "the reconciled record is durable before the first pass",
    );
    restarted.rebuild_ledger().expect("ledger");
    let entries = reclaimed_through(root);
    if really_removed {
        assert_eq!(
            entries["alpha"], horizon,
            "absent ⇒ the reclamation finished, so the entry is raised",
        );
        return;
    }
    assert!(
        entries.is_empty(),
        "present ⇒ retained, so nothing is claimed lost",
    );
    assert_eq!(
        restarted
            .housekeeping_pass(&known(&[("alpha", horizon)]), CAP)
            .expect("housekeeping")
            .removed_segments,
        1,
        "and the next pass re-plans it",
    );
}

/// A planned segment an operator renames between prepare and the
/// unlink is **not** reclaimed: the file half checks the header uuid
/// before removing anything. Unlinking by path alone would either
/// destroy whatever now sits at that path, or — when the path is empty
/// because the segment moved — count a survivor as removed and raise
/// `reclaimed_through` over frames still on disk.
fn a_renamed_planned_segment_is_not_counted_as_reclaimed() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let horizons = known(&[("alpha", first[0])]);
    let plan = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    wal.write_plan_record(&plan).expect("record");

    // The operator moves it while the file half is outstanding. The
    // name sorts *below* every `UUIDv7`, so the restart still opens
    // the newest segment as its append target and this one stays an
    // ordinary closed candidate.
    let moved = root.join("0-kept-by-an-operator.wal");
    std::fs::rename(&plan.segments[0].path, &moved).expect("rename the planned segment");

    let progress = wal
        .housekeeping_commit(unlink_planned(&plan))
        .expect("commit");
    assert_eq!(
        progress.removed_segments, 0,
        "a path that no longer holds the planned segment is not a reclamation",
    );
    assert!(moved.exists(), "and the segment itself is untouched");
    drop(wal);
    assert!(
        reclaimed_through(root).is_empty(),
        "nothing is claimed lost while its frames are still on disk",
    );

    // The restart reconciles by uuid: the segment is present, so its
    // planned entry is dropped and the rebuilt ledger finds it under
    // the name it now has.
    let mut restarted = open(root);
    assert!(planned_unlinks(root).is_empty());
    restarted.rebuild_ledger().expect("ledger");
    assert_eq!(
        restarted
            .housekeeping_pass(&horizons, CAP)
            .expect("housekeeping")
            .removed_segments,
        1,
        "and the next pass reclaims it at its real path",
    );
    assert!(!moved.exists());
}

/// A crash between the record write and the commit is reconciled the
/// same way, with the reconciled record durable before the first pass.
fn a_crash_between_the_record_write_and_the_commit_reconciles_the_same_way() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let plan = wal
        .housekeeping_prepare(&known(&[("alpha", first[0])]), CAP)
        .expect("prepare");
    wal.write_plan_record(&plan).expect("record");
    let outcome = unlink_planned(&plan);
    assert!(matches!(outcome, ReclaimOutcome::Unlinked { .. }));
    // No `housekeeping_commit`: the process dies between the halves.
    drop(wal);

    let restarted = open(root);
    assert_eq!(
        reclaimed_through(root)["alpha"],
        first[0],
        "an absent planned segment is a reclamation that finished",
    );
    assert!(
        planned_unlinks(root).is_empty(),
        "and the reconciled record is durable before the first pass",
    );
    drop(restarted);
}

/// Scenario RFC0052.17 — a failed record write unlinks nothing and loses nothing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_failed_record_write_unlinks_nothing_and_segments_are_reclaimed_later() {
    // Given: a pass whose record write fails.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");
    let before = wal.reclaim_state();
    let horizons = known(&[("alpha", first[0])]);
    let plan = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    let planned: Vec<PathBuf> = plan.segments.iter().map(|s| s.path.clone()).collect();

    // When: the file half reports it.
    let progress = wal
        .housekeeping_commit(ReclaimOutcome::RecordFailed(std::io::Error::other(
            "injected: no room for the slot write",
        )))
        .expect("commit");

    // Then: nothing was unlinked and the WAL's accounting is unchanged.
    assert_eq!(progress.removed_segments, 0);
    assert!(planned.iter().all(|p| p.exists()));
    let after = wal.reclaim_state();
    assert_eq!(after.unreclaimed_bytes, before.unreclaimed_bytes);
    assert_eq!(after.segment_count, before.segment_count);
    assert!(
        reclaimed_through(root).is_empty() && planned_unlinks(root).is_empty(),
        "and the record on disk records nothing about the attempt",
    );

    // And: the segments the pass popped are reclaimed by a later pass
    // once the write succeeds — never lost to the ledger.
    let progress = wal
        .housekeeping_pass(&horizons, CAP)
        .expect("the next pass");
    assert_eq!(progress.removed_segments, planned.len());
    assert!(planned.iter().all(|p| !p.exists()));
    drop(wal);
    drop(open(root));
    assert_eq!(
        reclaimed_through(root).keys().collect::<Vec<_>>(),
        vec!["alpha"],
    );
    assert!(root.join(CHECKPOINT).exists());
}
