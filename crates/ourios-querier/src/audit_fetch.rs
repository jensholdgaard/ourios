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

/// How long an idle worker waits for work before it exits.
const IDLE: Duration = Duration::from_secs(60);

static SHARED: LazyLock<Arc<FetchPool>> = LazyLock::new(|| FetchPool::new(FetchLimits::default()));

/// The process-wide worker pool and byte budget every fold fetches through.
pub(crate) struct FetchPool {
    limits: FetchLimits,
    state: Mutex<PoolState>,
    changed: Condvar,
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
    /// This fold's share of [`PoolState::reserved`].
    reserved: u64,
    ready: BTreeMap<usize, Outcome>,
    /// A fetch or the consumer failed: dispatch nothing more.
    stopped: bool,
}

struct Job {
    fold: u64,
    index: usize,
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
                key: key.clone(),
                fetch: Arc::clone(&fold.fetch),
                span: fold.span.clone(),
            };
            fold.next += 1;
            fold.reserved = fold.reserved.saturating_add(*size);
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
    fn publish(&mut self, fold: u64, index: usize, outcome: Outcome) {
        if let Some(fold) = self.folds.get_mut(&fold) {
            if !matches!(outcome, Ok(Ok(_))) {
                fold.stopped = true;
            }
            fold.ready.insert(index, outcome);
        }
    }
}

impl FetchPool {
    pub(crate) fn new(limits: FetchLimits) -> Arc<Self> {
        Arc::new(Self {
            limits: FetchLimits {
                requests: limits.requests.max(1),
                bytes: limits.bytes,
            },
            state: Mutex::new(PoolState::default()),
            changed: Condvar::new(),
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

    fn register(
        self: &Arc<Self>,
        objects: Arc<[(String, u64)]>,
        fetch: Fetch,
    ) -> Result<u64, QueryError> {
        let mut state = self.lock();
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
                ready: BTreeMap::new(),
                stopped: false,
            },
        );
        state.rotation.push_back(id);
        while state.workers < self.limits.requests {
            let pool = Arc::clone(self);
            match std::thread::Builder::new()
                .name("ourios-audit-fetch".to_owned())
                .spawn(move || pool.work())
            {
                Ok(_) => state.workers += 1,
                Err(e) if state.workers == 0 => {
                    state.folds.remove(&id);
                    return Err(QueryError::Storage {
                        detail: format!("start audit fetch worker: {e}"),
                    });
                }
                // Fewer workers only lowers the concurrency; one is enough
                // to make progress.
                Err(_) => break,
            }
        }
        drop(state);
        self.changed.notify_all();
        Ok(id)
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
                state.publish(job.fold, job.index, outcome);
                self.changed.notify_all();
                continue;
            }
            let (next, idle) = self
                .changed
                .wait_timeout(state, IDLE)
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

    /// Remove a fold — normally or while unwinding — returning its
    /// outstanding reservation to the budget. Its in-flight GETs finish and
    /// publish into nothing.
    fn deregister(&self, fold: u64) {
        let mut state = self.lock();
        if let Some(window) = state.folds.remove(&fold) {
            state.reserved = state.reserved.saturating_sub(window.reserved);
        }
        drop(state);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn workers(&self) -> usize {
        self.lock().workers
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
        FetchPool::new(FetchLimits { requests, bytes })
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
        gauged_pass(&FetchPool::new(limits), sizes, &gauge);
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
        let pool = FetchPool::new(limits);
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

    #[test]
    fn concurrent_folds_with_oversize_objects_all_complete() {
        // Every fold carries objects larger than the whole budget, which are
        // admitted only alone: the folds must take turns, not deadlock or
        // starve one another.
        let pool = FetchPool::new(FetchLimits {
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
