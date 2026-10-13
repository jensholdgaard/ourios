//! The background template-map refresh (RFC 0060).
//!
//! A refresher visits every tenant with an audit stream, one at a time, and
//! acquires its map through the same per-tenant single-flight a query uses
//! (`map_flight`), so the full folds RFC 0033 still runs are mostly paid
//! here instead of by a query. It decides nothing new: each acquisition
//! takes the usual path, publishes through the usual write-through, and a
//! query that arrives meanwhile joins it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::future::{Either, select};
use ourios_core::tenant::TenantId;
use ourios_parquet::percent_decode_tenant;
use tracing::Instrument as _;

use crate::map_flight::{MapAcquisition, TemplateMapFlights};
use crate::template_map::{self, AcquireTrigger, Persisted};
use crate::{Backend, QueryError, StoreRef, phase};

/// The most consecutive passes a tenant that persists nothing is skipped
/// for (RFC 0060 §3.3).
const MAX_SKIPPED_PASSES: u32 = 11;

/// The `querier` role's background template-map refresh task (RFC 0060).
/// Built by [`crate::Querier::template_map_refresher`], it shares that
/// querier's single-flight, so its acquisitions and the querier's queries
/// join each other.
pub struct TemplateMapRefresher {
    flights: Arc<TemplateMapFlights>,
    backend: Backend,
    /// Tenants whose recent acquisitions persisted nothing. In memory only:
    /// a restart forgets it.
    backoff: HashMap<TenantId, Backoff>,
    #[cfg(feature = "testing")]
    log: Arc<testing::PassLog>,
    #[cfg(feature = "testing")]
    before_list: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// One tenant's back-off: `k` consecutive acquisitions that persisted
/// nothing, and how many more passes skip it.
#[derive(Debug, Default)]
struct Backoff {
    misses: u32,
    skip: u32,
}

impl Backoff {
    /// Whether this pass skips the tenant, counting the skip.
    fn skips(&mut self) -> bool {
        let Some(left) = self.skip.checked_sub(1) else {
            return false;
        };
        self.skip = left;
        true
    }

    /// One more acquisition that persisted nothing: skip the next
    /// `min(2^k − 1, 11)` passes.
    fn persisted_nothing(&mut self) {
        self.misses = self.misses.saturating_add(1);
        self.skip = 2_u32
            .saturating_pow(self.misses)
            .saturating_sub(1)
            .min(MAX_SKIPPED_PASSES);
    }
}

/// How a pass ended.
enum PassEnd {
    Finished,
    /// The tenant listing failed, with this `error.type`.
    ListingFailed(&'static str),
    ShutDown,
}

impl TemplateMapRefresher {
    pub(crate) fn new(flights: Arc<TemplateMapFlights>, backend: Backend) -> Self {
        Self {
            flights,
            backend,
            backoff: HashMap::new(),
            #[cfg(feature = "testing")]
            log: Arc::default(),
            #[cfg(feature = "testing")]
            before_list: None,
        }
    }

    /// Refresh every tenant's template map until `shutdown` completes: a
    /// pass at once, then each next pass `interval` after the previous one
    /// finished, so passes never overlap and a long pass is never caught up
    /// on (RFC 0060 §3.1).
    ///
    /// Shutdown is checked between tenants and while idle. An acquisition
    /// in progress is abandoned, never cancelled: its fold finishes on the
    /// blocking pool and publishes, as a dropped query's does, but this
    /// returns without waiting for it. Nothing here fails the caller: a
    /// failed acquisition or tenant listing is recorded and retried later.
    pub async fn run(mut self, interval: Duration, shutdown: impl Future<Output = ()>) {
        let mut shutdown = pin!(shutdown);
        loop {
            if let PassEnd::ShutDown = self.pass(shutdown.as_mut()).await {
                return;
            }
            if let Either::Left(_) =
                select(shutdown.as_mut(), pin!(tokio::time::sleep(interval))).await
            {
                return;
            }
        }
    }

    /// One pass, as a root `refresh template_map` span (RFC 0060 §3.5) with
    /// one `refresh.duration` sample. Only a pass that ends early on a failed
    /// listing carries `error.type`; a failed tenant is its own span's error.
    async fn pass<S: Future<Output = ()>>(&mut self, shutdown: Pin<&mut S>) -> PassEnd {
        let span = tracing::info_span!(
            parent: None,
            "refresh template_map",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        #[cfg(feature = "testing")]
        self.log.started();
        let started = Instant::now();
        let end = self.visit(&span, shutdown).instrument(span.clone()).await;
        let error_type = match end {
            PassEnd::ListingFailed(error_type) => Some(error_type),
            PassEnd::Finished | PassEnd::ShutDown => None,
        };
        if let Some(error_type) = error_type {
            ourios_core::telemetry::record_span_error(&span, error_type);
        }
        template_map::record_refresh_pass(started.elapsed(), error_type);
        #[cfg(feature = "testing")]
        self.log.finished();
        end
    }

    /// List the tenants, then acquire each in listing order, one at a time.
    async fn visit<S: Future<Output = ()>>(
        &mut self,
        span: &tracing::Span,
        mut shutdown: Pin<&mut S>,
    ) -> PassEnd {
        // Shutdown races the listing as it races an acquisition: a slow
        // LIST is abandoned to the blocking pool, never awaited.
        let tenants = match select(shutdown.as_mut(), pin!(self.list_tenants(span))).await {
            Either::Left(_) => return PassEnd::ShutDown,
            Either::Right((Ok(tenants), _)) => tenants,
            Either::Right((Err(e), _)) => {
                return PassEnd::ListingFailed(template_map::error_type(&e));
            }
        };
        let listed: HashSet<&TenantId> = tenants.iter().collect();
        self.backoff.retain(|tenant, _| listed.contains(tenant));
        for tenant in &tenants {
            if shutdown.as_mut().now_or_never().is_some() {
                return PassEnd::ShutDown;
            }
            if self.backoff.get_mut(tenant).is_some_and(Backoff::skips) {
                continue;
            }
            let acquire =
                self.flights
                    .acquire_fetched(&self.backend, tenant, AcquireTrigger::Refresh);
            let acquired = match select(shutdown.as_mut(), pin!(acquire)).await {
                Either::Left(_) => return PassEnd::ShutDown,
                Either::Right((acquired, _)) => acquired,
            };
            self.settle(tenant, &acquired);
        }
        PassEnd::Finished
    }

    /// The tenants with an audit stream, listed on the blocking pool under
    /// the pass span.
    async fn list_tenants(&self, span: &tracing::Span) -> Result<Vec<TenantId>, QueryError> {
        let backend = self
            .backend
            .clone()
            .with_caller(ourios_parquet::Caller::TemplateMap);
        #[cfg(feature = "testing")]
        let gate = self.before_list.clone();
        phase::spawn_blocking(span, move || {
            #[cfg(feature = "testing")]
            if let Some(gate) = gate {
                gate();
            }
            audit_tenants(backend.store_ref())
        })
        .await
    }

    /// Record what `tenant`'s acquisition left in the store: a current
    /// artifact clears its back-off; nothing (abstained, failed publish,
    /// failed acquisition) extends it.
    fn settle(&mut self, tenant: &TenantId, acquired: &Result<MapAcquisition, QueryError>) {
        match acquired {
            Ok(MapAcquisition {
                persisted: Persisted::Current,
                ..
            }) => {
                self.backoff.remove(tenant);
            }
            Ok(_) | Err(_) => self
                .backoff
                .entry(tenant.clone())
                .or_default()
                .persisted_nothing(),
        }
    }
}

/// The tenants with an audit stream, in listing order: one delimited LIST
/// of `audit/`, each `tenant_id=` prefix decoded (RFC 0060 §3.1). A name
/// that does not decode is not an Ourios tenant and is skipped.
fn audit_tenants(backend: StoreRef<'_>) -> Result<Vec<TenantId>, QueryError> {
    let mut prefixes = match backend {
        StoreRef::Local(root) => local_subdirs(&root.join("audit"))?,
        StoreRef::Remote(store) => {
            store
                .list_common_prefixes_blocking(Some("audit"))
                .map_err(|e| QueryError::Storage {
                    detail: format!("list audit/: {e}"),
                })?
        }
    };
    prefixes.sort();
    Ok(prefixes
        .iter()
        .filter_map(|prefix| prefix.rsplit('/').next())
        .filter_map(|segment| segment.strip_prefix("tenant_id="))
        .filter_map(percent_decode_tenant)
        .map(TenantId::new)
        .collect())
}

/// The names of `dir`'s subdirectories; none when `dir` does not exist (a
/// fresh bucket).
fn local_subdirs(dir: &Path) -> Result<Vec<String>, QueryError> {
    let storage = |e: std::io::Error| QueryError::Storage {
        detail: format!("list {}: {e}", dir.display()),
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(storage(e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(storage)?;
        if entry.file_type().map_err(storage)?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Test seams for integration tests through the `testing` feature: single
/// passes, the back-off state, and when each pass started and finished.
#[cfg(feature = "testing")]
pub mod testing {
    use std::sync::{Arc, Mutex, PoisonError};

    use tokio::time::Instant;

    use super::TemplateMapRefresher;

    /// When each pass started and, once it has, finished, on the tokio
    /// clock (paused in tests).
    #[derive(Debug, Default)]
    pub struct PassLog {
        passes: Mutex<Vec<(Instant, Option<Instant>)>>,
    }

    impl PassLog {
        pub(super) fn started(&self) {
            self.lock().push((Instant::now(), None));
        }

        pub(super) fn finished(&self) {
            if let Some(pass) = self.lock().last_mut() {
                pass.1 = Some(Instant::now());
            }
        }

        /// Every pass so far: its start and, once finished, its end.
        #[must_use]
        pub fn passes(&self) -> Vec<(Instant, Option<Instant>)> {
            self.lock().clone()
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(Instant, Option<Instant>)>> {
            self.passes.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl TemplateMapRefresher {
        /// Run one pass to its end.
        #[doc(hidden)]
        pub async fn run_pass(&mut self) {
            self.pass(std::pin::pin!(std::future::pending::<()>()))
                .await;
        }

        /// Run `gate` on the blocking thread before every tenant listing, so a
        /// test can hold a pass inside its LIST.
        #[doc(hidden)]
        #[must_use]
        pub fn with_listing_gate(mut self, gate: Arc<dyn Fn() + Send + Sync>) -> Self {
            self.before_list = Some(gate);
            self
        }

        /// This refresher's pass log, shared with it.
        #[doc(hidden)]
        #[must_use]
        pub fn pass_log(&self) -> Arc<PassLog> {
            Arc::clone(&self.log)
        }

        /// The tenants it holds a back-off for, sorted.
        #[doc(hidden)]
        #[must_use]
        pub fn backed_off(&self) -> Vec<String> {
            let mut tenants: Vec<String> = self
                .backoff
                .keys()
                .map(|tenant| tenant.as_str().to_owned())
                .collect();
            tenants.sort();
            tenants
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Backoff;

    /// The passes (1-based) on which a tenant that never persists is
    /// acquired: each miss skips `min(2^k − 1, 11)` passes.
    #[test]
    fn backoff_skips_two_to_the_k_minus_one_capped_at_eleven() {
        let mut backoff = Backoff::default();
        let mut acquired = Vec::new();
        for pass in 1..=40 {
            if !backoff.skips() {
                acquired.push(pass);
                backoff.persisted_nothing();
            }
        }
        assert_eq!(acquired, vec![1, 3, 7, 15, 27, 39]);
    }
}
