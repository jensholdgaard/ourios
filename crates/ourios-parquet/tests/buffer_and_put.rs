//! RFC 0013 buffer-and-put round-trip (the object-store-native I/O model):
//! encode `MinedRecord`s to in-memory Parquet bytes → `Store.put` →
//! `Store.get` → `Reader::open_bytes` → decode, with the rows recovered
//! intact. Foundation for migrating the writer/reader off filesystem paths
//! onto `Store`; uses the `LocalFileSystem` backend (the S3 backend +
//! testcontainer lane lands in a later green slice).

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, PartitionKey, Reader, Store, encode_records_to_parquet};

fn rec(i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("t"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        time_unix_nano: 1_775_127_480_000_000_000 + i * 1_000,
        observed_time_unix_nano: Some(1_775_127_480_000_000_000 + i * 1_000 + 1),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: format!("{i}"),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn records_round_trip_through_store_buffer_and_put() {
    let records: Vec<MinedRecord> = (0..500).map(rec).collect();

    // Encode in memory (no filesystem path), put + get via the local Store,
    // then decode the recovered bytes.
    let bytes = encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = Store::local(dir.path()).expect("local store");

    // Key derived from the records' own partition (Hive path under an empty
    // root), so the partition segments match the fixture timestamps.
    let partition = PartitionKey::derive(&records[0]).expect("derive partition");
    let key = format!(
        "{}/file.parquet",
        partition
            .data_path(std::path::Path::new(""))
            .to_string_lossy()
    );
    store.put(&key, bytes).await.expect("put");
    let got = store.get(&key).await.expect("get");

    let decoded = Reader::open_bytes(bytes::Bytes::from(got))
        .expect("open_bytes")
        .read_all()
        .expect("read_all");

    assert_eq!(decoded.len(), records.len(), "row count preserved");
    assert_eq!(decoded, records, "every row recovered byte-for-byte");
}
