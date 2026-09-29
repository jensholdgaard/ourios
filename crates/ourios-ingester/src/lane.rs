//! A bounded queue served by threads that are replaced after a panic —
//! the one mechanism behind the encode pool (RFC 0035 §3.1) and RFC 0052
//! §3.1's publisher.
//!
//! Every item a lane accepts is settled: served, or salvaged when the
//! thread that would serve it is gone. A bare thread panic would strand
//! whatever was queued behind the failing item, and with it every guard
//! those items carry — `quiesce` and `quiesce_publishes` would then wait
//! for the life of the process (issue #837). So each item is served under
//! `catch_unwind`, and a panic **retires the lane's generation**:
//!
//! 1. the panicking thread marks the slot [`State::Closed`] under the slot
//!    mutex, which drops the only sender, so no enqueue can succeed into a
//!    channel that is about to be abandoned;
//! 2. only then does it drain its receiver, salvaging every queued item;
//! 3. and exits. The next enqueue finds the slot closed and starts a new
//!    generation.
//!
//! Sends happen under the slot mutex, which is what makes step 1 a real
//! fence: an item is either in the channel before the close — and the
//! drain reaches it — or finds the slot closed and is handed back.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

/// What a lane does with its items.
pub(crate) trait Serve<T>: Send + Sync + 'static {
    /// Serve one item. A panic is caught by the lane and retires its
    /// generation.
    fn serve(&self, item: T);

    /// Settle an item that was queued behind a panic and will not be
    /// served by the generation that accepted it.
    fn salvage(&self, item: T);
}

/// Why [`Lane::try_send`] handed an item back.
pub(crate) enum Refused<T> {
    /// The queue is at its bound.
    Full(T),
    /// A panic retired the generation this enqueue would have joined. A
    /// new one has been started, so the next enqueue is served.
    Closed(T),
}

/// The slot's state. The channel exists only in [`State::Open`], so an
/// enqueue can never reach a sender the slot has given up.
enum State<T> {
    /// No generation has been started yet.
    Idle,
    /// A generation is serving.
    Open { tx: SyncSender<T>, generation: u64 },
    /// A panic retired the last generation; its threads are draining or
    /// gone.
    Closed,
    /// The lane is being dropped.
    Stopped,
}

struct Slot<T> {
    state: State<T>,
    generations: u64,
    handles: Vec<JoinHandle<()>>,
}

struct Shared<T> {
    slot: Mutex<Slot<T>>,
    /// Signalled whenever the queue may have room, or the slot changed.
    space: Condvar,
    capacity: usize,
    threads: usize,
    serve: Arc<dyn Serve<T>>,
}

/// A bounded queue of `T`, served by `threads` threads per generation.
/// Dropping the lane closes the queue, lets the threads serve what is
/// already in it, and joins them.
pub(crate) struct Lane<T: Send + 'static> {
    shared: Arc<Shared<T>>,
}

impl<T: Send + 'static> Lane<T> {
    /// A lane of `threads` threads over a queue of `capacity` items. No
    /// thread starts until the first enqueue or [`Self::start`].
    pub(crate) fn new(threads: usize, capacity: usize, serve: Arc<dyn Serve<T>>) -> Self {
        Self {
            shared: Arc::new(Shared {
                slot: Mutex::new(Slot {
                    state: State::Idle,
                    generations: 0,
                    handles: Vec::new(),
                }),
                space: Condvar::new(),
                capacity: capacity.max(1),
                threads: threads.max(1),
                serve,
            }),
        }
    }

    /// Start the first generation now rather than on the first enqueue.
    pub(crate) fn start(&self) {
        let mut slot = self.shared.lock();
        if matches!(slot.state, State::Idle) {
            self.shared.spawn(&mut slot);
        }
    }

    /// Enqueue without waiting. An item handed back is the caller's to
    /// settle.
    pub(crate) fn try_send(&self, item: T) -> Result<(), Refused<T>> {
        let mut slot = self.shared.lock();
        match &slot.state {
            State::Open { tx, .. } => match tx.try_send(item) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(item)) => Err(Refused::Full(item)),
                // Every thread of the generation is gone without having
                // closed the slot — nothing short of an abort does that,
                // but the item is handed back all the same.
                Err(TrySendError::Disconnected(item)) => {
                    self.shared.spawn(&mut slot);
                    Err(Refused::Closed(item))
                }
            },
            State::Idle => {
                self.shared.spawn(&mut slot);
                match &slot.state {
                    State::Open { tx, .. } => tx.try_send(item).map_err(|e| match e {
                        TrySendError::Full(item) => Refused::Full(item),
                        TrySendError::Disconnected(item) => Refused::Closed(item),
                    }),
                    State::Idle | State::Closed | State::Stopped => Err(Refused::Closed(item)),
                }
            }
            State::Closed => {
                self.shared.spawn(&mut slot);
                Err(Refused::Closed(item))
            }
            State::Stopped => Err(Refused::Closed(item)),
        }
    }

    /// Enqueue, waiting while the queue is full — the backpressure a
    /// bounded lane exists for. A retired generation is replaced and the
    /// item sent into the new one.
    pub(crate) fn send(&self, mut item: T) {
        let mut slot = self.shared.lock();
        loop {
            match &slot.state {
                State::Open { tx, .. } => match tx.try_send(item) {
                    Ok(()) => return,
                    Err(TrySendError::Full(back)) => {
                        item = back;
                        slot = self
                            .shared
                            .space
                            .wait(slot)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                    Err(TrySendError::Disconnected(back)) => {
                        item = back;
                        self.shared.spawn(&mut slot);
                    }
                },
                State::Idle | State::Closed => {
                    self.shared.spawn(&mut slot);
                }
                State::Stopped => {
                    drop(slot);
                    self.shared.serve.salvage(item);
                    return;
                }
            }
        }
    }
}

impl<T: Send + 'static> Drop for Lane<T> {
    fn drop(&mut self) {
        let handles = {
            let mut slot = self.shared.lock();
            slot.state = State::Stopped;
            std::mem::take(&mut slot.handles)
        };
        self.shared.space.notify_all();
        for handle in handles {
            drop(handle.join());
        }
    }
}

impl<T: Send + 'static> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, Slot<T>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start a new generation in `slot`. Called with the slot mutex held,
    /// so exactly one caller replaces a retired generation.
    fn spawn(self: &Arc<Self>, slot: &mut Slot<T>) {
        if matches!(slot.state, State::Stopped) {
            return;
        }
        slot.handles.retain(|handle| !handle.is_finished());
        let generation = slot.generations;
        slot.generations += 1;
        let (tx, rx) = sync_channel::<T>(self.capacity);
        // `mpsc::Receiver` is single-consumer; the mutex turns it into a
        // shared work queue — pickup serializes, the work does not.
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..self.threads {
            let shared = Arc::clone(self);
            let rx = Arc::clone(&rx);
            slot.handles
                .push(std::thread::spawn(move || shared.work(&rx, generation)));
        }
        slot.state = State::Open { tx, generation };
        self.space.notify_all();
    }

    fn work(&self, rx: &Mutex<Receiver<T>>, generation: u64) {
        loop {
            let item = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
            let Ok(item) = item else {
                return; // the sender is gone: closed, retired or stopped
            };
            self.made_space();
            let served = catch_unwind(AssertUnwindSafe(|| self.serve.serve(item)));
            if served.is_err() {
                self.retire(rx, generation);
                return;
            }
        }
    }

    /// Wake a sender waiting for room. The slot mutex is taken first so a
    /// sender between its failed `try_send` and its wait cannot miss this.
    fn made_space(&self) {
        drop(self.lock());
        self.space.notify_all();
    }

    /// Close the slot, then salvage everything still queued — in that
    /// order, so no enqueue lands after the drain.
    fn retire(&self, rx: &Mutex<Receiver<T>>, generation: u64) {
        {
            let mut slot = self.lock();
            if matches!(slot.state, State::Open { generation: open, .. } if open == generation) {
                slot.state = State::Closed;
            }
        }
        self.space.notify_all();
        let rx = rx.lock().unwrap_or_else(PoisonError::into_inner);
        for item in rx.try_iter() {
            // A salvage that panics must not strand the items behind it.
            drop(catch_unwind(AssertUnwindSafe(|| self.serve.salvage(item))));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Serves by counting; an item of `0` panics, after waiting for the
    /// test's release.
    #[derive(Default)]
    struct Counting {
        served: AtomicUsize,
        salvaged: AtomicUsize,
        entered: AtomicBool,
        release: AtomicBool,
    }

    impl Serve<u32> for Counting {
        fn serve(&self, item: u32) {
            if item == 0 {
                self.entered.store(true, Ordering::Release);
                while !self.release.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                panic!("injected lane panic");
            }
            self.served.fetch_add(1, Ordering::AcqRel);
        }

        fn salvage(&self, _item: u32) {
            self.salvaged.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn await_flag(flag: &AtomicBool) {
        while !flag.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }

    fn await_count(count: &AtomicUsize, want: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while count.load(Ordering::Acquire) < want {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_panic_salvages_the_queue_behind_it_and_the_next_enqueue_restarts() {
        let serve = Arc::new(Counting::default());
        let lane = Lane::new(1, 8, Arc::clone(&serve) as Arc<dyn Serve<u32>>);
        assert!(lane.try_send(0).is_ok());
        await_flag(&serve.entered);
        assert!(lane.try_send(1).is_ok());
        assert!(lane.try_send(2).is_ok());

        serve.release.store(true, Ordering::Release);
        await_count(&serve.salvaged, 2);
        assert_eq!(serve.served.load(Ordering::Acquire), 0);

        // The enqueue that finds the retired generation is handed back and
        // starts the next one; the one after it is served.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match lane.try_send(3) {
                Err(Refused::Closed(_)) => break,
                Ok(()) => panic!("an enqueue landed in a retired generation"),
                Err(Refused::Full(_)) => {
                    assert!(std::time::Instant::now() < deadline, "never closed");
                    std::thread::yield_now();
                }
            }
        }
        assert!(lane.try_send(4).is_ok());
        await_count(&serve.served, 1);
    }

    #[test]
    fn a_full_queue_hands_the_item_back() {
        let serve = Arc::new(Counting::default());
        let lane = Lane::new(1, 1, Arc::clone(&serve) as Arc<dyn Serve<u32>>);
        assert!(lane.try_send(0).is_ok());
        await_flag(&serve.entered);
        assert!(lane.try_send(1).is_ok(), "one slot of room");
        assert!(matches!(lane.try_send(2), Err(Refused::Full(2))));
        serve.release.store(true, Ordering::Release);
    }

    #[test]
    fn a_blocking_send_behind_a_panic_reaches_the_next_generation() {
        let serve = Arc::new(Counting::default());
        let lane = Arc::new(Lane::new(1, 1, Arc::clone(&serve) as Arc<dyn Serve<u32>>));
        lane.send(0);
        await_flag(&serve.entered);
        lane.send(1);
        let blocked = {
            let lane = Arc::clone(&lane);
            std::thread::spawn(move || lane.send(2))
        };
        serve.release.store(true, Ordering::Release);
        blocked.join().expect("the blocked send returned");
        // The next generation can serve item 2 before the retiring thread
        // has finished salvaging item 1, so wait for both.
        await_count(&serve.served, 1);
        await_count(&serve.salvaged, 1);
        assert_eq!(
            serve.served.load(Ordering::Acquire) + serve.salvaged.load(Ordering::Acquire),
            2,
            "every item behind the panic was served or salvaged",
        );
    }

    #[test]
    fn dropping_the_lane_serves_what_is_queued() {
        let serve = Arc::new(Counting::default());
        let lane = Lane::new(2, 16, Arc::clone(&serve) as Arc<dyn Serve<u32>>);
        for item in 1..=10 {
            lane.send(item);
        }
        drop(lane);
        assert_eq!(serve.served.load(Ordering::Acquire), 10);
    }
}
