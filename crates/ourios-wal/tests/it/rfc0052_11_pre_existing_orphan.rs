//! RFC0052.11 — A node already wedged before this RFC halts actionably
//! at open.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! A hand-built directory fixture rather than fault injection (RFC 0052
//! §6): the state predates the code under test. §3.3 withdrew the
//! shape-based heuristic — an unreadable newest segment is
//! indistinguishable from real corruption — so open still halts; what
//! this RFC adds is the error's guidance.

use ourios_wal::{
    FrameKind, FrameSink, OpenError, RecoveryError, RotationFaults, RotationKind, RotationSite,
    Wal, WalConfig, WalOffset,
};

use crate::rfc0052_support::{
    CHECKPOINT, RECLAIM, build_closed_segment, default_config, downgrade_segments, segment_files,
    truncate_segment_header,
};

/// Scenario RFC0052.11 — a partial-header newest `*.wal` still halts as `Corrupt`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_11_legacy_orphan_halts_open_naming_the_file_and_shape() {
    // Given: a WAL directory written before this RFC — version-1
    // segments and neither sidecar — whose newest `*.wal` has partial
    // header bytes.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"kept"]);
    build_closed_segment(root, &[b"orphan"]);
    std::fs::remove_file(root.join(RECLAIM)).expect("a pre-RFC root has no record");
    assert!(!root.join(CHECKPOINT).exists(), "and no checkpoint");
    downgrade_segments(root);
    let newest = segment_files(root).pop().expect("two segments");
    truncate_segment_header(&newest);

    // When: the node starts.
    let failure = Wal::open(default_config(root)).expect_err("open must halt");

    // Then: `OpenError::Corrupt`, naming the file and the shape, and
    // naming a failed rotation as one possible cause without asserting
    // it — §3.3 withdrew the heuristic that would have unlinked it.
    let OpenError::Corrupt { detail } = failure else {
        panic!("expected Corrupt, got {failure:?}");
    };
    assert!(
        detail.contains(&newest.display().to_string()),
        "the error names the file: {detail}",
    );
    assert!(
        detail.contains("newest segment") && detail.contains("header"),
        "the error describes the observable shape: {detail}",
    );
    assert!(
        detail.contains("rotation") && detail.contains("operator"),
        "a failed rotation is named as one way to produce it, and the decision is an operator's: {detail}",
    );
    assert!(
        newest.exists(),
        "nothing is unlinked: an unreadable header is indistinguishable from real corruption",
    );
    assert_eq!(
        segment_files(root).len(),
        2,
        "and no other segment is touched either",
    );
}

/// Every surviving frame, so `replay` proves each segment readable
/// rather than only the newest one `Wal::open` validates.
#[derive(Default)]
struct CountingSink(usize);

impl FrameSink for CountingSink {
    fn consume(
        &mut self,
        _offset: WalOffset,
        _kind: FrameKind,
        _payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.0 += 1;
        Ok(())
    }
}

/// Scenario RFC0052.11 — a node rotating under this RFC cannot reach that state.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// The population above is finite and shrinking because the shape is
/// now unreachable: §3.3 fsyncs the header under the `.wal.partial`
/// name *before* the rename, and `list_segments` returns only `*.wal`,
/// so a rotation that dies at any step leaves no `*.wal` whose header
/// bytes might be missing. Asserted by killing a rotation at each of
/// the five sites in turn and reopening.
#[test]
fn rfc0052_11_post_rfc_rotations_never_leave_a_selectable_orphan() {
    for site in RotationSite::ALL {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        let config = WalConfig {
            segment_age_secs: 1,
            ..default_config(root)
        };

        let mut wal = Wal::open(config.clone()).expect("open");
        wal.append(FrameKind::OtlpBatch, b"a frame worth keeping")
            .expect("append");
        wal.sync().expect("sync");
        wal.arm_rotation_faults(RotationFaults::always(site));

        // The rotation dies at `site`; dropping the handle without a
        // further call is this process ending there.
        let _ = wal.rotate(RotationKind::Owed);
        drop(wal);

        for path in segment_files(root) {
            let bytes = std::fs::read(&path).expect("read segment");
            assert!(
                bytes.len() >= 24 && &bytes[0..4] == b"OWAL",
                "{site:?}: every surviving *.wal has a complete header: {}",
                path.display(),
            );
        }
        let mut reopened = Wal::open(config).expect("reopen");
        reopened
            .replay(&mut CountingSink::default())
            .expect("replay reads every surviving segment");
    }
}
