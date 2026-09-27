//! RFC0052.3 — Sustained ingest does not grow the WAL without bound.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! The one criterion a unit test cannot express (RFC 0052 §6): the
//! defect is the *absence* of a periodic call, and only elapsed
//! cadence reveals it. The `ourios-bench` soak harness (`src/soak.rs`)
//! cannot do this today — its sampler only flushes the sink and runs
//! compaction, and its synthetic clock advances record timestamps
//! rather than driving a timer — so the harness extension is part of
//! this criterion's cost: the soak loop gains the §3.2 sequence on the
//! synthetic clock. Its own binary, like the other bench tests under
//! `tests/`.

/// Scenario RFC0052.3 — bytes and segment count stay bounded under a complete floor.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.3 stub — implemented in the crash-and-soak green slice F (soak harness extended with the §3.2 cadence on the synthetic clock)"]
fn rfc0052_3_wal_bytes_and_segments_stay_bounded_over_a_long_run() {
    todo!(
        "RFC0052.3 — a capacity-balanced soak with a healthy store and \
         every tenant with WAL data holding a valid and advancing \
         snapshot, run long enough to roll many segments, once the \
         reclamation cadence has had time to act: the WAL's on-disk \
         byte total and segment count are bounded rather than \
         monotonically increasing — the #793 signature (1,113 segments, \
         nothing ever unlinked) cannot reproduce; while the floor is \
         Pinned the pinning tenants' frames are retained by design and \
         the state is visible; a rate above capacity grows the WAL by \
         construction and is not claimed here"
    );
}

/// Scenario RFC0052.3 — the append-independent path reclaims after the last append.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.3 stub — implemented in the crash-and-soak green slice F (idle barrier tick + owed rotation on the synthetic clock)"]
fn rfc0052_3_idle_node_reclaims_every_eligible_segment_without_further_traffic() {
    todo!(
        "RFC0052.3 — after the last append with no further traffic, one \
         barrier_secs plus one housekeeping_secs later the checkpoint \
         has advanced and every eligible closed segment is reclaimed; \
         within segment_age_secs + barrier_secs + housekeeping_secs the \
         last segment has rotated on a barrier tick and been reclaimed \
         by the following pass"
    );
}
