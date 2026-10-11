//! The five §6.8 observable gauges, registered once per meter provider.
//!
//! The `OTel` SDK never unregisters an observable-instrument callback:
//! dropping the gauge handle leaves the callback, and everything it
//! captures, with the provider until the provider itself goes. Gauges
//! registered per [`super::MinerMetrics`] would therefore keep every
//! dropped cluster's state alive and reporting stale values (#945).
//!
//! Instead, one set of callbacks is registered per provider. They read
//! a [`GaugeRegistry`] of `Weak` references to the live clusters'
//! states and prune the dead ones as they go, so a dropped cluster
//! stops reporting and frees its state, and a collection costs
//! O(live clusters).
//!
//! The binding follows the process-global provider: a cluster built
//! after `global::set_meter_provider` installs a new provider gets a
//! fresh registry with gauges on that provider, while clusters built
//! earlier keep reporting where they were registered. That is the
//! same attribution as a per-cluster registration, at one registration
//! per provider rather than one per cluster.

use std::sync::{Arc, Mutex, Weak};

use opentelemetry::metrics::{AsyncInstrument, Meter, MeterProvider, ObservableGauge};
use opentelemetry::{KeyValue, global};

use ourios_core::tenant::TenantId;
use ourios_semconv as semconv;

use super::{METER_NAME, MinerMetricsState, lock_recovering, quantile_of, ratio, service_attrs};

/// Registry length below which registration skips the prune pass.
const MIN_PRUNE_AT: usize = 64;

type SharedState = Arc<Mutex<MinerMetricsState>>;
type GlobalProvider = Arc<dyn MeterProvider + Send + Sync>;

/// The live clusters' gauge states for one provider's callbacks.
#[derive(Default)]
pub(super) struct GaugeRegistry {
    entries: Mutex<Entries>,
}

#[derive(Default)]
struct Entries {
    states: Vec<Weak<Mutex<MinerMetricsState>>>,
    /// Length at which the next registration prunes dead entries.
    /// Doubling from the post-prune length keeps the prune amortised
    /// O(1) per registration, and keeps the registry bounded when no
    /// collection ever runs (the no-op provider).
    prune_at: usize,
}

impl GaugeRegistry {
    pub(super) fn register(&self, state: &SharedState) {
        let mut entries = lock_recovering(&self.entries);
        if entries.states.len() >= entries.prune_at {
            entries.states.retain(|state| state.strong_count() > 0);
            entries.prune_at = (entries.states.len() * 2).max(MIN_PRUNE_AT);
        }
        entries.states.push(Arc::downgrade(state));
    }

    /// The live states, pruning the dropped ones from the registry.
    fn live(&self) -> Vec<SharedState> {
        let mut entries = lock_recovering(&self.entries);
        let mut live = Vec::with_capacity(entries.states.len());
        entries.states.retain(|state| match state.upgrade() {
            Some(state) => {
                live.push(state);
                true
            }
            None => false,
        });
        live
    }
}

/// The registry bound to one provider, plus that provider's gauge
/// handles, retained so the registration outlives this call whatever
/// the SDK does with a dropped handle.
struct Binding {
    provider: Weak<dyn MeterProvider + Send + Sync>,
    registry: Arc<GaugeRegistry>,
    _gauges: ObservableGauges,
}

static GLOBAL_BINDING: Mutex<Option<Binding>> = Mutex::new(None);

/// The registry whose gauges report to the current global provider,
/// registering them on that provider the first time it is seen.
pub(super) fn global_registry() -> Arc<GaugeRegistry> {
    registry_for(&global::meter_provider(), &GLOBAL_BINDING)
}

fn registry_for(provider: &GlobalProvider, slot: &Mutex<Option<Binding>>) -> Arc<GaugeRegistry> {
    let mut binding = lock_recovering(slot);
    match binding.as_ref() {
        // The binding's `Weak` keeps the provider allocation alive, so a
        // replaced provider's address is never reused and the comparison
        // cannot alias.
        Some(bound) if Weak::ptr_eq(&bound.provider, &Arc::downgrade(provider)) => {
            Arc::clone(&bound.registry)
        }
        _ => {
            let registry = Arc::new(GaugeRegistry::default());
            let gauges = register(&provider.meter(METER_NAME), &registry);
            *binding = Some(Binding {
                provider: Arc::downgrade(provider),
                registry: Arc::clone(&registry),
                _gauges: gauges,
            });
            registry
        }
    }
}

/// The five §6.8 observable gauges. `ourios.miner.template.count` is
/// the only `u64` gauge; the fractions and quantiles are `f64`.
pub(super) struct ObservableGauges {
    _template_count: ObservableGauge<u64>,
    _body_retention_utilization: ObservableGauge<f64>,
    _params_overflow_utilization: ObservableGauge<f64>,
    _confidence_p50: ObservableGauge<f64>,
    _confidence_p01: ObservableGauge<f64>,
}

/// Register the §6.8 observable gauges (`ourios.miner.template.count`,
/// `…confidence.p50`, `…confidence.p01`, `…body_retention.utilization`,
/// `…params.overflow.utilization`) with callbacks over `registry`. A
/// callback emits one data point per `(tenant, service)` (or per
/// tenant) of each live cluster — nothing until real traffic
/// populates a state.
pub(super) fn register(meter: &Meter, registry: &Arc<GaugeRegistry>) -> ObservableGauges {
    let reg = Arc::clone(registry);
    let template_count = meter
        .u64_observable_gauge(semconv::OURIOS_MINER_TEMPLATE_COUNT)
        .with_unit("{template}")
        .with_callback(move |obs| {
            for state in reg.live() {
                observe_template_counts(&lock_recovering(&state), obs);
            }
        })
        .build();

    let reg = Arc::clone(registry);
    let body_retention_utilization = meter
        .f64_observable_gauge(semconv::OURIOS_MINER_BODY_RETENTION_UTILIZATION)
        .with_unit("1")
        .with_callback(move |obs| {
            for state in reg.live() {
                observe_body_retention(&lock_recovering(&state), obs);
            }
        })
        .build();

    let reg = Arc::clone(registry);
    let params_overflow_utilization = meter
        .f64_observable_gauge(semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION)
        .with_unit("1")
        .with_callback(move |obs| {
            for state in reg.live() {
                observe_params_overflow(&lock_recovering(&state), obs);
            }
        })
        .build();

    ObservableGauges {
        _template_count: template_count,
        _body_retention_utilization: body_retention_utilization,
        _params_overflow_utilization: params_overflow_utilization,
        _confidence_p50: register_quantile(
            meter,
            registry,
            semconv::OURIOS_MINER_CONFIDENCE_P50,
            0.50,
        ),
        _confidence_p01: register_quantile(
            meter,
            registry,
            semconv::OURIOS_MINER_CONFIDENCE_P01,
            0.01,
        ),
    }
}

fn tenant_attr(tenant: &TenantId) -> [KeyValue; 1] {
    [KeyValue::new(
        semconv::OURIOS_TENANT,
        tenant.as_str().to_owned(),
    )]
}

fn observe_template_counts(state: &MinerMetricsState, obs: &dyn AsyncInstrument<u64>) {
    for (tenant, count) in &state.template_counts {
        obs.observe(*count, &tenant_attr(tenant));
    }
}

fn observe_body_retention(state: &MinerMetricsState, obs: &dyn AsyncInstrument<f64>) {
    for (tenant, lines) in &state.body_lines {
        let retained = state.body_retentions.get(tenant).copied().unwrap_or(0);
        obs.observe(ratio(retained, *lines), &tenant_attr(tenant));
    }
}

fn observe_params_overflow(state: &MinerMetricsState, obs: &dyn AsyncInstrument<f64>) {
    for (tenant, tallies) in &state.by_service {
        for (service, tally) in tallies.iter() {
            obs.observe(
                ratio(tally.overflow_lines, tally.lines),
                &service_attrs(tenant, service),
            );
        }
    }
}

/// One `(tenant, service)` confidence window, copied out of a state.
type ConfidenceWindow = (TenantId, Option<String>, Vec<f64>);

fn confidence_windows(state: &MinerMetricsState, out: &mut Vec<ConfidenceWindow>) {
    for (tenant, tallies) in &state.by_service {
        for (service, tally) in tallies.iter() {
            out.push((
                tenant.clone(),
                service.map(str::to_owned),
                tally.confidence.snapshot(),
            ));
        }
    }
}

/// Register one confidence-quantile observable gauge
/// (`ourios.miner.confidence.p50` / `…p01`) over the
/// per-`(tenant, service)` reservoirs.
fn register_quantile(
    meter: &Meter,
    registry: &Arc<GaugeRegistry>,
    name: &'static str,
    q: f64,
) -> ObservableGauge<f64> {
    let reg = Arc::clone(registry);
    meter
        .f64_observable_gauge(name)
        .with_unit("1")
        .with_callback(move |obs| {
            // Snapshot each window under its state's lock (a cheap O(n)
            // copy), then sort + compute the quantiles AFTER releasing
            // it — collection must never block the ingest hot path on
            // the O(n log n) sort.
            let mut windows = Vec::new();
            for state in reg.live() {
                confidence_windows(&lock_recovering(&state), &mut windows);
            }
            for (tenant, service, samples) in windows {
                if let Some(v) = quantile_of(samples, q) {
                    obs.observe(v, &service_attrs(&tenant, service.as_deref()));
                }
            }
        })
        .build()
}

#[cfg(test)]
mod tests;
