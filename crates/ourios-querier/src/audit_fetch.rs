//! Bounded-concurrency, order-preserving object fetch for the remote audit
//! scan (#853).
//!
//! A cold template-map fold GETs every audit object of the tenant. Issued one
//! at a time, each pays a full object-store round trip, so the fold's wall
//! time is the sum of the latencies. [`fetch_in_order`] keeps several GETs in
//! flight but hands their bodies to the consumer strictly in listing order,
//! so the fold sees exactly the sequence the serial scan saw.
//!
//! The limits are **process-wide**: every fold fetches through one
//! [`FetchPool`] ([`FetchPool::shared`]), so concurrent cold folds for
//! different tenants share them rather than multiplying them.
//!
//! - `requests` worker threads serve every registered fold, one GET per turn
//!   in rotation, so it caps both the GETs in flight and the threads, across
//!   all folds. Workers start on demand and exit after a minute idle.
//! - `bytes` caps the listed size of every object dispatched but not yet
//!   consumed, summed over all folds — in flight, or fetched and waiting for
//!   an earlier one — so out-of-order completions cannot pile up behind a
//!   slow GET. An object larger than the whole budget is fetched alone.
//!
//! When the next fold in rotation cannot fit its next object, workers wait
//! for room rather than serving another fold, so a large object is never
//! starved by a stream of small ones. That wait always ends: every reserved
//! byte belongs to an object already being fetched or already fetched, and
//! each fold's consumer only waits on objects it has already been granted.
//! A fold that fails or is dropped releases its fetched bodies at once, but
//! its GETs still in flight keep their bytes until they finish, so the
//! budget bounds every body actually held, not just those of live folds.
//!
//! The consumer decodes one object at a time, as the serial scan did, so the
//! decoded-events footprint is unchanged.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::QueryError;

/// The process-wide fetch limits of a [`FetchPool`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchLimits {
    /// Worker threads, and so GETs in flight, across all folds (at least one).
    pub(crate) requests: usize,
    /// Listed bytes dispatched but not yet consumed, across all folds.
    pub(crate) bytes: u64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            requests: 16,
            bytes: 32 * 1024 * 1024,
        }
    }
}

/// One GET: the store call a fold's objects are fetched with.
pub(crate) type Fetch = Arc<dyn Fn(&str) -> Result<Vec<u8>, QueryError> + Send + Sync>;

/// A fetched body, a fetch error, or the payload of a panicking fetch (re-
/// raised on the consumer's thread when it reaches that object).
type Outcome = std::thread::Result<Result<Vec<u8>, QueryError>>;

/// How long an idle worker of the shared pool waits for work before it
/// exits.
const IDLE: Duration = Duration::from_secs(60);

/// Test-built pools' idle lifetime: short, so a test's workers exit soon
/// after its folds finish instead of piling up across tests.
#[cfg(test)]
const TEST_IDLE: Duration = Duration::from_millis(20);

static SHARED: LazyLock<Arc<FetchPool>> = LazyLock::new(|| FetchPool::new(FetchLimits::default()));

/// The process-wide worker pool and byte budget every fold fetches through.
pub(crate) struct FetchPool {
    limits: FetchLimits,
    idle: Duration,
    state: Mutex<PoolState>,
    changed: Condvar,
    /// Test hook: make every worker spawn fail.
    #[cfg(test)]
    fail_spawns: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct PoolState {
    /// Listed bytes dispatched but not yet consumed, over all folds.
    reserved: u64,
    workers: usize,
    /// Folds with objects left to dispatch, in serving order.
    rotation: VecDeque<u64>,
    folds: HashMap<u64, Fold>,
    next_id: u64,
}

struct Fold {
    objects: Arc<[(String, u64)]>,
    fetch: Fetch,
    span: tracing::Span,
    /// The next object index to dispatch; every index below it is
    /// dispatched, so the consumer's next object always is.
    next: usize,
    /// This fold's share of [`PoolState::reserved`]: its GETs in flight
    /// plus its fetched objects not yet consumed.
    reserved: u64,
    /// The in-flight part of `reserved`. A deregistered fold's in-flight
    /// GETs stay charged until their workers finish them.
    in_flight: u64,
    ready: BTreeMap<usize, Outcome>,
    /// A fetch or the consumer failed: dispatch nothing more.
    stopped: bool,
}

struct Job {
    fold: u64,
    index: usize,
    size: u64,
    key: String,
    fetch: Fetch,
    span: tracing::Span,
}

impl PoolState {
    /// The next object to fetch, in rotation, if the byte budget admits it.
    fn claim(&mut self, budget: u64) -> Option<Job> {
        while let Some(&id) = self.rotation.front() {
            let Some(fold) = self.folds.get_mut(&id) else {
                self.rotation.pop_front();
                continue;
            };
            if fold.stopped || fold.next == fold.objects.len() {
                self.rotation.pop_front();
                continue;
            }
            let (key, size) = &fold.objects[fold.next];
            if self.reserved != 0 && self.reserved.saturating_add(*size) > budget {
                return None;
            }
            let job = Job {
                fold: id,
                index: fold.next,
                size: *size,
                key: key.clone(),
                fetch: Arc::clone(&fold.fetch),
                span: fold.span.clone(),
            };
            fold.next += 1;
            fold.reserved = fold.reserved.saturating_add(*size);
            fold.in_flight = fold.in_flight.saturating_add(*size);
            self.reserved = self.reserved.saturating_add(*size);
            self.rotation.rotate_left(1);
            return Some(job);
        }
        None
    }

    /// Publish one fetch's outcome. A failed fetch also stops its fold's
    /// dispatch at once: the pass will fail when the consumer reaches it, so
    /// GETs started in the meantime would be wasted. The error itself is
    /// still delivered in listing order.
    ///
    /// The object's bytes stay charged while its body waits to be consumed.
    /// If the fold is gone — it failed or was dropped while this GET was in
    /// flight — nobody will consume it, so the charge is released here, only
    /// now that the body has been dropped.
    fn publish(&mut self, job: &Job, outcome: Outcome) {
        if let Some(fold) = self.folds.get_mut(&job.fold) {
            fold.in_flight = fold.in_flight.saturating_sub(job.size);
            if !matches!(outcome, Ok(Ok(_))) {
                fold.stopped = true;
            }
            fold.ready.insert(job.index, outcome);
        } else {
            drop(outcome);
            self.reserved = self.reserved.saturating_sub(job.size);
        }
    }
}

impl FetchPool {
    fn new(limits: FetchLimits) -> Arc<Self> {
        Self::with_idle(limits, IDLE)
    }

    /// A pool for tests: its own limits, and a short idle lifetime.
    #[cfg(test)]
    pub(crate) fn for_test(limits: FetchLimits) -> Arc<Self> {
        Self::with_idle(limits, TEST_IDLE)
    }

    fn with_idle(limits: FetchLimits, idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            limits: FetchLimits {
                requests: limits.requests.max(1),
                bytes: limits.bytes,
            },
            idle,
            state: Mutex::new(PoolState::default()),
            changed: Condvar::new(),
            #[cfg(test)]
            fail_spawns: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The pool every audit scan in the process shares.
    pub(crate) fn shared() -> &'static Arc<FetchPool> {
        &SHARED
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, PoolState>) -> MutexGuard<'a, PoolState> {
        self.changed
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Register a fold for dispatch. Workers are topped up *before* the fold
    /// is added, so a pool that cannot start any worker returns the error
    /// with nothing registered — no fold, no rotation entry to leak.
    fn register(
        self: &Arc<Self>,
        objects: Arc<[(String, u64)]>,
        fetch: Fetch,
    ) -> Result<u64, QueryError> {
        let mut state = self.lock();
        while state.workers < self.limits.requests {
            match self.spawn_worker() {
                Ok(()) => state.workers += 1,
                Err(e) if state.workers == 0 => {
                    return Err(QueryError::Storage {
                        detail: format!("start audit fetch worker: {e}"),
                    });
                }
                // Fewer workers only lowers the concurrency; one is enough
                // to make progress.
                Err(_) => break,
            }
        }
        let id = state.next_id;
        state.next_id += 1;
        state.folds.insert(
            id,
            Fold {
                objects,
                fetch,
                span: tracing::Span::current(),
                next: 0,
                reserved: 0,
                in_flight: 0,
                ready: BTreeMap::new(),
                stopped: false,
            },
        );
        state.rotation.push_back(id);
        drop(state);
        self.changed.notify_all();
        Ok(id)
    }

    fn spawn_worker(self: &Arc<Self>) -> std::io::Result<()> {
        #[cfg(test)]
        if self.fail_spawns.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("injected spawn failure"));
        }
        let pool = Arc::clone(self);
        std::thread::Builder::new()
            .name("ourios-audit-fetch".to_owned())
            .spawn(move || pool.work())
            .map(drop)
    }

    fn work(self: Arc<Self>) {
        let mut state = self.lock();
        loop {
            if let Some(job) = state.claim(self.limits.bytes) {
                drop(state);
                let outcome = job
                    .span
                    .in_scope(|| catch_unwind(AssertUnwindSafe(|| (job.fetch)(&job.key))));
                state = self.lock();
                state.publish(&job, outcome);
                self.changed.notify_all();
                continue;
            }
            let (next, idle) = self
                .changed
                .wait_timeout(state, self.idle)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if idle.timed_out() && state.rotation.is_empty() {
                state.workers -= 1;
                return;
            }
        }
    }

    fn take(&self, fold: u64, index: usize) -> Outcome {
        let mut state = self.lock();
        loop {
            match state.folds.get_mut(&fold) {
                Some(window) => {
                    if let Some(outcome) = window.ready.remove(&index) {
                        return outcome;
                    }
                }
                None => return Ok(Err(abandoned())),
            }
            state = self.wait(state);
        }
    }

    fn stop(&self, fold: u64) {
        if let Some(window) = self.lock().folds.get_mut(&fold) {
            window.stopped = true;
        }
        self.changed.notify_all();
    }

    fn release(&self, fold: u64, size: u64) {
        let mut state = self.lock();
        if let Some(window) = state.folds.get_mut(&fold) {
            window.reserved = window.reserved.saturating_sub(size);
            state.reserved = state.reserved.saturating_sub(size);
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Remove a fold — normally or while unwinding. Its fetched but
    /// unconsumed bodies are dropped and their bytes released now; its GETs
    /// still in flight keep their bytes charged until their workers finish,
    /// so a detached GET can never allocate against budget another fold has
    /// already reused.
    fn deregister(&self, fold: u64) {
        let mut state = self.lock();
        if let Some(window) = state.folds.remove(&fold) {
            let fetched = window.reserved.saturating_sub(window.in_flight);
            state.reserved = state.reserved.saturating_sub(fetched);
        }
        drop(state);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn workers(&self) -> usize {
        self.lock().workers
    }

    #[cfg(test)]
    fn reserved(&self) -> u64 {
        self.lock().reserved
    }

    /// Folds registered and rotation entries queued, for leak checks.
    #[cfg(test)]
    fn registered(&self) -> (usize, usize) {
        let state = self.lock();
        (state.folds.len(), state.rotation.len())
    }
}

fn abandoned() -> QueryError {
    QueryError::Storage {
        detail: "audit fetch window closed".to_owned(),
    }
}

/// Deregisters a fold when its consumer leaves, so the workers drop it and
/// its reservation frees even if `consume` panics.
struct Registration<'a> {
    pool: &'a FetchPool,
    fold: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.pool.deregister(self.fold);
    }
}

/// Fetch every `(key, listed size)` object with `fetch` through `pool`, and
/// pass each body to `consume` in `objects` order with its index. Stops at
/// the first error — a failed GET or a failed `consume` — and returns it;
/// GETs already in flight finish and are dropped. A panicking `fetch` is
/// re-raised here when the consumer reaches its object.
///
/// # Errors
///
/// The first error in `objects` order, from `fetch` or `consume`, or a
/// [`QueryError::Storage`] if no fetch worker could be started.
pub(crate) fn fetch_in_order<C>(
    pool: &Arc<FetchPool>,
    objects: Arc<[(String, u64)]>,
    fetch: Fetch,
    mut consume: C,
) -> Result<(), QueryError>
where
    C: FnMut(usize, Vec<u8>) -> Result<(), QueryError>,
{
    if objects.is_empty() {
        return Ok(());
    }
    let sizes: Vec<u64> = objects.iter().map(|(_, size)| *size).collect();
    let fold = pool.register(objects, fetch)?;
    let _registration = Registration { pool, fold };
    for (index, size) in sizes.iter().enumerate() {
        let outcome = match pool.take(fold, index) {
            Ok(fetched) => fetched.and_then(|body| consume(index, body)),
            Err(payload) => resume_unwind(payload),
        };
        // Stop before releasing: the release wakes budget-blocked workers,
        // which must see the failure instead of claiming another GET.
        if outcome.is_err() {
            pool.stop(fold);
        }
        pool.release(fold, *size);
        outcome?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn objects(sizes: &[u64]) -> Arc<[(String, u64)]> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, size)| (format!("k{i:04}"), *size))
            .collect()
    }

    fn pool(requests: usize, bytes: u64) -> Arc<FetchPool> {
        FetchPool::for_test(FetchLimits { requests, bytes })
    }

    fn fetcher(f: impl Fn(&str) -> Result<Vec<u8>, QueryError> + Send + Sync + 'static) -> Fetch {
        Arc::new(f)
    }

    /// A body that names its key, so the consumer can check it got the
    /// right object at the right index.
    fn body(key: &str) -> Vec<u8> {
        key.as_bytes().to_vec()
    }

    fn index_of(key: &str) -> usize {
        key[1..].parse().expect("index")
    }

    /// Later objects return sooner, so completions arrive out of order.
    fn delay(key: &str) -> Duration {
        let index = index_of(key) as u64;
        Duration::from_millis(20u64.saturating_sub(index % 20))
    }

    #[test]
    fn consumes_in_listing_order_despite_out_of_order_completion() {
        let objects = objects(&[10; 64]);
        let mut seen = Vec::new();

        fetch_in_order(
            &pool(8, u64::MAX),
            Arc::clone(&objects),
            fetcher(|key| {
                std::thread::sleep(delay(key));
                Ok(body(key))
            }),
            |index, bytes| {
                assert_eq!(bytes, body(&objects[index].0));
                seen.push(index);
                Ok(())
            },
        )
        .expect("fetch");

        assert_eq!(seen, (0..objects.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_failed_get_fails_the_pass_in_order() {
        let objects = objects(&[10; 40]);
        let mut consumed = Vec::new();

        let err = fetch_in_order(
            &pool(16, 32 * 1024 * 1024),
            objects,
            fetcher(|key| {
                std::thread::sleep(delay(key));
                match key {
                    "k0017" => Err(QueryError::Storage {
                        detail: format!("GET {key}"),
                    }),
                    _ => Ok(body(key)),
                }
            }),
            |index, _| {
                consumed.push(index);
                Ok(())
            },
        )
        .expect_err("a failed GET fails the pass");

        assert!(matches!(err, QueryError::Storage { detail } if detail == "GET k0017"));
        assert_eq!(
            consumed,
            (0..17).collect::<Vec<_>>(),
            "everything before it folded"
        );
    }

    #[test]
    fn a_failed_consume_stops_the_pass() {
        let objects = objects(&[10; 200]);
        let fetched = Arc::new(AtomicUsize::new(0));

        let err = fetch_in_order(
            &pool(4, 40),
            Arc::clone(&objects),
            fetcher({
                let fetched = Arc::clone(&fetched);
                move |key| {
                    fetched.fetch_add(1, Ordering::SeqCst);
                    Ok(body(key))
                }
            }),
            |index, _| match index {
                3 => Err(QueryError::Storage {
                    detail: "decode".to_owned(),
                }),
                _ => Ok(()),
            },
        )
        .expect_err("a failed consume fails the pass");

        assert!(matches!(err, QueryError::Storage { .. }));
        assert!(
            fetched.load(Ordering::SeqCst) < objects.len(),
            "dispatch stops after the failure",
        );
    }

    #[test]
    fn a_panicking_consume_propagates_instead_of_hanging() {
        // One object's worth of budget: while the consumer holds index 0,
        // every other worker is parked on the byte budget.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let objects = objects(&[10; 32]);
            let outcome = std::panic::catch_unwind(|| {
                fetch_in_order(
                    &pool(4, 10),
                    objects,
                    fetcher(|key| Ok(body(key))),
                    |_, _| -> Result<(), QueryError> { panic!("consume panics") },
                )
            });
            let _ = tx.send(outcome.is_err());
        });

        let panicked = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("fetch_in_order hung after a consume panic");
        assert!(panicked, "the consume panic propagates");
    }

    /// Spin until `counter` reaches `target` (bounded, so a bug fails the
    /// test instead of hanging it).
    fn await_count(counter: &AtomicUsize, target: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while counter.load(Ordering::SeqCst) < target {
            assert!(
                std::time::Instant::now() < deadline,
                "never reached {target}"
            );
            std::thread::yield_now();
        }
    }

    /// Counts GET starts across a pass whose first object fails, under a
    /// budget of two objects. The failure is held back until both budgeted
    /// GETs (indices 0 and 1) have started, so exactly two start before it;
    /// a third would be one started after the failure.
    fn gets_started_after_first_failure(fail_get: bool) -> usize {
        let objects = objects(&[10; 64]);
        let started = Arc::new(AtomicUsize::new(0));
        let err = fetch_in_order(
            &pool(8, 20),
            objects,
            fetcher({
                let started = Arc::clone(&started);
                move |key| {
                    started.fetch_add(1, Ordering::SeqCst);
                    match key {
                        "k0000" if fail_get => {
                            await_count(&started, 2);
                            Err(QueryError::Storage {
                                detail: "GET".to_owned(),
                            })
                        }
                        _ => Ok(body(key)),
                    }
                }
            }),
            |index, _| match index {
                0 => {
                    await_count(&started, 2);
                    Err(QueryError::Storage {
                        detail: "consume".to_owned(),
                    })
                }
                _ => Ok(()),
            },
        );
        assert!(err.is_err());
        started.load(Ordering::SeqCst)
    }

    /// One worker and an unlimited byte budget: only the producer can stop
    /// dispatch in time. Object 1's GET fails (or panics) while the consumer
    /// is still slow on object 0; the worker must not go on to object 2.
    fn gets_started_after_producer_failure(panic: bool) -> usize {
        let objects = objects(&[10; 64]);
        let started = Arc::new(AtomicUsize::new(0));
        let pass = || {
            fetch_in_order(
                &pool(1, u64::MAX),
                objects,
                fetcher({
                    let started = Arc::clone(&started);
                    move |key| {
                        started.fetch_add(1, Ordering::SeqCst);
                        match key {
                            "k0001" if panic => panic!("GET panics"),
                            "k0001" => Err(QueryError::Storage {
                                detail: "GET".to_owned(),
                            }),
                            _ => Ok(body(key)),
                        }
                    }
                }),
                |_, _| {
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                },
            )
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(pass)) {
            Ok(outcome) => assert!(outcome.is_err()),
            Err(_) => assert!(panic, "only the panicking GET may unwind"),
        }
        started.load(Ordering::SeqCst)
    }

    #[test]
    fn a_failed_fetch_stops_dispatch_before_the_consumer_reaches_it() {
        assert_eq!(
            gets_started_after_producer_failure(false),
            2,
            "after an Err"
        );
        assert_eq!(
            gets_started_after_producer_failure(true),
            2,
            "after a panic"
        );
    }

    #[test]
    fn no_get_starts_after_a_failure() {
        for _ in 0..50 {
            assert_eq!(
                gets_started_after_first_failure(true),
                2,
                "after a failed GET"
            );
            assert_eq!(
                gets_started_after_first_failure(false),
                2,
                "after a failed consume"
            );
        }
    }

    #[test]
    fn a_panicking_get_fails_the_pass_instead_of_hanging() {
        let objects = objects(&[10; 8]);
        let outcome = std::panic::catch_unwind(|| {
            fetch_in_order(
                &pool(16, 32 * 1024 * 1024),
                objects,
                fetcher(|key| {
                    assert_ne!(key, "k0005", "GET panics");
                    Ok(body(key))
                }),
                |_, _| Ok(()),
            )
        });
        // The worker's panic is re-raised once the consumer reaches that
        // object; reaching here at all is the no-hang guarantee.
        assert!(outcome.is_err());
    }

    /// Tracks GETs in flight and listed bytes outstanding (dispatched, not
    /// yet consumed), keeping their high-water marks.
    #[derive(Default)]
    struct Gauge {
        requests: AtomicUsize,
        max_requests: AtomicUsize,
        bytes: AtomicU64,
        max_bytes: AtomicU64,
    }

    /// One fold over `sizes` through `pool`, recording into `gauge`.
    fn gauged_pass(pool: &Arc<FetchPool>, sizes: &[u64], gauge: &Arc<Gauge>) {
        let objects = objects(sizes);
        let fetch = fetcher({
            let (gauge, objects) = (Arc::clone(gauge), Arc::clone(&objects));
            move |key| {
                let size = objects[index_of(key)].1;
                let bytes = gauge.bytes.fetch_add(size, Ordering::SeqCst) + size;
                gauge.max_bytes.fetch_max(bytes, Ordering::SeqCst);
                let requests = gauge.requests.fetch_add(1, Ordering::SeqCst) + 1;
                gauge.max_requests.fetch_max(requests, Ordering::SeqCst);
                std::thread::sleep(delay(key));
                gauge.requests.fetch_sub(1, Ordering::SeqCst);
                Ok(body(key))
            }
        });
        fetch_in_order(pool, Arc::clone(&objects), fetch, |index, _| {
            std::thread::sleep(Duration::from_millis(1));
            gauge.bytes.fetch_sub(objects[index].1, Ordering::SeqCst);
            Ok(())
        })
        .expect("fetch");
    }

    fn bounded_pass(sizes: &[u64], limits: FetchLimits) -> Gauge {
        let gauge = Arc::new(Gauge::default());
        gauged_pass(&FetchPool::for_test(limits), sizes, &gauge);
        Arc::into_inner(gauge).expect("no other gauge holders")
    }

    #[test]
    fn in_flight_requests_and_bytes_stay_within_the_limits() {
        let sizes: Vec<u64> = (0..120).map(|i| 1_000 + (i * 37) % 900).collect();
        let limits = FetchLimits {
            requests: 6,
            bytes: 5_000,
        };

        let gauge = bounded_pass(&sizes, limits);

        let max_requests = gauge.max_requests.load(Ordering::SeqCst);
        let max_bytes = gauge.max_bytes.load(Ordering::SeqCst);
        assert!(
            max_requests <= limits.requests,
            "{max_requests} GETs in flight"
        );
        assert!(max_requests > 1, "the pass ran GETs concurrently");
        assert!(max_bytes <= limits.bytes, "{max_bytes} B outstanding");
    }

    #[test]
    fn an_object_larger_than_the_budget_is_fetched_alone() {
        let sizes = [100, 100, 10_000, 100, 100];
        let limits = FetchLimits {
            requests: 4,
            bytes: 1_000,
        };

        let gauge = bounded_pass(&sizes, limits);

        assert_eq!(gauge.max_bytes.load(Ordering::SeqCst), 10_000);
    }

    /// Run `folds` folds concurrently through one pool, each on its own
    /// thread, failing (not hanging) if they don't all finish in time.
    fn concurrent_folds(pool: &Arc<FetchPool>, folds: &[Vec<u64>], gauge: &Arc<Gauge>) {
        let (tx, rx) = std::sync::mpsc::channel();
        for sizes in folds {
            let (pool, sizes, gauge, tx) = (
                Arc::clone(pool),
                sizes.clone(),
                Arc::clone(gauge),
                tx.clone(),
            );
            std::thread::spawn(move || {
                gauged_pass(&pool, &sizes, &gauge);
                let _ = tx.send(());
            });
        }
        for _ in folds {
            rx.recv_timeout(Duration::from_secs(60))
                .expect("a concurrent fold hung");
        }
    }

    #[test]
    fn concurrent_folds_share_the_process_wide_limits() {
        let limits = FetchLimits {
            requests: 4,
            bytes: 5_000,
        };
        let pool = FetchPool::for_test(limits);
        let gauge = Arc::new(Gauge::default());
        let folds: Vec<Vec<u64>> = (0..3)
            .map(|f| (0..60).map(|i| 1_000 + (i * 37 + f * 101) % 900).collect())
            .collect();

        concurrent_folds(&pool, &folds, &gauge);

        let max_requests = gauge.max_requests.load(Ordering::SeqCst);
        let max_bytes = gauge.max_bytes.load(Ordering::SeqCst);
        assert!(
            max_requests <= limits.requests,
            "{max_requests} GETs in flight across folds"
        );
        assert!(max_requests > 1, "the folds ran GETs concurrently");
        assert!(
            max_bytes <= limits.bytes,
            "{max_bytes} B outstanding across folds"
        );
        assert!(
            pool.workers() <= limits.requests,
            "worker threads stay bounded"
        );
    }

    /// Spin until `pool`'s reservation is exactly `bytes` (bounded).
    fn await_reserved(pool: &FetchPool, bytes: u64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while pool.reserved() != bytes {
            assert!(
                std::time::Instant::now() < deadline,
                "reserved {}",
                pool.reserved()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Fold A, 100 B objects under a 300 B budget: 0, 1 and 2 dispatch.
    /// GET 0 fails once all three have started; GET 1 completes (fetched,
    /// never consumed); GET 2 is held in flight until `released` is set.
    fn fail_with_held_get(
        pool: &Arc<FetchPool>,
        started: &Arc<AtomicUsize>,
        released: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), QueryError> {
        let (started, released) = (Arc::clone(started), Arc::clone(released));
        let fetch = fetcher(move |key| {
            started.fetch_add(1, Ordering::SeqCst);
            match key {
                "k0000" => {
                    await_count(&started, 3);
                    Err(QueryError::Storage {
                        detail: "GET".to_owned(),
                    })
                }
                "k0002" => {
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while !released.load(Ordering::SeqCst) {
                        assert!(std::time::Instant::now() < deadline, "never released");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(body(key))
                }
                _ => Ok(body(key)),
            }
        });
        fetch_in_order(pool, objects(&[100; 10]), fetch, |_, _| Ok(()))
    }

    /// One 8-object fold through `pool`, returning the highest reservation
    /// any of its GETs saw when it started.
    fn max_reserved_during_pass(pool: &Arc<FetchPool>) -> u64 {
        let max_reserved = Arc::new(AtomicU64::new(0));
        let fetch = fetcher({
            let (pool, max_reserved) = (Arc::clone(pool), Arc::clone(&max_reserved));
            move |key| {
                max_reserved.fetch_max(pool.reserved(), Ordering::SeqCst);
                Ok(body(key))
            }
        });
        fetch_in_order(pool, objects(&[100; 8]), fetch, |_, _| Ok(())).expect("fold B");
        max_reserved.load(Ordering::SeqCst)
    }

    /// A failed fold's GETs still in flight stay charged until they finish:
    /// another fold cannot reuse their bytes and push the bodies actually
    /// held past the budget. Its fetched-but-unconsumed bodies are released
    /// at once.
    #[test]
    fn a_departed_folds_in_flight_gets_stay_charged_until_they_finish() {
        let pool = pool(4, 300);
        let started = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let err = fail_with_held_get(&pool, &started, &released);
        assert!(err.is_err());
        assert_eq!(started.load(Ordering::SeqCst), 3);

        // A is gone. GET 1's body (fetched, or published to nothing once it
        // finishes) is released; GET 2, still in flight, stays charged.
        await_reserved(&pool, 100);

        // Fold B runs while GET 2 is held: the outstanding total, B's GETs
        // plus A's detached one, never exceeds the budget.
        let max_reserved = max_reserved_during_pass(&pool);
        assert!(max_reserved <= 300, "{max_reserved} B outstanding");
        assert!(max_reserved >= 200, "B's GETs ran beside A's held one");
        assert_eq!(pool.reserved(), 100, "A's held GET is still charged");

        // Once the held GET finishes, everything is released.
        released.store(true, Ordering::SeqCst);
        await_reserved(&pool, 0);
    }

    /// A pool that cannot start any worker fails the fold with nothing left
    /// registered — no fold, no rotation entry — however often it happens,
    /// and serves the next fold normally once spawning works again.
    #[test]
    fn a_failed_worker_spawn_leaves_nothing_registered() {
        let pool = pool(4, u64::MAX);
        pool.fail_spawns.store(true, Ordering::SeqCst);
        for _ in 0..5 {
            let err = fetch_in_order(
                &pool,
                objects(&[10; 8]),
                fetcher(|key| Ok(body(key))),
                |_, _| Ok(()),
            )
            .expect_err("no worker could start");
            assert!(matches!(err, QueryError::Storage { .. }));
            assert_eq!(pool.registered(), (0, 0), "nothing left registered");
            assert_eq!(pool.workers(), 0);
        }

        pool.fail_spawns.store(false, Ordering::SeqCst);
        let mut seen = Vec::new();
        fetch_in_order(
            &pool,
            objects(&[10; 8]),
            fetcher(|key| Ok(body(key))),
            |index, _| {
                seen.push(index);
                Ok(())
            },
        )
        .expect("a later fold works");
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
        assert_eq!(pool.registered(), (0, 0), "the finished fold deregistered");
    }

    /// A test-built pool's workers exit soon after its last fold, dropping
    /// their hold on the pool, so tests do not accumulate idle threads.
    #[test]
    fn a_test_pools_workers_exit_once_idle() {
        let pool = pool(8, u64::MAX);
        fetch_in_order(
            &pool,
            objects(&[10; 32]),
            fetcher(|key| Ok(body(key))),
            |_, _| Ok(()),
        )
        .expect("fetch");
        assert!(pool.workers() > 0, "the pass started workers");

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while pool.workers() != 0 || Arc::strong_count(&pool) != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "{} workers still alive",
                pool.workers()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn concurrent_folds_with_oversize_objects_all_complete() {
        // Every fold carries objects larger than the whole budget, which are
        // admitted only alone: the folds must take turns, not deadlock or
        // starve one another.
        let pool = FetchPool::for_test(FetchLimits {
            requests: 2,
            bytes: 1_000,
        });
        let gauge = Arc::new(Gauge::default());
        let folds: Vec<Vec<u64>> = (0..4)
            .map(|f| {
                (0..20)
                    .map(|i| if (i + f) % 7 == 0 { 5_000 } else { 300 })
                    .collect()
            })
            .collect();

        concurrent_folds(&pool, &folds, &gauge);

        assert!(gauge.max_requests.load(Ordering::SeqCst) <= 2);
        assert!(gauge.max_bytes.load(Ordering::SeqCst) <= 5_000);
    }
}
