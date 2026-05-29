//! RFC0008.3 — Crash-recovery non-amplification `[H3]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. Recovery wall time scales O(N) in segment count;
//! per-record work is dominated by the miner's tokenize cost
//! (corpus-bench-governed). The real assertion lives in a
//! `criterion` benchmark (fixture corpus of N ∈ {1, 4, 16}
//! segments, ±20 % tolerance for warm/cold cache); this
//! integration test is the smoke-test sentinel that the
//! recovery loop emits no per-record fsync and no per-record
//! audit event.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.3)"]
#[test]
fn rfc0008_3_recovery_emits_no_per_record_fsync() {
    unimplemented!(
        "RFC0008.3 — replay over a multi-segment fixture, assert wal_syncs_total \
         does not advance during replay (the §6.6 step 4 heal fsync is the only \
         exception, and only on a torn-tail newest segment this fixture omits)"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.3)"]
#[test]
fn rfc0008_3_recovery_emits_no_per_record_audit_event() {
    unimplemented!(
        "RFC0008.3 — the recovery driver emits an audit event only on the \
         RFC0008.5 corruption path; happy-path replay emits zero"
    );
}
