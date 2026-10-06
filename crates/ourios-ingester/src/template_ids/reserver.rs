//! Handing reserved blocks to the miner without store I/O under its lock
//! (RFC 0059 §3.3).
//!
//! The miner holds its current block; this side holds up to
//! [`READY_BLOCKS`] more, reserved by a background refiller. Taking a
//! block is an in-memory pop. When none is ready the miner's fresh
//! allocations fail at once and the refiller keeps retrying.
//!
//! Startup replay is the exception (RFC 0059 §3.4): the recovery driver
//! owns the miner before any listener opens, so no ingest waits on it,
//! and a replay that drains the ready blocks reserves the next one
//! synchronously rather than failing a template a healthy store could
//! have given an id. The refiller starts only once replay ends, so the
//! two never reserve at once: on the local backend's unconditional
//! overwrite, two concurrent reservations could hand out the same block.
//!
//! Once the root's seated marker is written, every block's end is
//! recorded in it before the block can be taken, so the marker's
//! `max_reserved_seen` covers every id this root can issue (RFC 0059
//! §3.1's rollback check).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ourios_miner::cluster::{IdBlock, IdReservationError, IdReserver, MinerCluster};
use ourios_parquet::Store;

use super::marker::{Marker, read_marker, write_marker};
use super::{BootstrapPolicy, Seated, SnapshotTrust, TemplateIdsError, names, reserve, seat};

/// Blocks kept ready beside the one the miner allocates from.
const READY_BLOCKS: usize = 2;
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long [`TemplateIds::shutdown`] waits for an in-flight reservation on
/// a store whose writes are compare-and-swaps.
const STOP_WITHIN: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Ready {
    blocks: VecDeque<IdBlock>,
    /// The highest block end reserved so far: the floor of the next one.
    highest: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The seated marker this root keeps current.
struct Ledger {
    root: PathBuf,
    marker: Marker,
}

/// What the startup seat, the refiller and the miner's reserver share.
struct Shared {
    store: Store,
    ready: Mutex<Ready>,
    /// `None` until the root's marker is written; held across recording a
    /// block and making it ready, so no block becomes takeable unrecorded.
    ledger: Mutex<Option<Ledger>>,
    /// A reservation replay could not make, for recovery to fail with.
    replay_failure: Mutex<Option<TemplateIdsError>>,
    /// Set once, under the ledger lock, by [`TemplateIds::shutdown`]: no
    /// reservation starts and no block is recorded or made ready after.
    stopped: AtomicBool,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// The running refiller: its thread, and a channel that disconnects when
/// the thread exits, so a wait for it can be bounded.
struct Refiller {
    thread: JoinHandle<()>,
    exited: Receiver<()>,
}

/// [`TemplateIds::shutdown`] gave up waiting for a reservation in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefillerStillRunning {
    pub waited: Duration,
}

impl std::fmt::Display for RefillerStillRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the template-id refiller was still reserving after {:?}; its compare-and-swap \
             write cannot lower the high-water, and it records nothing more",
            self.waited
        )
    }
}

impl std::error::Error for RefillerStillRunning {}

/// One store's template-id allocation: the startup seat, and the
/// reserver the miner draws from.
pub struct TemplateIds {
    shared: Arc<Shared>,
    refill: SyncSender<()>,
    refill_rx: Mutex<Option<Receiver<()>>>,
    refiller: Mutex<Option<Refiller>>,
    replaying: Arc<AtomicBool>,
    allow_bootstrap: bool,
}

impl TemplateIds {
    #[must_use]
    pub fn new(store: Store) -> Self {
        // One slot: a request while one is pending coalesces into it.
        let (refill, refill_rx) = mpsc::sync_channel(1);
        Self {
            shared: Arc::new(Shared {
                store,
                ready: Mutex::new(Ready::default()),
                ledger: Mutex::new(None),
                replay_failure: Mutex::new(None),
                stopped: AtomicBool::new(false),
            }),
            refill,
            refill_rx: Mutex::new(Some(refill_rx)),
            refiller: Mutex::new(None),
            replaying: Arc::new(AtomicBool::new(true)),
            allow_bootstrap: false,
        }
    }

    /// Authorise this start to bootstrap the high-water on a store that
    /// already holds data: the one upgraded replica of the RFC 0059 §3.5
    /// upgrade, never a normal start.
    #[must_use]
    pub fn with_bootstrap_allowed(mut self, allowed: bool) -> Self {
        self.allow_bootstrap = allowed;
        self
    }

    /// Recovery has handed the miner to the pipeline: from now on an
    /// exhausted range fails fresh mints instead of reserving under the
    /// miner lock (RFC 0059 §3.3), and the background refiller starts,
    /// the only reserver from here on.
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError::Refiller`] when its thread cannot start.
    pub fn finish_replay(&self) -> Result<(), TemplateIdsError> {
        self.replaying.store(false, Ordering::Release);
        let receiver = lock(&self.refill_rx).take();
        if let Some(receiver) = receiver {
            let shared = Arc::clone(&self.shared);
            let (exiting, exited) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("template-id-refill".to_owned())
                .spawn(move || {
                    let _exiting = exiting;
                    refill_loop(&shared, &receiver);
                })
                .map_err(|e| TemplateIdsError::Refiller(e.to_string()))?;
            *lock(&self.refiller) = Some(Refiller { thread, exited });
            request_refill(&self.refill);
        }
        Ok(())
    }

    /// Stop the refiller and wait for it, before the receiver releases its
    /// roots: a reservation still in flight once another receiver starts
    /// over them could otherwise overwrite the high-water and the marker
    /// below what that receiver reserved. After this returns no block is
    /// recorded or made ready. Idempotent; dropping `self` does the same.
    ///
    /// On a store without compare-and-swap (the local backend) the wait is
    /// unbounded, since a stale unconditional write could lower the
    /// high-water; its calls are local filesystem calls. Elsewhere a stale
    /// write fails its `If-Match`, so the wait is bounded by
    /// ten seconds.
    ///
    /// # Errors
    ///
    /// [`RefillerStillRunning`] when the bounded wait ran out.
    pub fn shutdown(&self) -> Result<(), RefillerStillRunning> {
        {
            let _ledger = lock(&self.shared.ledger);
            self.shared.stopped.store(true, Ordering::Release);
        }
        request_refill(&self.refill);
        let Some(Refiller { thread, exited }) = lock(&self.refiller).take() else {
            return Ok(());
        };
        if self.shared.store.supports_conditional_update() {
            match exited.recv_timeout(STOP_WITHIN) {
                Err(RecvTimeoutError::Disconnected) => {}
                Ok(()) | Err(RecvTimeoutError::Timeout) => {
                    return Err(RefillerStillRunning {
                        waited: STOP_WITHIN,
                    });
                }
            }
        }
        // A panic in the refiller has already been reported by the hook.
        drop(thread.join());
        Ok(())
    }

    /// The reservation replay could not make, if any. The miner turns it
    /// into a parse failure, which replay must not settle for: recovery
    /// fails with it instead (RFC 0059 §3.4).
    #[must_use]
    pub fn take_replay_failure(&self) -> Option<TemplateIdsError> {
        lock(&self.shared.replay_failure).take()
    }

    /// The store the high-water lives in.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.shared.store
    }

    /// The reserver to install on the miner with
    /// [`MinerCluster::with_id_reserver`].
    #[must_use]
    pub fn reserver(&self) -> Box<dyn IdReserver> {
        Box::new(StoreIdReserver {
            shared: Arc::clone(&self.shared),
            refill: self.refill.clone(),
            replaying: Arc::clone(&self.replaying),
        })
    }

    /// Seat `miner` above the high-water and reserve its current block and
    /// two more synchronously (RFC 0059 §3.4). Runs once, at startup,
    /// before any listener opens; the refiller starts at
    /// [`Self::finish_replay`].
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError`] when the high-water cannot be read,
    /// bootstrapped or reserved from; startup fails closed.
    pub fn start(
        &self,
        miner: &mut MinerCluster,
        trust: SnapshotTrust,
    ) -> Result<Seated, TemplateIdsError> {
        let policy = match (trust, self.allow_bootstrap) {
            (SnapshotTrust::Seated { max_reserved_seen }, _) => BootstrapPolicy::Refuse {
                floor: max_reserved_seen,
            },
            (SnapshotTrust::PredatesHighWater, _) => BootstrapPolicy::Refuse { floor: 0 },
            (SnapshotTrust::Bootstrap, false) => BootstrapPolicy::IfStoreEmpty,
            (SnapshotTrust::Bootstrap, true) => BootstrapPolicy::Authorized,
        };
        let shared = &self.shared;
        let seated = seat(&shared.store, miner, policy)?;
        lock(&shared.ready).highest = miner.highest_allocated();
        fill(shared)?;
        if !lock(&shared.ready).blocks.is_empty() {
            miner
                .reserve_current_block()
                .map_err(TemplateIdsError::FirstBlock)?;
            fill(shared)?;
        }
        Ok(seated)
    }

    /// Write the root's seated marker after [`Self::start`], recording
    /// every block reserved so far, and keep it current from then on. A
    /// root that seated before keeps its `seated_above`; a new one seats
    /// above the high-water `seated` read.
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError`] when the marker cannot be read or written.
    pub fn record_seat(
        &self,
        snapshots_root: &Path,
        trust: SnapshotTrust,
        seated: Seated,
    ) -> Result<(), TemplateIdsError> {
        let base = match trust {
            SnapshotTrust::Seated { .. } => {
                read_marker(snapshots_root)?.ok_or_else(|| TemplateIdsError::MarkerInvalid {
                    detail: "the seated marker vanished during startup".to_owned(),
                })?
            }
            SnapshotTrust::Bootstrap | SnapshotTrust::PredatesHighWater => Marker {
                seated_above: seated.high_water,
                max_reserved_seen: seated.high_water,
            },
        };
        let mut ledger = lock(&self.shared.ledger);
        let highest = lock(&self.shared.ready).highest;
        let marker = Marker {
            max_reserved_seen: base.max_reserved_seen.max(highest),
            ..base
        };
        write_marker(snapshots_root, marker)?;
        *ledger = Some(Ledger {
            root: snapshots_root.to_path_buf(),
            marker,
        });
        Ok(())
    }
}

/// Reserve blocks until [`READY_BLOCKS`] are ready, or until the id
/// domain has none left: fresh mints then fail once the held ids run
/// out, and no retry can help.
fn fill(shared: &Shared) -> Result<(), TemplateIdsError> {
    loop {
        if shared.stopped() {
            return Ok(());
        }
        let floor = {
            let held = lock(&shared.ready);
            if held.blocks.len() >= READY_BLOCKS {
                return Ok(());
            }
            held.highest
        };
        let block = match reserve(&shared.store, floor) {
            Ok(block) => block,
            Err(TemplateIdsError::Exhausted(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut ledger = lock(&shared.ledger);
        if shared.stopped() {
            return Ok(());
        }
        if let Some(ledger) = ledger.as_mut() {
            record(ledger, block.through())?;
        }
        let mut held = lock(&shared.ready);
        held.highest = held.highest.max(block.through());
        held.blocks.push_back(block);
    }
}

/// Raise the marker's `max_reserved_seen` to `through`, durably, before
/// the block ending there can be taken.
fn record(ledger: &mut Ledger, through: u64) -> Result<(), TemplateIdsError> {
    if through <= ledger.marker.max_reserved_seen {
        return Ok(());
    }
    let marker = Marker {
        max_reserved_seen: through,
        ..ledger.marker
    };
    write_marker(&ledger.root, marker)?;
    ledger.marker = marker;
    Ok(())
}

/// Ask the refiller to top the ready blocks up, without blocking: a
/// request already pending covers this one.
fn request_refill(refill: &SyncSender<()>) {
    // Full means a request is pending; disconnected means the refiller is
    // gone with the process.
    let _ = refill.try_send(());
}

/// Refill on every request, retrying a failure with capped backoff until
/// it lands or every sender is gone. Requests arriving during a backoff do
/// not cut it short. A deleted high-water stops the refiller for good
/// (RFC 0059 §3.1): an object that reappears may sit below blocks other
/// receivers hold, so only a restart, which fails closed, may decide what
/// it is worth. An exhausted id domain is no failure (`fill` returns
/// `Ok`): it surfaces only as `id_reservation_failed` parse failures.
fn refill_loop(shared: &Shared, requests: &Receiver<()>) {
    while requests.recv().is_ok() && !shared.stopped() {
        let mut backoff = BACKOFF_START;
        loop {
            match fill(shared) {
                Ok(()) => break,
                Err(error @ TemplateIdsError::HighWaterDeleted) => {
                    tracing::error!(
                        name: names::REFILL_STOPPED,
                        { { crate::metrics::ERROR_TYPE } = error.error_type() },
                        "template-id refill stopped; fresh templates fail parse once the \
                         held blocks are spent, until a restart: {error}",
                    );
                    return;
                }
                Err(error) => tracing::warn!(
                    name: names::REFILL_FAILED,
                    { { crate::metrics::ERROR_TYPE } = error.error_type() },
                    "template-id refill failed; retrying with backoff while the held blocks \
                     last: {error}",
                ),
            }
            if !wait_out(shared, requests, Instant::now() + backoff) {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Wait until `deadline` whatever requests arrive meanwhile; `false` once
/// every sender is gone, or once a shutdown's request arrives.
fn wait_out(shared: &Shared, requests: &Receiver<()>, deadline: Instant) -> bool {
    loop {
        if shared.stopped() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match requests.recv_timeout(left) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

impl Drop for TemplateIds {
    fn drop(&mut self) {
        // A startup that fails after replay drops these without a
        // shutdown; the bounded case has nothing left to do.
        let _ = self.shutdown();
    }
}

/// The miner's side: an in-memory pop, never a store call once the
/// listeners are open.
struct StoreIdReserver {
    shared: Arc<Shared>,
    refill: SyncSender<()>,
    replaying: Arc<AtomicBool>,
}

impl StoreIdReserver {
    fn take(&self, floor: u64) -> Option<IdBlock> {
        let mut held = lock(&self.shared.ready);
        std::iter::from_fn(|| held.blocks.pop_front())
            .find_map(|block| IdBlock::new(block.after().max(floor), block.through()))
    }
}

impl IdReserver for StoreIdReserver {
    fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError> {
        let taken = match self.take(floor) {
            None if self.replaying.load(Ordering::Acquire) => {
                if let Err(failure) = fill(&self.shared) {
                    let detail = failure.to_string();
                    *lock(&self.shared.replay_failure) = Some(failure);
                    return Err(IdReservationError::new(detail));
                }
                self.take(floor)
            }
            taken => taken,
        };
        request_refill(&self.refill);
        taken.ok_or_else(|| IdReservationError::new("no reserved template-id block is ready"))
    }
}

#[cfg(test)]
mod tests {
    use ourios_config::MinerConfig;

    use super::*;
    use crate::template_ids::{BLOCK, HIGH_WATER_KEY, encode};

    /// A seated root whose marker holds no reservation above the store's.
    const SEATED: SnapshotTrust = SnapshotTrust::Seated {
        max_reserved_seen: 0,
    };

    /// A high-water rolled back after the trust decision read it, and
    /// before seating reads it again, fails startup closed before any
    /// block is reserved: seating holds the object to the marker's
    /// `max_reserved_seen`, not only the first read.
    #[test]
    fn a_rollback_between_the_trust_decision_and_seating_fails_closed() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path().join("snapshots");
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(8_000))
            .expect("put");
        crate::template_ids::mark_seated(&root, 8_000).expect("mark");
        let trust = SnapshotTrust::of(&root, &store).expect("current");

        store
            .put_blocking(HIGH_WATER_KEY, encode(0))
            .expect("stale copy");
        let ids = TemplateIds::new(store.clone());
        let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
        let err = ids.start(&mut miner, trust).expect_err("rolled back");

        assert!(
            matches!(
                err,
                TemplateIdsError::HighWaterRolledBack {
                    seen: 8_000,
                    found: 0
                }
            ),
            "{err}"
        );
        assert_eq!(
            crate::template_ids::read(&store)
                .expect("read")
                .expect("present")
                .reserved_through,
            0,
            "no block was reserved over the stale copy"
        );
        assert!(lock(&ids.shared.ready).blocks.is_empty());
        assert_eq!(miner.highest_allocated(), 0);
    }

    #[test]
    fn startup_seats_above_the_high_water_and_readies_two_blocks() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(500))
            .expect("put");
        let ids = TemplateIds::new(store.clone());
        let mut miner = MinerCluster::new(MinerConfig::default());
        let seated = ids.start(&mut miner, SEATED).expect("start");
        assert_eq!(seated.high_water, 500);
        assert_eq!(miner.highest_allocated(), 500);
        let mut reserver = ids.reserver();
        let first = reserver.reserve(500).expect("ready");
        assert_eq!((first.after(), first.through()), (500, 500 + BLOCK));
        let second = reserver.reserve(first.through()).expect("ready");
        assert_eq!(second.after(), 500 + BLOCK);
    }

    /// Before replay ends only replay's synchronous fill reserves: no
    /// refiller races it, which on the local backend's unconditional
    /// overwrite could hand the same block out twice.
    #[test]
    fn nothing_reserves_in_the_background_until_replay_ends() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::local(tmp.path()).expect("local");
        store.put_blocking(HIGH_WATER_KEY, encode(0)).expect("put");
        let ids = TemplateIds::new(store.clone());
        let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
        ids.start(&mut miner, SEATED).expect("start");
        let high_water = || {
            crate::template_ids::read(&store)
                .expect("read")
                .expect("present")
                .reserved_through
        };
        let ready = || lock(&ids.shared.ready).blocks.len();

        let mut reserver = ids.reserver();
        let taken: Vec<(u64, u64)> = [BLOCK, 2 * BLOCK, 3 * BLOCK]
            .into_iter()
            .map(|floor| {
                let block = reserver.reserve(floor).expect("a block");
                (block.after(), block.through())
            })
            .collect();
        assert_eq!(
            taken,
            [
                (BLOCK, 2 * BLOCK),
                (2 * BLOCK, 3 * BLOCK),
                (3 * BLOCK, 4 * BLOCK)
            ]
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            high_water(),
            5 * BLOCK,
            "only the synchronous fill reserved"
        );
        assert_eq!(ready(), 1, "nothing topped the ready blocks up");

        ids.finish_replay().expect("refiller");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ready() < READY_BLOCKS && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(ready(), READY_BLOCKS, "the refiller runs once replay ends");
        assert_eq!(high_water(), 6 * BLOCK);
    }

    #[test]
    fn startup_leaves_a_current_block_and_two_ready() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(500))
            .expect("put");
        let ids = TemplateIds::new(store.clone());
        let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
        ids.start(&mut miner, SEATED).expect("start");
        let ready: Vec<(u64, u64)> = lock(&ids.shared.ready)
            .blocks
            .iter()
            .map(|b| (b.after(), b.through()))
            .collect();
        assert_eq!(
            ready,
            [
                (500 + BLOCK, 500 + 2 * BLOCK),
                (500 + 2 * BLOCK, 500 + 3 * BLOCK)
            ],
            "the current block is in the miner, two more are ready"
        );
        let high_water = crate::template_ids::read(&store)
            .expect("read")
            .expect("present");
        assert_eq!(high_water.reserved_through, 500 + 3 * BLOCK);
    }

    fn shared(store: Store) -> Arc<Shared> {
        Arc::new(Shared {
            store,
            ready: Mutex::new(Ready::default()),
            ledger: Mutex::new(None),
            replay_failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        })
    }

    #[test]
    fn a_taken_block_never_reaches_below_the_floor() {
        let shared = shared(Store::in_memory());
        let (refill, _rx) = mpsc::sync_channel(1);
        {
            let mut held = lock(&shared.ready);
            held.blocks.push_back(IdBlock::new(0, 10).expect("block"));
            held.blocks.push_back(IdBlock::new(10, 20).expect("block"));
        }
        let mut reserver = StoreIdReserver {
            shared,
            refill,
            replaying: Arc::new(AtomicBool::new(false)),
        };
        let block = reserver.reserve(14).expect("trimmed");
        assert_eq!((block.after(), block.through()), (14, 20));
        assert!(reserver.reserve(20).is_err(), "nothing is ready");
    }

    /// Running out of ids is no refill failure: `fill` returns `Ok`, so
    /// the refiller logs nothing and fresh mints fail as
    /// `id_reservation_failed` once the held blocks are spent.
    #[test]
    fn exhausting_the_id_domain_is_not_a_refill_failure() {
        let store = Store::in_memory();
        store
            .put_blocking(
                HIGH_WATER_KEY,
                encode(ourios_miner::cluster::MAX_TEMPLATE_ID),
            )
            .expect("put");
        let shared = shared(store);
        fill(&shared).expect("exhaustion is not an error");
        assert!(lock(&shared.ready).blocks.is_empty());
    }

    /// A reserver over a healthy store holding a high-water, with nothing
    /// ready.
    fn drained(replaying: bool) -> StoreIdReserver {
        let store = Store::in_memory();
        store.put_blocking(HIGH_WATER_KEY, encode(50)).expect("put");
        let (refill, _rx) = mpsc::sync_channel(1);
        StoreIdReserver {
            shared: shared(store),
            refill,
            replaying: Arc::new(AtomicBool::new(replaying)),
        }
    }

    #[test]
    fn every_block_is_recorded_in_the_marker_before_it_is_ready() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(500))
            .expect("put");
        let ids = TemplateIds::new(store);
        let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
        let seated = ids.start(&mut miner, SEATED).expect("start");
        ids.record_seat(tmp.path(), SnapshotTrust::PredatesHighWater, seated)
            .expect("seat");
        let marker = |root: &Path| read_marker(root).expect("read").expect("present");
        assert_eq!(
            marker(tmp.path()),
            Marker {
                seated_above: 500,
                max_reserved_seen: 500 + 3 * BLOCK
            },
            "startup's three blocks are recorded"
        );

        lock(&ids.shared.ready).blocks.clear();
        fill(&ids.shared).expect("refill");
        assert_eq!(
            marker(tmp.path()).max_reserved_seen,
            500 + 5 * BLOCK,
            "each refilled block is recorded"
        );
    }

    #[test]
    fn after_replay_a_drained_reserver_fails_without_touching_the_store() {
        // The store would grant a block; a reserver that asked it would
        // not fail.
        assert!(drained(false).reserve(50).is_err());
    }

    #[test]
    fn during_replay_a_drained_reserver_reserves_on_demand() {
        let block = drained(true).reserve(50).expect("reserved on demand");
        assert_eq!((block.after(), block.through()), (50, 50 + BLOCK));
    }
}
