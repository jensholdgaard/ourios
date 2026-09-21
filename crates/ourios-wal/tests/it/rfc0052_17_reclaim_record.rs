//! RFC0052.17 — The reclaim record is the only startup witness, and it
//! is fail-closed.
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
//!
//! The legs that read a *snapshot* — halt-or-pin per tenant, the
//! legacy stale-gap check, the consumer mode a pass adopts — need the
//! `SnapshotHorizons` surface §3.7 puts on `housekeeping_prepare`, so
//! they move to the housekeeping slice with the pass that carries it.
//! What lands here is every row the WAL can decide from its own two
//! sidecars and its segment headers.

use ourios_wal::{FrameKind, LedgerError, OpenError, Wal};

use crate::rfc0052_support::{
    CHECKPOINT, CHECKPOINT_ARMED, CHECKPOINT_SEEN, DEFAULT_MAX_TENANTS,
    DEFAULT_MAX_UNLINKS_PER_PASS, FILE_HEADER_LEN, MODE_KNOWN, MODE_UNRECORDED, REBUILD, RECLAIM,
    build_closed_segment, checkpoint_version, default_config, downgrade_segments, live_slot, open,
    reclaim_file, segment_files, set_live_witness, slot_len, stored_slot_len, tear_inactive_slot,
    write_legacy_checkpoint,
};

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

/// Scenario RFC0052.17 — a corrupt record fails open as `Corrupt`, never as missing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_corrupt_record_fails_open_naming_the_file() {
    // Given: a root whose `RECLAIM` fails its version byte, and one
    // whose live slot fails its checksum.
    for poison in [poison_version, poison_live_slot] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        drop(open(root));
        poison(root);

        // When: the node starts.
        let failure = Wal::open(default_config(root)).expect_err("open must halt");

        // Then: `OpenError::Corrupt` naming the file.
        let OpenError::Corrupt { detail } = failure else {
            panic!("expected Corrupt, got {failure:?}");
        };
        assert!(
            detail.contains(&root.join(RECLAIM).display().to_string()),
            "the error names the record: {detail}",
        );
        // And: never read as missing. Missing means "never reclaimed"
        // and is a different, distinctly worded state.
        assert!(
            !detail.contains("is missing"),
            "a damaged record is not read as an absent one: {detail}",
        );
    }
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

/// Scenario RFC0052.17 — a torn slot leaves the previous slot live.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_crash_inside_the_inactive_slot_write_keeps_the_previous_slot() {
    // Given: a root whose record has been committed twice — the first
    // checkpoint arms it and then writes `checkpoint_seen` — so both
    // slots hold a valid record and the reader takes the greater
    // generation.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_closed_segment(root, &[b"a1", b"a2"]);
    let second = build_closed_segment(root, &[b"b1", b"b2"]);
    build_closed_segment(root, &[b"c1"]);
    let mark = *second.last().expect("segment two's offsets");
    let mut wal = open(root);
    wal.checkpoint(mark).expect("checkpoint");
    drop(wal);

    let before = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    let (generation, flags, _) = live_slot(&before);
    assert!(generation > 1, "the second commit raised the generation");
    assert_eq!(
        flags,
        CHECKPOINT_ARMED | CHECKPOINT_SEEN,
        "the live slot is the one the last complete commit wrote",
    );

    // When: a crash lands inside the inactive slot's write, so its
    // bytes no longer check out.
    tear_inactive_slot(root);

    // Then: the reader takes the other one, and the record the
    // previous pass left is what the node opens on.
    let mut wal = open(root);
    let after = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    assert_eq!(
        live_slot(&after),
        (generation, flags, MODE_UNRECORDED),
        "the surviving slot is unchanged by the torn write",
    );
    assert!(
        wal.reclaim_state().reclaimable,
        "the witness survives, so the root is still one a pass may plan segments on",
    );

    // And: the next pass proceeds from that record.
    assert_eq!(
        segment_files(root).len(),
        3,
        "nothing is reclaimed before the pass",
    );
    wal.housekeeping(None).expect("housekeeping");
    assert_eq!(
        segment_files(root).len(),
        1,
        "the two segments at or below the surviving record's checkpoint are reclaimed",
    );
    assert!(
        !first.is_empty(),
        "the reclaimed segments really held the frames the mark covers",
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

/// Scenario RFC0052.17 — a replayed tenant key over 128 bytes fails open.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_overlong_tenant_key_in_replay_fails_open_naming_the_frame() {
    // Given: a frame written before the codec was amended, carrying a
    // tenant longer than 128 bytes. The encoder refuses it now, so the
    // payload is laid out by hand in RFC 0046 §3.3's shape.
    const OVERLONG: usize = 200;
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut payload = Vec::new();
    payload.extend_from_slice(&u16::try_from(OVERLONG).expect("fits u16").to_le_bytes());
    payload.extend(std::iter::repeat_n(b't', OVERLONG));
    payload.extend_from_slice(b"protobuf");

    let mut wal = open(root);
    wal.append(FrameKind::OtlpBatch, b"a short frame first")
        .expect("append");
    let offset = wal
        .append(FrameKind::TenantOtlpBatch, &payload)
        .expect("append the pre-amendment frame");
    wal.sync().expect("sync");
    drop(wal);

    // When: startup rebuilds the ledger from the surviving frames.
    let mut wal = open(root);
    let failure = wal.rebuild_ledger().expect_err("startup must fail closed");

    // Then: it fails naming the frame offset and the length, rather
    // than truncating the key or dropping the frame.
    let LedgerError::TenantTooLong {
        offset: at,
        found,
        limit,
    } = failure
    else {
        panic!("expected TenantTooLong, got {failure:?}");
    };
    assert_eq!(at, offset, "the error names the frame's own offset");
    assert_eq!(found, OVERLONG, "and the length it found");
    assert_eq!(limit, 128, "against RFC 0048 §3.1's bound");
    assert_eq!(
        segment_files(root).len(),
        1,
        "the frame is not dropped and its segment is not touched",
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

/// Scenario RFC0052.17 — geometry growth rebuilds through `RECLAIM.new`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_larger_geometry_rebuilds_atomically_smaller_opens_in_place() {
    // Given: a root whose record was built for capacities below the
    // configured ones, beside a `RECLAIM.new` a previous crashed
    // rebuild left behind.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"one"]);
    std::fs::write(
        root.join(RECLAIM),
        reclaim_file(2, 1, CHECKPOINT_ARMED, MODE_KNOWN),
    )
    .expect("a narrow record");
    std::fs::write(root.join(REBUILD), b"a crashed rebuild's leftovers").expect("stale temp");

    // When: the node starts.
    let wal = open(root);
    drop(wal);

    // Then: the file is rebuilt at the larger geometry and the record
    // survives it — ids are copied to the same index in the wider
    // stride, never reassigned.
    let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    let wide = slot_len(DEFAULT_MAX_TENANTS, DEFAULT_MAX_UNLINKS_PER_PASS);
    assert_eq!(stored_slot_len(&bytes), wide, "the stored slot_len grew");
    assert_eq!(
        bytes.len(),
        FILE_HEADER_LEN + 2 * wide,
        "and the file is the whole two-slot size",
    );
    let (_, flags, mode) = live_slot(&bytes);
    assert_eq!(
        (flags, mode),
        (CHECKPOINT_ARMED, MODE_KNOWN),
        "the record the narrow file held survives the rebuild",
    );
    assert!(
        !root.join(REBUILD).exists(),
        "the rebuild renames its temp away rather than leaving one per attempt",
    );

    // And: a configured value at or below the stored capacity opens
    // without rewriting — the file is never shrunk and never churned.
    let before = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    drop(open(root));
    let after = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    assert_eq!(before, after, "an in-place open rewrites nothing");
}

/// Scenario RFC0052.17 — a full volume fails open, never a rotation state; passes never allocate.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_17_full_volume_fails_open_and_passes_write_without_allocating() {
    // Given: a root where the record's allocation cannot succeed. A
    // full volume is not reachable from a test, so the allocation is
    // made to fail the same way — the path cannot be opened for
    // writing — which is the branch under test.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    std::fs::create_dir_all(root.join(RECLAIM)).expect("block the allocation");

    // When: the node starts. Then: `OpenError::Io` naming the file,
    // and the node does not start — not a rotation state, which no
    // running process exists to report.
    let failure = Wal::open(default_config(root)).expect_err("open must fail");
    let OpenError::Io { op, .. } = failure else {
        panic!("expected Io, got {failure:?}");
    };
    assert!(
        op.contains(RECLAIM),
        "the failing operation names the file: {op}"
    );
    assert!(
        segment_files(root).is_empty(),
        "and no segment is created behind a record that does not exist",
    );

    // And: it starts normally once space is freed.
    std::fs::remove_dir(root.join(RECLAIM)).expect("free the space");
    let mut wal = open(root);
    let mark = wal.append(FrameKind::OtlpBatch, b"x").expect("append");
    wal.sync().expect("sync");

    // And: every pass writes its slot without allocating, whatever the
    // tenant set does — `slot_len` never changes for the life of the
    // file and the file never grows.
    let before = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    let (generation, ..) = live_slot(&before);
    wal.checkpoint(mark).expect("checkpoint");
    wal.housekeeping(None).expect("housekeeping");
    let after = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    assert_eq!(after.len(), before.len(), "the file never grows");
    assert_eq!(
        stored_slot_len(&after),
        stored_slot_len(&before),
        "slot_len is fixed for the life of the file",
    );
    assert!(
        live_slot(&after).0 > generation,
        "and the writes really happened, in place",
    );
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

/// Flip the file header's version byte: the record carries a version
/// this build does not know.
fn poison_version(root: &std::path::Path) {
    let path = root.join(RECLAIM);
    let mut bytes = std::fs::read(&path).expect("read RECLAIM");
    bytes[4] = 0xFF;
    std::fs::write(&path, &bytes).expect("poison the version");
}

/// Flip a byte inside the live slot's covered range: its CRC32-C stops
/// matching, and the other slot was never written.
fn poison_live_slot(root: &std::path::Path) {
    let path = root.join(RECLAIM);
    let mut bytes = std::fs::read(&path).expect("read RECLAIM");
    bytes[FILE_HEADER_LEN + 20] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("poison the live slot");
}
