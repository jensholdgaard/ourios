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

use ourios_wal::{FrameKind, PassOutcome, SkipReason, TenantBatch, WalOffset, unlink_planned};

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
        plan.segments.len(),
        CAP,
        "prepare plans at most the cap, not the backlog",
    );

    // When: an append is taken with the file half still outstanding.
    let appended = wal
        .append(FrameKind::TenantOtlpBatch, &frame("alpha", b"live"))
        .expect("an append never waits for the RECLAIM write or an unlink");
    wal.sync().expect("sync");

    // Then: it completed, and the pass still settles correctly around
    // it.
    wal.write_plan_record(&plan).expect("record");
    let progress = wal
        .housekeeping_commit(unlink_planned(&plan))
        .expect("commit");
    assert_eq!(progress.removed_segments, CAP);
    assert!(
        appended > covered,
        "the frame landed above the checkpoint the pass reclaimed under",
    );
    assert!(
        segment_files(root).contains(&newest_segment(root)),
        "and its segment is still there",
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
    assert!(
        first.horizon_remaining < backlog_before,
        "the cursor moved: {} -> {}",
        backlog_before,
        first.horizon_remaining,
    );
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
    assert_eq!(
        progress.outcome,
        PassOutcome::Skipped(SkipReason::NoCheckpoint),
        "and the sweep runs on a pass that plans no segment at all",
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
