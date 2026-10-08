//! Per-tenant single-flight over the RFC 0033 template-map acquisition
//! (#853).
//!
//! Every query still LISTs the tenant's audit prefix and acquires the map
//! through [`template_map::load_or_derive_resolved`]: an artifact GET, or on
//! a miss the fold and write-through. What this adds is that at most one
//! acquisition runs per tenant at a time. A query whose listed frontier
//! equals the running acquisition's frontier waits for its result, success
//! or error alike, and reports its bytes; a query whose frontier differs
//! waits for it to finish and then lists again, so it is never handed a map
//! of another frontier.
//!
//! The acquisition runs to completion on the blocking pool even when the
//! query that started it is dropped (a client timeout); a retry arriving
//! meanwhile joins it instead of starting another fold. Nothing outlives a
//! flight: a tenant's entry exists only while its acquisition runs, and is
//! removed when it completes — successfully, with an error, or by a panic.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ourios_core::tenant::TenantId;
use tokio::sync::watch;

use crate::template_map::{self, TemplateMap};
use crate::{Backend, QueryError, StoreRef, audit_scan};

/// One acquisition's answer: the map plus its template-map acquisition
/// bytes ([`crate::QueryResult::registry_bytes_read`]).
pub(crate) type Acquired = Result<(Arc<TemplateMap>, u64), QueryError>;

/// The in-flight acquisitions, by tenant. Shared by every clone of a
/// [`crate::Querier`].
#[derive(Default)]
pub(crate) struct TemplateMapFlights {
    flights: Mutex<HashMap<TenantId, Flight>>,
    #[cfg(any(test, feature = "testing"))]
    pub(crate) hooks: hooks::Hooks,
}

impl fmt::Debug for TemplateMapFlights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TemplateMapFlights")
            .field("in_flight", &self.lock().len())
            .finish()
    }
}

/// An acquisition in progress: the frontier it was listed at and where its
/// result will be published.
struct Flight {
    frontier: Vec<String>,
    done: watch::Receiver<Option<Acquired>>,
}

enum Step {
    Done(Acquired),
    Wait {
        done: watch::Receiver<Option<Acquired>>,
        same_frontier: bool,
    },
}

impl TemplateMapFlights {
    /// Acquire `tenant`'s [`TemplateMap`] at its current audit frontier,
    /// joining an in-flight acquisition of the same frontier if there is
    /// one. The blocking IO runs on the tokio blocking pool with the
    /// caller's span re-entered (RFC 0038 §3.3).
    ///
    /// # Errors
    ///
    /// The listing's or the acquisition's [`QueryError`] — a joined
    /// acquisition's error is every joiner's error.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        backend: &Backend,
        tenant: &TenantId,
    ) -> Acquired {
        loop {
            let span = tracing::Span::current();
            let flights = Arc::clone(self);
            let backend = backend.clone();
            let owned = tenant.clone();
            let step = tokio::task::spawn_blocking(move || {
                span.in_scope(|| flights.step(backend.store_ref(), &owned))
            })
            .await
            .map_err(|e| QueryError::Storage {
                detail: format!("blocking storage task: {e}"),
            })??;
            match step {
                Step::Done(acquired) => return acquired,
                Step::Wait {
                    mut done,
                    same_frontier,
                } => {
                    // The flight's guard always publishes before dropping its
                    // sender, so a closed channel without a value is
                    // unreachable; it is still an error, never a hang.
                    let outcome = match done.wait_for(Option::is_some).await {
                        Ok(value) => (*value).clone(),
                        Err(_) => None,
                    };
                    if same_frontier {
                        let acquired = outcome.unwrap_or_else(|| Err(abandoned()));
                        // The acquiring query recorded its own outcome; a
                        // failed acquisition records nothing for anyone.
                        if acquired.is_ok() {
                            template_map::record_joined_lookup();
                            #[cfg(any(test, feature = "testing"))]
                            self.hooks.joined();
                        }
                        return acquired;
                    }
                }
            }
        }
    }

    /// One blocking pass: LIST, then join the running flight or become the
    /// flight and acquire against this listing.
    fn step(&self, backend: StoreRef<'_>, tenant: &TenantId) -> Result<Step, QueryError> {
        let resolved = audit_scan::resolve_audit_set(backend, tenant)?;
        let guard = {
            let mut flights = self.lock();
            if let Some(flight) = flights.get(tenant) {
                #[cfg(any(test, feature = "testing"))]
                self.hooks.waited();
                return Ok(Step::Wait {
                    done: flight.done.clone(),
                    same_frontier: flight.frontier == resolved.frontier(),
                });
            }
            let (tx, done) = watch::channel(None);
            flights.insert(
                tenant.clone(),
                Flight {
                    frontier: resolved.frontier().to_vec(),
                    done,
                },
            );
            FlightGuard {
                flights: self,
                tenant,
                tx: Some(tx),
            }
        };
        #[cfg(any(test, feature = "testing"))]
        self.hooks.before_acquire();
        let acquired = template_map::load_or_derive_resolved(backend, tenant, resolved)
            .map(|(map, bytes, _)| (Arc::new(map), bytes));
        guard.finish(acquired.clone());
        Ok(Step::Done(acquired))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<TenantId, Flight>> {
        self.flights.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn abandoned() -> QueryError {
    QueryError::Storage {
        detail: "template-map acquisition ended without a result".to_owned(),
    }
}

/// Completes a [`Flight`] exactly once: on [`Self::finish`], or on drop
/// (a panicking acquisition unwinds through here), so waiters are never
/// left on a flight nobody will finish.
struct FlightGuard<'a> {
    flights: &'a TemplateMapFlights,
    tenant: &'a TenantId,
    tx: Option<watch::Sender<Option<Acquired>>>,
}

impl FlightGuard<'_> {
    fn finish(mut self, acquired: Acquired) {
        self.complete(acquired);
    }

    fn complete(&mut self, acquired: Acquired) {
        let Some(tx) = self.tx.take() else {
            return;
        };
        self.flights.lock().remove(self.tenant);
        tx.send_replace(Some(acquired));
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.complete(Err(abandoned()));
    }
}

/// Test seams (unit tests, and integration tests through the `testing`
/// feature): acquisitions started, acquisitions that waited on a flight,
/// `joined` lookups recorded, and a gate the acquiring thread runs just
/// before it acquires.
#[cfg(any(test, feature = "testing"))]
pub(crate) mod hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

    #[derive(Default)]
    pub(crate) struct Hooks {
        pub(crate) acquisitions: AtomicUsize,
        pub(crate) waits: AtomicUsize,
        pub(crate) joins: AtomicUsize,
        pub(crate) before_acquire: Mutex<Option<Hook>>,
    }

    impl Hooks {
        pub(crate) fn waited(&self) {
            self.waits.fetch_add(1, Ordering::SeqCst);
        }

        pub(crate) fn joined(&self) {
            self.joins.fetch_add(1, Ordering::SeqCst);
        }

        pub(crate) fn before_acquire(&self) {
            self.acquisitions.fetch_add(1, Ordering::SeqCst);
            let hook = self
                .before_acquire
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use ourios_core::audit::{
        AuditEvent, AuditPayload, AuditSink as _, TemplateChange, hash_triggering_line,
    };

    use super::*;

    use super::hooks::Hook;

    const TENANT: &str = "acme";
    /// 2026-04-02T10:58:00 UTC.
    const TS0: u64 = 1_775_127_480_000_000_000;
    const DAY_NS: u64 = 24 * 3_600 * 1_000_000_000;
    const N: usize = 8;

    fn adopted(template_id: u64, ts_ns: u64) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(TENANT),
            timestamp: std::time::UNIX_EPOCH + Duration::from_nanos(ts_ns),
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: hash_triggering_line(b"line"),
                triggering_line_sample: None,
                change: TemplateChange::Adopted {
                    template_version: 1,
                    new_template: "user <*>".to_string(),
                },
            },
        }
    }

    fn write_audit(bucket: &Path, events: &[AuditEvent]) {
        let store = ourios_parquet::Store::local(bucket).expect("store");
        let mut sink = ourios_parquet::ParquetAuditSink::new(store);
        for event in events {
            sink.emit(event.clone());
        }
        assert_eq!(sink.write_failures(), 0);
    }

    fn seeded() -> tempfile::TempDir {
        let bucket = tempfile::tempdir().expect("temp");
        write_audit(bucket.path(), &[adopted(1, TS0), adopted(2, TS0 - DAY_NS)]);
        bucket
    }

    fn tenant() -> TenantId {
        TenantId::new(TENANT)
    }

    /// Spin until `counter` reaches `target` (bounded, so a bug fails the
    /// test instead of hanging it).
    fn await_count(counter: &AtomicUsize, target: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while counter.load(Ordering::SeqCst) < target {
            assert!(Instant::now() < deadline, "never reached {target}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn set_hook(flights: &TemplateMapFlights, hook: Option<Hook>) {
        *flights
            .hooks
            .before_acquire
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = hook;
    }

    /// Hold the acquiring thread until `waiters` other acquisitions are
    /// parked on its flight, then run `then`.
    fn gate_until_waiting(
        flights: &Arc<TemplateMapFlights>,
        waiters: usize,
        then: impl Fn() + Send + Sync + 'static,
    ) {
        let weak = Arc::downgrade(flights);
        set_hook(
            flights,
            Some(Arc::new(move || {
                if let Some(flights) = weak.upgrade() {
                    await_count(&flights.hooks.waits, waiters);
                }
                then();
            })),
        );
    }

    async fn acquire_concurrently(
        flights: &Arc<TemplateMapFlights>,
        backend: &Backend,
    ) -> Vec<Acquired> {
        let tasks: Vec<_> = (0..N)
            .map(|_| {
                let flights = Arc::clone(flights);
                let backend = backend.clone();
                tokio::spawn(async move { flights.acquire(&backend, &tenant()).await })
            })
            .collect();
        let mut results = Vec::with_capacity(N);
        for task in tasks {
            results.push(task.await.expect("acquire task"));
        }
        results
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquisitions_share_one_acquisition() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let flights = Arc::new(TemplateMapFlights::default());
        gate_until_waiting(&flights, N - 1, || {});

        let results = acquire_concurrently(&flights, &backend).await;

        assert_eq!(
            count(&flights.hooks.acquisitions),
            1,
            "one fold for {N} queries"
        );
        assert_eq!(
            count(&flights.hooks.joins),
            N - 1,
            "every waiter is a `joined` lookup"
        );
        let acquired: Vec<_> = results
            .into_iter()
            .map(|r| r.expect("every waiter gets the map"))
            .collect();
        let (first, bytes) = &acquired[0];
        assert!(*bytes > 0, "the cold acquisition folded the audit stream");
        for (map, joined_bytes) in &acquired {
            assert!(Arc::ptr_eq(map, first), "every waiter shares the one map");
            assert_eq!(
                joined_bytes, bytes,
                "joiners report the acquisition's bytes"
            );
        }
        let expected = template_map::derive_template_map(backend.store_ref(), &tenant())
            .expect("fresh fold")
            .0;
        assert_eq!(first.folded_files(), expected.folded_files());
        assert_eq!(first.registry(), expected.registry());
        assert!(flights.lock().is_empty(), "no entry outlives its flight");
    }

    /// The RFC 0033 §3.3 / RFC0033.6 path is unchanged outside a flight:
    /// the next query acquires again, through the artifact the first one
    /// published, and reports its exact size.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_later_query_reads_the_artifact_again() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let flights = Arc::new(TemplateMapFlights::default());

        flights.acquire(&backend, &tenant()).await.expect("cold");
        let (_, warm_bytes) = flights.acquire(&backend, &tenant()).await.expect("warm");

        let artifact = bucket
            .path()
            .join("audit")
            .join(format!("tenant_id={TENANT}"))
            .join(template_map::TEMPLATE_MAP_FILENAME);
        let artifact_len = std::fs::metadata(artifact).expect("published").len();
        assert_eq!(warm_bytes, artifact_len);
        assert_eq!(count(&flights.hooks.acquisitions), 2);
        assert_eq!(count(&flights.hooks.joins), 0);
    }

    /// A query whose listing differs from the running flight's waits for it,
    /// lists again, and acquires at its own frontier — never the other's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_different_frontier_waits_then_acquires_its_own() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let flights = Arc::new(TemplateMapFlights::default());
        let first_done = Arc::new(AtomicBool::new(false));
        gate_until_waiting(&flights, 1, {
            let first_done = Arc::clone(&first_done);
            move || first_done.store(true, Ordering::SeqCst)
        });

        let first = tokio::spawn({
            let (flights, backend) = (Arc::clone(&flights), backend.clone());
            async move { flights.acquire(&backend, &tenant()).await }
        });
        await_count(&flights.hooks.acquisitions, 1);
        set_hook(&flights, None);
        write_audit(bucket.path(), &[adopted(3, TS0 + DAY_NS)]);
        let second = flights.acquire(&backend, &tenant()).await.expect("second");
        let first = first.await.expect("task").expect("first");

        assert!(first_done.load(Ordering::SeqCst));
        assert_eq!(count(&flights.hooks.acquisitions), 2);
        assert_eq!(
            count(&flights.hooks.joins),
            0,
            "a mismatched waiter is not a join"
        );
        assert!(second.0.folded_files().len() > first.0.folded_files().len());
        assert!(second.0.registry().contains_key(&(3, 1)));
        assert!(!first.0.registry().contains_key(&(3, 1)));
    }

    /// Dropping the query that started a flight (a client timeout) does not
    /// cancel the acquisition: a same-frontier query arriving meanwhile
    /// joins it, and no second fold runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dropped_starter_leaves_its_flight_to_be_joined() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let flights = Arc::new(TemplateMapFlights::default());
        let released = Arc::new(AtomicBool::new(false));
        set_hook(
            &flights,
            Some(Arc::new({
                let released = Arc::clone(&released);
                move || {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while !released.load(Ordering::SeqCst) {
                        assert!(Instant::now() < deadline, "never released");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            })),
        );

        let starter = tokio::spawn({
            let (flights, backend) = (Arc::clone(&flights), backend.clone());
            async move { flights.acquire(&backend, &tenant()).await }
        });
        await_count(&flights.hooks.acquisitions, 1);
        starter.abort();
        assert!(starter.await.expect_err("aborted").is_cancelled());

        let joiner = tokio::spawn({
            let (flights, backend) = (Arc::clone(&flights), backend.clone());
            async move { flights.acquire(&backend, &tenant()).await }
        });
        await_count(&flights.hooks.waits, 1);
        released.store(true, Ordering::SeqCst);
        let (map, bytes) = joiner.await.expect("task").expect("joined");

        assert_eq!(count(&flights.hooks.acquisitions), 1, "no second fold");
        assert_eq!(
            count(&flights.hooks.joins),
            1,
            "the joiner records `joined`"
        );
        assert!(bytes > 0, "the joiner reports the acquisition's bytes");
        assert_eq!(map.registry().len(), 2);
        assert!(
            flights.lock().is_empty(),
            "the flight completed and cleared"
        );
    }

    /// Plant a non-Parquet `*.parquet` beside a real audit file: the fold
    /// fails reading it, as any unreadable audit object does.
    fn plant_unreadable(bucket: &Path) -> std::path::PathBuf {
        let mut dir = bucket.join("audit").join(format!("tenant_id={TENANT}"));
        for _ in 0..3 {
            dir = std::fs::read_dir(&dir)
                .expect("read partition dir")
                .map(|entry| entry.expect("dir entry").path())
                .find(|path| path.is_dir())
                .expect("partition subdir");
        }
        let bad = dir.join("zz-unreadable.parquet");
        std::fs::write(&bad, b"not parquet").expect("plant unreadable audit file");
        bad
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_acquisition_reaches_every_waiter_then_retries() {
        let bucket = seeded();
        let bad = plant_unreadable(bucket.path());
        let backend = Backend::Local(bucket.path().to_path_buf());
        let flights = Arc::new(TemplateMapFlights::default());
        gate_until_waiting(&flights, N - 1, || {});

        let results = acquire_concurrently(&flights, &backend).await;

        assert_eq!(count(&flights.hooks.acquisitions), 1);
        for result in &results {
            assert!(
                matches!(result, Err(QueryError::Storage { .. })),
                "every waiter gets the acquisition's error: {result:?}",
            );
        }
        assert_eq!(
            count(&flights.hooks.joins),
            0,
            "a failure records no `joined`"
        );
        assert!(flights.lock().is_empty());
        set_hook(&flights, None);
        assert!(flights.acquire(&backend, &tenant()).await.is_err());
        assert_eq!(
            count(&flights.hooks.acquisitions),
            2,
            "the next query retries"
        );

        std::fs::remove_file(&bad).expect("remove unreadable file");
        flights
            .acquire(&backend, &tenant())
            .await
            .expect("recovers once the store is readable");
        assert_eq!(count(&flights.hooks.acquisitions), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panicked_acquisition_does_not_wedge_waiters() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let flights = Arc::new(TemplateMapFlights::default());
        let armed = Arc::new(AtomicBool::new(true));
        gate_until_waiting(&flights, N - 1, {
            let armed = Arc::clone(&armed);
            move || assert!(!armed.swap(false, Ordering::SeqCst), "acquisition panics")
        });

        let results = acquire_concurrently(&flights, &backend).await;

        assert_eq!(count(&flights.hooks.acquisitions), 1);
        assert!(
            results.iter().all(Result::is_err),
            "the panicked acquisition's starter and every joiner get an error",
        );
        assert_eq!(count(&flights.hooks.joins), 0);
        assert!(flights.lock().is_empty());
        set_hook(&flights, None);
        flights
            .acquire(&backend, &tenant())
            .await
            .expect("the next query acquires afresh");
        assert_eq!(count(&flights.hooks.acquisitions), 2);
    }
}
