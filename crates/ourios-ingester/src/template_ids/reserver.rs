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
//! have given an id.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ourios_miner::cluster::{IdBlock, IdReservationError, IdReserver, MinerCluster};
use ourios_parquet::Store;

use super::{BootstrapPolicy, Seated, TemplateIdsError, reserve, seat};

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

fn lock(ready: &Mutex<Ready>) -> MutexGuard<'_, Ready> {
    ready.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One store's template-id allocation: the startup seat, and the
/// reserver the miner draws from.
pub struct TemplateIds {
    store: Store,
    ready: Arc<Mutex<Ready>>,
    refill: Sender<()>,
    refill_rx: Mutex<Option<Receiver<()>>>,
    replaying: Arc<AtomicBool>,
    allow_bootstrap: bool,
}

impl TemplateIds {
    #[must_use]
    pub fn new(store: Store) -> Self {
        let (refill, refill_rx) = mpsc::channel();
        Self {
            store,
            ready: Arc::new(Mutex::new(Ready::default())),
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
    /// miner lock (RFC 0059 §3.3).
    pub fn finish_replay(&self) {
        self.replaying.store(false, Ordering::Release);
    }

    /// The store the high-water lives in.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The reserver to install on the miner with
    /// [`MinerCluster::with_id_reserver`].
    #[must_use]
    pub fn reserver(&self) -> Box<dyn IdReserver> {
        Box::new(StoreIdReserver {
            store: self.store.clone(),
            ready: Arc::clone(&self.ready),
            refill: self.refill.clone(),
            replaying: Arc::clone(&self.replaying),
        })
    }

    /// Seat `miner` above the high-water, reserve the first blocks
    /// synchronously, and start the background refiller (RFC 0059 §3.4).
    /// Runs once, at startup, before any listener opens.
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError`] when the high-water cannot be read,
    /// bootstrapped or reserved from; startup fails closed.
    pub fn start(
        &self,
        miner: &mut MinerCluster,
        may_bootstrap: bool,
    ) -> Result<Seated, TemplateIdsError> {
        let policy = match (may_bootstrap, self.allow_bootstrap) {
            (false, _) => BootstrapPolicy::Refuse,
            (true, false) => BootstrapPolicy::IfStoreEmpty,
            (true, true) => BootstrapPolicy::Authorized,
        };
        let seated = seat(&self.store, miner, policy)?;
        lock(&self.ready).highest = miner.highest_allocated();
        fill(&self.store, &self.ready)?;
        let receiver = self
            .refill_rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(receiver) = receiver {
            let (store, ready) = (self.store.clone(), Arc::clone(&self.ready));
            std::thread::Builder::new()
                .name("template-id-refill".to_owned())
                .spawn(move || refill_loop(&store, &ready, &receiver))
                .map_err(|e| TemplateIdsError::Refiller(e.to_string()))?;
        }
        Ok(seated)
    }
}

/// Reserve blocks until [`READY_BLOCKS`] are ready.
fn fill(store: &Store, ready: &Mutex<Ready>) -> Result<(), TemplateIdsError> {
    loop {
        let floor = {
            let held = lock(ready);
            if held.blocks.len() >= READY_BLOCKS {
                return Ok(());
            }
            held.highest
        };
        let block = reserve(store, floor)?;
        let mut held = lock(ready);
        held.highest = held.highest.max(block.through());
        held.blocks.push_back(block);
    }
}

/// Refill on every request, retrying a failure with capped backoff until
/// it lands or every sender is gone. A deleted high-water stops the
/// refiller for good (RFC 0059 §3.1): an object that reappears may sit
/// below blocks other receivers hold, so only a restart, which fails
/// closed, may decide what it is worth.
fn refill_loop(store: &Store, ready: &Mutex<Ready>, requests: &Receiver<()>) {
    while requests.recv().is_ok() {
        let mut backoff = BACKOFF_START;
        loop {
            match fill(store, ready) {
                Ok(()) => break,
                Err(error @ TemplateIdsError::HighWaterDeleted) => {
                    tracing::error!(
                        { crate::metrics::ERROR_TYPE } = error.error_type(),
                        "template-id refill stopped; fresh templates fail parse until a \
                         restart: {error}",
                    );
                    return;
                }
                Err(error) => tracing::warn!(
                    { crate::metrics::ERROR_TYPE } = error.error_type(),
                    "template-id refill failed; fresh templates fail parse until it lands: {error}",
                ),
            }
            match requests.recv_timeout(backoff) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => backoff = (backoff * 2).min(BACKOFF_MAX),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// The miner's side: an in-memory pop, never a store call once the
/// listeners are open.
struct StoreIdReserver {
    store: Store,
    ready: Arc<Mutex<Ready>>,
    refill: Sender<()>,
    replaying: Arc<AtomicBool>,
}

impl StoreIdReserver {
    fn take(&self, floor: u64) -> Option<IdBlock> {
        let mut held = lock(&self.ready);
        std::iter::from_fn(|| held.blocks.pop_front())
            .find_map(|block| IdBlock::new(block.after().max(floor), block.through()))
    }
}

impl IdReserver for StoreIdReserver {
    fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError> {
        let taken = match self.take(floor) {
            None if self.replaying.load(Ordering::Acquire) => {
                fill(&self.store, &self.ready).map_err(IdReservationError::new)?;
                self.take(floor)
            }
            taken => taken,
        };
        // A send fails only once the refiller is gone with the process.
        let _ = self.refill.send(());
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
        let seated = ids.start(&mut miner, true).expect("start");
        assert_eq!(seated.high_water, 500);
        assert_eq!(miner.highest_allocated(), 500);
        let mut reserver = ids.reserver();
        let first = reserver.reserve(500).expect("ready");
        assert_eq!((first.after(), first.through()), (500, 500 + BLOCK));
        let second = reserver.reserve(first.through()).expect("ready");
        assert_eq!(second.after(), 500 + BLOCK);
    }

    #[test]
    fn a_taken_block_never_reaches_below_the_floor() {
        let ready = Arc::new(Mutex::new(Ready::default()));
        let (refill, _rx) = mpsc::channel();
        {
            let mut held = lock(&ready);
            held.blocks.push_back(IdBlock::new(0, 10).expect("block"));
            held.blocks.push_back(IdBlock::new(10, 20).expect("block"));
        }
        let mut reserver = StoreIdReserver {
            store: Store::in_memory(),
            ready,
            refill,
            replaying: Arc::new(AtomicBool::new(false)),
        };
        let block = reserver.reserve(14).expect("trimmed");
        assert_eq!((block.after(), block.through()), (14, 20));
        assert!(reserver.reserve(20).is_err(), "nothing is ready");
    }

    /// A reserver over a healthy store holding a high-water, with nothing
    /// ready.
    fn drained(replaying: bool) -> StoreIdReserver {
        let store = Store::in_memory();
        store.put_blocking(HIGH_WATER_KEY, encode(50)).expect("put");
        let (refill, _rx) = mpsc::channel();
        StoreIdReserver {
            store,
            ready: Arc::new(Mutex::new(Ready::default())),
            refill,
            replaying: Arc::new(AtomicBool::new(replaying)),
        }
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
