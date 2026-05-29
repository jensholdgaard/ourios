//! RFC0008.1 — WAL-before-ack `[§3.4]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate: the implementation is pending. The test asserts
//! the §5 contract — the 2xx (gRPC `OK`) is emitted only after
//! `Wal::sync` returns `Ok(_)`, measured by an `AtomicBool`
//! set **after** sync returns (not inside, which would let an
//! ack racing the sync trivially pass). Two fault arms exercise
//! the `append` and `sync` error paths.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.1)"]
#[test]
fn rfc0008_1_ack_only_after_sync_returns_ok() {
    unimplemented!(
        "RFC0008.1 — wire a fake receiver to a real Wal, assert the post-sync \
         AtomicBool flag is the gate (per docs/rfcs/0008-wal.md §5)"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.1)"]
#[test]
fn rfc0008_1_append_fault_suppresses_ack() {
    unimplemented!("RFC0008.1 fault arm — `AppendError::Io` MUST suppress the ack");
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.1)"]
#[test]
fn rfc0008_1_sync_fault_suppresses_ack() {
    unimplemented!("RFC0008.1 fault arm — `SyncError::Io` MUST suppress the ack");
}
