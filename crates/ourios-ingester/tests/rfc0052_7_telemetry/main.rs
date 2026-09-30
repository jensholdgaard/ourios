//! RFC0052.7 — The WAL's state is exported.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Its own test binary, like `perf_metrics.rs` (RFC0028.2 exempt list
//! in `README.md`): it installs the **global** in-memory meter provider
//! and a global `tracing` subscriber bridged onto an in-memory log
//! exporter, and two global-installing tests in one binary would race.
//! The three legs here share both installs, so they run one at a time.
//!
//! One binary, three legs, one module each: `instruments` (the exported
//! stream), `transitions` (each edge's event) and `live_check` (every
//! event against the registry); `harness`, `metric_read` and `drivers`
//! are what they share. `snapshot_discard` rides the same event capture
//! for startup recovery's discard event (#884), so CI's weaver rerun of
//! this binary live-checks it too.

#[path = "../it/ingest_support/mod.rs"]
mod ingest_support;
#[path = "../it/rfc0052_barrier_support.rs"]
mod rfc0052_barrier_support;

mod drivers;
mod harness;
mod instruments;
mod live_check;
mod metric_read;
mod snapshot_discard;
mod transitions;
