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
use std::time::{Duration, Instant};

use ourios_miner::cluster::{IdBlock, IdReservationError, IdReserver, MinerCluster};
use ourios_parquet::Store;

use super::marker::{Marker, read_marker, write_marker};
use super::{BootstrapPolicy, Seated, SnapshotTrust, TemplateIdsError, names, reserve, seat};

/// Blocks kept ready beside the one the miner allocates from.
const READY_BLOCKS: usize = 2;
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

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
}

/// One store's template-id allocation: the startup seat, and the
/// reserver the miner draws from.
pub struct TemplateIds {
    shared: Arc<Shared>,
    refill: SyncSender<()>,
    refill_rx: Mutex<Option<Receiver<()>>>,
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
            }),
            refill,
            refill_rx: Mutex::new(Some(refill_rx)),
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
            std::thread::Builder::new()
                .name("template-id-refill".to_owned())
                .spawn(move || refill_loop(&shared, &receiver))
                .map_err(|e| TemplateIdsError::Refiller(e.to_string()))?;
            request_refill(&self.refill);
        }
        Ok(())
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
            (SnapshotTrust::Seated | SnapshotTrust::PredatesHighWater, _) => {
                BootstrapPolicy::Refuse
            }
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
            SnapshotTrust::Seated => {
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
    while requests.recv().is_ok() {
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
            if !wait_out(requests, Instant::now() + backoff) {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Wait until `deadline` whatever requests arrive meanwhile; `false` once
/// every sender is gone.
fn wait_out(requests: &Receiver<()>, deadline: Instant) -> bool {
    loop {
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
                fill(&self.shared).map_err(IdReservationError::new)?;
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

    #[test]
    fn startup_seats_above_the_high_water_and_readies_two_blocks() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(500))
            .expect("put");
        let ids = TemplateIds::new(store.clone());
        let mut miner = MinerCluster::new(MinerConfig::default());
        let seated = ids.start(&mut miner, SnapshotTrust::Seated).expect("start");
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
        ids.start(&mut miner, SnapshotTrust::Seated).expect("start");
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
        ids.start(&mut miner, SnapshotTrust::Seated).expect("start");
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
        let seated = ids.start(&mut miner, SnapshotTrust::Seated).expect("start");
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
