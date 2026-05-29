//! RFC0008.2 — Crash-recovery completeness `[§3.4 / H3]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. This is the H3 normative requirement that runs
//! on **every PR** (failure blocks merge) once green.
//!
//! The assertion has two halves: **no fsync'd frame is lost**
//! (un-negotiable) AND **any un-fsync'd frame is handled
//! safely** (per RFC0008.5's three buckets — replayed if
//! complete + CRC-valid, torn-tail truncate on the newest
//! segment if partial, RFC0008.5 corruption if complete with
//! CRC mismatch). The test does *not* assert "exactly the
//! fsync'd frames" — kernel post-mortem flush admits surplus
//! unsynced frames being readable on restart.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.2)"]
#[test]
fn rfc0008_2_sigkill_between_append_and_sync_loses_no_fsynced_frame() {
    unimplemented!(
        "RFC0008.2 first arm — fork harness sends SIGKILL between append+sync; \
         on restart `replay` recovers every fsync'd frame, any un-fsync'd is \
         handled safely (per RFC0008.5 three buckets)"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.2)"]
#[test]
fn rfc0008_2_sigkill_between_sync_and_ack_loses_no_fsynced_frame() {
    unimplemented!("RFC0008.2 second arm — SIGKILL between sync+ack");
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.2)"]
#[test]
fn rfc0008_2_audit_event_frames_survive_alongside_otlp_batches() {
    unimplemented!(
        "RFC0008.2 audit arm — `FrameKind::AuditEvent` frames \
         appended-and-fsync'd before the kill survive identically (the RFC 0005 \
         §3.7 audit-durability contract)"
    );
}
