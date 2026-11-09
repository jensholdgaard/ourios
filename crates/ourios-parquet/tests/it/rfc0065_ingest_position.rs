//! RFC 0065 §6.6 — the ingest-position columns (`ingest_group`,
//! `ingest_seq`, `ingest_index`) amending RFC 0005 §3.2.
//!
//! Records mined from a replicated log carry a position and their files
//! carry the three OPTIONAL columns, dictionary off and
//! `DELTA_BINARY_PACKED`, with row-group min/max statistics. Local-backend
//! records carry none and their files keep today's schema (RFC0065.16).
//! Readers treat the columns as absent in old files (RFC 0005 §3.9).

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{Field, Schema};
use bytes::Bytes;
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, IngestPosition, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    BatchError, DEFAULT_ZSTD_LEVEL, IngestColumns, PartitionKey, PromotedAttributes, Reader,
    ReaderError, Store, Writer, WriterError, columns, data_schema_with_promoted,
    encode_records_to_parquet, mined_records_to_batch,
};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Encoding, Repetition};
use parquet::file::metadata::{ColumnChunkMetaData, ParquetMetaData};
use parquet::file::statistics::Statistics;

const TS0: u64 = 1_775_127_480_000_000_000;

fn record(i: u64, position: Option<IngestPosition>) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("tenant-x"),
        template_id: 1 + i % 3,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: TS0 + i * 1_000,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: i.to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
        ingest_position: position,
    }
}

/// Two frames of a group's log: `seq` 40 holds records 0..5, `seq` 41
/// holds the rest, each indexed by its place in the frame.
fn positioned(n: u64) -> Vec<MinedRecord> {
    (0..n)
        .map(|i| {
            let (seq, index) = if i < 5 { (40, i) } else { (41, i - 5) };
            let position = IngestPosition {
                group: 7,
                seq,
                index: u32::try_from(index).expect("small index"),
            };
            record(i, Some(position))
        })
        .collect()
}

fn local(n: u64) -> Vec<MinedRecord> {
    (0..n).map(|i| record(i, None)).collect()
}

fn leaves(bytes: &Bytes) -> Vec<String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes.clone()).expect("metadata");
    builder
        .parquet_schema()
        .columns()
        .iter()
        .map(|c| c.path().string())
        .collect()
}

fn metadata(bytes: &Bytes) -> Arc<ParquetMetaData> {
    ParquetRecordBatchReaderBuilder::try_new(bytes.clone())
        .expect("metadata")
        .metadata()
        .clone()
}

fn write_through_writer(records: &[MinedRecord], positions: bool) -> Bytes {
    let dir = tempfile::TempDir::new().expect("temp");
    let store = Store::local(dir.path()).expect("store");
    let partition = PartitionKey::derive(&records[0]).expect("derive");
    let mut writer = if positions {
        Writer::open_in_with_ingest_positions(
            &store,
            partition,
            DEFAULT_ZSTD_LEVEL,
            PromotedAttributes::default(),
        )
    } else {
        Writer::open_in(&store, partition)
    }
    .expect("open");
    writer.append_records(records).expect("append");
    let written = writer.close().expect("close");
    Bytes::from(store.get_blocking(&written.key).expect("get"))
}

fn read(bytes: Bytes) -> Result<Vec<MinedRecord>, ReaderError> {
    Reader::open_bytes(bytes)?.read_all()
}

/// RFC0065.16's file shape: a local write declares exactly the schema it
/// declared before RFC 0065, through both write entry points, and reads
/// back with no position.
#[test]
fn local_records_write_no_ingest_columns() {
    let records = local(20);
    let encoded =
        Bytes::from(encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode"));
    let written = write_through_writer(&records, false);
    for bytes in [encoded, written] {
        let schema = ParquetRecordBatchReaderBuilder::try_new(bytes.clone())
            .expect("metadata")
            .schema()
            .clone();
        let expected = data_schema_with_promoted(&PromotedAttributes::default());
        assert_eq!(
            schema.fields(),
            expected.fields(),
            "the local file declares the pre-RFC-0065 schema",
        );
        assert!(
            leaves(&bytes)
                .iter()
                .all(|leaf| !leaf.starts_with("ingest_")),
            "no ingest leaf: {:?}",
            leaves(&bytes),
        );
        assert_eq!(
            read(bytes).expect("read"),
            records,
            "absent columns read as no position"
        );
    }
}

/// The row-group min/max of a position column, widened to `i64`.
fn chunk_bounds(name: &str, chunk: &ColumnChunkMetaData) -> Option<(i64, i64)> {
    match chunk.statistics() {
        Some(Statistics::Int32(s)) => s
            .min_opt()
            .zip(s.max_opt())
            .map(|(lo, hi)| (i64::from(*lo), i64::from(*hi))),
        Some(Statistics::Int64(s)) => s.min_opt().copied().zip(s.max_opt().copied()),
        other => panic!("{name}: unexpected statistics {other:?}"),
    }
}

/// One column chunk is dictionary-free, `DELTA_BINARY_PACKED`, and
/// carries the expected min/max.
fn assert_chunk(name: &str, chunk: &ColumnChunkMetaData, min: i64, max: i64) {
    let encodings: Vec<Encoding> = chunk.encodings().collect();
    assert!(
        encodings.contains(&Encoding::DELTA_BINARY_PACKED),
        "{name}: {encodings:?}",
    );
    assert!(
        !encodings.contains(&Encoding::RLE_DICTIONARY)
            && !encodings.contains(&Encoding::PLAIN_DICTIONARY),
        "{name} has no dictionary: {encodings:?}",
    );
    assert!(chunk.dictionary_page_offset().is_none(), "{name}");
    assert_eq!(
        chunk_bounds(name, chunk),
        Some((min, max)),
        "{name} row-group min/max"
    );
}

/// Column `name` is OPTIONAL and every row group's chunk of it passes
/// [`assert_chunk`].
fn assert_position_column(meta: &ParquetMetaData, name: &str, min: i64, max: i64) {
    let descr = meta.file_metadata().schema_descr();
    let leaf = (0..descr.num_columns())
        .find(|&i| descr.column(i).path().string() == name)
        .unwrap_or_else(|| panic!("{name} is written"));
    assert_eq!(
        descr.column(leaf).self_type().get_basic_info().repetition(),
        Repetition::OPTIONAL,
        "{name} is OPTIONAL",
    );
    assert!(meta.num_row_groups() >= 1);
    for rg in meta.row_groups() {
        assert_chunk(name, rg.column(leaf), min, max);
    }
}

/// Positioned records round-trip their positions, and each of the three
/// columns is OPTIONAL, dictionary-free, `DELTA_BINARY_PACKED`, and
/// carries row-group min/max statistics.
#[test]
fn positions_round_trip_with_the_specified_encoding_and_statistics() {
    let records = positioned(12);
    let encoded =
        Bytes::from(encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode"));
    let written = write_through_writer(&records, true);
    for bytes in [encoded, written] {
        let meta = metadata(&bytes);
        assert_position_column(&meta, columns::INGEST_GROUP, 7, 7);
        assert_position_column(&meta, columns::INGEST_SEQ, 40, 41);
        assert_position_column(&meta, columns::INGEST_INDEX, 0, 6);
        assert_eq!(read(bytes).expect("read"), records, "positions round-trip");
    }
}

/// A batch mixing positioned and local records is rejected rather than
/// written with NULL positions, at every write entry point.
#[test]
fn a_mixed_batch_is_rejected() {
    let mut records = positioned(6);
    records[4].ingest_position = None;

    assert!(matches!(
        IngestColumns::of(&records),
        Err(BatchError::MixedIngestPositions { row_index: 4 }),
    ));
    assert!(matches!(
        mined_records_to_batch(&records),
        Err(BatchError::MixedIngestPositions { row_index: 4 }),
    ));
    assert!(matches!(
        encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL),
        Err(WriterError::Batch(BatchError::MixedIngestPositions {
            row_index: 4
        })),
    ));

    let dir = tempfile::TempDir::new().expect("temp");
    let store = Store::local(dir.path()).expect("store");
    let partition = PartitionKey::derive(&records[0]).expect("derive");
    let mut positioned_writer = Writer::open_in_with_ingest_positions(
        &store,
        partition.clone(),
        DEFAULT_ZSTD_LEVEL,
        PromotedAttributes::default(),
    )
    .expect("open");
    assert!(matches!(
        positioned_writer.append_records(&records),
        Err(WriterError::IngestColumnsMismatch {
            row_index: 4,
            declared: IngestColumns::Written,
        }),
    ));
    let mut local_writer = Writer::open_in(&store, partition).expect("open");
    assert!(matches!(
        local_writer.append_records(&positioned(3)),
        Err(WriterError::IngestColumnsMismatch {
            row_index: 0,
            declared: IngestColumns::Omitted,
        }),
    ));
    // Both pre-checks fire before any row lands: the writers stay usable.
    positioned_writer
        .append_records(&positioned(3))
        .expect("positioned rows");
    local_writer.append_records(&local(3)).expect("local rows");
}

/// Writes `batch` with stock writer properties, as a foreign or future
/// writer would.
fn write_batch(batch: &RecordBatch) -> Bytes {
    let mut buffer = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), None).expect("writer");
    writer.write(batch).expect("write");
    writer.close().expect("close");
    Bytes::from(buffer)
}

/// A positioned file with an unknown later column still decodes, its
/// positions intact (RFC 0005 §3.9 rule 1).
#[test]
fn an_unknown_column_beside_the_positions_is_ignored() {
    let records = positioned(8);
    let batch = mined_records_to_batch(&records).expect("batch");
    let mut fields: Vec<Field> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    fields.push(Field::new(
        "future_column",
        arrow_schema::DataType::Utf8,
        true,
    ));
    let mut arrays: Vec<ArrayRef> = batch.columns().to_vec();
    arrays.push(Arc::new(StringArray::from(vec![Some("x"); records.len()])));
    let wider = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("wider");

    assert_eq!(read(write_batch(&wider)).expect("read"), records);
}

/// A file carrying only some of the three columns names no record, so
/// the reader refuses it rather than dropping the key.
#[test]
fn a_partial_position_is_a_read_error() {
    let records = positioned(4);
    let batch = mined_records_to_batch(&records).expect("batch");
    let schema = batch.schema();
    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|&i| schema.field(i).name() != columns::INGEST_GROUP)
        .collect();
    let partial = batch.project(&keep).expect("project");

    let err = read(write_batch(&partial)).expect_err("partial position");
    assert!(
        matches!(err, ReaderError::Conversion { column, .. } if column == columns::INGEST_SEQ),
        "got {err:?}",
    );
}
