//! The process-global meter and event capture every leg reads through.

use std::sync::OnceLock;

use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use ourios_telemetry::TelemetryGuard;
use ourios_telemetry::live_check::{self, Event, EventCapture};

/// The process-global installs every leg reads through.
pub(crate) struct Harness {
    metrics_guard: TelemetryGuard,
    metrics: InMemoryMetricExporter,
    events: &'static EventCapture,
}

pub(crate) fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let (metrics_guard, metrics) = ourios_telemetry::init_in_memory("ourios-rfc0052-7");
        Harness {
            metrics_guard,
            metrics,
            events: live_check::event_capture().expect("the only subscriber this binary installs"),
        }
    })
}

/// One leg at a time: every leg resets and reads the shared exporters.
pub(crate) async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = SERIAL.lock().await;
    harness().reset_events();
    guard
}

impl Harness {
    /// One collection, and only that one.
    pub(crate) fn collect(&self) -> Vec<ResourceMetrics> {
        self.metrics.reset();
        self.metrics_guard.force_flush().expect("force_flush");
        self.metrics
            .get_finished_metrics()
            .expect("metrics exported")
    }

    /// Forget the events emitted so far.
    pub(crate) fn reset_events(&self) {
        self.events.reset();
    }

    /// The named events emitted since the leg began, in order.
    pub(crate) fn events(&self) -> Vec<Event> {
        self.events.events()
    }
}
