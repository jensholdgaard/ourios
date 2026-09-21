//! RFC0052.17 — §3.2's open-time matrix: the `CHECKPOINT` version, the
//! `RECLAIM` record and the segment headers read together.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Every row that cannot be told apart from a loss fails closed,
//! naming the files it read; every row that can is opened and, where
//! the crash left a witness half-written, promoted durably before
//! anything else reads it.
//!
//! The three rows whose decision also needs a *snapshot* stay
//! `#[ignore]`d stubs, each naming the green slice that discharges it.

use ourios_wal::{FrameKind, OpenError, Wal};

use crate::rfc0052_support::{
    CHECKPOINT, CHECKPOINT_ARMED, CHECKPOINT_SEEN, MODE_KNOWN, MODE_UNRECORDED, RECLAIM,
    build_closed_segment, checkpoint_version, default_config, downgrade_segments, live_slot, open,
    segment_files, set_live_witness, write_legacy_checkpoint,
};

/// Scenario RFC0052.17 — a deleted record on a post-RFC root fails open.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_deleted_record_on_a_post_rfc_root_fails_open() {
    // Given: a root that has checkpointed — so its `CHECKPOINT` is
    // version 2 — and whose `RECLAIM` is then deleted.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = open(root);
    let mark = wal.append(FrameKind::OtlpBatch, b"x").expect("append");
    wal.sync().expect("sync");
    wal.checkpoint(mark).expect("checkpoint");
    drop(wal);
    assert_eq!(checkpoint_version(root), 2, "the sidecar is post-RFC");
    std::fs::remove_file(root.join(RECLAIM)).expect("delete the record");

    // When: the node starts.
    let failure = Wal::open(default_config(root)).expect_err("open must halt");

    // Then: open fails naming the missing record rather than
    // recreating an empty one and pinning.
    let OpenError::Corrupt { detail } = failure else {
        panic!("expected Corrupt, got {failure:?}");
    };
    assert!(
        detail.contains(&root.join(RECLAIM).display().to_string())
            && detail.contains(&root.join(CHECKPOINT).display().to_string()),
        "the error names the missing record and the sidecar that proves the root is post-RFC: {detail}",
    );
    assert!(
        !root.join(RECLAIM).exists(),
        "no empty record is written over the loss",
    );
}
/// Scenario RFC0052.17 — both sidecars missing: version-2 segments are the witness.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_no_sidecars_fails_closed_only_beside_version_2_segments() {
    // Given: a root holding segments and neither sidecar, its segments
    // written under this RFC. A `NoConsumer` root is the same shape
    // from the WAL's side — it has no snapshot artefacts to witness
    // with — so the leg is asserted with and without a snapshots root,
    // which must make no difference.
    for with_snapshots in [false, true] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        build_closed_segment(root, &[b"one"]);
        let segment = segment_files(root).pop().expect("a segment");
        std::fs::remove_file(root.join(RECLAIM)).expect("lose the record");
        if with_snapshots {
            std::fs::create_dir_all(root.join("snapshots")).expect("snapshots root");
        }

        // When: the node starts. Then: it fails closed naming both.
        let failure = Wal::open(default_config(root)).expect_err("open must halt");
        let OpenError::Corrupt { detail } = failure else {
            panic!("expected Corrupt, got {failure:?}");
        };
        assert!(
            detail.contains(&root.join(RECLAIM).display().to_string())
                && detail.contains(&root.join(CHECKPOINT).display().to_string()),
            "the error names both files: {detail}",
        );
        assert!(
            detail.contains(&segment.display().to_string()),
            "and the version-2 segment that is the witness: {detail}",
        );
    }

    // And: a root whose segments are all version 1 — every live
    // pre-RFC root — opens on the legacy branch, creating no record.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"one"]);
    std::fs::remove_file(root.join(RECLAIM)).expect("lose the record");
    downgrade_segments(root);
    let wal = open(root);
    assert!(
        !root.join(RECLAIM).exists(),
        "the legacy branch creates no record; the first checkpoint does",
    );
    assert!(
        !wal.reclaim_state().reclaimable,
        "and a pass plans no segment until the upgrade lands",
    );

    // And: an empty directory with neither sidecar opens as a fresh
    // root, which is what creates the record.
    let tmp = tempfile::TempDir::new().expect("temp");
    let fresh = tmp.path();
    drop(open(fresh));
    assert!(fresh.join(RECLAIM).exists(), "a fresh root gains a record");
}
/// Scenario RFC0052.17 — the empty record is durable before the initial segment.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_fresh_open_writes_the_record_before_the_initial_segment() {
    // Given: a fresh `Wal::open`.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    drop(open(root));
    let record = std::fs::read(root.join(RECLAIM)).expect("the record exists");
    assert_eq!(segment_files(root).len(), 1, "and so does the segment");

    // When: the node restarts immediately afterwards. Then: it finds
    // the record and opens normally.
    drop(open(root));
    assert_eq!(
        std::fs::read(root.join(RECLAIM)).expect("read RECLAIM"),
        record,
        "the record is durable and untouched across the restart",
    );

    // And: with a crash between the record write and the initial
    // segment's creation. That is the only intermediate state the
    // ordering admits — a segment without a record is the shape
    // `rfc0052_17_no_sidecars_fails_closed_only_beside_version_2_segments`
    // pins as fail-closed, and it is unreachable precisely because the
    // record and its parent fsync come first.
    let tmp = tempfile::TempDir::new().expect("temp");
    let crashed = tmp.path();
    drop(open(crashed));
    for path in segment_files(crashed) {
        std::fs::remove_file(path).expect("undo the segment creation");
    }
    let wal = open(crashed);
    assert!(crashed.join(RECLAIM).exists(), "the record is still there");
    assert_eq!(
        wal.reclaim_state().segment_count,
        1,
        "and the interrupted open completes, minting the initial segment",
    );
}
/// Scenario RFC0052.17 — the `checkpoint_armed` / `checkpoint_seen` flag rows.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_checkpoint_flags_beside_a_missing_or_stale_checkpoint() {
    seen_without_a_checkpoint_fails_closed();
    armed_without_segments_opens_fresh_and_empty();
    armed_with_segments_retains_the_arming_and_mode();
    armed_beside_a_version_2_checkpoint_is_promoted_at_open();
    armed_beside_a_version_1_checkpoint_retries_the_upgrade();
}

/// `checkpoint_seen` with `CHECKPOINT` missing is a lost checkpoint and
/// fails closed naming both files, whatever the entries say.
fn seen_without_a_checkpoint_fails_closed() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    drop(open(root));
    set_live_witness(root, CHECKPOINT_ARMED | CHECKPOINT_SEEN, MODE_UNRECORDED);

    let failure = Wal::open(default_config(root)).expect_err("open must halt");
    let OpenError::Corrupt { detail } = failure else {
        panic!("expected Corrupt, got {failure:?}");
    };
    assert!(
        detail.contains(&root.join(RECLAIM).display().to_string())
            && detail.contains(&root.join(CHECKPOINT).display().to_string()),
        "the error names both files: {detail}",
    );
}

/// `checkpoint_armed` without `seen` and `CHECKPOINT` absent, with no
/// segments, is a genuinely fresh root whose arming preceded a
/// checkpoint that never landed: it opens with an empty record and is
/// re-armed by the next attempt. A record with neither flag opens the
/// same way.
fn armed_without_segments_opens_fresh_and_empty() {
    for flags in [CHECKPOINT_ARMED, 0] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        drop(open(root));
        for path in segment_files(root) {
            std::fs::remove_file(path).expect("a root that never held a segment");
        }
        set_live_witness(root, flags, MODE_UNRECORDED);

        let mut wal = open(root);
        let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
        assert_eq!(
            live_slot(&bytes).1,
            0,
            "the record is opened empty, whatever it was armed with",
        );

        let mark = wal.append(FrameKind::OtlpBatch, b"x").expect("append");
        wal.sync().expect("sync");
        wal.checkpoint(mark).expect("the next attempt re-arms it");
        let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
        assert_eq!(
            live_slot(&bytes).1,
            CHECKPOINT_ARMED | CHECKPOINT_SEEN,
            "and the next checkpoint arms and then witnesses it",
        );
    }
}

/// The same flags **with segments present** are a legacy root mid
/// migration — the first rotation on such a root writes the record
/// before its version-2 segment — so the record is retained with its
/// arming and its mode rather than emptied.
fn armed_with_segments_retains_the_arming_and_mode() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    drop(open(root));
    set_live_witness(root, CHECKPOINT_ARMED, MODE_KNOWN);

    let wal = open(root);
    let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    assert_eq!(
        (live_slot(&bytes).1, live_slot(&bytes).2),
        (CHECKPOINT_ARMED, MODE_KNOWN),
        "the arming and the mode a pass may already have adopted survive",
    );
    assert!(
        !wal.reclaim_state().reclaimable,
        "and the root opens on the legacy branch, owing the upgrade",
    );
}

/// An armed record beside a **present** version-2 `CHECKPOINT` is the
/// crash after the rename and before the record's next write: it opens
/// normally and is promoted to `seen` durably at open, never read as a
/// fault.
fn armed_beside_a_version_2_checkpoint_is_promoted_at_open() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = open(root);
    let mark = wal.append(FrameKind::OtlpBatch, b"x").expect("append");
    wal.sync().expect("sync");
    wal.checkpoint(mark).expect("checkpoint");
    drop(wal);
    set_live_witness(root, CHECKPOINT_ARMED, MODE_UNRECORDED);

    let wal = open(root);
    let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    assert_eq!(
        live_slot(&bytes).1,
        CHECKPOINT_ARMED | CHECKPOINT_SEEN,
        "open promotes the witness durably, before anything reads the matrix",
    );
    assert!(
        wal.reclaim_state().reclaimable,
        "and the root opens normally"
    );
}

/// An armed record beside a still-**version-1** `CHECKPOINT` is the
/// upgrade write failing between the arming and the rename: the root
/// opens on the legacy branch, reclaims nothing until the next
/// checkpoint retries the upgrade, and that retry succeeds against the
/// arming already on disk.
fn armed_beside_a_version_1_checkpoint_retries_the_upgrade() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"a1"]);
    let second = build_closed_segment(root, &[b"b1"]);
    build_closed_segment(root, &[b"c1"]);
    let mark = *second.last().expect("segment two's offsets");
    write_legacy_checkpoint(root, mark);
    set_live_witness(root, CHECKPOINT_ARMED, MODE_UNRECORDED);

    let mut wal = open(root);
    assert!(
        !wal.reclaim_state().reclaimable,
        "a version-1 sidecar is not a witness",
    );
    wal.housekeeping(None).expect("housekeeping");
    assert_eq!(
        segment_files(root).len(),
        3,
        "so the pass plans no segment, however far the checkpoint reaches",
    );

    // The retry: the mark equals the one on disk, which on an idle
    // node is every barrier after the first, and the upgrade still
    // happens because it is version-aware rather than mark-aware.
    wal.checkpoint(mark)
        .expect("the next checkpoint retries the upgrade");
    assert_eq!(
        checkpoint_version(root),
        2,
        "the sidecar is rewritten at version 2"
    );
    assert!(
        wal.reclaim_state().reclaimable,
        "and the witness now exists",
    );
    wal.housekeeping(None).expect("housekeeping");
    assert_eq!(
        segment_files(root).len(),
        1,
        "so the pass after the upgrade reclaims normally",
    );
}
/// Scenario RFC0052.17 — no record and no checkpoint: a pre-RFC root gains an empty record.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (its pin leg is a RetainFloor case, which the pass derives)"]
fn rfc0052_17_absent_record_and_absent_checkpoint_opens_and_seeds_an_empty_record() {
    todo!(
        "RFC0052.17 — a root with no RECLAIM and no CHECKPOINT at all: \
         Wal::open succeeds, an empty record is durable before the first \
         housekeeping pass, and a tenant without a snapshot pins rather \
         than halts"
    );
}
/// Scenario RFC0052.17 — the legacy-root migration window.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (its final leg, the legacy stale-gap check, compares a tenant's oldest surviving frame with its last recorded horizon)"]
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
/// Scenario RFC0052.17 — slot ids and the `published_seeded_*` flag rows.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.17 stub — implemented in the housekeeping green slice B (both legs read PUBLISHED, whose writer and format are RFC 0053's)"]
fn rfc0052_17_slot_ids_survive_a_published_only_write_and_seeding_flags_resolve() {
    todo!(
        "RFC0052.17 — a tenant introduced by a PUBLISHED-only write keeps \
         its slot id across a restart; published_seeded_armed without \
         confirmed and PUBLISHED absent leaves the next start free to \
         seed again, while the same record with PUBLISHED present is \
         promoted to confirmed durably at open, never read as a fault"
    );
}
