//! RFC0052.17 — §3.2's open-time matrix: the `CHECKPOINT` version, the
//! `RECLAIM` record and the segment headers read together.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Every row that cannot be told apart from a loss fails closed,
//! naming the files it read; every row that can is opened and, where
//! the crash left a witness half-written, promoted durably before
//! anything else reads it.
//!
//! The one row that needs the `PUBLISHED` sidecar stays an
//! `#[ignore]`d stub: its writer and format are RFC 0055's.

use std::collections::HashMap;

use ourios_wal::{
    FrameKind, MIN_SEGMENT_SIZE_BYTES, OpenError, RetainFloor, SnapshotHorizons, TenantBatch,
    TenantHorizon, Wal, WalOffset,
};

use crate::rfc0052_support::{
    CHECKPOINT, CHECKPOINT_ARMED, CHECKPOINT_SEEN, MODE_KNOWN, MODE_UNRECORDED, RECLAIM,
    build_closed_segment, build_tenant_segment, checkpoint_version, default_config,
    downgrade_segments, live_slot, open, segment_files, set_live_witness, tenant_id,
    write_legacy_checkpoint,
};

/// Scenario RFC0052.17 — a legacy root that rotates before its first
/// checkpoint stays openable.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// §3.2's ordering rule reaches rotation, not only open: **no
/// version-2 segment is ever created before the record is durable.**
/// Rotation is append-driven and the first barrier may be minutes
/// away, so without this a live pre-RFC root would be bricked by
/// rotating — it would come back holding a version-2 segment beside
/// no sidecar, the one shape
/// `rfc0052_17_no_sidecars_fails_closed_only_beside_version_2_segments`
/// fails closed on.
///
/// The crash-injected half of this row — a kill *between* the record
/// write and the segment's creation — stays
/// `rfc0052_17_legacy_root_rotation_writes_the_record_before_the_v2_segment`,
/// on the rotation slice that owns the injection hook.
#[test]
fn rfc0052_17_legacy_root_rotation_stays_openable_without_a_checkpoint() {
    // Given: a pre-RFC root — version-1 segments, neither sidecar —
    // that has never checkpointed.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"written before this RFC"]);
    std::fs::remove_file(root.join(RECLAIM)).expect("a pre-RFC root has no record");
    downgrade_segments(root);
    let mut config = default_config(root);
    config.segment_size_bytes = MIN_SEGMENT_SIZE_BYTES;
    let mut wal = Wal::open(config.clone()).expect("a legacy root opens");
    assert!(
        !root.join(RECLAIM).exists(),
        "the legacy branch creates no record at open; the rotation does",
    );

    // When: an append rotates it, long before any checkpoint. 16 MiB
    // fits a fresh 17 MiB segment; the next 2 MiB would straddle the
    // cap, so it triggers the rotation.
    wal.append(FrameKind::OtlpBatch, &vec![0xAA; 16 * 1024 * 1024])
        .expect("first append");
    wal.append(FrameKind::OtlpBatch, &vec![0xBB; 2 * 1024 * 1024])
        .expect("second append rotates");
    wal.sync().expect("sync");
    drop(wal);

    // Then: the record is durable beside the version-2 segment the
    // rotation created, so the restart opens rather than halting.
    assert!(
        root.join(RECLAIM).exists(),
        "the rotation writes and fsyncs the record before the version-2 segment",
    );
    assert_eq!(
        live_slot(&std::fs::read(root.join(RECLAIM)).expect("read RECLAIM")).1,
        CHECKPOINT_ARMED,
        "armed with its mode unrecorded, which the matrix reads as a root mid migration",
    );
    let wal = Wal::open(config).expect("the rotated legacy root still opens");
    assert!(
        !wal.reclaim_state().reclaimable,
        "and stays on the legacy branch until the first checkpoint upgrades the sidecar",
    );
}

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
fn rfc0052_17_absent_record_and_absent_checkpoint_opens_and_seeds_an_empty_record() {
    // Given: a root with no `RECLAIM` and no `CHECKPOINT` at all.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    assert!(!root.join(RECLAIM).exists() && !root.join(CHECKPOINT).exists());

    // When: `Wal::open` runs. Then: it succeeds and the empty record
    // is durable before anything else — the ordering that keeps the
    // fail-closed row from firing on a node's own first start.
    let mut wal = open(root);
    assert_eq!(
        live_slot(&std::fs::read(root.join(RECLAIM)).expect("read RECLAIM")).1,
        0,
        "the record is seeded empty",
    );
    assert!(!root.join(CHECKPOINT).exists(), "and nothing checkpointed");

    // And: a tenant without a snapshot pins rather than halts — the
    // record holds no entry for it, so it has lost nothing.
    let mark = wal
        .append(
            FrameKind::TenantOtlpBatch,
            &TenantBatch::encode("alpha", b"a1").expect("encode"),
        )
        .expect("append");
    wal.sync().expect("sync");
    wal.checkpoint(mark).expect("checkpoint");
    let plan = wal
        .housekeeping_prepare(&SnapshotHorizons::Known(HashMap::new()), 64)
        .expect("a tenant with no entry pins rather than halting");
    assert_eq!(
        plan.progress.floor,
        RetainFloor::Pinned {
            offset: mark,
            tenants: 1,
        },
    );
}
/// Scenario RFC0052.17 — the legacy-root migration window.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_version_1_checkpoint_opens_legacy_and_upgrades_on_first_checkpoint() {
    // Given: a root holding a version-1 `CHECKPOINT` — an RFC0008.7
    // fixture — and no `RECLAIM`.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"a1"]);
    let second = build_closed_segment(root, &[b"b1"]);
    build_closed_segment(root, &[b"c1"]);
    std::fs::remove_file(root.join(RECLAIM)).expect("a pre-RFC root has no record");
    downgrade_segments(root);
    let mark = *second.last().expect("segment two's offsets");
    write_legacy_checkpoint(root, mark);

    // When: it opens. Then: it is read as pre-RFC and gains no record.
    let wal = open(root);
    assert!(
        !root.join(RECLAIM).exists(),
        "the legacy branch creates no record",
    );
    assert!(!wal.reclaim_state().reclaimable);

    // And: a restart in that window takes the legacy branch again
    // rather than any fail-closed row.
    drop(wal);
    let mut wal = open(root);
    assert!(!root.join(RECLAIM).exists() && !wal.reclaim_state().reclaimable);

    // And: its first checkpoint rewrites the sidecar at version 2 and
    // creates the record armed on the same path, **even when the mark
    // equals the one on disk** — the idle-node case.
    wal.checkpoint(mark).expect("the upgrade");
    assert_eq!(checkpoint_version(root), 2);
    assert_eq!(
        live_slot(&std::fs::read(root.join(RECLAIM)).expect("read RECLAIM")).1,
        CHECKPOINT_ARMED | CHECKPOINT_SEEN,
        "the record is created armed and then witnessed on that path",
    );

    // While an equal mark on an already-version-2 sidecar takes the
    // no-write path.
    let untouched = std::fs::metadata(root.join(CHECKPOINT))
        .expect("stat")
        .modified()
        .expect("mtime");
    wal.checkpoint(mark).expect("the no-write path");
    assert_eq!(
        std::fs::metadata(root.join(CHECKPOINT))
            .expect("stat")
            .modified()
            .expect("mtime"),
        untouched,
        "a settled checkpoint re-asserted at the same mark writes nothing",
    );

    legacy_stale_gap_fails_closed_naming_the_tenant();
}

/// On a legacy root the deployment invariant behind the branch — #793
/// means no served root ever reclaimed — is belted rather than
/// trusted: a tenant whose snapshot does not restore and whose oldest
/// surviving frame sits above its last recorded horizon is the shape
/// reclamation under a version-1 checkpoint leaves behind.
fn legacy_stale_gap_fails_closed_naming_the_tenant() {
    let build = || {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path().to_path_buf();
        let frames = build_tenant_segment(&root, &[("alpha", b"a1")]);
        build_tenant_segment(&root, &[("alpha", b"a2")]);
        std::fs::remove_file(root.join(RECLAIM)).expect("a pre-RFC root has no record");
        downgrade_segments(&root);
        write_legacy_checkpoint(&root, frames[0]);
        (tmp, root, frames[0])
    };

    // A recorded horizon at or above the oldest surviving frame is the
    // ordinary pre-RFC root: it pins and carries on.
    let (_tmp, root, oldest) = build();
    let mut wal = open(&root);
    wal.rebuild_ledger().expect("ledger");
    wal.housekeeping_prepare(
        &SnapshotHorizons::Known(HashMap::from([(
            tenant_id("alpha"),
            TenantHorizon::RecordedOnly(oldest),
        )])),
        64,
    )
    .expect("a horizon that explains the oldest surviving frame");

    // One *below* it is the stale gap, and so is no horizon at all: a
    // snapshot that cannot be decoded even for that field has nothing
    // to compare.
    let below = WalOffset {
        segment: uuid::Uuid::nil(),
        byte: 0,
    };
    for (what, horizons) in [
        (
            "a recorded horizon below the oldest surviving frame",
            SnapshotHorizons::Known(HashMap::from([(
                tenant_id("alpha"),
                TenantHorizon::RecordedOnly(below),
            )])),
        ),
        ("no horizon at all", SnapshotHorizons::Known(HashMap::new())),
    ] {
        let (_tmp, root, _) = build();
        let mut wal = open(&root);
        wal.rebuild_ledger().expect("ledger");
        let failure = wal
            .housekeeping_prepare(&horizons, 64)
            .expect_err("must fail closed");
        assert!(
            format!("{failure}").contains("alpha"),
            "{what}: the refusal names the tenant: {failure}",
        );
    }
}
/// Scenario RFC0052.17 — slot ids and the `published_seeded_*` flag rows.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// Both legs read the `PUBLISHED` sidecar. §3.2 defines the two
/// `published_seeded_*` header bits and says they are RFC 0053's,
/// amending this header — but the **writer and the file's format**
/// belong to **RFC 0055** (publication frontiers), which is still
/// `drafted`. The earlier reason here misattributed them to RFC 0053;
/// nothing in slice B can seed a tenant through a file that has no
/// writer, and inventing one would put the id space's owner in the
/// wrong RFC.
#[test]
#[ignore = "RFC0052.17 stub — blocked on RFC 0055 (publication frontiers), which owns the PUBLISHED writer and format; the slice that lands it discharges this"]
fn rfc0052_17_slot_ids_survive_a_published_only_write_and_seeding_flags_resolve() {
    todo!(
        "RFC0052.17 — a tenant introduced by a PUBLISHED-only write keeps \
         its slot id across a restart; published_seeded_armed without \
         confirmed and PUBLISHED absent leaves the next start free to \
         seed again, while the same record with PUBLISHED present is \
         promoted to confirmed durably at open, never read as a fault"
    );
}
