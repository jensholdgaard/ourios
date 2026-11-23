//! RFC0069.4: compaction carries a row with an undecodable cell in its
//! stored form, every raw cell byte for byte, through the in-memory sort,
//! spilled runs and an intermediate merge, and re-projects its promoted
//! columns under §3.2's preservation boundary.

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::TimestampNanosecondType;
use arrow_array::{Array, ArrayRef, BinaryArray, RecordBatch, StringArray};
use arrow_schema::DataType;
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use super::*;
use crate::columns;
use crate::promoted::{PromotedClass, PromotedKey};
use crate::record_batch::mined_records_to_batch_with_promoted;

const BAD_ATTRIBUTES: &str = r#"[{"key":"k","value":{"stringValue":"x"}"#;
const BAD_RESOURCE: &str = "{";
const BAD_SCOPE: &str = "not json";
const NOT_UTF8: &[u8] = b"line \xff\xfe";

/// The healthy row, the row whose log and resource attribute cells do
/// not decode, and the row whose body and scope attributes do not.
const HEALTHY: u64 = TS0;
const BAD_ATTRS: u64 = TS0 + 1;
const BAD_BODY: u64 = TS0 + 2;

fn any(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        key_strindex: 0,
    }
}

fn string(key: &str, value: &str) -> KeyValue {
    any(key, Value::StringValue(value.to_owned()))
}

/// A lossy row whose body is retained, holding `k`, `n` and `m` in its
/// log attributes and a service in its resource attributes.
fn row(ts: u64) -> MinedRecord {
    MinedRecord {
        attributes: vec![
            string("k", "x"),
            any("n", Value::IntValue(5)),
            string("m", "y"),
        ],
        resource_attributes: vec![string("service.name", "cart")],
        scope_attributes: vec![string("scope.k", "s")],
        body: Some(format!("line {ts}")),
        lossy_flag: true,
        confidence: 0.1,
        ..rec(1, ts)
    }
}

fn key(key: &str, class: PromotedClass) -> PromotedKey {
    PromotedKey {
        key: key.to_owned(),
        class,
    }
}

/// The declaration the degraded file was written under: `k` as a string
/// and `n` as an integer.
fn written_under() -> PromotedAttributes {
    PromotedAttributes::new_typed(
        [key("service.name", PromotedClass::String)],
        [
            key("k", PromotedClass::String),
            key("n", PromotedClass::I64),
        ],
    )
}

/// The declaration the rewrite runs under: `k` unchanged, `n` re-typed
/// to a double and `m` newly promoted.
fn rewritten_under() -> PromotedAttributes {
    PromotedAttributes::new_typed(
        [key("service.name", PromotedClass::String)],
        [
            key("k", PromotedClass::String),
            key("n", PromotedClass::F64),
            key("m", PromotedClass::String),
        ],
    )
}

/// `batch` with `column`'s cell at `row` replaced by `stored`.
fn corrupt(batch: &RecordBatch, column: &str, row: usize, stored: &[u8]) -> RecordBatch {
    let index = batch.schema().index_of(column).expect("column");
    let old = arrow_cast::cast(batch.column(index), &DataType::Binary).expect("as binary");
    let old = old.as_binary::<i32>();
    let cells: Vec<Option<&[u8]>> = (0..old.len())
        .map(|i| {
            if i == row {
                Some(stored)
            } else {
                old.is_valid(i).then(|| old.value(i))
            }
        })
        .collect();
    let replaced: ArrayRef = match batch.schema().field(index).data_type() {
        DataType::Binary => Arc::new(BinaryArray::from(cells)),
        _ => Arc::new(StringArray::from(
            cells
                .into_iter()
                .map(|cell| cell.map(|b| std::str::from_utf8(b).expect("utf-8 cell")))
                .collect::<Vec<_>>(),
        )),
    };
    let mut arrays = batch.columns().to_vec();
    arrays[index] = replaced;
    RecordBatch::try_new(batch.schema(), arrays).expect("batch")
}

/// The three-row file holding the undecodable cells.
fn degraded_batch() -> RecordBatch {
    let rows = [row(HEALTHY), row(BAD_ATTRS), row(BAD_BODY)];
    let batch = mined_records_to_batch_with_promoted(&rows, &written_under()).expect("batch");
    let batch = corrupt(&batch, columns::ATTRIBUTES, 1, BAD_ATTRIBUTES.as_bytes());
    let batch = corrupt(
        &batch,
        columns::RESOURCE_ATTRIBUTES,
        1,
        BAD_RESOURCE.as_bytes(),
    );
    let batch = corrupt(&batch, columns::SCOPE_ATTRIBUTES, 2, BAD_SCOPE.as_bytes());
    corrupt(&batch, columns::BODY, 2, NOT_UTF8)
}

fn put_batch(store: &Store, batch: &RecordBatch) {
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, batch.schema(), None).expect("writer");
    writer.write(batch).expect("write");
    writer.close().expect("close");
    let key = format!(
        "{}/{}.parquet",
        partition_data_prefix(&partition()),
        uuid::Uuid::now_v7()
    );
    store.put_blocking(&key, bytes).expect("put");
}

/// A partition of the degraded file and two healthy files, the second
/// with no promoted columns at all, compacted under [`rewritten_under`].
fn compact(tuning: SortTuning) -> (RecordBatch, Vec<RecordBatch>) {
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = store_at(bucket.path());
    let input = degraded_batch();
    put_batch(&store, &input);
    write_file(&store, &[row(TS0 + 3)]);
    write_file(&store, &[row(TS0 + 4)]);
    let promoted = rewritten_under();
    let outcome = compact_sorted(
        &store,
        &partition(),
        &promoted,
        ClusterKeys::for_promoted(&promoted),
        tuning,
    )
    .expect("compaction succeeds");
    let committed = outcome.committed.expect("committed");
    let key = format!("{}/{}", partition_data_prefix(&partition()), committed.file);
    let bytes = store.get_blocking(&key).expect("output");
    let output = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
        .expect("open output")
        .build()
        .expect("reader")
        .collect::<Result<_, _>>()
        .expect("batches");
    (input, output)
}

/// `column`'s cells of `batches` by row time, as stored bytes.
fn stored(batches: &[RecordBatch], column: &str) -> BTreeMap<u64, Option<Vec<u8>>> {
    let mut by_time = BTreeMap::new();
    for batch in batches {
        let cells = arrow_cast::cast(
            batch.column_by_name(column).expect("column"),
            &DataType::Binary,
        )
        .expect("as binary");
        let cells = cells.as_binary::<i32>();
        for (i, t) in times(batch).into_iter().enumerate() {
            by_time.insert(t, cells.is_valid(i).then(|| cells.value(i).to_vec()));
        }
    }
    by_time
}

/// `column`'s cells of `batches` by row time, as text.
fn text(batches: &[RecordBatch], column: &str) -> BTreeMap<u64, Option<String>> {
    let mut by_time = BTreeMap::new();
    for batch in batches {
        let cells = arrow_cast::cast(
            batch.column_by_name(column).expect("column"),
            &DataType::Utf8,
        )
        .expect("as text");
        let cells = cells.as_string::<i32>();
        for (i, t) in times(batch).into_iter().enumerate() {
            by_time.insert(t, cells.is_valid(i).then(|| cells.value(i).to_owned()));
        }
    }
    by_time
}

fn times(batch: &RecordBatch) -> Vec<u64> {
    batch
        .column_by_name(columns::TIME_UNIX_NANO)
        .expect("time")
        .as_primitive::<TimestampNanosecondType>()
        .values()
        .iter()
        .map(|&t| u64::try_from(t).expect("positive"))
        .collect()
}

fn tunings() -> [(&'static str, SortTuning); 2] {
    [
        ("in memory", SortTuning::default()),
        (
            "spilled",
            SortTuning {
                in_memory_max_bytes: 0,
                fan_in: 2,
                ..SortTuning::default()
            },
        ),
    ]
}

/// Scenario RFC0069.4: compaction succeeds, and every raw cell of a row
/// with an undecodable cell is unchanged in the output, the non-UTF-8
/// body included; no output row carries the reserved attribute. See
/// `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[test]
fn rfc0069_4_compaction_carries_an_undecodable_cell_byte_for_byte() {
    for (how, tuning) in tunings() {
        let (input, output) = compact(tuning);
        let rows: usize = output.iter().map(RecordBatch::num_rows).sum();
        assert_eq!(rows, 5, "{how}: every row carried");
        for column in [
            columns::BODY,
            columns::ATTRIBUTES,
            columns::RESOURCE_ATTRIBUTES,
            columns::SCOPE_ATTRIBUTES,
        ] {
            let (before, after) = (
                stored(std::slice::from_ref(&input), column),
                stored(&output, column),
            );
            for t in [BAD_ATTRS, BAD_BODY] {
                assert_eq!(
                    after[&t], before[&t],
                    "{how}: {column} of row {t} byte for byte"
                );
            }
        }
        for (t, cell) in text(&output, columns::ATTRIBUTES) {
            let cell = cell.unwrap_or_default();
            assert!(
                !cell.contains(ourios_semconv::OURIOS_ATTRIBUTE_UNDECODABLE),
                "{how}: row {t} carries the reserved attribute: {cell}"
            );
        }
    }
}

/// Scenario RFC0069.4, the preservation boundary: with `n` re-typed and
/// `m` newly promoted since the degraded file was written, a row whose
/// attribute cell decodes projects both from it as a healthy row does; a
/// row whose attribute cell is undecodable keeps `k`, promoted at the
/// same type, and its service, and has `n` and `m` `NULL`.
#[test]
fn rfc0069_4_promoted_columns_follow_the_preservation_boundary() {
    for (how, tuning) in tunings() {
        let (_, output) = compact(tuning);
        let schema = output[0].schema();
        let type_of = |name: &str| {
            schema
                .field_with_name(name)
                .expect(name)
                .data_type()
                .clone()
        };
        assert_eq!(
            type_of("attr.n"),
            DataType::Float64,
            "{how}: the current class"
        );
        let (k, n, m) = (
            text(&output, "attr.k"),
            text(&output, "attr.n"),
            text(&output, "attr.m"),
        );
        let service = text(&output, "resource.service.name");
        for t in [HEALTHY, BAD_BODY] {
            assert_eq!(k[&t].as_deref(), Some("x"), "{how}: k of row {t}");
            assert_eq!(
                n[&t].as_deref(),
                Some("5.0"),
                "{how}: n of row {t} from its cell"
            );
            assert_eq!(
                m[&t].as_deref(),
                Some("y"),
                "{how}: m of row {t} from its cell"
            );
        }
        assert_eq!(k[&BAD_ATTRS].as_deref(), Some("x"), "{how}: k kept");
        assert_eq!(n[&BAD_ATTRS], None, "{how}: a re-typed key is NULL");
        assert_eq!(m[&BAD_ATTRS], None, "{how}: a newly promoted key is NULL");
        assert_eq!(
            service[&BAD_ATTRS].as_deref(),
            Some("cart"),
            "{how}: service kept"
        );
    }
}

/// RFC 0069 §3.2, erasure's leg in the rewrite: a drop filter is never
/// offered a row whose log or resource attributes did not decode, so it
/// cannot drop it; the row is kept unchanged and reported by its file and
/// row. A row with only a bad body or scope still has its identity, and is
/// judged as usual.
#[test]
fn a_drop_filter_keeps_and_reports_a_row_it_cannot_identify() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = store_at(bucket.path());
    let input = degraded_batch();
    put_batch(&store, &input);
    let drop_all: &RowFilter<'_> = &|_| true;
    let mut hooks = RowHooks {
        drop: Some(drop_all),
        ..RowHooks::default()
    };

    let outcome = compact_partition_hooked(&store, &partition(), &written_under(), &mut hooks)
        .expect("compaction succeeds");

    assert_eq!(outcome.rows, 1, "only the unidentified row survives");
    assert_eq!(
        outcome.rows_dropped, 2,
        "the healthy row and the bad body's row"
    );
    let [unidentified] = outcome.unidentified.as_slice() else {
        panic!("one unidentified row, got {:?}", outcome.unidentified);
    };
    assert!(
        unidentified.file.ends_with(".parquet"),
        "{}",
        unidentified.file
    );
    assert_eq!(unidentified.row, 1);
    assert_eq!(
        unidentified.columns,
        [columns::ATTRIBUTES, columns::RESOURCE_ATTRIBUTES]
    );
}

/// RFC 0036 §3.1 holds for a row whose resource attributes did not
/// decode: it sorts by the `service.name` cell it keeps, so the output is
/// in the order its `sorting_columns` declare, in memory and spilled.
#[test]
fn a_row_with_unreadable_resource_attributes_sorts_by_its_kept_service() {
    let in_service_a = |ts: u64| MinedRecord {
        resource_attributes: vec![string("service.name", "a")],
        ..row(ts)
    };
    for (how, tuning) in tunings() {
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = store_at(bucket.path());
        put_batch(&store, &degraded_batch());
        write_file(&store, &[in_service_a(TS0 + 3), in_service_a(TS0 + 4)]);
        let promoted = rewritten_under();
        let outcome = compact_sorted(
            &store,
            &partition(),
            &promoted,
            ClusterKeys::for_promoted(&promoted),
            tuning,
        )
        .expect("compaction succeeds");
        let committed = outcome.committed.expect("committed");
        let key = format!("{}/{}", partition_data_prefix(&partition()), committed.file);
        let bytes = store.get_blocking(&key).expect("output");
        let mut services = Vec::new();
        for batch in ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
            .expect("open output")
            .build()
            .expect("reader")
        {
            let batch = batch.expect("batch");
            let column = batch
                .column_by_name("resource.service.name")
                .expect("column");
            let column = column.as_string::<i32>();
            services.extend((0..column.len()).map(|i| column.value(i).to_owned()));
        }
        assert_eq!(
            services,
            ["a", "a", "cart", "cart", "cart"],
            "{how}: in §3.1 order"
        );
    }
}
