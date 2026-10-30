//! RFC 0001 §6.8 telemetry instruments for the template miner.
//!
//! Instruments are resolved through the process-global meter provider
//! under the versioned `ourios.miner` scope
//! (`ourios_core::telemetry::scope`) per the §6.8 *Export
//! architecture* API/SDK split: this library depends only on the
//! lightweight `opentelemetry` API crate; the SDK + OTLP exporter
//! live in `ourios-telemetry`. With no provider installed the
//! instrument `record` / `add` calls are cheap no-ops, so a
//! [`MinerMetrics`] is always safe to construct and drive.
//!
//! The in-process observable-gauge state ([`MinerMetricsState`]) is
//! *not* gated on whether a provider is installed: the per-line
//! denominator / reservoir updates take the state mutex and touch the
//! tally maps on every ingest regardless. That cost is unconditional
//! by design — the gauges are derived in-process (§6.8 / RFC0001.8),
//! so the state must be maintained to serve a collection that may
//! arrive at any time — but it is bounded: the hot-path update clones
//! a key only on first sight of a `(tenant, service)` pair (see
//! [`ServiceTallyMap::tally_mut`]), the named tallies are capped at
//! [`SERVICE_TALLY_BUDGET`] and each reservoir at [`RESERVOIR_CAP`].
//!
//! # Metric names and attributes
//!
//! Names (`ourios.miner.template.count`, `ourios.miner.merges`, …)
//! and data-point attribute keys (`ourios.tenant`, `ourios.service`,
//! `ourios.miner.template_change`) come from the generated
//! [`ourios_semconv`] constants — the dotted-`ourios.*` weaver
//! registry (the shared `ourios-semconv` repo) alongside the compaction set
//! (RFC 0009 §3.6).
//!
//! # Sync vs. observable
//!
//! The §6.8 table fixes each instrument's kind. Counters and
//! histograms are **synchronous** — recorded at the hot-path site
//! that produces the measurement. The gauges are **observable**:
//! they read process state through a callback at collection time.
//! That state — per-`(tenant, service)` line tallies and a bounded
//! confidence reservoir, plus per-tenant template counts — lives in
//! [`MinerMetricsState`] behind a `Mutex`. The gauges are registered
//! once per global provider installation and read every live cluster's state through
//! a registry of `Weak` references ([`gauges`]), so a dropped cluster
//! stops reporting and its state is freed.
//!
//! # When instruments appear
//!
//! The mandatory §6.8 set is defined by the [`ourios_semconv`]
//! registry; each instrument is *registered* on the `ourios.miner`
//! meter at construction. `OTel`'s metric model is collect-on-read,
//! so an instrument contributes a data point on its **first real
//! measurement** — no synthetic zero-traffic points are emitted, so
//! every exported series carries the registry's `required`
//! attributes.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{KeyValue, global};

use ourios_core::audit::{EVENT_TYPE_TEMPLATE_TYPE_EXPANDED, EVENT_TYPE_TEMPLATE_WIDENED};
use ourios_core::otlp::{KeyValue as OtlpKeyValue, any_value};
use ourios_core::tenant::TenantId;
use ourios_semconv as semconv;

mod gauges;
#[cfg(test)]
mod zero_points_tests;

use gauges::{Binding, GaugeRegistry, GlobalProvider};

/// Meter name per RFC 0001 §6.8, resolved under a versioned scope
/// (`ourios_core::telemetry::scope(METER_NAME)`).
const METER_NAME: &str = "ourios.miner";

/// Resource-attribute key the source service is read from. The
/// §6.8 `service` attribute is the *log's source* service (distinct
/// from Ourios's own `service.name` resource attribute), read from
/// the ingested record's `resource_attributes`.
const RESOURCE_SERVICE_NAME: &str = "service.name";

/// Confidence-reservoir window size. Bounded so a high-volume
/// `(tenant, service)` cannot grow the reservoir without limit
/// (§3.2's cardinality discipline applied to our own telemetry
/// state).
const RESERVOIR_CAP: usize = 1024;

/// Bucket boundaries for `ourios.miner.confidence` (`similarity /
/// threshold`), exactly as RFC 0001 §6.8 fixes them: straddling the §6.3
/// decision boundary at 1.0, with `+Inf` implicit. Buckets are
/// upper-inclusive; `0.99` and `0.999` resolve the values just under 1.0.
const CONFIDENCE_BUCKETS: [f64; 13] = [
    0.1, 0.3, 0.5, 0.7, 0.9, 0.95, 0.99, 0.999, 1.0, 1.05, 1.2, 1.5, 2.0,
];

/// The `ourios.miner.template_change` registry enum: a tenant's
/// `ourios.miner.merges` zero points (RFC 0001 §6.8).
const TEMPLATE_CHANGES: [&str; 2] = [
    EVENT_TYPE_TEMPLATE_WIDENED,
    EVENT_TYPE_TEMPLATE_TYPE_EXPANDED,
];

/// The `ourios.miner.parse_failure.reason` registry enum: a tenant's
/// `ourios.miner.parse_failures` zero points (RFC 0001 §6.8).
const PARSE_FAILURE_REASONS: [&str; 7] = [
    "below_floor",
    "empty_line",
    "tokenizer_failure",
    "degenerate_widening",
    "line_too_long",
    "template_ceiling",
    "id_reservation_failed",
];

/// Bucket boundaries (seconds) for `ourios.miner.duration`, one line's
/// mining time: microseconds normally, milliseconds when a tree search runs
/// long.
const LINE_DURATION_BUCKETS_S: [f64; 14] = [
    0.000_001,
    0.000_002_5,
    0.000_005,
    0.000_01,
    0.000_025,
    0.000_05,
    0.000_1,
    0.000_25,
    0.000_5,
    0.001,
    0.002_5,
    0.005,
    0.01,
    0.1,
];

/// Explicit bucket boundaries (bytes) for the `ourios.miner.structured_body.size`
/// histogram (RFC 0037 §3.2). The SDK's default boundaries top out near 10 000,
/// which for a byte measure collapses every structured body over ~10 KiB into
/// one `+Inf` bucket — useless for the metric's purpose (seeing where large
/// structured bodies sit). These span the structured-body range from the
/// `param_byte_limit` reference (256 B) to 16 MiB, so an operator can actually
/// tell a 50 KiB chat history from a multi-MiB one. Bytes are `u64` at the
/// record site; the boundaries API takes `f64` (exact for every value here).
const STRUCTURED_BODY_SIZE_BUCKETS: [f64; 9] = [
    256.0,        // param_byte_limit reference
    1_024.0,      // 1 KiB
    4_096.0,      // 4 KiB
    16_384.0,     // 16 KiB
    65_536.0,     // 64 KiB
    262_144.0,    // 256 KiB
    1_048_576.0,  // 1 MiB
    4_194_304.0,  // 4 MiB
    16_777_216.0, // 16 MiB
];

/// Read the source `service.name` from a record's
/// `resource_attributes`, returning `None` when it is absent, empty,
/// or non-string. The proto `KeyValue` carries
/// `value: Option<AnyValue>`; only a non-empty `StringValue` is a
/// meaningful service identity.
///
/// `ourios.service` is `recommended` (not `required`) in the semconv
/// registry: an absent source `service.name` means the attribute is
/// **omitted**, not synthesized to a sentinel. Synthesizing a value
/// would change the attribute's meaning and create a fake series that
/// could collide with a real service of that name.
#[must_use]
pub(crate) fn service_of(resource_attributes: &[OtlpKeyValue]) -> Option<String> {
    resource_attributes
        .iter()
        .find(|kv| kv.key == RESOURCE_SERVICE_NAME)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|av| av.value.as_ref())
        .and_then(|v| match v {
            any_value::Value::StringValue(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
}

/// Lock a metrics mutex, recovering the guard if it was poisoned by an
/// unrelated panic. Telemetry is best-effort: a poisoned lock must never
/// crash an ingest write or a collection callback.
fn lock_recovering<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Increment a per-tenant counter by one, cloning the tenant key only on
/// its first sight — the hot path (a tenant already in the map) reuses
/// the borrowed key via `get_mut`.
fn bump_tenant(map: &mut HashMap<TenantId, u64>, tenant: &TenantId) {
    if let Some(count) = map.get_mut(tenant) {
        *count += 1;
    } else {
        map.insert(tenant.clone(), 1);
    }
}

/// Set a per-tenant value, cloning the tenant key only on first sight.
fn set_tenant(map: &mut HashMap<TenantId, u64>, tenant: &TenantId, value: u64) {
    if let Some(slot) = map.get_mut(tenant) {
        *slot = value;
    } else {
        map.insert(tenant.clone(), value);
    }
}

/// Named `(tenant, service)` tallies one miner keeps across all its
/// tenants. `service.name` comes from the ingested data, so the count is
/// bounded here, by our own telemetry state, never by a per-tenant cap on
/// the data model.
const SERVICE_TALLY_BUDGET: usize = 4_096;

/// Every `(tenant, service)` tally behind the ratio and quantile gauges,
/// bounded to a budget of named tallies across all tenants.
///
/// Each touch stamps the tally with a logical clock. Inserting past the
/// budget evicts the least recently touched eighth in one pass, so a
/// flood of distinct names costs an amortised O(1) per insert rather than
/// a scan per line. An evicted service's gauges stop until it logs again
/// and then restart from an empty window. Service-less slots are one per
/// tenant and sit outside the budget.
struct ServiceTallyMap {
    tenants: HashMap<TenantId, ServiceTallies>,
    named: usize,
    clock: u64,
    budget: usize,
}

impl Default for ServiceTallyMap {
    fn default() -> Self {
        Self::with_budget(SERVICE_TALLY_BUDGET)
    }
}

impl ServiceTallyMap {
    fn with_budget(budget: usize) -> Self {
        Self {
            tenants: HashMap::new(),
            named: 0,
            clock: 0,
            budget: budget.max(1),
        }
    }

    fn iter(&self) -> impl Iterator<Item = (&TenantId, &ServiceTallies)> {
        self.tenants.iter()
    }

    /// Borrow the `(tenant, service)` tally for a mutable update,
    /// allocating owned keys only on the first sight of the pair.
    ///
    /// The hot path — a `(tenant, service)` already in flight — clones
    /// nothing: the outer probe is `get_mut(&TenantId)` and the inner
    /// probe is `get_mut(name)` keyed on a borrowed `&str`. A `None`
    /// service — a line whose source carried no `service.name` — lands in
    /// the dedicated [`ServiceTallies::no_service`] slot, attributed to
    /// the tenant alone (`ourios.service` omitted, see [`service_attrs`]).
    fn tally_mut(&mut self, tenant: &TenantId, service: Option<&str>) -> &mut ServiceTally {
        self.clock += 1;
        let now = self.clock;
        let is_new = match service {
            Some(name) => self
                .tenants
                .get(tenant)
                .is_none_or(|t| !t.by_name.contains_key(name)),
            None => false,
        };
        if is_new && self.named >= self.budget {
            self.evict_least_recent();
        }
        // `contains_key` releases its borrow before the follow-up
        // `get_mut` / `entry`, sidestepping the get-then-insert
        // borrow-checker limitation while still cloning a key only on
        // first insert.
        if !self.tenants.contains_key(tenant) {
            self.tenants
                .insert(tenant.clone(), ServiceTallies::default());
        }
        let tallies = self
            .tenants
            .get_mut(tenant)
            .unwrap_or_else(|| unreachable!("inserted above when absent"));
        let tally = match service {
            None => &mut tallies.no_service,
            Some(name) if !is_new => tallies
                .by_name
                .get_mut(name)
                .unwrap_or_else(|| unreachable!("is_new was false")),
            Some(name) => {
                self.named += 1;
                tallies.by_name.entry(name.to_owned()).or_default()
            }
        };
        tally.last_touched = now;
        tally
    }

    /// Drop the least recently touched eighth of the named tallies.
    /// Clock stamps are unique, so exactly that many go.
    fn evict_least_recent(&mut self) {
        let mut stamps: Vec<u64> = self
            .tenants
            .values()
            .flat_map(|t| t.by_name.values().map(|tally| tally.last_touched))
            .collect();
        let evict = (self.budget / 8).clamp(1, stamps.len());
        let (_, cutoff, _) = stamps.select_nth_unstable(evict - 1);
        let cutoff = *cutoff;
        for tallies in self.tenants.values_mut() {
            let before = tallies.by_name.len();
            tallies
                .by_name
                .retain(|_, tally| tally.last_touched > cutoff);
            // `retain` keeps the table's allocation; without shrinking, each
            // tenant that once filled the budget would hold a peak-sized
            // table after its services are gone.
            if tallies.by_name.len() < before {
                tallies.by_name.shrink_to_fit();
            }
        }
        self.named -= evict;
    }
}

/// Per-tenant tallies, split so the common named-service path probes a
/// borrowed `&str` key and only the service-less lines share a slot.
#[derive(Default)]
struct ServiceTallies {
    /// Tallies for lines carrying a non-empty `service.name`.
    by_name: HashMap<String, ServiceTally>,
    /// Tally for lines whose source set no `service.name`. These count
    /// toward the tenant but emit no `ourios.service` attribute. A
    /// tenant with only named services leaves this at its zero default;
    /// the gauge callbacks gate on `lines > 0` so an unused slot emits
    /// no point.
    no_service: ServiceTally,
}

impl ServiceTallies {
    /// Yield each populated `(service, tally)` for a gauge callback:
    /// every named service plus the service-less slot, the latter only
    /// once it has seen a real line (`lines > 0`) so an unused slot
    /// emits no point.
    fn iter(&self) -> impl Iterator<Item = (Option<&str>, &ServiceTally)> {
        let named = self
            .by_name
            .iter()
            .map(|(name, t)| (Some(name.as_str()), t));
        let unnamed = (self.no_service.lines > 0).then_some((None, &self.no_service));
        named.chain(unnamed)
    }
}

/// Per-`(tenant, service)` rolling tallies for the derived gauges.
///
/// `lines` is the denominator for `ourios.miner.params.overflow.utilization`;
/// `overflow_lines` is its numerator. `confidence` is a bounded
/// reservoir over the same key, the source for the
/// `ourios.miner.confidence.p50` / `ourios.miner.confidence.p01` quantile gauges.
#[derive(Default)]
struct ServiceTally {
    lines: u64,
    overflow_lines: u64,
    confidence: Reservoir,
    /// [`ServiceTallyMap`] clock stamp of the last update.
    last_touched: u64,
}

/// Process state the observable-gauge callbacks read at collection
/// time. Guarded by a `Mutex` because `OTel` may invoke a callback
/// from any thread on collection.
#[derive(Default)]
struct MinerMetricsState {
    /// Per-tenant, per-service tallies driving the ratio + quantile
    /// gauges. Nested (not a flat `(TenantId, _)` key) so the common
    /// hot-path update borrows the tenant for the outer lookup
    /// (`get_mut(tenant)`) and clones a key only on the first sight of
    /// a `(tenant, service)` pair. Each tenant's [`ServiceTallies`]
    /// splits a named-service map (`by_name`) from a single
    /// `no_service` slot for lines whose source carried no
    /// `service.name` — those still count toward the tenant, attributed
    /// without a service (`ourios.service` omitted, see [`service_attrs`]).
    by_service: ServiceTallyMap,
    /// Per-tenant template count, mirrored from the cluster so the
    /// `ourios.miner.template.count` observable gauge can report it
    /// without borrowing the cluster.
    template_counts: HashMap<TenantId, u64>,
    /// Per-tenant body-retention numerator / line denominator for
    /// the `ourios.miner.body_retention.utilization` gauge.
    body_retentions: HashMap<TenantId, u64>,
    body_lines: HashMap<TenantId, u64>,
}

/// A bounded confidence reservoir: an exact quantile over a capped
/// FIFO window of recent samples.
///
/// **Quantile mechanism (flagged decision).** RFC 0001 §6.8 does
/// not pin the quantile algorithm or window size. This is an exact
/// quantile over a bounded ring of the most recent
/// [`RESERVOIR_CAP`] samples per `(tenant, service)` — small,
/// allocation-bounded, and correct over its window, with no sketch
/// approximation error. The gauges are quantiles of this window, not
/// of the `confidence` histogram, which records every sample
/// independently and survives tally eviction. A future contract change could instead make p50/p01
/// backend-derived quantiles over the exported `confidence` histogram
/// (independent of the now-landed dotted-semconv naming); if so, this
/// in-process reservoir is replaced under that change's own review.
///
/// The window grows on demand rather than preallocating the cap, so a
/// long tail of quiet services costs a few samples each, not 8 KiB.
#[derive(Default)]
struct Reservoir {
    samples: VecDeque<f64>,
}

impl Reservoir {
    fn observe(&mut self, value: f64) {
        if self.samples.len() == RESERVOIR_CAP {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    /// Copy the current sample window — O(n), no sort — so a collection
    /// callback can take it under the metrics lock and compute the
    /// quantile *after* releasing the lock.
    fn snapshot(&self) -> Vec<f64> {
        self.samples.iter().copied().collect()
    }
}

/// Exact nearest-rank `q`-quantile (`q` in `[0.0, 1.0]`) over a sample
/// window, or `None` when empty. Takes an owned `Vec` and sorts it so the
/// caller can run it OFF the metrics lock — collection must not block the
/// ingest hot path on the `O(n log n)` sort.
fn quantile_of(mut samples: Vec<f64>, q: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    // `total_cmp` is a total order over f64 (no panic; any stray NaN sorts
    // to an end) — avoids `partial_cmp(...).expect(...)`.
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    // Nearest-rank: rank = ceil(q * n), clamped to [1, n].
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = (q * n as f64).ceil().max(1.0) as usize;
    Some(samples[rank.min(n) - 1])
}

/// The miner's §6.8 instrument set plus the shared state its
/// observable gauges read.
///
/// Constructed once per [`crate::cluster::MinerCluster`]. The
/// synchronous instruments are recorded at the hot-path sites; the
/// observable gauges, registered once per installation (see [`gauges`]),
/// read this [`MinerMetricsState`] for as long as the `MinerMetrics`
/// lives and need no per-call maintenance beyond updating it.
pub(crate) struct MinerMetrics {
    state: Arc<Mutex<MinerMetricsState>>,
    merges_total: Counter<u64>,
    parse_failures_total: Counter<u64>,
    upstream_template_processed: Counter<u64>,
    params_overflow_total: Counter<u64>,
    template_version_changes_total: Counter<u64>,
    confidence: Histogram<f64>,
    miner_duration: Histogram<f64>,
    structured_body_size: Histogram<u64>,
}

impl MinerMetrics {
    /// Register every §6.8 instrument on the `ourios.miner` meter.
    /// The mandatory set is defined by the [`ourios_semconv`]
    /// registry; each instrument surfaces in the export on its first
    /// real measurement (§3.1.2).
    pub(crate) fn new() -> Self {
        Self::for_provider(&global::meter_provider(), &gauges::GLOBAL_BINDINGS)
    }

    /// Derive the synchronous instruments and the gauge binding from one
    /// provider snapshot, so a concurrent `set_meter_provider` cannot
    /// split a cluster's instruments across two providers.
    fn for_provider(provider: &GlobalProvider, bindings: &Mutex<Vec<Binding>>) -> Self {
        Self::with_registry(
            &provider.meter_with_scope(ourios_core::telemetry::scope(METER_NAME)),
            &gauges::registry_for(provider, bindings),
        )
    }

    /// Build the synchronous instruments on `meter` and enrol a fresh
    /// state with the gauges reading `registry`.
    fn with_registry(meter: &Meter, registry: &GaugeRegistry) -> Self {
        let state = Arc::new(Mutex::new(MinerMetricsState::default()));
        registry.register(&state);

        let merges_total = meter
            .u64_counter(semconv::OURIOS_MINER_MERGES)
            .with_description(semconv::OURIOS_MINER_MERGES_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_MERGES_UNIT)
            .build();
        let parse_failures_total = meter
            .u64_counter(semconv::OURIOS_MINER_PARSE_FAILURES)
            .with_description(semconv::OURIOS_MINER_PARSE_FAILURES_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_PARSE_FAILURES_UNIT)
            .build();
        let upstream_template_processed = meter
            .u64_counter(semconv::OURIOS_MINER_UPSTREAM_TEMPLATE_PROCESSED)
            .with_description(semconv::OURIOS_MINER_UPSTREAM_TEMPLATE_PROCESSED_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_UPSTREAM_TEMPLATE_PROCESSED_UNIT)
            .build();
        let params_overflow_total = meter
            .u64_counter(semconv::OURIOS_MINER_PARAMS_OVERFLOW)
            .with_description(semconv::OURIOS_MINER_PARAMS_OVERFLOW_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_PARAMS_OVERFLOW_UNIT)
            .build();
        let template_version_changes_total = meter
            .u64_counter(semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES)
            .with_description(semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES_UNIT)
            .build();
        let confidence = meter
            .f64_histogram(semconv::OURIOS_MINER_CONFIDENCE)
            .with_description(semconv::OURIOS_MINER_CONFIDENCE_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_CONFIDENCE_UNIT)
            .with_boundaries(CONFIDENCE_BUCKETS.to_vec())
            .build();
        let miner_duration = meter
            .f64_histogram(semconv::OURIOS_MINER_DURATION)
            .with_description(semconv::OURIOS_MINER_DURATION_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_DURATION_UNIT)
            .with_boundaries(LINE_DURATION_BUCKETS_S.to_vec())
            .build();
        let structured_body_size = meter
            .u64_histogram(semconv::OURIOS_MINER_STRUCTURED_BODY_SIZE)
            .with_description(semconv::OURIOS_MINER_STRUCTURED_BODY_SIZE_DESCRIPTION)
            .with_unit(semconv::OURIOS_MINER_STRUCTURED_BODY_SIZE_UNIT)
            .with_boundaries(STRUCTURED_BODY_SIZE_BUCKETS.to_vec())
            .build();

        Self {
            state,
            merges_total,
            parse_failures_total,
            upstream_template_processed,
            params_overflow_total,
            template_version_changes_total,
            confidence,
            miner_duration,
            structured_body_size,
        }
    }

    /// Bump the per-`(tenant, service)` line denominator (for
    /// `ourios.miner.params.overflow.utilization`) and the per-tenant
    /// body-line denominator (for
    /// `ourios.miner.body_retention.utilization`).
    ///
    /// **Ordering invariant.** This runs once, at the start of an
    /// ingest, *before* any numerator bump (`record_overflow`,
    /// `record_body_retention`) for the same line. The ratio gauges'
    /// callbacks may collect concurrently from another thread;
    /// incrementing the denominator first guarantees they never
    /// observe `numerator > denominator` (utilization > 1) for a line
    /// in flight.
    pub(crate) fn record_line_denominator(&self, tenant: &TenantId, service: Option<&str>) {
        let first_sight = {
            let mut st = lock_recovering(&self.state);
            let first_sight = !st.body_lines.contains_key(tenant);
            bump_tenant(&mut st.body_lines, tenant);
            st.by_service.tally_mut(tenant, service).lines += 1;
            first_sight
        };
        if first_sight {
            self.record_zero_points(tenant);
        }
    }

    /// RFC 0001 §6.8's per-tenant zero points: `0` on each lazily-created
    /// counter, so a tenant with no merges, parse failures or overflows
    /// exports zero rather than nothing. `ourios.service` has no natural
    /// first value and is omitted; a required enum attribute gets one
    /// point per registry member.
    fn record_zero_points(&self, tenant: &TenantId) {
        let tenant = KeyValue::new(semconv::OURIOS_TENANT, tenant.as_str().to_owned());
        for change in TEMPLATE_CHANGES {
            self.merges_total.add(
                0,
                &[
                    tenant.clone(),
                    KeyValue::new(semconv::OURIOS_MINER_TEMPLATE_CHANGE, change),
                ],
            );
        }
        for reason in PARSE_FAILURE_REASONS {
            self.parse_failures_total.add(
                0,
                &[
                    tenant.clone(),
                    KeyValue::new(semconv::OURIOS_MINER_PARSE_FAILURE_REASON, reason),
                ],
            );
        }
        let tenant_only = std::slice::from_ref(&tenant);
        self.params_overflow_total.add(0, tenant_only);
        self.template_version_changes_total.add(0, tenant_only);
    }

    /// Observe one ingested line's `confidence` on both the §6.8
    /// histogram and the per-`(tenant, service)` reservoir feeding the
    /// p50/p01 gauges. The line denominator is bumped separately and
    /// earlier by [`Self::record_line_denominator`].
    pub(crate) fn record_line(&self, tenant: &TenantId, service: Option<&str>, confidence: f64) {
        self.confidence
            .record(confidence, &service_attrs(tenant, service));
        let mut st = lock_recovering(&self.state);
        st.by_service
            .tally_mut(tenant, service)
            .confidence
            .observe(confidence);
    }

    /// Record the miner's per-line processing duration (§6.8 `ourios.miner.duration`).
    pub(crate) fn record_duration(&self, tenant: &TenantId, seconds: f64) {
        self.miner_duration.record(
            seconds,
            &[KeyValue::new(
                semconv::OURIOS_TENANT,
                tenant.as_str().to_owned(),
            )],
        );
    }

    /// Observe one structured (non-string) body's canonical-JSON byte
    /// length (RFC 0037 §3.2, `ourios.miner.structured_body.size`).
    /// Structured bodies are retained whole and never capped; this
    /// histogram is the hazard-#2 guard — an operator watches its
    /// per-`(tenant, service)` distribution to spot services emitting
    /// oversized payloads (the fix belongs at the emitter, not a
    /// store-side truncation).
    pub(crate) fn record_structured_body_bytes(
        &self,
        tenant: &TenantId,
        service: Option<&str>,
        bytes: u64,
    ) {
        self.structured_body_size
            .record(bytes, &service_attrs(tenant, service));
    }

    /// Record `count` per-parameter overflow events on one line for
    /// `(tenant, service)`: bumps the `params_overflow_total`
    /// counter and the per-service overflow-line numerator (the
    /// line is counted once toward the ratio regardless of how many
    /// of its params overflowed).
    pub(crate) fn record_overflow(&self, tenant: &TenantId, service: Option<&str>, count: u64) {
        if count == 0 {
            return;
        }
        self.params_overflow_total
            .add(count, &service_attrs(tenant, service));
        let mut st = lock_recovering(&self.state);
        st.by_service.tally_mut(tenant, service).overflow_lines += 1;
    }

    /// Record one parse-failure line (§6.8 `ourios.miner.parse_failures`).
    pub(crate) fn record_parse_failure(
        &self,
        tenant: &TenantId,
        service: Option<&str>,
        reason: &'static str,
    ) {
        debug_assert!(
            PARSE_FAILURE_REASONS.contains(&reason),
            "{reason} is not an ourios.miner.parse_failure.reason member"
        );
        // RFC 0023 §3.4 — the cause dimension rides the one counter
        // as an attribute (the OTel error.type convention), never
        // per-cause counters. Values are the
        // `ourios.miner.parse_failure.reason` enum members. Built
        // with exact capacity — this is a per-line hot path on
        // saturated tenants.
        let mut attrs = Vec::with_capacity(2 + usize::from(service.is_some()));
        attrs.push(KeyValue::new(
            semconv::OURIOS_TENANT,
            tenant.as_str().to_owned(),
        ));
        if let Some(name) = service {
            attrs.push(KeyValue::new(semconv::OURIOS_SERVICE, name.to_owned()));
        }
        attrs.push(KeyValue::new(
            semconv::OURIOS_MINER_PARSE_FAILURE_REASON,
            reason,
        ));
        self.parse_failures_total.add(1, &attrs);
    }

    /// Record one upstream-template handling outcome (RFC 0050,
    /// `ourios.miner.upstream_template.processed`). The `OTel`
    /// processor `.processed` shape: one counter for successes and
    /// failures, `error.type` present only on rejection —
    /// `error_type: None` means the string was associated
    /// (`observe`) or adopted (`adopt`); `Some(cause)` means it was
    /// rejected (`byte_limit`, `grammar`, `alignment`,
    /// `template_ceiling`) and the record was mined instead.
    pub(crate) fn record_upstream_template_processed(
        &self,
        tenant: &TenantId,
        service: Option<&str>,
        error_type: Option<&'static str>,
    ) {
        const ERROR_TYPE: &str = "error.type";
        let mut attrs = Vec::with_capacity(
            1 + usize::from(service.is_some()) + usize::from(error_type.is_some()),
        );
        attrs.push(KeyValue::new(
            semconv::OURIOS_TENANT,
            tenant.as_str().to_owned(),
        ));
        if let Some(name) = service {
            attrs.push(KeyValue::new(semconv::OURIOS_SERVICE, name.to_owned()));
        }
        if let Some(cause) = error_type {
            attrs.push(KeyValue::new(ERROR_TYPE, cause));
        }
        self.upstream_template_processed.add(1, &attrs);
    }

    /// Record one body-retention event for the
    /// `ourios.miner.body_retention.utilization` numerator (§6.3 retention paths).
    pub(crate) fn record_body_retention(&self, tenant: &TenantId) {
        let mut st = lock_recovering(&self.state);
        bump_tenant(&mut st.body_retentions, tenant);
    }

    /// Record one merge event (§6.8 `ourios.miner.merges`,
    /// `ourios.miner.template_change` attribute) and one
    /// `ourios.miner.template.version_changes` bump (every merge
    /// advances `template_version` per §6.7 / H5).
    pub(crate) fn record_merge(&self, tenant: &TenantId, event_type: &str) {
        debug_assert!(
            TEMPLATE_CHANGES.contains(&event_type),
            "{event_type} is not an ourios.miner.template_change member"
        );
        self.merges_total.add(
            1,
            &[
                KeyValue::new(semconv::OURIOS_TENANT, tenant.as_str().to_owned()),
                KeyValue::new(semconv::OURIOS_MINER_TEMPLATE_CHANGE, event_type.to_owned()),
            ],
        );
        self.template_version_changes_total.add(
            1,
            &[KeyValue::new(
                semconv::OURIOS_TENANT,
                tenant.as_str().to_owned(),
            )],
        );
    }

    /// Mirror a tenant's current template count into the state the
    /// `ourios.miner.template.count` observable gauge reads.
    pub(crate) fn set_template_count(&self, tenant: &TenantId, count: u64) {
        let mut st = lock_recovering(&self.state);
        set_tenant(&mut st.template_counts, tenant, count);
    }
}

/// Data-point attributes for a `(tenant, service)` measurement.
///
/// `ourios.tenant` is `required`, so it is always present.
/// `ourios.service` is `recommended`: it is emitted only when the
/// source carried a `service.name` (`Some`). A service-less line
/// (`None`) is attributed to the tenant alone — the point is **not**
/// dropped, and no synthetic service value is fabricated.
fn service_attrs(tenant: &TenantId, service: Option<&str>) -> Vec<KeyValue> {
    let mut attrs = Vec::with_capacity(1 + usize::from(service.is_some()));
    attrs.push(KeyValue::new(
        semconv::OURIOS_TENANT,
        tenant.as_str().to_owned(),
    ));
    if let Some(name) = service {
        attrs.push(KeyValue::new(semconv::OURIOS_SERVICE, name.to_owned()));
    }
    attrs
}

/// `numerator / denominator`, or `0.0` when the denominator is zero
/// (a service that has seen no lines has a 0 ratio, not a NaN).
#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Reservoir, ServiceTallyMap, ratio, service_of};
    use ourios_core::otlp::{AnyValue, KeyValue as OtlpKeyValue, any_value};
    use ourios_core::tenant::TenantId;

    fn named(map: &ServiceTallyMap) -> BTreeSet<(String, String)> {
        map.iter()
            .flat_map(|(tenant, tallies)| {
                tallies
                    .by_name
                    .keys()
                    .map(|name| (tenant.as_str().to_owned(), name.clone()))
            })
            .collect()
    }

    fn touch(map: &mut ServiceTallyMap, tenant: &str, service: &str) {
        map.tally_mut(&TenantId::new(tenant.to_owned()), Some(service))
            .lines += 1;
    }

    #[test]
    fn service_tallies_never_exceed_the_budget() {
        let mut map = ServiceTallyMap::with_budget(16);
        for i in 0..1_000 {
            touch(&mut map, &format!("t{}", i % 3), &format!("s{i}"));
            assert!(map.named <= 16, "{} tallies after {i} inserts", map.named);
            assert_eq!(map.named, named(&map).len());
        }
    }

    #[test]
    fn eviction_drops_the_least_recently_touched_services() {
        let mut map = ServiceTallyMap::with_budget(16);
        for i in 0..16 {
            touch(&mut map, "t", &format!("s{i}"));
        }
        touch(&mut map, "t", "s0");
        touch(&mut map, "t", "new");

        let left = named(&map);
        for gone in ["s1", "s2"] {
            assert!(!left.contains(&("t".into(), gone.into())), "{gone} kept");
        }
        for kept in ["s0", "s3", "s15", "new"] {
            assert!(left.contains(&("t".into(), kept.into())), "{kept} evicted");
        }
        assert_eq!(map.named, 15);
    }

    #[test]
    fn an_evicted_service_restarts_from_an_empty_tally() {
        let mut map = ServiceTallyMap::with_budget(8);
        touch(&mut map, "t", "old");
        for i in 0..8 {
            touch(&mut map, "t", &format!("s{i}"));
        }
        let back = map.tally_mut(&TenantId::new("t".to_owned()), Some("old"));
        assert_eq!(back.lines, 0);
    }

    #[test]
    fn evicted_tenants_release_their_table_capacity() {
        const BUDGET: usize = 64;
        let mut map = ServiceTallyMap::with_budget(BUDGET);
        for tenant in 0..20 {
            for service in 0..BUDGET {
                touch(&mut map, &format!("t{tenant}"), &format!("s{service}"));
            }
        }
        let capacity: usize = map.iter().map(|(_, t)| t.by_name.capacity()).sum();
        assert!(
            capacity <= 4 * BUDGET,
            "{capacity} table slots held for a budget of {BUDGET}"
        );
    }

    #[test]
    fn service_less_slots_sit_outside_the_budget() {
        let mut map = ServiceTallyMap::with_budget(1);
        touch(&mut map, "t0", "only");
        for i in 0..100 {
            map.tally_mut(&TenantId::new(format!("t{i}")), None).lines += 1;
        }
        assert_eq!(named(&map).len(), 1);
        assert_eq!(map.named, 1);
    }

    #[test]
    fn reservoir_nearest_rank_quantiles() {
        let mut r = Reservoir::default();
        for v in [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0] {
            r.observe(v);
        }
        // Nearest-rank p50 over 10 samples = rank ceil(0.5*10)=5 → 5th
        // smallest = 0.5; p01 = rank ceil(0.01*10)=1 → smallest = 0.1.
        // Computed off-lock from a snapshot, as the gauge callback does.
        assert!((super::quantile_of(r.snapshot(), 0.50).unwrap() - 0.5).abs() < 1e-9);
        assert!((super::quantile_of(r.snapshot(), 0.01).unwrap() - 0.1).abs() < 1e-9);
    }

    /// RFC 0001 §6.8 fixes the `ourios.miner.confidence` buckets.
    #[test]
    fn confidence_buckets_are_the_rfc_0001_6_8_boundaries() {
        assert_eq!(
            super::CONFIDENCE_BUCKETS,
            [
                0.1, 0.3, 0.5, 0.7, 0.9, 0.95, 0.99, 0.999, 1.0, 1.05, 1.2, 1.5, 2.0
            ],
        );
    }

    #[test]
    fn reservoir_empty_is_none() {
        assert!(super::quantile_of(Reservoir::default().snapshot(), 0.5).is_none());
    }

    #[test]
    fn reservoir_is_bounded() {
        let mut r = Reservoir::default();
        for i in 0..(super::RESERVOIR_CAP + 500) {
            r.observe(f64::from(u32::try_from(i).unwrap()));
        }
        assert_eq!(r.samples.len(), super::RESERVOIR_CAP);
    }

    #[test]
    fn ratio_zero_denominator_is_zero() {
        assert!((ratio(3, 0) - 0.0).abs() < f64::EPSILON);
        assert!((ratio(1, 4) - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn service_of_reads_service_name_string() {
        let attrs = vec![OtlpKeyValue {
            key: "service.name".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue("checkout".to_string())),
            }),
            ..Default::default()
        }];
        assert_eq!(service_of(&attrs).as_deref(), Some("checkout"));
    }

    #[test]
    fn service_of_absent_is_none() {
        // `ourios.service` is `recommended`: an absent source
        // `service.name` is omitted, not synthesized to a sentinel.
        assert_eq!(service_of(&[]), None);
    }

    #[test]
    fn service_of_empty_string_is_none() {
        let attrs = vec![OtlpKeyValue {
            key: "service.name".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(String::new())),
            }),
            ..Default::default()
        }];
        assert_eq!(service_of(&attrs), None);
    }
}
