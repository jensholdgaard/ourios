//! The querier's in-process layer over the RFC 0033 template-map read path
//! (#853): a per-tenant single-flight around the derivation, and the last
//! derived map held in memory, keyed by its audit frontier.
//!
//! Every acquisition still LISTs the tenant's audit prefix — the §3.3
//! freshness check is unchanged; this layer saves what comes after it. A
//! held map whose `folded_files` equals the listing is served without a
//! single GET: it is the fold of exactly that frontier, the same answer an
//! artifact hit at that listing gives. Otherwise at most one derivation
//! ([`template_map::load_or_derive_resolved`] — artifact GET, then the fold
//! and write-through on a miss) runs per tenant at a time. A query whose
//! listing equals the running derivation's waits for its result, success
//! or error alike; a query whose listing differs waits for it to finish and
//! then lists again, so it is never handed a map of another frontier.
//!
//! The derivation runs to completion on the blocking pool even when the
//! query that started it is dropped (a client timeout), and its result
//! lands here, so the retry hits or joins it instead of starting another
//! full fold. A derivation that fails or panics clears its flight and
//! hands its error to every waiter; the next query derives afresh.
//!
//! Memory: one [`TemplateMap`] per tenant this querier has rendered or
//! alias-resolved for — the latest derived frontier only, replaced by the
//! next derivation (an in-use map lives on until its queries finish). A map
//! is the decoded content of the tenant's published artifact: the registry
//! (bounded by the RFC 0023 per-tenant template cap), the alias classes, and
//! one frontier entry per audit file — kilobytes to low megabytes per
//! tenant (RFC 0033 §3.2's sizing).

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

/// The per-querier cache. Shared by every clone of a [`crate::Querier`].
#[derive(Default)]
pub(crate) struct TemplateMapCache {
    tenants: Mutex<HashMap<TenantId, Slot>>,
    #[cfg(test)]
    hooks: tests::Hooks,
}

impl fmt::Debug for TemplateMapCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TemplateMapCache")
            .field("tenants", &self.lock().len())
            .finish()
    }
}

#[derive(Default)]
struct Slot {
    held: Option<Arc<TemplateMap>>,
    flight: Option<Flight>,
}

/// A derivation in progress: the frontier it folds and where its result
/// will be published.
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

impl TemplateMapCache {
    /// Resolve `tenant`'s [`TemplateMap`] at its current audit frontier:
    /// from memory, by joining an in-flight derivation of the same
    /// frontier, or by deriving it. The blocking IO runs on the tokio
    /// blocking pool with the caller's span re-entered (RFC 0038 §3.3).
    ///
    /// # Errors
    ///
    /// The listing's or the derivation's [`QueryError`] — a joined
    /// derivation's error is every joiner's error.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        backend: &Backend,
        tenant: &TenantId,
    ) -> Acquired {
        loop {
            let span = tracing::Span::current();
            let cache = Arc::clone(self);
            let backend = backend.clone();
            let owned = tenant.clone();
            let step = tokio::task::spawn_blocking(move || {
                span.in_scope(|| cache.step(backend.store_ref(), &owned))
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
                        return outcome.unwrap_or_else(|| Err(abandoned()));
                    }
                }
            }
        }
    }

    /// One blocking pass: LIST, then serve from memory, join the running
    /// flight, or become the flight and derive against this listing.
    fn step(&self, backend: StoreRef<'_>, tenant: &TenantId) -> Result<Step, QueryError> {
        let resolved = audit_scan::resolve_audit_set(backend, tenant)?;
        let flight = {
            let mut tenants = self.lock();
            let slot = tenants.entry(tenant.clone()).or_default();
            match (&slot.held, &slot.flight) {
                (Some(held), _) if held.folded_files() == resolved.frontier() => {
                    return Ok(Step::Done(Ok((Arc::clone(held), 0))));
                }
                (_, Some(flight)) => {
                    #[cfg(test)]
                    self.hooks.waited();
                    return Ok(Step::Wait {
                        done: flight.done.clone(),
                        same_frontier: flight.frontier == resolved.frontier(),
                    });
                }
                (_, None) => {
                    let (tx, done) = watch::channel(None);
                    slot.flight = Some(Flight {
                        frontier: resolved.frontier().to_vec(),
                        done,
                    });
                    FlightGuard {
                        cache: self,
                        tenant,
                        tx: Some(tx),
                    }
                }
            }
        };
        #[cfg(test)]
        self.hooks.before_derive();
        let acquired = template_map::load_or_derive_resolved(backend, tenant, resolved)
            .map(|(map, bytes, _)| (Arc::new(map), bytes));
        flight.finish(acquired.clone());
        Ok(Step::Done(acquired))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<TenantId, Slot>> {
        self.tenants.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn abandoned() -> QueryError {
    QueryError::Storage {
        detail: "template-map derivation ended without a result".to_owned(),
    }
}

/// Completes a [`Flight`] exactly once: on [`Self::finish`], or on drop
/// (a panicking derivation unwinds through here), so waiters are never
/// left on a flight nobody will finish.
struct FlightGuard<'a> {
    cache: &'a TemplateMapCache,
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
        let mut tenants = self.cache.lock();
        let slot = tenants.entry(self.tenant.clone()).or_default();
        slot.flight = None;
        if let Ok((map, _)) = &acquired {
            slot.held = Some(Arc::clone(map));
        }
        tx.send_replace(Some(acquired));
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.complete(Err(abandoned()));
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

    type Hook = Arc<dyn Fn() + Send + Sync>;

    /// Test-only instrumentation: how many derivations started, how many
    /// acquisitions waited on a flight, and a callback run by the deriving
    /// thread just before it derives.
    #[derive(Default)]
    pub(super) struct Hooks {
        derives: AtomicUsize,
        waits: AtomicUsize,
        before_derive: Mutex<Option<Hook>>,
    }

    impl Hooks {
        pub(super) fn waited(&self) {
            self.waits.fetch_add(1, Ordering::SeqCst);
        }

        pub(super) fn before_derive(&self) {
            self.derives.fetch_add(1, Ordering::SeqCst);
            let hook = self
                .before_derive
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

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

    /// Hold the deriving thread until every other acquisition is parked on
    /// its flight, so the test observes real joins rather than late
    /// memory hits.
    fn gate_until_joined(cache: &Arc<TemplateMapCache>, then: impl Fn() + Send + Sync + 'static) {
        let weak = Arc::downgrade(cache);
        let hook: Hook = Arc::new(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while let Some(cache) = weak.upgrade()
                && cache.hooks.waits.load(Ordering::SeqCst) < N - 1
            {
                assert!(Instant::now() < deadline, "joiners never arrived");
                std::thread::sleep(Duration::from_millis(5));
            }
            then();
        });
        *cache
            .hooks
            .before_derive
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    fn clear_gate(cache: &TemplateMapCache) {
        *cache
            .hooks
            .before_derive
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    async fn acquire_concurrently(
        cache: &Arc<TemplateMapCache>,
        backend: &Backend,
    ) -> Vec<Acquired> {
        let tasks: Vec<_> = (0..N)
            .map(|_| {
                let cache = Arc::clone(cache);
                let backend = backend.clone();
                tokio::spawn(async move { cache.acquire(&backend, &TenantId::new(TENANT)).await })
            })
            .collect();
        let mut results = Vec::with_capacity(N);
        for task in tasks {
            results.push(task.await.expect("acquire task"));
        }
        results
    }

    fn derives(cache: &TemplateMapCache) -> usize {
        cache.hooks.derives.load(Ordering::SeqCst)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquisitions_share_one_derivation() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let cache = Arc::new(TemplateMapCache::default());
        gate_until_joined(&cache, || {});

        let results = acquire_concurrently(&cache, &backend).await;

        assert_eq!(
            derives(&cache),
            1,
            "one derivation for {N} concurrent queries"
        );
        let acquired: Vec<_> = results
            .into_iter()
            .map(|r| r.expect("every waiter gets the map"))
            .collect();
        let (first, bytes) = &acquired[0];
        assert!(*bytes > 0, "the cold derivation folded the audit stream");
        for (map, joined_bytes) in &acquired {
            assert!(Arc::ptr_eq(map, first), "every waiter shares the one map");
            assert_eq!(joined_bytes, bytes, "joiners report the derivation's bytes");
        }
        let expected =
            template_map::derive_template_map(backend.store_ref(), &TenantId::new(TENANT))
                .expect("fresh fold")
                .0;
        assert_eq!(first.folded_files(), expected.folded_files());
        assert_eq!(first.registry(), expected.registry());

        // Same frontier again: served from memory, nothing fetched.
        let (again, bytes) = cache
            .acquire(&backend, &TenantId::new(TENANT))
            .await
            .expect("memory hit");
        assert!(Arc::ptr_eq(&again, first));
        assert_eq!(bytes, 0);
        assert_eq!(derives(&cache), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changed_frontier_rederives() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let cache = Arc::new(TemplateMapCache::default());
        let tenant = TenantId::new(TENANT);

        let (before, _) = cache.acquire(&backend, &tenant).await.expect("cold");
        write_audit(bucket.path(), &[adopted(3, TS0 + DAY_NS)]);
        let (after, bytes) = cache.acquire(&backend, &tenant).await.expect("re-derive");

        assert_eq!(derives(&cache), 2, "a new audit file re-derives");
        assert!(bytes > 0);
        assert!(after.folded_files().len() > before.folded_files().len());
        assert!(after.registry().contains_key(&(3, 1)));
        let (held, bytes) = cache.acquire(&backend, &tenant).await.expect("hit");
        assert!(
            Arc::ptr_eq(&held, &after),
            "the cache holds the latest frontier"
        );
        assert_eq!(bytes, 0);
        assert_eq!(derives(&cache), 2);
    }

    /// Plant a non-Parquet `*.parquet` beside a real audit file: the fold
    /// fails reading it, as any unreadable audit object does.
    fn plant_unreadable(bucket: &Path) -> std::path::PathBuf {
        let tenant_root = bucket.join("audit").join(format!("tenant_id={TENANT}"));
        let mut dir = tenant_root;
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
    async fn failed_derivation_reaches_every_waiter_then_retries() {
        let bucket = seeded();
        let bad = plant_unreadable(bucket.path());
        let backend = Backend::Local(bucket.path().to_path_buf());
        let cache = Arc::new(TemplateMapCache::default());
        gate_until_joined(&cache, || {});

        let results = acquire_concurrently(&cache, &backend).await;

        assert_eq!(derives(&cache), 1);
        for result in &results {
            assert!(
                matches!(result, Err(QueryError::Storage { .. })),
                "every waiter gets the derivation's error: {result:?}",
            );
        }
        clear_gate(&cache);
        assert!(
            cache
                .acquire(&backend, &TenantId::new(TENANT))
                .await
                .is_err()
        );
        assert_eq!(
            derives(&cache),
            2,
            "an error is not cached: the next query retries"
        );

        std::fs::remove_file(&bad).expect("remove unreadable file");
        cache
            .acquire(&backend, &TenantId::new(TENANT))
            .await
            .expect("recovers once the store is readable");
        assert_eq!(derives(&cache), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panicked_derivation_does_not_wedge_waiters() {
        let bucket = seeded();
        let backend = Backend::Local(bucket.path().to_path_buf());
        let cache = Arc::new(TemplateMapCache::default());
        let armed = Arc::new(AtomicBool::new(true));
        gate_until_joined(&cache, {
            let armed = Arc::clone(&armed);
            move || assert!(!armed.swap(false, Ordering::SeqCst), "derivation panics")
        });

        let results = acquire_concurrently(&cache, &backend).await;

        assert_eq!(derives(&cache), 1);
        assert!(
            results.iter().all(Result::is_err),
            "the panicked derivation's starter and every joiner get an error",
        );
        cache
            .acquire(&backend, &TenantId::new(TENANT))
            .await
            .expect("the next query derives afresh");
        assert_eq!(derives(&cache), 2);
    }
}
