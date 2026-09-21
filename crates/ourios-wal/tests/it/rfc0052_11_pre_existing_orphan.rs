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

/// Scenario RFC0052.11 — a partial-header newest `*.wal` still halts as `Corrupt`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.11 stub — implemented in the reclaim-record green slice A (OpenError::Corrupt names the file and the rotation-remnant shape)"]
fn rfc0052_11_legacy_orphan_halts_open_naming_the_file_and_shape() {
    todo!(
        "RFC0052.11 — a WAL directory whose newest *.wal has partial \
         header bytes, written before this RFC; Wal::open reports \
         OpenError::Corrupt rather than unlinking anything, and the \
         error names the file, describes the observable shape and names \
         a failed rotation as one possible cause without asserting it"
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
