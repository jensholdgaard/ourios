//! RFC0052.11 — A node already wedged before this RFC halts actionably
//! at open.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! A hand-built directory fixture rather than fault injection (RFC 0052
//! §6): the state predates the code under test. §3.3 withdrew the
//! shape-based heuristic — an unreadable newest segment is
//! indistinguishable from real corruption — so open still halts; what
//! this RFC adds is the error's guidance.

use ourios_wal::{OpenError, Wal};

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

/// Scenario RFC0052.11 — a node rotating under this RFC cannot reach that state.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.11 stub — implemented in the rotation green slice C (header fsynced under the temporary name before the rename)"]
fn rfc0052_11_post_rfc_rotations_never_leave_a_selectable_orphan() {
    todo!(
        "RFC0052.11 — with a crash injected at every step of a §3.3 \
         rotation, no surviving *.wal has unreadable header bytes: the \
         header is durable under the .wal.partial name before the \
         rename, so Wal::open never meets the legacy shape on a root \
         whose rotations all happened under this RFC"
    );
}
