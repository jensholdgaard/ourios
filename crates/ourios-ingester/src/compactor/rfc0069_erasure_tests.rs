//! RFC0069.5: an erasure stays pending past a row whose attributes cannot
//! be read, since that row may be the conversation's.

use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};
use ourios_core::record::MinedRecord;
use ourios_parquet::{PartitionKey, ShapeValidation, Store, Writer, batch_to_degraded_records};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tracing_subscriber::prelude::*;

use super::tests::{NOW_SEALED, TS0, rec, store_at};
use super::*;

/// A value nested well past the reader's depth bound: the writer encodes
/// it, the reader cannot decode it (RFC 0069 §3.1).
fn too_deep() -> KeyValue {
    let mut value = AnyValue {
        value: Some(Value::StringValue("leaf".to_owned())),
    };
    for _ in 0..100 {
        value = AnyValue {
            value: Some(Value::KvlistValue(KeyValueList {
                values: vec![KeyValue {
                    key: "k".to_owned(),
                    value: Some(value),
                    key_strindex: 0,
                }],
            })),
        };
    }
    KeyValue {
        key: "deep".to_owned(),
        value: Some(value),
        key_strindex: 0,
    }
}

/// The conversation an erasure here targets: template 1's rows.
fn erase_template_1(record: &MinedRecord, _: &str) -> bool {
    record.template_id == 1
}

/// The partition's rows, each undecodable cell degraded, read from every
/// object under it.
fn stored_rows(store: &Store) -> Vec<MinedRecord> {
    let mut rows = Vec::new();
    for key in store.list_blocking(Some("data/")).expect("list") {
        if !key.ends_with(".parquet") {
            continue;
        }
        let bytes = store.get_blocking(&key).expect("get");
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))
            .expect("open")
            .build()
            .expect("reader");
        for batch in reader {
            let batch = batch.expect("batch");
            let decoded =
                batch_to_degraded_records(&batch, 0, ShapeValidation::Skip).expect("decode");
            rows.extend(decoded.records);
        }
    }
    rows.sort_by_key(|r| r.time_unix_nano);
    rows
}

type EventFields = Vec<(String, String)>;

/// The fields of each undecodable-row event `run` logs.
fn undecodable_events(run: impl FnOnce()) -> Vec<EventFields> {
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
            if event.metadata().name() != ourios_semconv::EVENT_OURIOS_READER_ROW_UNDECODABLE {
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

/// Scenario RFC0069.5: the erasure rewrite carries the row whose
/// attributes do not decode forward byte for byte, drops the
/// conversation's other rows, leaves the marker in the rows phase, does not
/// report the erasure complete, and logs the row. See
/// `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[test]
fn rfc0069_5_erasure_stays_pending_past_an_undecodable_identity() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = store_at(bucket.path());
    let unreadable = MinedRecord {
        attributes: vec![too_deep()],
        ..rec("a", 1, TS0 + 1)
    };
    let rows = [rec("a", 1, TS0), unreadable, rec("a", 2, TS0 + 2)];
    let partition = PartitionKey::derive(&rows[0]).expect("derive");
    let mut writer = Writer::open_in(&store, partition).expect("open writer");
    writer.append_records(&rows).expect("append");
    let input = writer.close().expect("close").key;
    let stored = ourios_core::otlp::canonical::encode_attributes(&[too_deep()]).expect("encode");
    request_erasure(&store, "a", "c-1").expect("request");

    let mut report = None;
    let events = undecodable_events(|| {
        let mut hooks = SweepHooks {
            observe: None,
            erasure_match: Some(&erase_template_1),
            gauges: None,
        };
        report = Some(
            run_sweep_committing(
                &store,
                SweepClock::sealed_at(NOW_SEALED),
                &CompactionPolicy::default(),
                &PromotedAttributes::default(),
                &mut hooks,
                &mut |_: &PartitionCommitted<'_>| {},
            )
            .expect("sweep"),
        );
    });
    let report = report.expect("swept");

    let [outcome] = report.erasures.as_slice() else {
        panic!("one erasure, got {:?}", report.erasures);
    };
    assert_eq!(
        outcome.phase,
        ErasurePhase::Rows,
        "the marker does not advance"
    );
    assert!(!outcome.finished, "the erasure is not reported complete");
    assert_eq!(
        pending_erasures(&store).expect("pending")[0].phase,
        ErasurePhase::Rows,
        "the stored marker is still in the rows phase"
    );

    let kept = stored_rows(&store);
    let templates: Vec<u64> = kept.iter().map(|r| r.template_id).collect();
    assert_eq!(
        templates,
        [1, 2],
        "the readable conversation row is dropped"
    );
    let Some(Value::StringValue(cell)) = kept[0].attributes[0]
        .value
        .as_ref()
        .and_then(|v| v.value.as_ref())
    else {
        panic!("the unreadable row is degraded: {:?}", kept[0].attributes);
    };
    assert_eq!(cell.as_bytes(), stored.as_slice(), "carried byte for byte");

    let expected: EventFields = vec![
        ("file.path".to_owned(), input),
        (
            ourios_semconv::OURIOS_READER_ROW_INDEX.to_owned(),
            "1".to_owned(),
        ),
        (
            ourios_semconv::OURIOS_READER_COLUMN_NAME.to_owned(),
            ourios_parquet::columns::ATTRIBUTES.to_owned(),
        ),
    ];
    let logged: Vec<EventFields> = events
        .into_iter()
        .map(|fields| {
            fields
                .into_iter()
                .filter(|(name, _)| name != "message")
                .collect()
        })
        .collect();
    assert_eq!(logged, [expected], "the row is logged once, by its cell");
}
