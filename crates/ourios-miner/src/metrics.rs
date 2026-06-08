//! RFC 0001 §6.8 telemetry instruments for the template miner.
//!
//! Instruments are resolved through the process-global meter
//! (`global::meter("ourios.miner")`) per the §6.8 *Export
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
//! [`tally_mut`]) and the reservoir is capped at [`RESERVOIR_CAP`].
//!
//! # Metric names and attributes
//!
//! Names (`ourios.miner.template.count`, `ourios.miner.merges`, …)
//! and data-point attribute keys (`ourios.tenant`, `ourios.service`,
//! `ourios.miner.template_change`) come from the generated
//! [`ourios_semconv`] constants — the dotted-`ourios.*` weaver
//! registry (`semconv/registry/`) alongside the compaction set
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
//! [`MinerMetricsState`] behind a `Mutex`, shared with the
//! callbacks via `Arc`.
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
use std::sync::{Arc, Mutex};

use opentelemetry::metrics::{Counter, Histogram, Meter, ObservableGauge};
use opentelemetry::{KeyValue, global};

use ourios_core::otlp::{KeyValue as OtlpKeyValue, any_value};
use ourios_core::tenant::TenantId;
use ourios_semconv as semconv;

/// Meter name per RFC 0001 §6.8 (`global::meter("ourios.miner")`).
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

/// Lock the metrics state, recovering the guard if the mutex was poisoned
/// by an unrelated panic. Telemetry is best-effort: a poisoned lock must
/// never crash an ingest write or a collection callback.
fn lock_state(state: &Mutex<MinerMetricsState>) -> std::sync::MutexGuard<'_, MinerMetricsState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Borrow the `(tenant, service)` tally for a mutable update,
/// allocating owned keys only on the first sight of the pair.
///
/// The hot path — a `(tenant, service)` already in flight — clones
/// nothing: the outer probe is `get_mut(&TenantId)` (no clone) and the
/// `Some(name)` inner probe is `get_mut(name)` keyed on a borrowed
/// `&str`. Only the cold first-insert path owns a key (the prior code
/// cloned the tenant *and* the service on every line). A `None` service
/// — a line whose source carried no `service.name` — lands in the
/// dedicated [`ServiceTallies::no_service`] slot, attributed to the
/// tenant alone (`ourios.service` omitted, see [`service_attrs`]).
fn tally_mut<'a>(
    by_service: &'a mut HashMap<TenantId, ServiceTallies>,
    tenant: &TenantId,
    service: Option<&str>,
) -> &'a mut ServiceTally {
    // `contains_key` releases its borrow before the follow-up `get_mut`
    // / `entry`, sidestepping the get-then-insert borrow-checker
    // limitation while still cloning a key only on first insert.
    if !by_service.contains_key(tenant) {
        by_service.insert(tenant.clone(), ServiceTallies::default());
    }
    let tallies = by_service
        .get_mut(tenant)
        .unwrap_or_else(|| unreachable!("inserted above when absent"));
    match service {
        None => &mut tallies.no_service,
        Some(name) if tallies.by_name.contains_key(name) => tallies
            .by_name
            .get_mut(name)
            .unwrap_or_else(|| unreachable!("contains_key was true")),
        Some(name) => tallies.by_name.entry(name.to_owned()).or_default(),
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
/// `confidence_p50` / `confidence_p01` quantile gauges.
#[derive(Default)]
struct ServiceTally {
    lines: u64,
    overflow_lines: u64,
    confidence: Reservoir,
}

/// Process state the observable-gauge callbacks read at collection
/// time. Guarded by a `Mutex` because `OTel` may invoke a callback
/// from any thread on collection.
#[derive(Default)]
struct MinerMetricsState {
    /// Per-tenant, per-optional-service tallies driving the ratio +
    /// quantile gauges. Nested (not a flat `(TenantId, _)` key) so the
    /// common hot-path update borrows the tenant for the outer lookup
    /// (`get_mut(tenant)`) and clones a key only on the first sight of
    /// a `(tenant, service)` pair. The inner `Option<String>` key is
    /// `None` for a line whose source carried no `service.name` — the
    /// line still counts toward the tenant, attributed without a
    /// service (`ourios.service` omitted, see [`service_attrs`]).
    by_service: HashMap<TenantId, ServiceTallies>,
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
/// approximation error. The `confidence` histogram remains the
/// §6.8 source of truth; the two gauges are named in-process views
/// per RFC0001.8. A future contract change could instead make p50/p01
/// backend-derived quantiles over the exported `confidence` histogram
/// (independent of the now-landed dotted-semconv naming); if so, this
/// in-process reservoir is replaced under that change's own review.
struct Reservoir {
    samples: VecDeque<f64>,
}

impl Default for Reservoir {
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(RESERVOIR_CAP),
        }
    }
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
/// observable gauges are registered with callbacks over the shared
/// [`MinerMetricsState`] at construction and need no per-call
/// maintenance beyond updating that state.
pub(crate) struct MinerMetrics {
    state: Arc<Mutex<MinerMetricsState>>,
    merges_total: Counter<u64>,
    parse_failures_total: Counter<u64>,
    params_overflow_total: Counter<u64>,
    template_version_changes_total: Counter<u64>,
    confidence: Histogram<f64>,
    miner_duration: Histogram<f64>,
    /// The observable gauges are held for the [`MinerMetrics`]'s
    /// lifetime so their collection callbacks stay registered with
    /// the meter — dropping a handle deregisters its callback, after
    /// which the gauge would vanish from the exported stream. The
    /// values are never read directly; the SDK invokes their
    /// callbacks on collect.
    _observable_gauges: ObservableGauges,
}

/// The five §6.8 observable gauges, retained to keep their callbacks
/// registered (see [`MinerMetrics`]). `ourios.miner.template.count`
/// is the only `u64` gauge; the fractions and quantiles are `f64`.
struct ObservableGauges {
    _template_count: ObservableGauge<u64>,
    _body_retention_utilization: ObservableGauge<f64>,
    _params_overflow_utilization: ObservableGauge<f64>,
    _confidence_p50: ObservableGauge<f64>,
    _confidence_p01: ObservableGauge<f64>,
}

impl MinerMetrics {
    /// Register every §6.8 instrument on the `ourios.miner` meter.
    /// The mandatory set is defined by the [`ourios_semconv`]
    /// registry; each instrument surfaces in the export on its first
    /// real measurement (§3.1.2).
    pub(crate) fn new() -> Self {
        let meter = global::meter(METER_NAME);
        let state = Arc::new(Mutex::new(MinerMetricsState::default()));

        let merges_total = meter
            .u64_counter(semconv::OURIOS_MINER_MERGES)
            .with_unit("{merge}")
            .build();
        let parse_failures_total = meter
            .u64_counter(semconv::OURIOS_MINER_PARSE_FAILURES)
            .with_unit("{failure}")
            .build();
        let params_overflow_total = meter
            .u64_counter(semconv::OURIOS_MINER_PARAMS_OVERFLOW)
            .with_unit("{overflow}")
            .build();
        let template_version_changes_total = meter
            .u64_counter(semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES)
            .with_unit("{change}")
            .build();
        let confidence = meter
            .f64_histogram(semconv::OURIOS_MINER_CONFIDENCE)
            .with_unit("1")
            .build();
        let miner_duration = meter
            .f64_histogram(semconv::OURIOS_MINER_DURATION)
            .with_unit("s")
            .build();

        let observable_gauges = Self::register_observable_gauges(&meter, &state);

        Self {
            state,
            merges_total,
            parse_failures_total,
            params_overflow_total,
            template_version_changes_total,
            confidence,
            miner_duration,
            _observable_gauges: observable_gauges,
        }
    }

    /// Register the §6.8 observable gauges
    /// (`ourios.miner.template.count`, `…confidence.p50`,
    /// `…confidence.p01`, `…body_retention.utilization`,
    /// `…params.overflow.utilization`) with callbacks over the shared
    /// state. A callback emits one data point per `(tenant, service)`
    /// (or per tenant) present in the state — nothing until real
    /// traffic populates it.
    ///
    /// Returns the gauge handles so the caller can retain them: a
    /// dropped handle deregisters its callback (see
    /// [`ObservableGauges`]).
    fn register_observable_gauges(
        meter: &Meter,
        state: &Arc<Mutex<MinerMetricsState>>,
    ) -> ObservableGauges {
        let s = Arc::clone(state);
        let template_count = meter
            .u64_observable_gauge(semconv::OURIOS_MINER_TEMPLATE_COUNT)
            .with_unit("{template}")
            .with_callback(move |obs| {
                let st = lock_state(&s);
                for (tenant, count) in &st.template_counts {
                    obs.observe(
                        *count,
                        &[KeyValue::new(
                            semconv::OURIOS_TENANT,
                            tenant.as_str().to_owned(),
                        )],
                    );
                }
            })
            .build();

        let s = Arc::clone(state);
        let body_retention_utilization = meter
            .f64_observable_gauge(semconv::OURIOS_MINER_BODY_RETENTION_UTILIZATION)
            .with_unit("1")
            .with_callback(move |obs| {
                let st = lock_state(&s);
                for (tenant, lines) in &st.body_lines {
                    let retained = st.body_retentions.get(tenant).copied().unwrap_or(0);
                    obs.observe(
                        ratio(retained, *lines),
                        &[KeyValue::new(
                            semconv::OURIOS_TENANT,
                            tenant.as_str().to_owned(),
                        )],
                    );
                }
            })
            .build();

        let s = Arc::clone(state);
        let params_overflow_utilization = meter
            .f64_observable_gauge(semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION)
            .with_unit("1")
            .with_callback(move |obs| {
                let st = lock_state(&s);
                for (tenant, tallies) in &st.by_service {
                    for (service, tally) in tallies.iter() {
                        obs.observe(
                            ratio(tally.overflow_lines, tally.lines),
                            &service_attrs(tenant, service),
                        );
                    }
                }
            })
            .build();

        let confidence_p50 =
            Self::register_quantile_gauge(meter, state, semconv::OURIOS_MINER_CONFIDENCE_P50, 0.50);
        let confidence_p01 =
            Self::register_quantile_gauge(meter, state, semconv::OURIOS_MINER_CONFIDENCE_P01, 0.01);

        ObservableGauges {
            _template_count: template_count,
            _body_retention_utilization: body_retention_utilization,
            _params_overflow_utilization: params_overflow_utilization,
            _confidence_p50: confidence_p50,
            _confidence_p01: confidence_p01,
        }
    }

    /// Register one confidence-quantile observable gauge
    /// (`ourios.miner.confidence.p50` / `…p01`) over the
    /// per-`(tenant, service)` reservoir, returning its handle.
    fn register_quantile_gauge(
        meter: &Meter,
        state: &Arc<Mutex<MinerMetricsState>>,
        name: &'static str,
        q: f64,
    ) -> ObservableGauge<f64> {
        let s = Arc::clone(state);
        meter
            .f64_observable_gauge(name)
            .with_unit("1")
            .with_callback(move |obs| {
                // Snapshot the sample windows under the lock (cheap O(n)
                // copy), then sort + compute the quantiles AFTER releasing
                // it — collection must never block the ingest hot path on
                // the O(n log n) sort.
                let snapshots: Vec<(TenantId, Option<String>, Vec<f64>)> = {
                    let st = lock_state(&s);
                    st.by_service
                        .iter()
                        .flat_map(|(tenant, tallies)| {
                            tallies.iter().map(move |(service, tally)| {
                                (
                                    tenant.clone(),
                                    service.map(str::to_owned),
                                    tally.confidence.snapshot(),
                                )
                            })
                        })
                        .collect()
                };
                for (tenant, service, samples) in snapshots {
                    if let Some(v) = quantile_of(samples, q) {
                        obs.observe(v, &service_attrs(&tenant, service.as_deref()));
                    }
                }
            })
            .build()
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
        let mut st = lock_state(&self.state);
        *st.body_lines.entry(tenant.clone()).or_insert(0) += 1;
        tally_mut(&mut st.by_service, tenant, service).lines += 1;
    }

    /// Observe one ingested line's `confidence` on both the §6.8
    /// histogram and the per-`(tenant, service)` reservoir feeding the
    /// p50/p01 gauges. The line denominator is bumped separately and
    /// earlier by [`Self::record_line_denominator`].
    pub(crate) fn record_line(&self, tenant: &TenantId, service: Option<&str>, confidence: f64) {
        self.confidence
            .record(confidence, &service_attrs(tenant, service));
        let mut st = lock_state(&self.state);
        tally_mut(&mut st.by_service, tenant, service)
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
        let mut st = lock_state(&self.state);
        tally_mut(&mut st.by_service, tenant, service).overflow_lines += 1;
    }

    /// Record one parse-failure line (§6.8 `ourios.miner.parse_failures`).
    pub(crate) fn record_parse_failure(&self, tenant: &TenantId, service: Option<&str>) {
        self.parse_failures_total
            .add(1, &service_attrs(tenant, service));
    }

    /// Record one body-retention event for the
    /// `ourios.miner.body_retention.utilization` numerator (§6.3 retention paths).
    pub(crate) fn record_body_retention(&self, tenant: &TenantId) {
        let mut st = lock_state(&self.state);
        *st.body_retentions.entry(tenant.clone()).or_insert(0) += 1;
    }

    /// Record one merge event (§6.8 `ourios.miner.merges`,
    /// `ourios.miner.template_change` attribute) and one
    /// `ourios.miner.template.version_changes` bump (every merge
    /// advances `template_version` per §6.7 / H5).
    pub(crate) fn record_merge(&self, tenant: &TenantId, event_type: &str) {
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
        let mut st = lock_state(&self.state);
        st.template_counts.insert(tenant.clone(), count);
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
    use super::{Reservoir, ratio, service_of};
    use ourios_core::otlp::{AnyValue, KeyValue as OtlpKeyValue, any_value};

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
