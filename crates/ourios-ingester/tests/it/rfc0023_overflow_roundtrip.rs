//! RFC 0023 §5 — RFC0023.2: ceiling-overflow lines stay stored and
//! searchable. The miner-level scenarios (`.1/.3/.4/.5/.6`) live in
//! `crates/ourios-miner/tests/rfc0023_bounded_memory.rs`; this one
//! crosses the ingest path (miner overflow → Parquet → body
//! read-back), which needs both crates.

use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, Reader, encode_records_to_parquet};

/// Scenario RFC0023.2 — overflow lines stay stored and searchable.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
fn rfc0023_2_overflow_bodies_round_trip_bit_identically() {
    let tenant = TenantId::new("overflow");
    let sink = SharedRecordSink::new();
    let config = MinerConfig::default()
        .with_max_templates(1)
        .expect("non-zero ceiling");
    let mut cluster = MinerCluster::new(config).with_record_sink(Box::new(sink.clone()));

    // Distinct shapes: the first mints (and fills the ceiling), the
    // second diverts to the parse-failure path with its body retained.
    let overflow_line = "gamma rejected request from peer five with cause umbrella";
    for text in ["alpha started", overflow_line] {
        cluster.ingest(&OtlpLogRecord {
            tenant_id: tenant.clone(),
            body: Some(Body::String(text.to_string())),
            time_unix_nano: 1_775_127_480_000_000_000,
            ..Default::default()
        });
    }
    let mined = sink.drain();
    assert_eq!(mined.len(), 2);
    assert_eq!(mined[1].template_id, NO_TEMPLATE, "second line diverted");

    // Through the RFC 0005 writer and back: the overflow body is
    // bit-identical in the Parquet body column.
    let bytes = encode_records_to_parquet(&mined, DEFAULT_ZSTD_LEVEL).expect("encode");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("overflow.parquet");
    std::fs::write(&path, bytes).expect("write parquet");
    let rows = Reader::open_file(&path)
        .expect("open parquet")
        .read_all()
        .expect("read rows");
    assert_eq!(rows.len(), 2);
    let overflow_row = rows
        .iter()
        .find(|r| r.template_id == NO_TEMPLATE)
        .expect("the diverted row is stored");
    assert_eq!(
        overflow_row.body.as_deref(),
        Some(overflow_line),
        "overflow body round-trips bit-identically",
    );
}
