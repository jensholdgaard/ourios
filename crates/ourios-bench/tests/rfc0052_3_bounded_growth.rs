//! RFC0052.3 — Sustained ingest does not grow the WAL without bound.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The one criterion a unit test cannot express (RFC 0052 §6): the
//! defect is the *absence* of a periodic call, and only elapsed cadence
//! reveals it. Both legs run on the reclamation soak harness
//! (`src/soak/reclaim.rs`), which drives the receiver's barrier and
//! housekeeping ticks — §3.2's sequence — on a stepped synthetic clock,
//! so hours of cadence elapse in seconds and every run is the same run.
//! Its own binary, like the other bench tests under `tests/`.

use ourios_bench::reclaim::{
    GrowthBound, PassResult, ReclaimSample, ReclaimSoak, ReclaimSoakConfig, Tick,
};
use ourios_ingester::barrier::CutOutcome;
use ourios_wal::{RetainFloor, WalOffset};

/// Segments the bounded-growth run rolls: three times the default
/// config's bound, so a WAL that never unlinked would break it well
/// before the run ends.
const ROLLED_SEGMENTS: u64 = 12;

/// Scenario RFC0052.3 — bytes and segment count stay bounded under a complete floor.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_3_wal_bytes_and_segments_stay_bounded_over_a_long_run() {
    // Given a capacity-balanced soak on the receiver's cadences — a
    // barrier every 300 s, a pass every 60 s — with a healthy store and
    // three tenants that each snapshot on every cut.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let config = ReclaimSoakConfig::default();
    let bound = config.growth_bound();
    let mut soak = ReclaimSoak::open(tmp.path(), config.clone()).expect("open the soak");

    // When it runs long enough to roll many segments.
    soak.run_load(ROLLED_SEGMENTS * config.fewest_frames_per_segment())
        .await
        .expect("every batch acks");
    assert_rolled_on_size_alone(&soak, bound);

    // Then the segment count and bytes never left the bound — not after
    // warm-up, at any sample — and the #793 signature, segments rolled
    // and none unlinked, is absent.
    let witnessed = first_stamp(&soak);
    assert_bounded(soak.samples(), bound, witnessed);
    let removed: usize = soak
        .passes()
        .iter()
        .map(|pass| match pass.result {
            PassResult::Completed { removed_segments } => removed_segments,
            PassResult::Failed(_) | PassResult::Panicked => 0,
        })
        .sum();
    let rolled = usize::try_from(soak.segments_rolled()).expect("fits");
    let bounded = usize::try_from(bound.segments).expect("fits");
    assert!(
        removed >= rolled - bounded,
        "{removed} of {rolled} segments unlinked"
    );

    // And the floor was complete and advancing once the first cut
    // stamped, and every pass after it completed.
    assert_complete_and_advancing_floor(&soak, witnessed);

    // And each cut wrote at most one object per active partition beyond
    // the rotation flushes coalesced into it — measured, not assumed.
    for cut in soak.cuts() {
        assert!(
            cut.objects_written <= config.objects_per_cut_bound(),
            "{cut:?} over {}",
            config.objects_per_cut_bound()
        );
    }
}

/// Scenario RFC0052.3 — the append-independent path reclaims after the last append.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_3_idle_node_reclaims_every_eligible_segment_without_further_traffic() {
    // Given a node that has rolled closed segments, whose last append
    // lands just after a barrier tick — the checkpoint trails it and no
    // pass has run since — so the next tick is nearly a whole
    // `barrier_secs` away.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let config = ReclaimSoakConfig {
        segment_age_secs: 1,
        secs_per_batch: 8,
        ..ReclaimSoakConfig::default()
    };
    let mut soak = ReclaimSoak::open(tmp.path(), config.clone()).expect("open the soak");
    soak.run_load(39).await.expect("every batch acks");
    let last = soak.ingest().await.expect("the last batch acks");
    let before = soak.state();
    assert!(before.segment_count >= 3, "{before:?}");
    assert!(before.checkpoint.is_some_and(|c| c < last), "{before:?}");
    assert!(
        soak.now_secs() > config.barrier_secs,
        "a barrier tick ran before the last append"
    );

    // When no traffic arrives for one `barrier_secs` plus one
    // `housekeeping_secs`.
    soak.advance(config.barrier_secs + config.housekeeping_secs)
        .await
        .expect("the ticks run");

    // Then the checkpoint has reached the last append and every closed
    // segment is gone: only the open one survives.
    let after = soak.state();
    assert_eq!(after.checkpoint, Some(last), "{after:?}");
    assert_eq!(after.segment_count, 1, "{after:?}");

    // And within `segment_age_secs` more, the last append's segment has
    // rotated on a barrier tick and the following pass has reclaimed
    // it: no frame survives.
    soak.advance(config.segment_age_secs)
        .await
        .expect("the ticks run");
    let idle = soak.state();
    assert!(!soak.segment_on_disk(last), "{idle:?}");
    assert_eq!(idle.segment_count, 1, "{idle:?}");
    assert_eq!(idle.unreclaimed_bytes, 0, "{idle:?}");
    assert!(idle.oldest_unreclaimed.is_none(), "{idle:?}");
}

/// The bound assumes size rotation only; a run that rotated on age or
/// wrote a frame over the ceiling would be held to the wrong number.
fn assert_rolled_on_size_alone(soak: &ReclaimSoak, bound: GrowthBound) {
    let config = soak.config();
    assert!(
        soak.largest_frame_bytes() <= config.frame_ceiling_bytes(),
        "a {} B frame exceeds the {} B ceiling",
        soak.largest_frame_bytes(),
        config.frame_ceiling_bytes(),
    );
    let appended = ROLLED_SEGMENTS * config.fewest_frames_per_segment();
    assert!(
        soak.segments_rolled() <= appended.div_ceil(config.fewest_frames_per_segment()),
        "rolled {} segments, more than size alone",
        soak.segments_rolled(),
    );
    assert!(
        soak.segments_rolled() >= 3 * u64::from(bound.segments),
        "rolled only {} segments against a bound of {}",
        soak.segments_rolled(),
        bound.segments,
    );
}

/// The instant of the first cut, which must have stamped: from there
/// on the WAL has its reclamation witness.
fn first_stamp(soak: &ReclaimSoak) -> u64 {
    let first = soak.cuts().first().expect("the run took a cut");
    assert_eq!(first.outcome, CutOutcome::Stamped, "{first:?}");
    first.synthetic_secs
}

/// Every cut stamped, every pass after the first stamp completed, and
/// the floor those passes derived was a minimum over every tenant — no
/// pin — that rose over the run.
fn assert_complete_and_advancing_floor(soak: &ReclaimSoak, witnessed: u64) {
    let cuts = soak.cuts();
    assert!(cuts.len() >= 10, "{} cuts", cuts.len());
    for cut in cuts {
        assert_eq!(cut.outcome, CutOutcome::Stamped, "{cut:?}");
    }
    for pass in soak.passes() {
        assert!(
            pass.synthetic_secs <= witnessed || matches!(pass.result, PassResult::Completed { .. }),
            "{pass:?}"
        );
    }
    let floors: Vec<WalOffset> = soak
        .samples()
        .iter()
        .filter(|s| s.after == Tick::Housekeeping && s.synthetic_secs > witnessed)
        .map(|s| match s.floor {
            RetainFloor::Min(floor) => floor,
            other => panic!(
                "the floor is not complete at {}: {other:?}",
                s.synthetic_secs
            ),
        })
        .collect();
    assert!(floors.is_sorted(), "the floor went backwards: {floors:?}");
    assert!(floors.first() < floors.last(), "the floor never advanced");
}

/// No sample breaks the bound, and the sidecars beside the segments do
/// not grow once the first stamp has written them.
fn assert_bounded(samples: &[ReclaimSample], bound: GrowthBound, witnessed: u64) {
    for sample in samples {
        assert!(
            sample.segment_count <= bound.segments,
            "{sample:?} over {bound:?}"
        );
        assert!(
            sample.segment_bytes <= bound.segment_bytes,
            "{sample:?} over {bound:?}"
        );
    }
    let sidecars: Vec<u64> = samples
        .iter()
        .filter(|s| s.synthetic_secs > witnessed)
        .map(|s| s.disk_bytes - s.segment_bytes)
        .collect();
    assert!(
        sidecars.windows(2).all(|w| w[0] == w[1]),
        "the sidecars grew: {sidecars:?}"
    );
}
