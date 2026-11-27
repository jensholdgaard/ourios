//! RFC0069.5: an erasure stays pending past a row whose attributes cannot
//! be read, since that row may be the conversation's.

use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};
use ourios_core::record::MinedRecord;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PartitionKey, ShapeValidation, Store, Writer, batch_to_degraded_records,
};
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
/// object under it, by row time.
fn stored_rows(store: &Store) -> Vec<MinedRecord> {
    rows_with_cells(store, &[])
        .into_iter()
        .map(|(record, _)| record)
        .collect()
}

/// A row's cells of the requested columns: each as its one-row array's
/// `Debug` rendering, equal exactly when the stored values are, or `None`
/// when `NULL`.
type Cells = Vec<Option<String>>;

/// The partition's rows by row time, each undecodable cell degraded, with
/// their cells of `columns`.
fn rows_with_cells(store: &Store, columns: &[&str]) -> Vec<(MinedRecord, Cells)> {
    let mut rows: Vec<(MinedRecord, Cells)> = store
        .list_blocking(Some("data/"))
        .expect("list")
        .into_iter()
        .filter(|key| key.ends_with(".parquet"))
        .flat_map(|key| object_rows(store, &key, columns))
        .collect();
    rows.sort_by_key(|(record, _)| record.time_unix_nano);
    rows
}

/// The rows of object `key`, as [`rows_with_cells`] gives them.
fn object_rows(store: &Store, key: &str, columns: &[&str]) -> Vec<(MinedRecord, Cells)> {
    let bytes = store.get_blocking(key).expect("get");
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))
        .expect("open")
        .build()
        .expect("reader");
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.expect("batch");
        let decoded = batch_to_degraded_records(&batch, 0, ShapeValidation::Skip).expect("decode");
        for (i, record) in decoded.records.into_iter().enumerate() {
            let cells = columns
                .iter()
                .map(|column| {
                    let cells = batch.column_by_name(column).expect("column");
                    (!cells.is_null(i)).then(|| format!("{:?}", cells.slice(i, 1)))
                })
                .collect();
            rows.push((record, cells));
        }
    }
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
    let input = seed_partition(&store);
    let stored = ourios_core::otlp::canonical::encode_attributes(&[too_deep()]).expect("encode");
    request_erasure(&store, "a", "c-1").expect("request");

    let (report, events) = sweep_logged(&store, &PromotedAttributes::default());

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
    assert_eq!(
        degraded_cell(&kept[0]),
        stored.as_slice(),
        "carried byte for byte"
    );
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
    assert_eq!(events, [expected], "the row is logged once, by its cell");
}

/// One flush file holding a readable row of the conversation, its row 1
/// whose attributes do not decode, and a row of another conversation; its
/// key.
fn seed_partition(store: &Store) -> String {
    let unreadable = MinedRecord {
        attributes: vec![too_deep()],
        ..rec("a", 1, TS0 + 1)
    };
    let rows = [rec("a", 1, TS0), unreadable, rec("a", 2, TS0 + 2)];
    let partition = PartitionKey::derive(&rows[0]).expect("derive");
    let mut writer = Writer::open_in(store, partition).expect("open writer");
    writer.append_records(&rows).expect("append");
    writer.close().expect("close").key
}

/// One sweep erasing template 1's rows, and the fields of each
/// undecodable-row event it logs, less the message.
fn sweep_logged(store: &Store, promoted: &PromotedAttributes) -> (SweepReport, Vec<EventFields>) {
    let mut report = None;
    let events = undecodable_events(|| {
        let mut hooks = SweepHooks {
            observe: None,
            erasure_match: Some(&erase_template_1),
            gauges: None,
        };
        report = Some(
            run_sweep_committing(
                store,
                SweepClock::sealed_at(NOW_SEALED),
                &CompactionPolicy::default(),
                promoted,
                &mut hooks,
                &mut |_: &PartitionCommitted<'_>| {},
            )
            .expect("sweep"),
        );
    });
    let events = events
        .into_iter()
        .map(|fields| {
            fields
                .into_iter()
                .filter(|(name, _)| name != "message")
                .collect()
        })
        .collect();
    (report.expect("swept"), events)
}

/// The stored JSON a degraded row's reserved attribute holds.
fn degraded_cell(row: &MinedRecord) -> &[u8] {
    match row.attributes[0]
        .value
        .as_ref()
        .and_then(|v| v.value.as_ref())
    {
        Some(Value::StringValue(cell)) => cell.as_bytes(),
        _ => panic!("the unreadable row is degraded: {:?}", row.attributes),
    }
}

/// Scenario RFC0069.5, the rewrite's other half: under a declaration that
/// keeps `k` and newly promotes `m`, the erasure carries the unreadable
/// row's every other cell unchanged, keeps its stored `k`, and leaves its
/// `m` `NULL`, while a readable row projects both from its attributes.
#[test]
fn rfc0069_5_the_erasure_rewrite_keeps_the_row_and_its_promoted_boundary() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = store_at(bucket.path());
    let with_keys = |template, ts, mut attributes: Vec<KeyValue>| {
        attributes.extend([string("k", "x"), string("m", "y")]);
        MinedRecord {
            attributes,
            ..rec("a", template, ts)
        }
    };
    let rows = [
        with_keys(1, TS0, Vec::new()),
        with_keys(1, TS0 + 1, vec![too_deep()]),
        with_keys(2, TS0 + 2, Vec::new()),
    ];
    let written = PromotedAttributes::new(Vec::<String>::new(), ["k".to_owned()]);
    let partition = PartitionKey::derive(&rows[0]).expect("derive");
    let mut writer = Writer::open_in_with_promoted(&store, partition, DEFAULT_ZSTD_LEVEL, written)
        .expect("open writer");
    writer.append_records(&rows).expect("append");
    writer.close().expect("close");
    let before = stored_rows(&store);
    let before_cells = raw_cells(&store, TS0 + 1);
    request_erasure(&store, "a", "c-1").expect("request");

    let current = PromotedAttributes::new(Vec::<String>::new(), ["k".to_owned(), "m".to_owned()]);
    let (report, _) = sweep_logged(&store, &current);

    assert_eq!(
        report.erasures[0].phase,
        ErasurePhase::Rows,
        "still pending"
    );
    let after = stored_rows(&store);
    assert_eq!(after.len(), 2, "the readable conversation row is dropped");
    assert_eq!(after[0], before[1], "the unreadable row, decoded");
    assert_eq!(
        raw_cells(&store, TS0 + 1),
        before_cells,
        "the unreadable row's raw cells, as stored"
    );
    let x = || Some("x".to_owned());
    let y = || Some("y".to_owned());
    assert_eq!(
        promoted(&store, "attr.k"),
        [(TS0 + 1, x()), (TS0 + 2, x())],
        "k kept on the unreadable row, projected on the readable one"
    );
    assert_eq!(
        promoted(&store, "attr.m"),
        [(TS0 + 1, None), (TS0 + 2, y())],
        "m newly promoted: NULL where its cell cannot be read"
    );
}

fn string(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_owned())),
        }),
        key_strindex: 0,
    }
}

/// The columns every row carries as stored bytes (RFC 0069 §3.2).
const RAW_COLUMNS: [&str; 6] = [
    "body",
    "attributes",
    "resource_attributes",
    "scope_attributes",
    "params",
    "separators",
];

/// The raw cells of the partition's row at `time`.
fn raw_cells(store: &Store, time: u64) -> Cells {
    rows_with_cells(store, &RAW_COLUMNS)
        .into_iter()
        .find(|(record, _)| record.time_unix_nano == time)
        .map(|(_, cells)| cells)
        .expect("the row")
}

/// The partition's string `column` cells by row time, `None` when `NULL`.
fn promoted(store: &Store, column: &str) -> Vec<(u64, Option<String>)> {
    rows_with_cells(store, &[column])
        .into_iter()
        .map(|(record, cells)| (record.time_unix_nano, cells[0].as_deref().map(string_value)))
        .collect()
}

/// The value in a one-row string array's `Debug` rendering, which quotes
/// it: `StringArray\n[\n  "x",\n]`.
fn string_value(rendered: &str) -> String {
    let start = rendered.find('"').expect("a quoted value") + 1;
    let end = rendered.rfind('"').expect("a quoted value");
    rendered[start..end].to_owned()
}
