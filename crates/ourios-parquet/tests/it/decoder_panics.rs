//! Files on which the upstream `parquet`/`arrow` decoders panic, instead of
//! returning an error, come back from every reader as a typed
//! `DecodePanicked` error naming the object; the process does not abort.
//! The fixtures under `testdata/corrupt-parquet/` are `audit_reader` fuzz
//! crashes (RFC 0015) and byte mutations of a production-written data
//! file, one per distinct panic site.

use std::path::{Path, PathBuf};

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    AuditReader, AuditReaderError, CompactionError, IdColumns, PartitionKey, Reader, ReaderError,
    Store, Writer, compact_partition, decoded_max, object_max_id,
};

/// Audit-file fixtures and the decoder panic each one trips.
const AUDIT_FIXTURES: [&str; 6] = [
    // column/reader/decoder.rs: "Decoder for dict should have been set"
    "audit-dict-decoder-unset.parquet",
    // file/metadata/mod.rs: "column start and length should not be negative"
    "audit-negative-column-range.parquet",
    // column/page.rs: `Option::unwrap()` on `None`
    "audit-page-header-unwrap.parquet",
    // arrow/decoder/dictionary_index.rs: index out of bounds
    "audit-dictionary-index-oob.parquet",
    // encodings/decoding/byte_stream_split_decoder.rs: index out of bounds
    "audit-byte-stream-split-oob.parquet",
    // util/bit_util.rs: "num_bits <= size_of::<T>() * 8"
    "audit-bit-width-assert.parquet",
];

/// Data-file fixtures: dictionary decoder unset, a negative column range in
/// the footer, a zero divisor in the byte-array reader.
const DATA_FIXTURES: [&str; 3] = [
    "data-dict-decoder-unset.parquet",
    "data-negative-column-range.parquet",
    "data-divide-by-zero.parquet",
];

fn fixture(name: &str) -> bytes::Bytes {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corrupt-parquet")
        .join(name);
    bytes::Bytes::from(std::fs::read(&path).expect("read the fixture"))
}

#[derive(Debug, Clone, Copy)]
enum Via {
    AuditReader,
    DataReader,
}

/// Read the fixture `name` to the end through `via`, opened under its own
/// name as the object key: the object a `DecodePanicked` error names, or
/// the outcome otherwise.
fn decode_via(via: Via, name: &str) -> Result<PathBuf, String> {
    let bytes = fixture(name);
    match via {
        Via::AuditReader => {
            match AuditReader::open_object_bytes(bytes, name).and_then(AuditReader::read_all) {
                Err(AuditReaderError::DecodePanicked { path, .. }) => Ok(path),
                other => Err(format!("{other:?}")),
            }
        }
        Via::DataReader => {
            let partition = PartitionKey::derive(&record(0)).expect("derive partition");
            match Reader::open_partition_bytes(bytes, partition, name).and_then(Reader::read_all) {
                Err(ReaderError::DecodePanicked { path, .. }) => Ok(path),
                other => Err(format!("{other:?}")),
            }
        }
    }
}

#[test]
fn both_readers_turn_decoder_panics_into_typed_errors_naming_the_object() {
    let cases = AUDIT_FIXTURES
        .map(|name| (Via::AuditReader, name))
        .into_iter()
        .chain(DATA_FIXTURES.map(|name| (Via::DataReader, name)));
    for (via, name) in cases {
        // Act
        let named = decode_via(via, name);

        // Assert
        assert_eq!(named, Ok(PathBuf::from(name)), "{via:?}");
    }
}

#[test]
fn a_reader_that_panicked_keeps_failing_instead_of_decoding_on() {
    // Arrange: this fixture opens cleanly and panics on its first page.
    let mut reader =
        Reader::open_bytes(fixture("data-dict-decoder-unset.parquet")).expect("open the reader");

    // Act
    let first = reader.next_batch();
    let second = reader.next_batch();

    // Assert
    assert!(
        matches!(first, Err(ReaderError::DecodePanicked { .. })),
        "{first:?}"
    );
    assert!(
        matches!(second, Err(ReaderError::DecodePanicked { .. })),
        "{second:?}"
    );
}

#[test]
fn decoded_max_turns_decoder_panics_into_typed_errors() {
    for name in [
        "audit-ids-dict-decoder-unset.parquet",
        "audit-negative-column-range.parquet",
    ] {
        // Act
        let max = decoded_max(fixture(name), IdColumns::Audit);

        // Assert
        assert!(
            matches!(max, Err(AuditReaderError::DecodePanicked { .. })),
            "{name}: {max:?}",
        );
    }
}

#[test]
fn object_max_id_names_the_object_whose_decode_panicked() {
    // Arrange
    let bucket = tempfile::TempDir::new().expect("temp dir");
    let store = Store::local(bucket.path()).expect("local store");
    let key = "audit/tenant_id=acme/corrupt.parquet";
    store
        .put_blocking(
            key,
            fixture("audit-ids-dict-decoder-unset.parquet").to_vec(),
        )
        .expect("put the fixture");

    // Act
    let max = object_max_id(&store, key, IdColumns::Audit);

    // Assert
    let err = max.expect_err("the decoder panics on this file");
    assert!(err.is_decode_panicked(), "{err}");
    assert!(err.to_string().contains(key), "{err}");
}

fn record(i: u64) -> MinedRecord {
    let ts_ns: u64 = 1_791_676_800_000_000_000 + i;
    MinedRecord {
        tenant_id: TenantId::new("acme"),
        template_id: i % 3,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib".to_string()),
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns),
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
    }
}

#[test]
fn compaction_reports_a_corrupt_input_instead_of_aborting() {
    for name in DATA_FIXTURES {
        // Arrange: two committed inputs, one then overwritten by a corrupt
        // object.
        let bucket = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(bucket.path()).expect("local store");
        let partition = PartitionKey::derive(&record(0)).expect("derive partition");
        let mut keys = Vec::new();
        for i in 0..2 {
            let mut writer = Writer::open_in(&store, partition.clone()).expect("open writer");
            writer.append_records(&[record(i)]).expect("append");
            keys.push(writer.close().expect("close").key);
        }
        store
            .put_blocking(&keys[1], fixture(name).to_vec())
            .expect("corrupt an input");

        // Act
        let compacted = compact_partition(&store, &partition);

        // Assert
        assert!(
            matches!(
                &compacted,
                Err(CompactionError::Read(ReaderError::DecodePanicked { path, .. }))
                    if path == Path::new(&keys[1])
            ),
            "{name}: {compacted:?}",
        );
    }
}
