//! RFC0052.17 — the `RECLAIM` file's own format: what the bytes must
//! say, and what a reader does when they do not.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Missing means "never reclaimed", damaged means "reclaimed, extent
//! unknown", and only the first is safe to proceed from — so every row
//! here is about telling those apart from the file alone.

use ourios_wal::{FrameKind, LedgerError, OpenError, Wal};

use crate::rfc0052_support::{
    CHECKPOINT_ARMED, CHECKPOINT_SEEN, DEFAULT_MAX_TENANTS, DEFAULT_MAX_UNLINKS_PER_PASS,
    FILE_HEADER_LEN, MODE_KNOWN, MODE_UNRECORDED, REBUILD, RECLAIM, build_closed_segment,
    default_config, live_slot, open, reclaim_file, segment_files, slot_len, stored_slot_len,
    tear_inactive_slot,
};

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
