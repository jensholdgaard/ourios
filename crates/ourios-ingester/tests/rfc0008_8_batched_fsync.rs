//! RFC0008.8 — the batched-fsync (group-commit) knob `[§3.4]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Relocated from `crates/ourios-wal/tests/rfc0008_8_batched_fsync.rs`:
//! ack/receiver latency is an *ingester* concept (the WAL stays a
//! synchronous `append`/`sync` per RFC 0008 §6.3; the windowing is the
//! caller's job — `CommitCoordinator`, RFC0008.8). Same relocation
//! precedent as the RFC0001.5/.6 miner→querier move.
//!
//! Three arms over the live [`CommitCoordinator`] backed by a real `Wal`
//! in a tempdir:
//! - **P99 ack latency tracks the batch window** across three settings,
//!   and clearly *scales* with the window (the robust ordering check);
//! - **syncs advance per-batch, not per-record** (`appends_per_sync ≫ 1`);
//! - the **§3.4 gate**: a `commit` returns `Ok` only after a `sync` that
//!   covered its frame returned `Ok` (proven via a spy sink that records
//!   each sync's covered byte range + result before the commit resolves).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ourios_ingester::receiver::commit::CommitCoordinator;
use ourios_ingester::receiver::{Journal, ReceiveError};
use ourios_wal::{Wal, WalConfig, WalOffset};

fn wal_config(root: &Path, batch_window_ms: u64) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// A coordinator over a real `Wal` at `root` with the given window. A
/// huge segment size keeps the early-cut from firing — this exercises
/// the time-window path, not the segment-fill path.
fn real_coordinator(root: &Path, window_ms: u64) -> Arc<CommitCoordinator> {
    let config = wal_config(root, window_ms);
    let wal = Wal::open(config).expect("open WAL");
    CommitCoordinator::new(Box::new(wal), Duration::from_millis(window_ms), u64::MAX)
}

/// The p99 of `samples` (nearest-rank), in milliseconds.
fn p99_ms(mut samples: Vec<Duration>) -> f64 {
    assert!(!samples.is_empty());
    samples.sort_unstable();
    // Nearest-rank: ⌈0.99·n⌉, integer-only so there's no float-cast
    // truncation. (n·99 fits a usize for any realistic sample count.)
    let rank = samples.len().saturating_mul(99).div_ceil(100);
    let idx = rank.saturating_sub(1).min(samples.len() - 1);
    samples[idx].as_secs_f64() * 1_000.0
}

/// Drive `coordinator` with steadily-arriving concurrent commits for
/// `rounds` rounds of `concurrency` commits each, an `inter_arrival`
/// pause between rounds, and return each commit's individual ack latency.
async fn measure_latencies(
    coordinator: &Arc<CommitCoordinator>,
    rounds: u32,
    concurrency: u32,
    inter_arrival: Duration,
) -> Vec<Duration> {
    let mut handles = Vec::new();
    for _ in 0..rounds {
        for _ in 0..concurrency {
            let c = Arc::clone(coordinator);
            handles.push(tokio::spawn(async move {
                let start = Instant::now();
                c.commit(b"a steady-state OTLP batch frame")
                    .await
                    .expect("commit Ok");
                start.elapsed()
            }));
        }
        tokio::time::sleep(inter_arrival).await;
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("commit task joins"));
    }
    out
}

/// Scenario RFC0008.8 — P99 ack latency tracks the configured batch
/// window. See `docs/rfcs/0008-wal.md` §5.
///
/// Spec values are `{10, 100, 1000}` ms. To keep CI under a few seconds
/// we exercise `{10, 50, 150}` ms (documented deviation): the contract
/// the test enforces is that **P99 scales with the window** — each P99
/// lands within tolerance of its window, and the orderings
/// `p99(10) < p99(50) < p99(150)` hold (robust to CI scheduler noise).
/// The ±30 % of the spec is widened to a band of `[0.4·w, 1.8·w + FIXED]`
/// for loaded-CI jitter, where `FIXED` (one timer tick + the fsync that
/// doesn't scale with the window) absorbs the per-flush overhead that
/// dominates the smallest window — the same fixed cost the spec's larger
/// `100`/`1000` ms settings make negligible. The scaling ordering is the
/// load-bearing assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0008_8_p99_latency_tracks_batch_window() {
    // Per-flush fixed overhead (timer granularity + one fsync + task
    // scheduling) that does not scale with the window.
    const FIXED_MS: f64 = 15.0;

    let mut p99s = Vec::new();
    for window_ms in [10u64, 50, 150] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let coordinator = real_coordinator(tmp.path(), window_ms);
        // Arrivals every ~window/4 so several commits ride each window and
        // each waits a roughly-uniform fraction of the window → P99 near a
        // full window. Enough rounds for a stable tail.
        let inter_arrival = Duration::from_millis((window_ms / 4).max(1));
        let samples = measure_latencies(&coordinator, 40, 8, inter_arrival).await;
        let p99 = p99_ms(samples);
        // The window settings are small (≤ 150), so a u32→f64 widening is
        // exact — no precision loss.
        let window = f64::from(u32::try_from(window_ms).expect("small window"));
        let lower = window * 0.4;
        let upper = window * 1.8 + FIXED_MS;
        assert!(
            p99 >= lower && p99 <= upper,
            "window {window_ms}ms: P99 {p99:.1}ms outside [{lower:.1}, {upper:.1}]ms — \
             ack latency must be dominated by the window, not per-record fsync",
        );
        p99s.push(p99);
    }

    // The load-bearing, noise-robust assertion: P99 scales with the
    // window. A larger window must yield a larger P99.
    assert!(
        p99s[0] < p99s[1] && p99s[1] < p99s[2],
        "P99 must scale with the batch window, got {p99s:?} for [10, 50, 150] ms",
    );
}

/// Scenario RFC0008.8 — `wal_syncs_total` advances per-batch, not
/// per-record. See `docs/rfcs/0008-wal.md` §5.
///
/// Fire K concurrent commits inside one window and assert the WAL
/// fsync'd far fewer than K times (`appends_per_sync ≫ 1`) — the whole
/// point of group commit. Reads the real WAL's own `metrics()` counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0008_8_syncs_advance_per_batch_not_per_record() {
    const K: u32 = 64;

    let tmp = tempfile::TempDir::new().expect("temp");
    // A generous window so all K commits land inside it. We read the
    // counters through a `MetricsJournal` that wraps the real WAL.
    let metrics = Arc::new(SyncCounter::default());
    let wal = Wal::open(wal_config(tmp.path(), 200)).expect("open WAL");
    let journal = Box::new(MetricsJournal {
        inner: wal,
        counter: Arc::clone(&metrics),
    });
    let coordinator = CommitCoordinator::new(journal, Duration::from_millis(200), u64::MAX);

    let mut handles = Vec::with_capacity(K as usize);
    for _ in 0..K {
        let c = Arc::clone(&coordinator);
        handles.push(tokio::spawn(async move {
            c.commit(b"frame").await.expect("commit Ok")
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    let appends = metrics.appends.load(Ordering::SeqCst);
    let syncs = metrics.syncs.load(Ordering::SeqCst);
    assert_eq!(appends, u64::from(K), "every frame appended");
    assert!(syncs >= 1, "at least one sync covered the batch");
    // Group commit: K appends folded into a handful of syncs. Far fewer
    // than K — the batching contract. (Window jitter may split into a
    // few syncs, never one-per-record.)
    assert!(
        syncs * 4 < u64::from(K),
        "appends_per_sync must be ≫ 1: {appends} appends over {syncs} syncs",
    );
}

/// Scenario RFC0008.8 / §3.4 — a `commit` returns `Ok` only after a
/// `sync` that covered its frame returned `Ok`.
///
/// A spy sink records, for every `sync`, the byte range it made durable
/// and its result, in call order. After a commit resolves `Ok(offset)`,
/// some recorded `sync` must have returned `Ok` with a durable high-water
/// at or beyond that commit's `offset` — i.e. the ack was gated on a
/// covering, successful fsync (no ack-before-durable).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0008_8_commit_ack_is_gated_on_a_covering_successful_sync() {
    let syncs: Arc<Mutex<Vec<WalOffset>>> = Arc::new(Mutex::new(Vec::new()));
    let journal = Box::new(RecordingSyncJournal {
        byte: 0,
        syncs: Arc::clone(&syncs),
    });
    let coordinator = CommitCoordinator::new(journal, Duration::from_millis(20), u64::MAX);

    // Several concurrent commits; each must be covered by a successful
    // sync recorded before it resolved.
    let mut handles = Vec::new();
    for _ in 0..12u32 {
        let c = Arc::clone(&coordinator);
        handles.push(tokio::spawn(async move {
            c.commit(b"frame").await.expect("commit Ok")
        }));
    }
    for h in handles {
        let offset = h.await.expect("join");
        let recorded = syncs.lock().expect("syncs");
        assert!(
            recorded.iter().any(|durable| *durable >= offset),
            "a commit acked at {offset:?} with no covering successful sync among {recorded:?} \
             — that would be an ack before durability (§3.4)",
        );
    }
}

// ----- spy journals -----

/// Counts append + sync calls; delegates persistence to a real `Wal`.
#[derive(Default)]
struct SyncCounter {
    appends: AtomicU64,
    syncs: AtomicU64,
}

struct MetricsJournal {
    inner: Wal,
    counter: Arc<SyncCounter>,
}

impl Journal for MetricsJournal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
        self.counter.appends.fetch_add(1, Ordering::SeqCst);
        Journal::append_batch(&mut self.inner, payload)
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        self.counter.syncs.fetch_add(1, Ordering::SeqCst);
        Journal::sync(&mut self.inner)
    }

    fn unflushed_bytes(&self) -> u64 {
        Journal::unflushed_bytes(&self.inner)
    }
}

/// Records the durable high-water of every successful `sync`, in order;
/// persists nothing (a synthetic monotone offset).
struct RecordingSyncJournal {
    byte: u64,
    syncs: Arc<Mutex<Vec<WalOffset>>>,
}

impl Journal for RecordingSyncJournal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
        self.byte += payload.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        let durable = WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: self.byte,
        };
        self.syncs.lock().expect("syncs").push(durable);
        Ok(durable)
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }
}
