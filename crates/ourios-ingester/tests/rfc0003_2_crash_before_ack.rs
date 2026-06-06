//! RFC0003.2 — Crash-before-ack: at-least-once with retry tolerance `[§3.4]`.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands. Per §8 this uses a child-process harness mirroring
//! `wal_crash_fixture` (PR #126).

/// Scenario RFC0003.2 — Crash-before-ack: at-least-once with retry tolerance.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.2)"]
#[test]
fn rfc0003_2_crash_before_ack_is_at_least_once_not_lossy() {
    unimplemented!(
        "RFC0003.2 — SIGKILL the receiver between Wal::sync return and ack-emit, \
         restart, re-issue the export; the post-restart WAL holds TWO OtlpBatch \
         frames each round-tripping (prost) to a request equivalent to the input. \
         Asserts no loss + safe retry; explicitly does NOT assert dedup \
         (at-least-once per the OTLP duplicate-data section)."
    );
}
