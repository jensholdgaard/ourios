//! RFC0052.12 — A reclamation pass bounds its per-file work.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Per RFC 0052 §6 the capped-pass and rotation-retry tests together
//! **replace** `rfc0008_6_rotation_failure_quiesces_the_wal`, whose
//! "even after the underlying condition clears" assertion is the
//! contract §3.3 changes. That replacement needs explicit approval
//! (`CLAUDE.md` §6.2) and lands with slice C, not here: the original
//! stays untouched while the RFC is red.

use std::path::{Path, PathBuf};

use ourios_wal::{
    FrameKind, PassOutcome, ReclaimError, SkipReason, SnapshotHorizons, TenantBatch, WalOffset,
    unlink_planned,
};

use crate::rfc0052_support::{build_tenant_segment, known, open, segment_files, write_partial};

/// A backlog far larger than the cap under test. The incident's 1,113
/// segments is the shape; seven against a cap of two is the same
/// arithmetic without minting a thousand files per run.
const BACKLOG: usize = 7;
const CAP: usize = 2;

/// Scenario RFC0052.12 — a pass unlinks at most the cap and reads nothing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_12_pass_unlinks_at_most_the_cap() {
    // Given: a backlog far larger than `max_unlinks_per_pass`, every
    // segment covered, plus a file the pass must never look at.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered)
        .expect("checkpoint past the backlog");
    let current = newest_segment(root);
    // Placed after `open`, so only a *listing* on the pass could find
    // it and only a *header read* could choke on it. Both are what
    // §3.7 withdrew.
    let decoy = root.join("not-a-segment.wal");
    std::fs::write(&decoy, b"neither a header nor a frame").expect("decoy");

    // When: one pass runs.
    let first = wal
        .housekeeping_pass(&known(&[("alpha", covered)]), CAP)
        .expect("housekeeping");

    // Then: it unlinks at most the cap and says there is more to do.
    assert_eq!(first.removed_segments, CAP);
    assert!(first.capped, "the pass reports the backlog is not drained");
    assert!(
        first.horizon_remaining > 0,
        "and the backlog figure agrees — the segments still waiting are \
         waiting on horizon application, which is what §3.7 counts there \
         rather than in unlink_remaining: {first:?}",
    );
    assert!(
        decoy.exists(),
        "the pass listed no directory and read no header",
    );

    // And: successive passes drain the backlog to the same end state
    // an uncapped pass would reach — every closed segment gone, the
    // current append segment kept.
    let mut passes = 1;
    while wal
        .housekeeping_pass(&known(&[("alpha", covered)]), CAP)
        .expect("housekeeping")
        .removed_segments
        > 0
    {
        passes += 1;
        assert!(passes < 100, "the backlog must drain, not loop");
    }
    assert_eq!(
        segment_files(root),
        vec![current.clone(), decoy.clone()],
        "only the current append segment survives; the decoy was never touched",
    );
    assert_eq!(
        wal.housekeeping_pass(&known(&[("alpha", covered)]), CAP)
            .expect("housekeeping")
            .unlink_remaining,
        0,
        "and the pass reports nothing left to unlink",
    );
}

/// `capped` reports **either** half hitting its budget, and the pop
/// half has to say so on its own. A `NoConsumer` pass applies no
/// horizon, so its horizon half never raises the flag: whatever
/// `capped` says there is the pop half's answer, and a caller that
/// read "drained" off a pass that stopped at the cap would stop
/// scheduling the next one.
#[test]
fn rfc0052_12_the_pop_half_reports_capped_without_the_horizon_half() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let first = wal
        .housekeeping_pass(&SnapshotHorizons::NoConsumer, CAP)
        .expect("housekeeping");
    assert_eq!(first.removed_segments, CAP);
    assert!(first.capped, "the pop half hit its budget: {first:?}");
    assert_eq!(
        first.horizon_remaining, 0,
        "and a no-consumer pass reports no horizon backlog: it applies \
         none and can apply none, so the membership the ledger still \
         tracks is work no pass will ever reduce: {first:?}",
    );

    // And the pass that drains the backlog says the opposite, so the
    // flag is the pop half's own and not a constant.
    let mut last = first;
    while last.removed_segments > 0 {
        last = wal
            .housekeeping_pass(&SnapshotHorizons::NoConsumer, CAP)
            .expect("housekeeping");
    }
    assert!(!last.capped, "a pass with nothing left is not capped");
    assert_eq!(last.unlink_remaining, 0);
}

/// Scenario RFC0052.12 — an append is never held across the file half.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_12_append_completes_while_file_half_is_held() {
    // Given: a backlog far larger than the cap, planned but not yet
    // unlinked — the fault-injection point between
    // `housekeeping_prepare` and `housekeeping_commit`, where §3.7
    // releases the journal guard.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");
    let plan = wal
        .housekeeping_prepare(&known(&[("alpha", covered)]), CAP)
        .expect("prepare");

    // Then: the ledger half did O(cap) work whatever the backlog —
    // the regression guard, since an uncapped pass would have planned
    // the whole backlog here and held the position for all of it.
    assert_eq!(
        plan.segments().len(),
        CAP,
        "prepare plans at most the cap, not the backlog",
    );

    // When: an append is taken with the file half still outstanding.
    let appended = wal
        .append(FrameKind::TenantOtlpBatch, &frame("alpha", b"live"))
        .expect("an append never waits for the RECLAIM write or an unlink");
    wal.sync().expect("sync");
    // Captured here, not re-derived after the commit: `newest_segment`
    // picks from whatever survives, so asking it afterwards would be
    // true however the pass behaved.
    let landed = newest_segment(root);

    // Then: it completed, and the pass still settles correctly around
    // it.
    wal.write_plan_record(&plan).expect("record");
    let progress = wal
        .housekeeping_commit(plan.pass(), unlink_planned(&plan))
        .expect("commit");
    assert_eq!(progress.removed_segments, CAP);
    assert!(
        appended > covered,
        "the frame landed above the checkpoint the pass reclaimed under",
    );
    assert!(
        landed.exists(),
        "and the file the append landed in survives the commit: {}",
        landed.display(),
    );
}

/// Scenario RFC0052.12 — horizon application is capped and resumed per tenant.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_12_horizon_application_is_capped_and_resumes() {
    // Given: a tenant catching up after a long outage — every segment
    // below a horizon that has only just arrived.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    // Then: an unchanged *pinned* backlog costs none — no horizon, no
    // application, nothing to resume.
    let pinned = wal.housekeeping_pass(&known(&[]), CAP).expect("pinned");
    assert_eq!(pinned.removed_segments, 0);
    assert!(
        !pinned.capped,
        "a pass with nothing to apply does not claim work remains",
    );
    let backlog_before = pinned.horizon_remaining;
    assert_eq!(
        wal.housekeeping_pass(&known(&[]), CAP)
            .expect("pinned again")
            .horizon_remaining,
        backlog_before,
        "and a second such pass moves no cursor",
    );

    // When: the horizon arrives, application is capped per pass and
    // resumed from the per-tenant cursor on the next.
    let horizons = known(&[("alpha", covered)]);
    let first = wal.housekeeping_pass(&horizons, CAP).expect("housekeeping");
    assert!(first.capped, "one pass does not apply the whole backlog");
    // The cursor moved by exactly the budget: the walk spends its
    // whole allowance while a tenant is this far behind.
    assert_eq!(backlog_before - first.horizon_remaining, CAP);
    let mut previous = first.horizon_remaining;
    let mut progress = first;
    while progress.horizon_remaining > 0 {
        progress = wal.housekeeping_pass(&horizons, CAP).expect("housekeeping");
        assert!(
            progress.horizon_remaining < previous,
            "each pass resumes above the cursor the last one left",
        );
        previous = progress.horizon_remaining;
    }
}

/// Scenario RFC0052.12 — a checkpoint advance promotes nothing eagerly.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_12_a_segment_covered_above_the_mark_is_reclaimed_when_the_mark_passes() {
    // Given: two closed segments whose tenant's horizon already covers
    // both, and a checkpoint that reaches only the first.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    let second = build_tenant_segment(root, &[("alpha", b"a2")]);
    build_tenant_segment(root, &[("alpha", b"a3")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0])
        .expect("checkpoint below the second");

    let horizons = known(&[("alpha", second[0])]);
    assert_eq!(
        wal.housekeeping_pass(&horizons, CAP)
            .expect("housekeeping")
            .removed_segments,
        1,
        "the segment whose set emptied above the mark waits for the mark",
    );
    assert_eq!(segment_files(root).len(), 2);

    // When: the mark passes it. Then: the first pass after reclaims
    // it, with no new horizon and no promotion pass.
    wal.checkpoint(second[0]).expect("advance the checkpoint");
    let progress = wal.housekeeping_pass(&horizons, CAP).expect("housekeeping");
    assert_eq!(progress.removed_segments, 1);
    assert_eq!(segment_files(root).len(), 1);
}

/// Scenario RFC0052.12 — a pinned oldest segment does not shadow a later one.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_12_pinned_oldest_does_not_shadow_a_later_eligible_segment() {
    // Given: the oldest segment pinned by a tenant without a snapshot
    // and a later one fully covered.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_tenant_segment(root, &[("pinned", b"p1")]);
    let covered = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let before = segment_files(root);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered[0]).expect("checkpoint past both");

    // When: the pass runs. Then: it reclaims the later segment on its
    // first tick — eligibility is per segment, not a prefix scan that
    // would stop at the pinned head every time.
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", covered[0])]), CAP)
        .expect("housekeeping");
    assert_eq!(progress.removed_segments, 1);
    assert_eq!(
        segment_files(root),
        vec![before[0].clone(), before[2].clone()],
    );
}

/// Scenario RFC0052.12 — stale partials share the cap with segments.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_12_partials_are_swept_first_under_the_same_cap() {
    // Given: a segment backlog larger than the cap beside stale
    // partials left by a previous process.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let partials: Vec<PathBuf> = (0..2).map(|_| write_partial(root)).collect();
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    // When: the first pass runs with a cap of three.
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", covered)]), 3)
        .expect("housekeeping");

    // Then: it removes every partial from the list seeded at recovery
    // and spends only cap-minus-partials on segments.
    assert_eq!(progress.removed_partials, partials.len());
    assert_eq!(progress.removed_segments, 1, "3 - 2 partials = 1 segment");
    assert!(partials.iter().all(|p| !p.exists()));

    // And: a backlog of stale temporaries alone is bounded by the same
    // cap, so temp sweeping cannot make a "bounded" pass do unbounded
    // work.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let debris: Vec<PathBuf> = (0..BACKLOG).map(|_| write_partial(root)).collect();
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    let progress = wal
        .housekeeping_pass(&known(&[]), CAP)
        .expect("housekeeping");
    assert_eq!(progress.removed_partials, CAP);
    assert_eq!(
        debris.iter().filter(|p| p.exists()).count(),
        BACKLOG - CAP,
        "the rest wait for the next pass",
    );
    assert!(
        progress.capped,
        "and the partial half says so on its own — it is the only half \
         that spent any budget here: {progress:?}",
    );
    assert_eq!(
        progress.outcome,
        PassOutcome::Skipped(SkipReason::NoCheckpoint),
        "and the sweep runs on a pass that plans no segment at all",
    );
}

/// A plan that is never committed — §3.7's "the task panicked between
/// the halves" — strands nothing. Its segments stay marked reclaiming
/// and are re-planned ahead of anything newly eligible; its partials
/// left the sweep's list, which is their only record, so they go back
/// on it.
#[test]
fn rfc0052_12_a_plan_that_is_never_committed_strands_nothing() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let debris = write_partial(root);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let horizons = known(&[("alpha", covered)]);
    let abandoned = wal.housekeeping_prepare(&horizons, 3).expect("prepare");
    assert_eq!(abandoned.partials(), vec![debris.clone()]);
    assert_eq!(abandoned.segments().len(), 2, "3 - 1 partial = 2 segments");

    // No commit: the task died between the halves. The next pass
    // re-plans everything it held.
    let replanned = wal.housekeeping_prepare(&horizons, 3).expect("re-plan");
    assert_eq!(
        replanned.partials(),
        abandoned.partials(),
        "the partial is back on the sweep's list",
    );
    assert_eq!(
        replanned
            .segments()
            .iter()
            .map(|s| s.segment)
            .collect::<Vec<_>>(),
        abandoned
            .segments()
            .iter()
            .map(|s| s.segment)
            .collect::<Vec<_>>(),
        "and the entries still marked reclaiming are re-planned first",
    );

    wal.write_plan_record(&replanned).expect("record");
    let progress = wal
        .housekeeping_commit(replanned.pass(), unlink_planned(&replanned))
        .expect("commit");
    assert_eq!(
        (progress.removed_segments, progress.removed_partials),
        (2, 1)
    );
    assert!(!debris.exists());
}

/// The same abandonment, but **after** the unlinks ran: §3.7's panic
/// lands between `unlink_planned` and the commit, so the files are
/// gone and no commit ever said so. The re-plan must carry §3.2's
/// `uncertain` mark, or the next unlink reads the absent file as the
/// renamed-survivor shape and retries it for the life of the process.
#[test]
fn rfc0052_12_a_plan_abandoned_after_its_unlinks_completes_on_the_next_pass() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let horizons = known(&[("alpha", covered)]);
    let abandoned = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    wal.write_plan_record(&abandoned).expect("record");
    let planned: Vec<PathBuf> = abandoned
        .segments()
        .iter()
        .map(|s| s.path.clone())
        .collect();
    assert_eq!(planned.len(), CAP);
    let _ = unlink_planned(&abandoned);
    assert!(
        planned.iter().all(|path| !path.exists()),
        "the file half finished; only the commit did not",
    );

    // The task dies here. The next pass re-plans what it held.
    let replanned = wal.housekeeping_prepare(&horizons, CAP).expect("re-plan");
    assert_eq!(
        replanned
            .segments()
            .iter()
            .map(|s| (s.segment, s.uncertain))
            .collect::<Vec<_>>(),
        abandoned
            .segments()
            .iter()
            .map(|s| (s.segment, true))
            .collect::<Vec<_>>(),
        "an entry no commit reported on is re-planned as an uncertain deletion",
    );

    wal.write_plan_record(&replanned).expect("record");
    let progress = wal
        .housekeeping_commit(replanned.pass(), unlink_planned(&replanned))
        .expect("commit");
    assert_eq!(
        progress.removed_segments, CAP,
        "so the absent files complete the reclamation rather than failing forever",
    );
}

/// An abandoned plan's unlinks survive a horizon that regresses over
/// them. §3.7's rewind withdraws a popped entry back to eligible so a
/// regressed horizon cannot have its frames unlinked — but the file
/// half may already have removed the file, and a withdrawal that
/// forgets that leaves the segment to be re-planned later as a first
/// attempt, failing on the missing path for the life of the process.
#[test]
fn rfc0052_12_an_abandoned_unlink_survives_a_horizon_that_regresses_over_it() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let horizons = known(&[("alpha", covered)]);
    let abandoned = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    wal.write_plan_record(&abandoned).expect("record");
    let planned: Vec<PathBuf> = abandoned
        .segments()
        .iter()
        .map(|s| s.path.clone())
        .collect();
    let _ = unlink_planned(&abandoned);
    assert!(planned.iter().all(|path| !path.exists()));

    // The task dies, and the tenant's snapshot stops restoring before
    // the next pass: every segment it holds is withdrawn and pinned,
    // the ones already unlinked among them.
    let pinned = wal.housekeeping_prepare(&known(&[]), CAP).expect("re-plan");
    assert!(pinned.segments().is_empty(), "the regression withdrew them");
    wal.housekeeping_commit(pinned.pass(), unlink_planned(&pinned))
        .expect("commit");

    // When the snapshot restores again the withdrawn entries come
    // back, and the pass must still treat them as deletions that may
    // already have happened.
    let resumed = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    assert_eq!(
        resumed
            .segments()
            .iter()
            .filter(|s| planned.contains(&s.path))
            .map(|s| s.uncertain)
            .collect::<Vec<_>>(),
        vec![true; planned.len()],
        "the withdrawal did not forget that a file half had them",
    );
    wal.write_plan_record(&resumed).expect("record");
    assert_eq!(
        wal.housekeeping_commit(resumed.pass(), unlink_planned(&resumed))
            .expect("commit")
            .removed_segments,
        resumed.segments().len(),
        "so the pass completes instead of failing on the missing paths",
    );
}

/// A plan abandoned before its record write leaves the recorded mode
/// `Unrecorded`, so §3.2's mode guard admits any mode on the next
/// pass — and §3.7 re-plans a reclaiming entry unconditionally. A
/// `NoConsumer` pass pops by the checkpoint alone, with no tenant
/// constraint at all, so without withdrawing its entries first that
/// pair unlinks the frames of a tenant that has no snapshot: exactly
/// the loss the mode guard exists to prevent, through the one window
/// the guard cannot see.
#[test]
fn rfc0052_12_an_abandoned_no_consumer_plan_does_not_survive_into_a_known_pass() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let before = segment_files(root);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(first[0]).expect("checkpoint");

    let abandoned = wal
        .housekeeping_prepare(&SnapshotHorizons::NoConsumer, CAP)
        .expect("prepare");
    assert_eq!(
        abandoned.segments().len(),
        1,
        "the no-consumer pass popped it by the checkpoint alone",
    );
    // The task dies before the record write, so the root's mode is
    // still unrecorded and the next pass may pick any.

    let known_pass = wal.housekeeping_prepare(&known(&[]), CAP).expect("prepare");
    assert!(
        known_pass.segments().is_empty(),
        "a tenant with no snapshot pins its own segment, re-plan or not",
    );
    wal.write_plan_record(&known_pass).expect("record");
    wal.housekeeping_commit(known_pass.pass(), unlink_planned(&known_pass))
        .expect("commit");
    assert_eq!(segment_files(root), before, "so its frames are still there");
}

/// A plan a later `housekeeping_prepare` superseded is refused at the
/// record write. §3.7 makes the second prepare legal — it is the
/// abandoned-plan recovery — and a horizon that regressed in between
/// withdraws exactly the segments the first plan named, so submitting
/// the old plan would unlink frames the ledger has re-pinned. §3.2
/// orders the record before the unlinks, which is what makes this the
/// place the stale plan stops.
#[test]
fn rfc0052_12_a_superseded_plan_is_refused_at_the_record_write() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let horizons = known(&[("alpha", covered)]);
    let superseded = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    // The tenant's snapshot stops restoring: every segment it holds
    // goes back to pinned, the plan above included.
    let pinned = wal.housekeeping_prepare(&known(&[]), CAP).expect("re-plan");
    assert!(
        pinned.segments().is_empty(),
        "the regression withdrew what the first plan named",
    );

    let refused = wal
        .write_plan_record(&superseded)
        .expect_err("a superseded plan must not reach the file half");
    let text = refused.to_string();
    assert!(
        text.contains("superseded") && text.contains("§3.7"),
        "and the refusal says why: {text}",
    );
    assert_eq!(
        segment_files(root).len(),
        BACKLOG + 1,
        "nothing was unlinked",
    );

    wal.write_plan_record(&pinned)
        .expect("the live plan still writes");
}

/// ...and a superseded plan's *commit* is refused too, with the live
/// plan untouched. The record write's refusal is not enough on its
/// own: §3.7's protocol says a record-write failure is reported as
/// `RecordFailed`, so a caller doing exactly that with the stale plan
/// would otherwise restore the **newer** plan's entries and requeue
/// its partials, leaving that plan's own file half unaccounted for.
#[test]
fn rfc0052_12_a_superseded_commit_leaves_the_live_plan_alone() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let horizons = known(&[("alpha", covered)]);
    let superseded = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    let live = wal.housekeeping_prepare(&horizons, CAP).expect("re-plan");
    let refused = wal
        .write_plan_record(&superseded)
        .expect_err("the stale plan is refused at the record write");

    // The caller reports that failure the way §3.7 says to — with the
    // stale plan's own pass.
    let failure = wal
        .housekeeping_commit(
            superseded.pass(),
            ourios_wal::ReclaimOutcome::RecordFailed(refused),
        )
        .expect_err("and the commit refuses it as well");
    assert!(
        format!("{failure}").contains("superseded"),
        "naming why: {failure}",
    );

    // The live plan is still outstanding and still completes.
    wal.write_plan_record(&live).expect("the live plan writes");
    assert_eq!(
        wal.housekeeping_commit(live.pass(), unlink_planned(&live))
            .expect("commit")
            .removed_segments,
        live.segments().len(),
    );
}

/// A plan does not survive the `Wal` that made it. The per-pass
/// sequence alone repeats — a reopen of the same root starts again at
/// one — so a plan left over from the instance before would be taken
/// as the live one: its segments written into that root's record and
/// its outstanding state settled under a pass the new instance never
/// ran.
#[test]
fn rfc0052_12_a_plan_does_not_cross_from_one_wal_to_the_next() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let horizons = known(&[("alpha", covered)]);

    let mut first = open(root);
    first.rebuild_ledger().expect("ledger");
    first.checkpoint(covered).expect("checkpoint");
    let stale = first.housekeeping_prepare(&horizons, CAP).expect("prepare");
    drop(first);

    // The same root, reopened: its own first pass, its own plan.
    let mut reopened = open(root);
    reopened.rebuild_ledger().expect("ledger");
    let live = reopened
        .housekeeping_prepare(&horizons, CAP)
        .expect("prepare");
    assert_ne!(
        format!("{}", stale.pass()),
        format!("{}", live.pass()),
        "the sequence repeats across instances; the identity must not",
    );

    let refused = reopened
        .write_plan_record(&stale)
        .expect_err("the previous instance's plan is not this one's");
    assert!(format!("{refused}").contains("superseded"));
    assert!(
        reopened
            .housekeeping_commit(
                stale.pass(),
                ourios_wal::ReclaimOutcome::RecordFailed(refused),
            )
            .is_err(),
        "and neither is its outcome",
    );

    reopened
        .write_plan_record(&live)
        .expect("the live plan writes");
    assert_eq!(
        reopened
            .housekeeping_commit(live.pass(), unlink_planned(&live))
            .expect("commit")
            .removed_segments,
        live.segments().len(),
    );
}

/// An unlink that fails is kept for retry **and** reported. §3.1's
/// rule is that the failure is logged and the next pass retries it,
/// and a pass that returned `Ok` gave its caller neither.
#[test]
fn rfc0052_12_a_failed_unlink_is_reported_and_stays_queued() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, 1);
    // A directory under the reserved partial name: the sweep's list is
    // seeded from the name alone, and `remove_file` cannot take it.
    let wedged = root.join(format!("{}.wal.partial", uuid::Uuid::now_v7()));
    std::fs::create_dir(&wedged).expect("a partial no unlink can remove");
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered).expect("checkpoint");

    let horizons = known(&[("alpha", covered)]);
    let failure = wal
        .housekeeping_pass(&horizons, CAP)
        .expect_err("a pass whose unlink failed is not a clean pass");
    let ReclaimError::Housekeeping { progress, .. } = &failure else {
        panic!("an unlink failure is a housekeeping failure: {failure:?}");
    };
    assert_eq!(
        (progress.removed_segments, progress.removed_partials),
        (1, 0),
        "the half that could finish still did, and the commit still ran",
    );
    assert!(
        format!("{failure}").contains(&wedged.display().to_string()),
        "and the error names the path that failed: {failure}",
    );
    assert!(wedged.exists());

    let next = wal.housekeeping_prepare(&horizons, CAP).expect("prepare");
    assert_eq!(
        next.partials(),
        vec![wedged],
        "the failed path is back at the head of the sweep's list",
    );
}

/// A pass before `rebuild_ledger` reclaims **nothing**, and appending
/// first does not change that. `Wal::open` adopts an existing segment
/// as its append target without reading it, so until the rebuild walks
/// the root the ledger describes only what was appended since — a
/// segment whose earlier tenants are missing from it looks unheld, and
/// a pass would take it over their frames. §3.7 forbids seeding at
/// open (a byte count there would include a torn tail recovery has not
/// healed), so the ledger carries whether it has been walked instead.
#[test]
fn rfc0052_12_a_pass_before_the_ledger_is_rebuilt_reclaims_nothing() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let covered = backlog(root, BACKLOG);
    let mut seeded = open(root);
    seeded.rebuild_ledger().expect("ledger");
    seeded.checkpoint(covered).expect("checkpoint");
    drop(seeded);

    // Reopened and *not* rebuilt: the checkpoint is on disk and covers
    // every closed segment, and the tenant's horizon covers them too.
    let before = segment_files(root);
    let mut wal = open(root);
    // Appended to, so the ledger is not merely empty — it holds the
    // adopted segment, described by this frame alone.
    let appended = wal
        .append(FrameKind::TenantOtlpBatch, &frame("beta", b"b1"))
        .expect("append");
    wal.sync().expect("sync");
    wal.checkpoint(appended)
        .expect("checkpoint past everything");
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", covered), ("beta", appended)]), CAP)
        .expect("housekeeping");

    assert_eq!(
        (progress.removed_segments, progress.unlink_remaining),
        (0, 0),
        "a ledger that has not walked the root offers no candidate",
    );
    assert_eq!(segment_files(root), before, "so every segment survives");

    // And it resumes the moment the root has been walked. How much it
    // takes on this tick is the shared cap's business, which the legs
    // above own; what this one holds is that it takes anything at all.
    wal.rebuild_ledger().expect("ledger");
    let resumed = wal
        .housekeeping_pass(&known(&[("alpha", covered), ("beta", appended)]), CAP)
        .expect("housekeeping");
    assert!(
        resumed.removed_segments > 0 && resumed.capped,
        "the gate is the walk, not the pass: {resumed:?}",
    );
}

/// `count` closed segments for one tenant plus a current one. The
/// returned mark is the newest frame of all, so it covers every
/// segment: the current one is held back by its identity, not by the
/// bound, which is the guard these legs are aimed at.
fn backlog(root: &Path, count: usize) -> WalOffset {
    for index in 0..count {
        build_tenant_segment(root, &[("alpha", format!("f{index}").as_bytes())]);
    }
    build_tenant_segment(root, &[("alpha", b"current")])
        .into_iter()
        .next_back()
        .expect("the current segment holds a frame")
}

fn frame(tenant: &str, body: &[u8]) -> Vec<u8> {
    TenantBatch::encode(tenant, body).expect("encode")
}

/// The newest `<uuid>.wal`, which is the file the WAL appends into:
/// `UUIDv7` names sort chronologically, and a file whose stem is not a
/// uuid was never a segment.
fn newest_segment(root: &Path) -> PathBuf {
    segment_files(root)
        .into_iter()
        .rfind(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.parse::<uuid::Uuid>().is_ok())
        })
        .expect("a current segment")
}
