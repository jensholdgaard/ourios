//! RFC 0001 §6.8 telemetry instruments for the template miner.
//!
//! Instruments are resolved through the process-global meter
//! (`global::meter("ourios.miner")`) per the §6.8 *Export
//! architecture* API/SDK split: this library depends only on the
//! lightweight `opentelemetry` API crate; the SDK + OTLP exporter
//! live in `ourios-telemetry`. With no provider installed every
//! `record` / `add` is a cheap no-op, so a [`MinerMetrics`] is
//! always safe to construct and drive.
//!
//! # Metric names and attributes (pre-redesign)
//!
//! The instrument names (`template_count`, `merges_total`, …) and
//! data-point attribute keys (`tenant_id`, `service`,
//! `event_type`) are the **flat, pre-redesign** identifiers the
//! §6.8 table pins. The dotted-`ourios.*` semconv conversion (and
//! the `semconv/registry/` weaver entries) is the deferred §6.8
//! redesign, not this slice — exactly as the existing
//! `alias_assertions_total` / `alias_retractions_total` counters
//! (`ourios_core::alias`) are named.
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
//! # Init-seeding
//!
//! §3.1.2 requires the full mandatory set to appear in the first
//! collection cycle even at zero traffic. `OTel`'s metric model is
//! collect-on-read, so a synchronous instrument that never recorded
//! contributes no data point. The synchronous **counters** are
//! seeded with a zero-`add` against an `init` sentinel attribute set
//! so they surface; histograms are seeded the same way and the
//! observable gauges always emit at least one (sentinel) point. See
//! [`MinerMetrics::new`].

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{KeyValue, global};

use ourios_core::otlp::{KeyValue as OtlpKeyValue, any_value};
use ourios_core::tenant::TenantId;

/// Meter name per RFC 0001 §6.8 (`global::meter("ourios.miner")`).
const METER_NAME: &str = "ourios.miner";

// §6.8 metric names — flat, pre-redesign identifiers.
const METRIC_TEMPLATE_COUNT: &str = "template_count";
const METRIC_MERGES_TOTAL: &str = "merges_total";
const METRIC_CONFIDENCE: &str = "confidence";
const METRIC_CONFIDENCE_P50: &str = "confidence_p50";
const METRIC_CONFIDENCE_P01: &str = "confidence_p01";
const METRIC_BODY_RETENTION_RATIO: &str = "body_retention_ratio";
const METRIC_PARSE_FAILURES_TOTAL: &str = "parse_failures_total";
const METRIC_PARAMS_OVERFLOW_TOTAL: &str = "params_overflow_total";
const METRIC_PARAMS_OVERFLOW_RATIO: &str = "params_overflow_ratio";
const METRIC_TEMPLATE_VERSION_CHANGES_TOTAL: &str = "template_version_changes_total";
const METRIC_MINER_LATENCY_SECONDS: &str = "miner_latency_seconds";

// §6.8 data-point attribute keys — flat, pre-redesign.
const ATTR_TENANT_ID: &str = "tenant_id";
const ATTR_SERVICE: &str = "service";
const ATTR_EVENT_TYPE: &str = "event_type";

/// Sentinel attribute value for init-seeded data points — keeps the
/// mandatory set visible at zero traffic without colliding with any
/// real `(tenant, service)` series.
const INIT_SENTINEL: &str = "__init__";

/// Sentinel `service` value for records whose `resource_attributes`
/// carry no `service.name`. Keeps the per-`(tenant, service)`
/// breakdown total even when the source did not set the key, rather
/// than silently dropping the line from the denominator.
const SERVICE_UNKNOWN: &str = "unknown";

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
/// `resource_attributes`, falling back to [`SERVICE_UNKNOWN`] when
/// absent or non-string. The proto `KeyValue` carries
/// `value: Option<AnyValue>`; only the `StringValue` variant is a
/// meaningful service identity.
#[must_use]
pub(crate) fn service_of(resource_attributes: &[OtlpKeyValue]) -> String {
    resource_attributes
        .iter()
        .find(|kv| kv.key == RESOURCE_SERVICE_NAME)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|av| av.value.as_ref())
        .and_then(|v| match v {
            any_value::Value::StringValue(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| SERVICE_UNKNOWN.to_string())
}

/// Per-`(tenant, service)` rolling tallies for the derived gauges.
///
/// `lines` is the denominator for `params_overflow_ratio`;
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
    /// Per-`(tenant, service)` tallies driving the ratio + quantile
    /// gauges.
    by_service: HashMap<(TenantId, String), ServiceTally>,
    /// Per-tenant template count, mirrored from the cluster so the
    /// `template_count` observable gauge can report it without
    /// borrowing the cluster.
    template_counts: HashMap<TenantId, u64>,
    /// Per-tenant body-retention numerator / line denominator for
    /// the `body_retention_ratio` gauge.
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
/// per RFC0001.8. If the deferred §6.8 dotted-semconv redesign
/// makes p50/p01 backend-derived quantiles over the exported
/// histogram instead, this in-process reservoir is replaced under
/// that redesign's own review (the RFC flags this exact fork).
struct Reservoir {
    samples: VecDeque<f64>,
}

impl Default for Reservoir {
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(0),
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

    /// Exact `q`-quantile (`q` in `[0.0, 1.0]`) over the current
    /// window via nearest-rank, or `None` when empty.
    fn quantile(&self, q: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("confidence values are never NaN"));
        let n = sorted.len();
        // Nearest-rank: rank = ceil(q * n), clamped to [1, n].
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let rank = (q * n as f64).ceil().max(1.0) as usize;
        Some(sorted[rank.min(n) - 1])
    }
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
    miner_latency_seconds: Histogram<f64>,
}

impl MinerMetrics {
    /// Register every §6.8 instrument on the `ourios.miner` meter
    /// and seed the synchronous instruments so the full mandatory
    /// set is exposed at zero traffic (§3.1.2).
    pub(crate) fn new() -> Self {
        let meter = global::meter(METER_NAME);
        let state = Arc::new(Mutex::new(MinerMetricsState::default()));

        let merges_total = meter
            .u64_counter(METRIC_MERGES_TOTAL)
            .with_unit("{merge}")
            .build();
        let parse_failures_total = meter
            .u64_counter(METRIC_PARSE_FAILURES_TOTAL)
            .with_unit("{failure}")
            .build();
        let params_overflow_total = meter
            .u64_counter(METRIC_PARAMS_OVERFLOW_TOTAL)
            .with_unit("{overflow}")
            .build();
        let template_version_changes_total = meter
            .u64_counter(METRIC_TEMPLATE_VERSION_CHANGES_TOTAL)
            .with_unit("{change}")
            .build();
        let confidence = meter
            .f64_histogram(METRIC_CONFIDENCE)
            .with_unit("1")
            .build();
        let miner_latency_seconds = meter
            .f64_histogram(METRIC_MINER_LATENCY_SECONDS)
            .with_unit("s")
            .build();

        Self::register_observable_gauges(&meter, &state);
        Self::seed_synchronous(
            &merges_total,
            &parse_failures_total,
            &params_overflow_total,
            &template_version_changes_total,
            &confidence,
            &miner_latency_seconds,
        );

        Self {
            state,
            merges_total,
            parse_failures_total,
            params_overflow_total,
            template_version_changes_total,
            confidence,
            miner_latency_seconds,
        }
    }

    /// Seed the synchronous instruments so they surface in the
    /// first collection cycle at zero traffic (§3.1.2). Counters
    /// take a zero-`add` (no distortion); the histograms are seeded
    /// with their natural sentinel value so they appear without
    /// polluting a real distribution (confidence at the §6.1 `1.0`
    /// clean sentinel, latency at `0.0`). All seeded points carry an
    /// `init` sentinel attribute set, distinguishable from real
    /// traffic.
    fn seed_synchronous(
        merges_total: &Counter<u64>,
        parse_failures_total: &Counter<u64>,
        params_overflow_total: &Counter<u64>,
        template_version_changes_total: &Counter<u64>,
        confidence: &Histogram<f64>,
        miner_latency_seconds: &Histogram<f64>,
    ) {
        let tenant_only = [KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL)];
        let tenant_event = [
            KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL),
            KeyValue::new(ATTR_EVENT_TYPE, INIT_SENTINEL),
        ];
        let tenant_service = [
            KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL),
            KeyValue::new(ATTR_SERVICE, INIT_SENTINEL),
        ];
        merges_total.add(0, &tenant_event);
        parse_failures_total.add(0, &tenant_service);
        params_overflow_total.add(0, &tenant_service);
        template_version_changes_total.add(0, &tenant_only);
        confidence.record(1.0, &tenant_service);
        miner_latency_seconds.record(0.0, &tenant_only);
    }

    /// Register the §6.8 observable gauges (`template_count`,
    /// `confidence_p50`, `confidence_p01`, `body_retention_ratio`,
    /// `params_overflow_ratio`) with callbacks over the shared
    /// state. Each callback always emits at least one data point
    /// (an `init` sentinel series when state is empty) so the
    /// gauges surface at zero traffic per §3.1.2.
    fn register_observable_gauges(meter: &Meter, state: &Arc<Mutex<MinerMetricsState>>) {
        let s = Arc::clone(state);
        let _template_count = meter
            .u64_observable_gauge(METRIC_TEMPLATE_COUNT)
            .with_unit("{template}")
            .with_callback(move |obs| {
                let st = s.lock().expect("metrics state mutex poisoned");
                if st.template_counts.is_empty() {
                    obs.observe(0, &[KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL)]);
                }
                for (tenant, count) in &st.template_counts {
                    obs.observe(
                        *count,
                        &[KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned())],
                    );
                }
            })
            .build();

        let s = Arc::clone(state);
        let _body_retention_ratio = meter
            .f64_observable_gauge(METRIC_BODY_RETENTION_RATIO)
            .with_unit("1")
            .with_callback(move |obs| {
                let st = s.lock().expect("metrics state mutex poisoned");
                if st.body_lines.is_empty() {
                    obs.observe(0.0, &[KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL)]);
                }
                for (tenant, lines) in &st.body_lines {
                    let retained = st.body_retentions.get(tenant).copied().unwrap_or(0);
                    obs.observe(
                        ratio(retained, *lines),
                        &[KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned())],
                    );
                }
            })
            .build();

        let s = Arc::clone(state);
        let _params_overflow_ratio = meter
            .f64_observable_gauge(METRIC_PARAMS_OVERFLOW_RATIO)
            .with_unit("1")
            .with_callback(move |obs| {
                let st = s.lock().expect("metrics state mutex poisoned");
                if st.by_service.is_empty() {
                    obs.observe(
                        0.0,
                        &[
                            KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL),
                            KeyValue::new(ATTR_SERVICE, INIT_SENTINEL),
                        ],
                    );
                }
                for ((tenant, service), tally) in &st.by_service {
                    obs.observe(
                        ratio(tally.overflow_lines, tally.lines),
                        &service_attrs(tenant, service),
                    );
                }
            })
            .build();

        Self::register_quantile_gauge(meter, state, METRIC_CONFIDENCE_P50, 0.50);
        Self::register_quantile_gauge(meter, state, METRIC_CONFIDENCE_P01, 0.01);
    }

    /// Register one confidence-quantile observable gauge
    /// (`confidence_p50` / `confidence_p01`) over the per-`(tenant,
    /// service)` reservoir.
    fn register_quantile_gauge(
        meter: &Meter,
        state: &Arc<Mutex<MinerMetricsState>>,
        name: &'static str,
        q: f64,
    ) {
        let s = Arc::clone(state);
        let _gauge = meter
            .f64_observable_gauge(name)
            .with_unit("1")
            .with_callback(move |obs| {
                let st = s.lock().expect("metrics state mutex poisoned");
                let mut emitted = false;
                for ((tenant, service), tally) in &st.by_service {
                    if let Some(v) = tally.confidence.quantile(q) {
                        obs.observe(v, &service_attrs(tenant, service));
                        emitted = true;
                    }
                }
                if !emitted {
                    obs.observe(
                        0.0,
                        &[
                            KeyValue::new(ATTR_TENANT_ID, INIT_SENTINEL),
                            KeyValue::new(ATTR_SERVICE, INIT_SENTINEL),
                        ],
                    );
                }
            })
            .build();
    }

    /// Record one ingested line for `(tenant, service)`: bumps the
    /// per-service line denominator (for `params_overflow_ratio`)
    /// and observes its `confidence` on both the §6.8 histogram and
    /// the per-service reservoir feeding the p50/p01 gauges.
    pub(crate) fn record_line(&self, tenant: &TenantId, service: &str, confidence: f64) {
        self.confidence
            .record(confidence, &service_attrs(tenant, service));
        let mut st = self.state.lock().expect("metrics state mutex poisoned");
        *st.body_lines.entry(tenant.clone()).or_insert(0) += 1;
        let tally = st
            .by_service
            .entry((tenant.clone(), service.to_owned()))
            .or_default();
        tally.lines += 1;
        tally.confidence.observe(confidence);
    }

    /// Record the miner's per-line latency (§6.8 `miner_latency_seconds`).
    pub(crate) fn record_latency(&self, tenant: &TenantId, seconds: f64) {
        self.miner_latency_seconds.record(
            seconds,
            &[KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned())],
        );
    }

    /// Record `count` per-parameter overflow events on one line for
    /// `(tenant, service)`: bumps the `params_overflow_total`
    /// counter and the per-service overflow-line numerator (the
    /// line is counted once toward the ratio regardless of how many
    /// of its params overflowed).
    pub(crate) fn record_overflow(&self, tenant: &TenantId, service: &str, count: u64) {
        if count == 0 {
            return;
        }
        self.params_overflow_total
            .add(count, &service_attrs(tenant, service));
        let mut st = self.state.lock().expect("metrics state mutex poisoned");
        st.by_service
            .entry((tenant.clone(), service.to_owned()))
            .or_default()
            .overflow_lines += 1;
    }

    /// Record one parse-failure line (§6.8 `parse_failures_total`).
    pub(crate) fn record_parse_failure(&self, tenant: &TenantId, service: &str) {
        self.parse_failures_total
            .add(1, &service_attrs(tenant, service));
    }

    /// Record one body-retention event for the
    /// `body_retention_ratio` numerator (§6.3 retention paths).
    pub(crate) fn record_body_retention(&self, tenant: &TenantId) {
        let mut st = self.state.lock().expect("metrics state mutex poisoned");
        *st.body_retentions.entry(tenant.clone()).or_insert(0) += 1;
    }

    /// Record one merge event (§6.8 `merges_total`, `event_type`
    /// attribute) and one `template_version_changes_total` bump
    /// (every merge advances `template_version` per §6.7 / H5).
    pub(crate) fn record_merge(&self, tenant: &TenantId, event_type: &str) {
        self.merges_total.add(
            1,
            &[
                KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned()),
                KeyValue::new(ATTR_EVENT_TYPE, event_type.to_owned()),
            ],
        );
        self.template_version_changes_total.add(
            1,
            &[KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned())],
        );
    }

    /// Mirror a tenant's current template count into the state the
    /// `template_count` observable gauge reads.
    pub(crate) fn set_template_count(&self, tenant: &TenantId, count: u64) {
        let mut st = self.state.lock().expect("metrics state mutex poisoned");
        st.template_counts.insert(tenant.clone(), count);
    }
}

/// `(tenant_id, service)` data-point attribute pair.
fn service_attrs(tenant: &TenantId, service: &str) -> [KeyValue; 2] {
    [
        KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned()),
        KeyValue::new(ATTR_SERVICE, service.to_owned()),
    ]
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
        assert!((r.quantile(0.50).unwrap() - 0.5).abs() < 1e-9);
        assert!((r.quantile(0.01).unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn reservoir_empty_is_none() {
        assert!(Reservoir::default().quantile(0.5).is_none());
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
        assert_eq!(service_of(&attrs), "checkout");
    }

    #[test]
    fn service_of_absent_is_unknown() {
        assert_eq!(service_of(&[]), "unknown");
    }
}
