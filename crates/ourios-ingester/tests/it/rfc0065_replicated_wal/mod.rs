//! RFC 0065 — replicated WAL and a queryable tail, the in-process cluster
//! arms. See `docs/rfcs/0065-replicated-wal.md` §5 and §8: three receivers
//! and a querier on the in-memory store, a simulated network, a
//! controllable clock, and crash loss that cuts each segment back to the
//! last `Wal::sync`. The served-binary, config and chart arms are in
//! `ourios-server`'s `tests/it/rfc0065_replicated_wal.rs`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC is
//! red; each names the slice that discharges it.
//!
//! RFC0065.2, RFC0065.5, RFC0065.6, RFC0065.7, RFC0065.3's takeover arm,
//! RFC0065.4's paused-writer arm, RFC0065.15's write arm, RFC0065.24's
//! formation and unreadable-record arms, RFC0065.29's kind-gate arm and
//! RFC0065.30 run on the crate-internal cluster harness, so they are unit
//! tests: `replicated::write::tests`, `replicated::raft::cluster::tests`,
//! `replicated::raft::formation::tests` and `replicated::raft::flusher::tests`.

mod checkpoints;
mod formation;
mod local_backend;
mod log_entries;
mod mining;
mod replication;
mod tail;
