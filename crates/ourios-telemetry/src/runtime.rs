//! The `ourios.runtime.*` metrics: the stable parts of tokio's
//! [`RuntimeMetrics`](tokio::runtime::RuntimeMetrics) for the runtime
//! telemetry starts on, observed when the reader collects.
//!
//! `OTel` defines no cross-runtime metric and Ourios owns no `tokio.*`
//! namespace, so the names are Ourios-scoped. Blocking-pool size and busy
//! time need `tokio_unstable` and are left out.

use opentelemetry::metrics::Meter;
use ourios_semconv as semconv;
use tokio::runtime::Handle;

/// The instrumentation scope the runtime metrics are recorded under.
pub(crate) const SCOPE: &str = "ourios.runtime";

/// Register the runtime metrics for `runtime` on `meter`. The callbacks
/// live as long as the meter provider does.
pub(crate) fn register(meter: &Meter, runtime: &Handle) {
    let observed = [
        (
            semconv::OURIOS_RUNTIME_WORKER_COUNT,
            "{worker}",
            tokio::runtime::RuntimeMetrics::num_workers as fn(&_) -> usize,
        ),
        (
            semconv::OURIOS_RUNTIME_TASK_COUNT,
            "{task}",
            tokio::runtime::RuntimeMetrics::num_alive_tasks,
        ),
        (
            semconv::OURIOS_RUNTIME_GLOBAL_QUEUE_SIZE,
            "{task}",
            tokio::runtime::RuntimeMetrics::global_queue_depth,
        ),
    ];
    for (name, unit, read) in observed {
        let metrics = runtime.metrics();
        meter
            .i64_observable_up_down_counter(name)
            .with_unit(unit)
            .with_callback(move |observer| {
                observer.observe(i64::try_from(read(&metrics)).unwrap_or(i64::MAX), &[]);
            })
            .build();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use ourios_semconv as semconv;

    /// The value of an attribute-free i64 `UpDownCounter` in `exported`.
    fn up_down(exported: &[ResourceMetrics], name: &str) -> Option<(i64, String)> {
        exported
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .filter(|metric| metric.name() == name)
            .find_map(|metric| match metric.data() {
                AggregatedMetrics::I64(MetricData::Sum(sum)) if !sum.is_monotonic() => sum
                    .data_points()
                    .find(|point| point.attributes().next().is_none())
                    .map(|point| (point.value(), metric.unit().to_owned())),
                _ => None,
            })
    }

    /// With no flush asked for, the periodic reader exports the runtime
    /// metrics on its own interval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_runtime_metrics_export_at_the_metric_interval() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone())
            .with_interval(Duration::from_millis(50))
            .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        super::register(
            &provider.meter_with_scope(crate::scope(super::SCOPE)),
            &tokio::runtime::Handle::current(),
        );
        let parked = tokio::spawn(std::future::pending::<()>());

        let deadline = Instant::now() + Duration::from_secs(10);
        let collected = loop {
            let finished = exporter.get_finished_metrics().expect("exported");
            if up_down(&finished, semconv::OURIOS_RUNTIME_WORKER_COUNT).is_some() {
                break finished;
            }
            assert!(Instant::now() < deadline, "nothing exported in 10 s");
            std::thread::sleep(Duration::from_millis(20));
        };
        parked.abort();

        assert_eq!(
            up_down(&collected, semconv::OURIOS_RUNTIME_WORKER_COUNT),
            Some((2, "{worker}".to_owned()))
        );
        let tasks = up_down(&collected, semconv::OURIOS_RUNTIME_TASK_COUNT).expect("task count");
        assert!(tasks.0 >= 1 && tasks.1 == "{task}", "{tasks:?}");
        let queued = up_down(&collected, semconv::OURIOS_RUNTIME_GLOBAL_QUEUE_SIZE)
            .expect("global queue size");
        assert!(queued.0 >= 0 && queued.1 == "{task}", "{queued:?}");
        provider.shutdown().expect("shutdown");
    }
}
