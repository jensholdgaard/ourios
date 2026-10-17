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
use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ourios_core::tenant::TenantId;
use tokio::sync::watch;
use tracing::Instrument as _;

use crate::audit_scan::{AuditListing, ObjectTag};
use crate::template_map::{self, AcquireTrigger, Persisted, TEMPLATE_MAP_FILENAME, TemplateMap};
use crate::{Backend, QueryError, StoreRef, audit_scan, phase};

/// One acquisition's answer: the map plus its template-map acquisition
/// bytes ([`crate::QueryResult::registry_bytes_read`]).
#[cfg(test)]
pub(crate) type Acquired = Result<(Arc<TemplateMap>, u64), QueryError>;

/// One acquisition's answer as its flight publishes it: the map, its
/// acquisition bytes, and what it left in the store.
type Shared = Result<(Arc<TemplateMap>, u64, Persisted), QueryError>;

/// A successful acquisition as one query saw it. A query acquires at most
/// once (RFC 0033): at compile time when the DSL uses `resolves_to` (the
/// alias fold), otherwise lazily while rendering rows, so the alias map and
/// the registry can never come from different frontiers within one query
/// (§3.1's one-artifact rationale). Cloning shares the map; a query's
/// retries reuse its one acquisition rather than acquiring again.
#[derive(Clone)]
pub(crate) struct MapAcquisition {
    pub(crate) map: Arc<TemplateMap>,
    /// The acquisition's bytes ([`crate::QueryResult::registry_bytes_read`]),
    /// the same for the acquirer and every joiner.
    pub(crate) bytes: u64,
    /// The bytes this query fetched itself: `bytes` for the acquirer, `0`
    /// for a joiner, so a shared fold is counted once across its queries.
    pub(crate) fetched: u64,
    /// What the acquisition left in the store, the same for every joiner.
    pub(crate) persisted: Persisted,
}

impl MapAcquisition {
    fn new((map, bytes, persisted): (Arc<TemplateMap>, u64, Persisted), fetched: u64) -> Self {
        Self {
            map,
            bytes,
            fetched,
            persisted,
        }
    }
}

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

/// An acquisition in progress: the frontier it was listed at, who started
/// it, and where its result will be published.
struct Flight {
    frontier: Vec<String>,
    trigger: AcquireTrigger,
    done: watch::Receiver<Option<Shared>>,
}

enum Step<U> {
    Done {
        acquired: Shared,
        verified: Option<Verified>,
    },
    Wait {
        done: watch::Receiver<Option<Shared>>,
        same_frontier: bool,
        trigger: AcquireTrigger,
    },
    /// The listing proved the acquisition unnecessary (RFC 0060 §3.7).
    Unchanged(U),
}

/// How an acquisition ended: with a map, or, for a refresh pass, at a
/// listing that proved it unnecessary.
enum Settled<U> {
    Acquired {
        acquisition: MapAcquisition,
        /// The verified listing this acquisition's own hit established.
        verified: Option<Verified>,
    },
    Unchanged(U),
}

/// Who acquires: the tenant whose map is wanted, what initiated the
/// acquisition (RFC 0060 §3.5), and what it knows before listing.
#[derive(Clone)]
struct Acquirer<P> {
    tenant: TenantId,
    trigger: AcquireTrigger,
    prior: P,
}

/// What an acquisition knows before it lists (RFC 0060 §3.7). A query knows
/// nothing and so always acquires; a refresh pass may hold the tenant's
/// verified listing.
trait Prior: Clone + Send + 'static {
    /// Proof that the listing needs no acquisition.
    type Unchanged: Send + 'static;
    /// Whether a hit should establish a verified listing.
    const LEARNS: bool;

    fn unchanged(&self, listing: &AuditListing<'_>) -> Option<Self::Unchanged>;
}

impl Prior for () {
    type Unchanged = Infallible;
    const LEARNS: bool = false;

    fn unchanged(&self, _listing: &AuditListing<'_>) -> Option<Infallible> {
        None
    }
}

impl Prior for Option<Arc<Verified>> {
    type Unchanged = ();
    const LEARNS: bool = true;

    fn unchanged(&self, listing: &AuditListing<'_>) -> Option<()> {
        self.as_deref()
            .is_some_and(|verified| verified.matches(listing))
            .then_some(())
    }
}

/// A tenant's last verified hit (RFC 0060 §3.7): the frontier it was at, and
/// the artifact tag both its listing and its read saw. A later listing
/// equal to both proves a read would be the same hit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    frontier: Vec<String>,
    tag: ObjectTag,
}

impl Verified {
    /// The verified listing a hit establishes, only when the read's tag is the
    /// listed one: otherwise the artifact changed between the two.
    fn of_hit(
        frontier: Vec<String>,
        listed: Option<ObjectTag>,
        read: Option<&ObjectTag>,
    ) -> Option<Self> {
        let tag = listed?;
        (read == Some(&tag)).then_some(Self { frontier, tag })
    }

    fn matches(&self, listing: &AuditListing<'_>) -> bool {
        listing.artifact.as_ref() == Some(&self.tag) && listing.set.frontier() == self.frontier
    }
}

/// How a refresh pass's acquisition of one tenant ended (RFC 0060 §3.7).
pub(crate) enum Refreshed {
    /// The listing matched the tenant's verified listing: nothing was read.
    Unchanged,
    Acquired {
        persisted: Persisted,
        /// The verified listing to remember, when the acquisition was a hit
        /// whose read tag matched its listing.
        verified: Option<Verified>,
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
    #[cfg(test)]
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        backend: &Backend,
        tenant: &TenantId,
    ) -> Acquired {
        self.acquire_fetched(backend, tenant, AcquireTrigger::Query)
            .await
            .map(|acquired| (acquired.map, acquired.bytes))
    }

    /// [`Self::acquire`], also saying which bytes this query fetched itself:
    /// the acquisition's bytes for the query that ran it, `0` for a joiner,
    /// which was handed the result of another query's fetch.
    ///
    /// RFC 0038 §3.7.2: the acquisition is the query's `load template_map`
    /// span, a query phase whose object-store requests open `CLIENT` spans.
    /// `trigger` says who asks (RFC 0060 §3.5); an acquisition it starts
    /// records it, and one it joins records the starter's.
    ///
    /// # Errors
    ///
    /// As [`Self::acquire`].
    pub(crate) async fn acquire_fetched(
        self: &Arc<Self>,
        backend: &Backend,
        tenant: &TenantId,
        trigger: AcquireTrigger,
    ) -> Result<MapAcquisition, QueryError> {
        let request = Acquirer {
            tenant: tenant.clone(),
            trigger,
            prior: (),
        };
        match self.acquire_in_phase(backend, request).await? {
            Settled::Acquired { acquisition, .. } => Ok(acquisition),
            Settled::Unchanged(never) => match never {},
        }
    }

    /// A refresh pass's acquisition of `tenant` (RFC 0060 §3.7): as
    /// [`Self::acquire_fetched`] with [`AcquireTrigger::Refresh`], except that
    /// a listing matching `verified` ends it before the artifact is read.
    ///
    /// # Errors
    ///
    /// As [`Self::acquire`].
    pub(crate) async fn refresh(
        self: &Arc<Self>,
        backend: &Backend,
        tenant: &TenantId,
        verified: Option<Arc<Verified>>,
    ) -> Result<Refreshed, QueryError> {
        let request = Acquirer {
            tenant: tenant.clone(),
            trigger: AcquireTrigger::Refresh,
            prior: verified,
        };
        Ok(match self.acquire_in_phase(backend, request).await? {
            Settled::Acquired {
                acquisition,
                verified,
            } => Refreshed::Acquired {
                persisted: acquisition.persisted,
                verified,
            },
            Settled::Unchanged(()) => Refreshed::Unchanged,
        })
    }

    /// [`Self::acquire_shared`] inside its `load template_map` phase span,
    /// which records an error the acquisition ends with.
    async fn acquire_in_phase<P: Prior>(
        self: &Arc<Self>,
        backend: &Backend,
        request: Acquirer<P>,
    ) -> Result<Settled<P::Unchanged>, QueryError> {
        let phase = tracing::info_span!(
            "load template_map",
            otel.kind = "internal",
            ourios.tenant = request.tenant.as_str(),
            ourios.template_map.lookup.outcome = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let settled = self
            .acquire_shared(backend, request, &phase)
            .instrument(phase.clone())
            .await;
        if let Err(e) = &settled {
            ourios_core::telemetry::record_span_error(&phase, template_map::error_type(e));
        }
        settled
    }

    /// [`Self::acquire_fetched`]'s single-flight loop, inside `phase`.
    async fn acquire_shared<P: Prior>(
        self: &Arc<Self>,
        backend: &Backend,
        request: Acquirer<P>,
        phase: &tracing::Span,
    ) -> Result<Settled<P::Unchanged>, QueryError> {
        let mut waited = Duration::ZERO;
        loop {
            match self.blocking_step(backend, &request, phase).await? {
                Step::Done { acquired, verified } => {
                    return acquired.map(|shared| {
                        let bytes = shared.1;
                        Settled::Acquired {
                            acquisition: MapAcquisition::new(shared, bytes),
                            verified,
                        }
                    });
                }
                Step::Unchanged(unchanged) => return Ok(Settled::Unchanged(unchanged)),
                Step::Wait {
                    done,
                    same_frontier,
                    trigger,
                } => {
                    let outcome = Self::wait(done, &mut waited).await;
                    if !same_frontier {
                        continue;
                    }
                    let joined = Self::join(outcome, waited, trigger);
                    #[cfg(any(test, feature = "testing"))]
                    self.hooks.joined(&joined);
                    return joined.map(|acquisition| Settled::Acquired {
                        acquisition,
                        verified: None,
                    });
                }
            }
        }
    }

    /// One [`Self::step`] on the blocking pool, inside `phase`.
    async fn blocking_step<P: Prior>(
        self: &Arc<Self>,
        backend: &Backend,
        request: &Acquirer<P>,
        phase: &tracing::Span,
    ) -> Result<Step<P::Unchanged>, QueryError> {
        // The phase span itself, never `Span::current()`: when the
        // subscriber filters the phase out, the current span is the
        // query, which must not become the phase (`crate::phase`).
        let span = phase.clone();
        // The caller's subscriber too, not just its span: the fold opens
        // its own `derive template_map` span on the blocking thread,
        // which otherwise resolves only the global default.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let flights = Arc::clone(self);
        let backend = backend
            .clone()
            .with_caller(ourios_parquet::Caller::TemplateMap);
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                phase::in_phase(&span, || flights.step(backend.store_ref(), &request))
            })
        })
        .await
        .map_err(|e| QueryError::Storage {
            detail: format!("blocking storage task: {e}"),
        })?
    }

    /// Wait for a running flight's result, adding the wait to `waited`.
    async fn wait(
        mut done: watch::Receiver<Option<Shared>>,
        waited: &mut Duration,
    ) -> Option<Shared> {
        let started = Instant::now();
        // The flight's guard always publishes before dropping its sender, so
        // a closed channel without a value is unreachable; it is still an
        // error, never a hang.
        let outcome = async {
            match done.wait_for(Option::is_some).await {
                Ok(value) => (*value).clone(),
                Err(_) => None,
            }
        }
        .instrument(tracing::info_span!(
            "wait template_map",
            otel.kind = "internal"
        ))
        .await;
        *waited += started.elapsed();
        outcome
    }

    /// A joiner's answer: the shared acquisition's, with none of its bytes.
    fn join(
        outcome: Option<Shared>,
        waited: Duration,
        trigger: AcquireTrigger,
    ) -> Result<MapAcquisition, QueryError> {
        let acquired = outcome.unwrap_or_else(|| Err(abandoned()));
        // A joiner's wait is recorded whatever the shared acquisition
        // returned. The lookup outcome is not: the acquiring query recorded
        // its own, and a failed acquisition records none for anyone.
        template_map::record_acquire_wait(waited);
        if acquired.is_ok() {
            template_map::record_joined_lookup(trigger);
        }
        acquired.map(|shared| MapAcquisition::new(shared, 0))
    }

    /// One blocking pass: LIST, then stop if the listing proves the
    /// acquisition unnecessary, or join the running flight, or become the
    /// flight and acquire against this listing.
    fn step<P: Prior>(
        &self,
        backend: StoreRef<'_>,
        request: &Acquirer<P>,
    ) -> Result<Step<P::Unchanged>, QueryError> {
        let tenant = &request.tenant;
        let listing = audit_scan::list_audit_stream(backend, tenant, TEMPLATE_MAP_FILENAME)?;
        #[cfg(any(test, feature = "testing"))]
        self.hooks.after_listing();
        if let Some(unchanged) = request.prior.unchanged(&listing) {
            return Ok(Step::Unchanged(unchanged));
        }
        let AuditListing {
            set: resolved,
            artifact,
        } = listing;
        let guard = {
            let mut flights = self.lock();
            if let Some(flight) = flights.get(tenant) {
                #[cfg(any(test, feature = "testing"))]
                self.hooks.waited();
                return Ok(Step::Wait {
                    done: flight.done.clone(),
                    same_frontier: flight.frontier == resolved.frontier(),
                    trigger: flight.trigger,
                });
            }
            let (tx, done) = watch::channel(None);
            flights.insert(
                tenant.clone(),
                Flight {
                    frontier: resolved.frontier().to_vec(),
                    trigger: request.trigger,
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
        {
            let in_flight = self.lock().len();
            self.hooks.before_acquire(tenant, in_flight);
        }
        let frontier = P::LEARNS.then(|| resolved.frontier().to_vec());
        let loaded =
            template_map::load_or_derive_resolved(backend, tenant, resolved, request.trigger);
        let verified = match (&loaded, frontier) {
            (Ok(loaded), Some(frontier)) => {
                Verified::of_hit(frontier, artifact, loaded.hit_tag.as_ref())
            }
            _ => None,
        };
        let acquired = loaded.map(|loaded| (Arc::new(loaded.map), loaded.bytes, loaded.persisted));
        guard.finish(acquired.clone());
        Ok(Step::Done { acquired, verified })
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
    tx: Option<watch::Sender<Option<Shared>>>,
}

impl FlightGuard<'_> {
    fn finish(mut self, acquired: Shared) {
        self.complete(acquired);
    }

    fn complete(&mut self, acquired: Shared) {
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
/// feature): acquisitions started, the tenants they were for and how many
/// flights were in flight as each started, acquisitions that waited on a
/// flight, `joined` lookups recorded, and a gate the acquiring thread runs
/// just before it acquires.
#[cfg(any(test, feature = "testing"))]
pub(crate) mod hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use ourios_core::tenant::TenantId;

    pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

    #[derive(Default)]
    pub(crate) struct Hooks {
        pub(crate) acquisitions: AtomicUsize,
        pub(crate) started: Mutex<Vec<(TenantId, usize)>>,
        pub(crate) waits: AtomicUsize,
        pub(crate) joins: AtomicUsize,
        pub(crate) before_acquire: Mutex<Option<Hook>>,
        pub(crate) after_listing: Mutex<Option<Hook>>,
    }

    impl Hooks {
        pub(crate) fn waited(&self) {
            self.waits.fetch_add(1, Ordering::SeqCst);
        }

        /// Count a join that answered; a failed shared acquisition is none.
        pub(crate) fn joined<T, E>(&self, joined: &Result<T, E>) {
            if joined.is_ok() {
                self.joins.fetch_add(1, Ordering::SeqCst);
            }
        }

        pub(crate) fn after_listing(&self) {
            let hook = self
                .after_listing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook();
            }
        }

        pub(crate) fn before_acquire(&self, tenant: &TenantId, in_flight: usize) {
            self.started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((tenant.clone(), in_flight));
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
