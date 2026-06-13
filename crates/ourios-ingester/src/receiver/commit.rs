//! The §3.4 group-commit coordinator — windowed batched fsync.
//!
//! `CLAUDE.md` §3.4 makes batching the *caller's* job: the WAL exposes
//! `append` and `sync` separately and stays single-writer + synchronous
//! (RFC 0008 §3.1 / §6.3). This layer turns N concurrent per-request
//! appends into **one** fsync per window — "batch up to
//! `wal_batch_window_ms` *or* until the segment fills, whichever first"
//! (§3.4) — without weakening the invariant: a [`commit`](CommitCoordinator::commit)
//! returns `Ok` only after a `sync` that covered its frame returned `Ok`.
//!
//! ## How a commit is gated
//!
//! Each [`commit`](CommitCoordinator::commit):
//! 1. appends its frame under the journal mutex (the single-writer
//!    serialization point), taking a monotonic **sequence number** `seq`;
//! 2. arms one windowed flush (the first arriver after a flush schedules
//!    it; concurrent arrivals within the window ride the same flush) — or
//!    triggers an immediate flush when the WAL's unflushed bytes have
//!    reached the segment-fill threshold;
//! 3. awaits the broadcast flush *outcome* and returns `Ok(offset)` only
//!    once a flush whose covered-seq ≥ its `seq` reported success, or the
//!    matching `Err` if that flush's `sync` failed (so a frame in a
//!    failed sync is never acked).
//!
//! `seq` — not the byte offset — is the disambiguator: a `sync` makes
//! durable everything appended before it, so the flush records the
//! highest `seq` it attempted, and a waiter is *covered* exactly when its
//! `seq ≤ covered_seq`. Offsets would also order (they're `Ord`), but
//! `seq` is unambiguous across the append/flush race and segment
//! rotation.
//!
//! The journal mutex is **never held across an `.await`**: it is taken
//! for the append, released, and re-taken inside `flush` for the `sync` —
//! so the coordinator cannot deadlock the runtime, and `std::sync::Mutex`
//! is the right primitive (RFC 0008 §3.1's single writer).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ourios_wal::WalOffset;
use tokio::sync::watch;

use crate::receiver::pipeline::{Journal, ReceiveError};

/// The result of a [`CommitCoordinator::commit`]: the durability outcome
/// plus the append **sequence number** the frame consumed, if any.
///
/// `seq` is `Some` once the frame was appended (whether its covering
/// `sync` then succeeded or failed) and `None` only when the `append`
/// itself failed (no sequence consumed). The caller drives the
/// **in-order miner hand-off** with it: every `seq`-bearing outcome —
/// success *or* sync failure — must pass [`CommitCoordinator::await_ingest_turn`]
/// then [`CommitCoordinator::complete_ingest`] in `seq` order, so the
/// miner sees records in exact WAL-append order (template-id stability /
/// snapshot-restore §3.5.3) even though the fsyncs batched concurrently.
pub struct CommitOutcome {
    pub seq: Option<u64>,
    pub result: Result<WalOffset, ReceiveError>,
}

/// Outcome of one flush, broadcast to every waiting `commit`.
///
/// `covered_seq` is the highest append sequence the flush's `sync`
/// attempted to make durable; a waiter with `seq ≤ covered_seq` is
/// covered by this outcome. On `Ok` the durable high-water `offset` is
/// what the waiter returns; on `Err` the waiter returns
/// [`ReceiveError::WalSync`] and does **not** ack (§3.4).
#[derive(Clone)]
struct FlushOutcome {
    /// Monotonic flush generation — lets a waiter detect a *new* outcome
    /// versus the initial `watch` value (lost-wakeup safety).
    generation: u64,
    /// Highest append `seq` this flush's `sync` covered.
    covered_seq: u64,
    /// `Ok(durable high-water)` or the sync error to propagate.
    result: Result<WalOffset, SyncFailure>,
}

/// A `sync` failure, cloneable so it can ride the `watch` to every
/// covered waiter (the source `SyncError` is not `Clone`). Carries the
/// rendered detail; each covered waiter reconstructs a
/// [`ReceiveError::WalSync`]-shaped error from it.
#[derive(Clone)]
struct SyncFailure {
    detail: String,
}

/// Coordinator state behind one std mutex: whether a windowed flush is
/// already armed (so concurrent appends share one), the next sequence to
/// hand out, and the highest sequence appended-so-far (what the next
/// flush will cover).
struct FlushState {
    flush_pending: bool,
    next_seq: u64,
    appended_seq: u64,
}

/// The §3.4 windowed group-commit coordinator. Shared via `Arc`; one per
/// ingester process (it owns the single-writer WAL).
pub struct CommitCoordinator {
    /// The durability sink, serialized: append + sync both run under this
    /// lock, never held across an `.await`.
    journal: Mutex<Box<dyn Journal>>,
    /// `wal_batch_window_ms` (§3.4 / §6.9): the max time from first
    /// `append` to its `sync`.
    window: Duration,
    /// `wal_segment_size_bytes`: the early-cut threshold. When the WAL's
    /// unflushed bytes reach this, flush now rather than wait out the
    /// window ("until the segment fills", §3.4) — a frame can't outrun a
    /// segment, so this bounds `wal_unflushed_bytes` (RFC0008.9).
    segment_size_bytes: u64,
    /// Arm flag + sequence counters.
    flush_state: Mutex<FlushState>,
    /// Broadcasts each flush outcome to waiters.
    outcome_tx: watch::Sender<FlushOutcome>,
    /// Cuts a pending windowed flush short when the segment-fill
    /// threshold is reached. A monotonically-bumped counter rather than a
    /// `Notify`: the armed flush task subscribes at spawn and `select!`s
    /// on its window timer vs. a change here, so a fill cut wakes *that*
    /// task — and, unlike `Notify`'s stored permits, a fill bump from an
    /// earlier cycle can't linger and spuriously cut a later window short.
    fill: watch::Sender<u64>,
    /// The in-order miner hand-off gate: the next append `seq` cleared to
    /// run its post-durability miner step. A `commit`'s caller awaits its
    /// turn on this and advances it, so the miner ingests in exact
    /// WAL-append order despite concurrent, batched commits.
    ingest_gate: watch::Sender<u64>,
}

impl CommitCoordinator {
    /// Build a coordinator over `journal`, the batch window, and the
    /// segment-fill early-cut threshold.
    #[must_use]
    pub fn new(journal: Box<dyn Journal>, window: Duration, segment_size_bytes: u64) -> Arc<Self> {
        // The initial outcome is generation 0 covering nothing; no waiter
        // ever matches it (a real append takes seq ≥ 1 and waits for a
        // generation ≥ 1), so it is purely the `watch`'s required seed.
        let (outcome_tx, _outcome_rx) = watch::channel(FlushOutcome {
            generation: 0,
            covered_seq: 0,
            result: Ok(WalOffset {
                segment: uuid::Uuid::nil(),
                byte: 0,
            }),
        });
        // The ingest gate starts at 1 — the first `seq` handed out — so
        // the first commit runs its miner step immediately.
        let (ingest_gate, _ingest_rx) = watch::channel(1u64);
        let (fill, _fill_rx) = watch::channel(0u64);
        Arc::new(Self {
            journal: Mutex::new(journal),
            window,
            segment_size_bytes,
            flush_state: Mutex::new(FlushState {
                flush_pending: false,
                next_seq: 1,
                appended_seq: 0,
            }),
            outcome_tx,
            fill,
            ingest_gate,
        })
    }

    /// Append `payload` and return once its durability is decided (§3.4).
    ///
    /// Concurrent calls batch: each appends under the journal lock, then
    /// one windowed `sync` covers them all. The returned [`CommitOutcome`]
    /// carries the append `seq` (so the caller can drive the in-order
    /// miner hand-off — [`Self::await_ingest_turn`] / [`Self::complete_ingest`])
    /// and the durable high-water [`WalOffset`] once the covering `sync`
    /// succeeds.
    ///
    /// The `result` is:
    /// - [`ReceiveError::WalAppend`] (with `seq: None`) if the append
    ///   itself fails — no frame, no sequence, not acked.
    /// - [`ReceiveError::WalSync`] (with `seq: Some`) if the covering
    ///   `sync` fails — the frame consumed a sequence but is not durable;
    ///   the caller still drives the gate (in order) so it doesn't stall
    ///   later sequences, and does not ack.
    pub async fn commit(self: &Arc<Self>, payload: &[u8]) -> CommitOutcome {
        // Subscribe *before* appending so no flush outcome can slip
        // between the append and the first wait (lost-wakeup safety: the
        // receiver exists and holds the value the wait loop re-checks).
        let mut rx = self.outcome_tx.subscribe();

        // Append under the lock, take this frame's seq, and read the
        // WAL's unflushed bytes to decide on an early cut — all while the
        // lock is held so the seq ↔ append ordering is atomic.
        let (seq, unflushed) = {
            let mut journal = self.lock_journal();
            if let Err(e) = journal.append_batch(payload) {
                // No sequence consumed: append never happened.
                return CommitOutcome {
                    seq: None,
                    result: Err(e),
                };
            }
            let mut state = self.lock_flush_state();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.appended_seq = seq;
            (seq, journal.unflushed_bytes())
        };

        // Arm (or piggy-back on) the windowed flush. An unflushed volume
        // at the segment-fill threshold cuts the window short.
        let fill_cut = unflushed >= self.segment_size_bytes;
        self.arm_flush(fill_cut);

        // Wait until a flush outcome covers this seq. Check the *current*
        // value before the first `.changed().await` — a flush may already
        // have completed between the append and here, so awaiting first
        // would be a lost wakeup.
        let result = loop {
            if let Some(result) = covered_outcome(&rx.borrow_and_update(), seq) {
                break result;
            }
            // Only `None` once the sender is dropped, which the
            // coordinator (held by the pipeline) outlives for any live
            // commit; treat it as a sync failure rather than panic.
            if rx.changed().await.is_err() {
                break Err(sync_failure_error("commit coordinator stopped"));
            }
        };
        CommitOutcome {
            seq: Some(seq),
            result,
        }
    }

    /// Await this `seq`'s turn in the in-order miner hand-off: returns
    /// once every lower sequence has completed its post-durability step.
    /// The miner thus sees records in exact WAL-append order even though
    /// the fsyncs batched concurrently (template-id stability /
    /// snapshot-restore §3.5.3). Must be paired with
    /// [`Self::complete_ingest`] (called for **every** `seq`-bearing
    /// outcome, success or sync failure, so the gate never stalls).
    pub async fn await_ingest_turn(&self, seq: u64) {
        let mut rx = self.ingest_gate.subscribe();
        // The gate is monotonic; `>= seq` means it's reached this seq
        // (each seq is unique, so it's exactly this caller's turn).
        // Check the current value before awaiting (lost-wakeup safety).
        while *rx.borrow_and_update() < seq {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Release the in-order hand-off to the next sequence. Called after a
    /// `seq`'s post-durability miner step (or immediately, with no miner
    /// step, for a sync-failed `seq`) so `seq + 1` may proceed.
    pub fn complete_ingest(&self, seq: u64) {
        self.ingest_gate.send_modify(|next| *next = seq + 1);
    }

    /// Arm a windowed flush if one is not already pending. The first
    /// arriver after a flush sets the flag and spawns **one** flush task
    /// that waits out the window *or* a fill cut, whichever comes first;
    /// concurrent arrivals ride it. `immediate` (the segment-fill early
    /// cut) wakes the pending flush via [`Self::fill`] instead of
    /// spawning another — so a burst of fill-crossing appends produces at
    /// most one extra flush task, not one per append.
    fn arm_flush(self: &Arc<Self>, immediate: bool) {
        let spawn = {
            let mut state = self.lock_flush_state();
            if state.flush_pending {
                false
            } else {
                state.flush_pending = true;
                true
            }
        };
        // Subscribe the about-to-spawn task's fill receiver *before* the
        // bump below, so an immediate cut at arm time is observed by it
        // (a later `.changed()` only fires for bumps after the subscribe).
        let fill_rx = spawn.then(|| self.fill.subscribe());
        // Cut a pending windowed flush short (the already-running task's
        // receiver, subscribed at its own spawn, sees this bump). A bump
        // with no flush armed is harmless: the next task subscribes after
        // it, so it doesn't carry over.
        if immediate {
            self.fill.send_modify(|n| *n += 1);
        }
        if let Some(mut fill_rx) = fill_rx {
            let coordinator = Arc::clone(self);
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(coordinator.window) => {}
                    _ = fill_rx.changed() => {}
                }
                coordinator.flush().await;
            });
        }
    }

    /// Fsync the WAL and broadcast the outcome to every covered waiter.
    ///
    /// Ordering: the arm flag is reset (and `covered_seq` captured)
    /// *before* the `sync`. An append that lands during the `sync` is
    /// already covered by that `sync`'s EOF-based durable offset, so the
    /// broadcast outcome satisfies it; but its `seq > covered_seq`, so it
    /// does **not** treat *this* outcome as covering it — it re-arms a
    /// fresh flush and waits for that one (a redundant-but-harmless extra
    /// fsync, never a missed ack). Resetting before the sync is what lets
    /// that just-in-time append arm the next window at all.
    ///
    /// The `sync` (a blocking fsync) is offloaded to `spawn_blocking` so
    /// it never stalls a runtime worker. The journal mutex is acquired
    /// *inside* the blocking closure and released there — never held
    /// across the `.await`.
    async fn flush(self: &Arc<Self>) {
        let covered_seq = {
            let mut state = self.lock_flush_state();
            state.flush_pending = false;
            state.appended_seq
        };

        let coordinator = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || coordinator.lock_journal().sync())
            .await
            .unwrap_or_else(|join| Err(sync_failure_error(&format!("flush task failed: {join}"))));

        let new = FlushOutcome {
            generation: self.next_generation(),
            covered_seq,
            result: match result {
                Ok(offset) => Ok(offset),
                Err(e) => Err(SyncFailure {
                    detail: e.to_string(),
                }),
            },
        };
        // Publish **monotonically in `covered_seq`**: two flushes can be
        // in flight (a segment-fill cut spawned while a windowed flush is
        // pending), and each broadcasts only after releasing the journal
        // lock, so a later-starting flush with a higher `covered_seq` can
        // reach this `send` *before* an earlier one. Overwriting a higher
        // `covered_seq` with a stale lower one would strand a waiter the
        // higher outcome already covered (it would re-await with no
        // further flush armed under idle traffic). Dropping the stale
        // lower outcome is also §3.4-safe: a higher *failed* flush masking
        // a lower success only yields a spurious error → client retry
        // (at-least-once replay absorbs the duplicate), never an ack of a
        // non-durable frame.
        self.outcome_tx.send_if_modified(|current| {
            if new.covered_seq >= current.covered_seq {
                *current = new;
                true
            } else {
                false
            }
        });
    }

    /// The next flush generation — only used to distinguish a real
    /// outcome from the generation-0 `watch` seed; coverage itself is by
    /// `covered_seq`, so concurrent flushes computing an equal generation
    /// is harmless.
    fn next_generation(&self) -> u64 {
        self.outcome_tx.borrow().generation + 1
    }

    fn lock_journal(&self) -> std::sync::MutexGuard<'_, Box<dyn Journal>> {
        self.journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_flush_state(&self) -> std::sync::MutexGuard<'_, FlushState> {
        self.flush_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Resolve a flush `outcome` against a waiter's `seq`: `Some(result)` once
/// a flush of generation ≥ 1 covered this `seq` (the durable offset on
/// success, the propagated sync error on failure); `None` if this outcome
/// does not yet cover the waiter (an older/uncovering flush, or the
/// generation-0 seed).
fn covered_outcome(outcome: &FlushOutcome, seq: u64) -> Option<Result<WalOffset, ReceiveError>> {
    if outcome.generation == 0 || outcome.covered_seq < seq {
        return None;
    }
    Some(match &outcome.result {
        Ok(offset) => Ok(*offset),
        Err(failure) => Err(sync_failure_error(&failure.detail)),
    })
}

/// A [`ReceiveError::WalSync`] reconstructed from a broadcast failure
/// detail. The `watch` can't carry the non-`Clone` source `SyncError`, so
/// the rendered detail is wrapped in a transport-equivalent error: the
/// HTTP/gRPC layers map any `WalSync` to 500 / `INTERNAL` and surface its
/// `Display`, which this preserves.
fn sync_failure_error(detail: &str) -> ReceiveError {
    ReceiveError::WalSync(ourios_wal::SyncError::Io {
        op: "group-commit sync",
        source: std::io::Error::other(detail.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A spy `Journal` over an in-memory byte count: `append` bumps
    /// unflushed bytes + an append counter, `sync` advances a synthetic
    /// offset and records a sync, and can be made to fail every sync.
    struct SpyJournal {
        appends: Arc<AtomicU64>,
        syncs: Arc<AtomicU64>,
        unflushed: u64,
        byte: u64,
        fail_sync: bool,
    }

    impl Journal for SpyJournal {
        fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
            self.appends.fetch_add(1, Ordering::SeqCst);
            self.unflushed += payload.len() as u64;
            Ok(())
        }

        fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if self.fail_sync {
                return Err(sync_failure_error("spy sync failure"));
            }
            self.byte += self.unflushed;
            self.unflushed = 0;
            Ok(WalOffset {
                segment: uuid::Uuid::from_u128(1),
                byte: self.byte,
            })
        }

        fn unflushed_bytes(&self) -> u64 {
            self.unflushed
        }
    }

    fn spy(fail_sync: bool) -> (Arc<CommitCoordinator>, Arc<AtomicU64>, Arc<AtomicU64>) {
        let appends = Arc::new(AtomicU64::new(0));
        let syncs = Arc::new(AtomicU64::new(0));
        let journal = Box::new(SpyJournal {
            appends: appends.clone(),
            syncs: syncs.clone(),
            unflushed: 0,
            byte: 0,
            fail_sync,
        });
        // A large segment size so the early-cut never fires in these
        // window-behaviour tests.
        let coordinator = CommitCoordinator::new(journal, Duration::from_millis(20), u64::MAX);
        (coordinator, appends, syncs)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_commits_share_one_sync() {
        let (coordinator, appends, syncs) = spy(false);
        let mut handles = Vec::new();
        for _ in 0..16u32 {
            let c = Arc::clone(&coordinator);
            handles.push(tokio::spawn(async move { c.commit(b"frame").await }));
        }
        for h in handles {
            h.await.expect("join").result.expect("commit Ok");
        }
        assert_eq!(appends.load(Ordering::SeqCst), 16, "every frame appended");
        // 16 commits in one ~20 ms window fsync far fewer than 16 times —
        // the whole point of batching (appends_per_sync >> 1).
        let syncs = syncs.load(Ordering::SeqCst);
        assert!(syncs < 16, "batched: {syncs} syncs for 16 commits");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_sync_fails_every_covered_waiter() {
        let (coordinator, _appends, _syncs) = spy(true);
        let mut handles = Vec::new();
        for _ in 0..8u32 {
            let c = Arc::clone(&coordinator);
            handles.push(tokio::spawn(async move { c.commit(b"frame").await }));
        }
        for h in handles {
            let outcome = h.await.expect("join");
            assert!(
                outcome.seq.is_some(),
                "the frame appended (consumed a seq) before its sync failed",
            );
            assert!(
                matches!(outcome.result, Err(ReceiveError::WalSync(_))),
                "a frame in a failed sync is not acked",
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_commit_is_durable_before_returning() {
        let (coordinator, appends, syncs) = spy(false);
        let offset = coordinator
            .commit(b"frame")
            .await
            .result
            .expect("commit Ok");
        assert_eq!(appends.load(Ordering::SeqCst), 1);
        assert_eq!(syncs.load(Ordering::SeqCst), 1, "one append, one sync");
        assert_eq!(offset.byte, b"frame".len() as u64);
    }

    /// Concurrent fill-cut and window flushes interleave (a small
    /// segment fires the early cut while a windowed flush is pending),
    /// so two flushes can be in flight and broadcast out of order. The
    /// monotonic `send_if_modified` must keep every waiter from being
    /// stranded by a stale lower-`covered_seq` outcome — all commits
    /// resolve well within the timeout. (Pre-fix, a stranded waiter
    /// would hang until the next append; here the burst then goes idle,
    /// so a strand would time out.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn interleaved_flushes_never_strand_a_waiter() {
        let appends = Arc::new(AtomicU64::new(0));
        let syncs = Arc::new(AtomicU64::new(0));
        let journal = Box::new(SpyJournal {
            appends: appends.clone(),
            syncs: syncs.clone(),
            unflushed: 0,
            byte: 0,
            fail_sync: false,
        });
        // A small segment (frequent fill-cuts) + a non-trivial window:
        // both flush paths fire, overlapping.
        let coordinator = CommitCoordinator::new(journal, Duration::from_millis(10), 8);
        let mut handles = Vec::new();
        for _ in 0..64u32 {
            let c = Arc::clone(&coordinator);
            handles.push(tokio::spawn(async move { c.commit(b"frame").await }));
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            for h in handles {
                h.await.expect("join").result.expect("commit Ok");
            }
        })
        .await
        .expect("no waiter stranded — all commits resolve within the timeout");
        assert_eq!(appends.load(Ordering::SeqCst), 64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn segment_fill_cuts_the_window_short() {
        // A tiny segment threshold + a long window: the early-cut must
        // fire on fill, not wait out the (here, very long) window.
        let appends = Arc::new(AtomicU64::new(0));
        let syncs = Arc::new(AtomicU64::new(0));
        let journal = Box::new(SpyJournal {
            appends: appends.clone(),
            syncs: syncs.clone(),
            unflushed: 0,
            byte: 0,
            fail_sync: false,
        });
        let coordinator = CommitCoordinator::new(
            journal,
            Duration::from_secs(3_600),
            4, // one 4-byte frame fills the segment
        );
        // Resolves via the fill cut despite the hour-long window.
        let offset = tokio::time::timeout(Duration::from_secs(5), coordinator.commit(b"abcd"))
            .await
            .expect("fill cut resolves well within the window")
            .result
            .expect("commit Ok");
        assert_eq!(offset.byte, 4);
        assert_eq!(syncs.load(Ordering::SeqCst), 1);
    }
}
