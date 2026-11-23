//! RFC0068.5: a heal is reported as one `WARN` event and counted.

use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use ourios_wal::{FrameKind, FrameSink, RecoveryError, TailHeal, Wal, WalConfig, WalOffset};
use tracing_subscriber::prelude::*;

struct Discard;

impl FrameSink for Discard {
    fn consume(&mut self, _: WalOffset, _: FrameKind, _: &[u8]) -> Result<(), RecoveryError> {
        Ok(())
    }
}

fn config(root: &std::path::Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

/// The heal a replay makes after `tail` is appended to a newest segment
/// holding one synced frame.
fn heal_of(tail: &[u8]) -> TailHeal {
    let root = tempfile::tempdir().expect("temp");
    let mut wal = Wal::open(config(root.path())).expect("open");
    wal.append(FrameKind::OtlpBatch, b"acknowledged")
        .expect("append");
    wal.sync().expect("sync");
    drop(wal);
    let segment = std::fs::read_dir(root.path())
        .expect("read_dir")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .expect("a segment");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .expect("open segment")
        .write_all(tail)
        .expect("forge the tail");
    let mut wal = Wal::open(config(root.path())).expect("reopen");
    wal.replay(&mut Discard).expect("replay heals the tail");
    wal.healed_tail().expect("the replay healed a tail").clone()
}

/// One logged event's fields, as `(name, value)`.
type EventFields = Vec<(String, String)>;

/// The fields of each tail-truncated event `run` logs.
fn truncated_events(run: impl FnOnce()) -> Vec<EventFields> {
    #[derive(Default)]
    struct Fields(EventFields);
    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push((field.name().to_owned(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push((field.name().to_owned(), value.to_owned()));
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.push((field.name().to_owned(), value.to_string()));
        }
    }
    struct Capture(Arc<Mutex<Vec<EventFields>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().name() != ourios_semconv::EVENT_OURIOS_WAL_RECOVERY_TAIL_TRUNCATED {
                return;
            }
            assert_eq!(*event.metadata().level(), tracing::Level::WARN);
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(fields.0);
        }
    }
    crate::global_subscriber();
    let events = Arc::default();
    let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&events)));
    tracing::subscriber::with_default(subscriber, run);
    events
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The truncated-size counter's total per tail shape.
fn truncated_by_shape(metrics: &[ResourceMetrics]) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let sums = metrics
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == ourios_semconv::OURIOS_WAL_RECOVERY_TRUNCATED_SIZE);
    for metric in sums {
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("the truncated size is a u64 counter");
        };
        for point in sum.data_points() {
            let shape = point
                .attributes()
                .find(|kv| kv.key.as_str() == ourios_semconv::OURIOS_WAL_RECOVERY_TAIL_SHAPE)
                .map(|kv| kv.value.as_str().into_owned())
                .unwrap_or_default();
            out.push((shape, point.value()));
        }
    }
    out
}

fn field<'a>(event: &'a [(String, String)], name: &str) -> &'a str {
    event
        .iter()
        .find(|(n, _)| n == name)
        .map_or_else(|| panic!("the event carries {name}"), |(_, v)| v.as_str())
}

fn assert_reported(heal: &TailHeal, shape: &str) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let meter = provider.meter_with_scope(ourios_core::telemetry::scope("ourios.wal"));

    let events = truncated_events(|| super::report(heal, &meter));
    provider.force_flush().expect("flush");

    let [event] = events.as_slice() else {
        panic!("exactly one tail-truncated event, got {events:?}");
    };
    assert_eq!(
        field(event, "file.path"),
        heal.segment.display().to_string()
    );
    assert_eq!(field(event, "file.size"), heal.size.to_string());
    assert_eq!(
        field(event, ourios_semconv::OURIOS_WAL_RECOVERY_TAIL_OFFSET),
        heal.offset.to_string()
    );
    assert_eq!(
        field(event, ourios_semconv::OURIOS_WAL_RECOVERY_TAIL_SHAPE),
        shape
    );
    let metrics = exporter.get_finished_metrics().expect("metrics");
    assert_eq!(
        truncated_by_shape(&metrics),
        vec![(shape.to_owned(), heal.size - heal.offset)],
        "the counter grows by E - B under the heal's shape",
    );
}

#[test]
fn rfc0068_5_a_zero_tail_heal_is_reported() {
    let heal = heal_of(&[0; 700]);
    assert_eq!(heal.size - heal.offset, 700);
    assert_reported(&heal, "zero_tail");
}

#[test]
fn rfc0068_5_a_partial_frame_heal_is_reported() {
    let heal = heal_of(&[0xAB; 5]);
    assert_eq!(heal.size - heal.offset, 5);
    assert_reported(&heal, "partial_frame");
}
