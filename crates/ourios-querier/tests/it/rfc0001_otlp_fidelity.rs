//! RFC 0001 — masking a timestamp never touches the record's time fields or
//! body bytes (RFC0001.20).
//!
//! Like RFC0001.10, this is an ingest → Parquet → query scenario: the real
//! miner masks the body, its records and audit stream land in a store, and
//! the row-returning `Querier::run` reads them back.

use crate::common::{TS0, kv, write_all};
use ourios_config::MinerConfig;
use ourios_core::audit::ParamType;
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, Body, KeyValueList, OtlpLogRecord};
use ourios_core::record::{BodyKind, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::reconstruct::Reconstruction;
use ourios_parquet::{ParquetAuditSink, Store};
use ourios_querier::{LogBody, Querier, QueryRequest};
use tempfile::TempDir;

/// An ISO 8601 instant in the body, distinct from both record times.
const LINE: &str = "2026-10-18T13:28:29.674004+02:00 worker started";
const BODY_TS: &str = "2026-10-18T13:28:29.674004+02:00";
const TIME: u64 = TS0;
const OBSERVED: u64 = TS0 + 1_000_000_007;

fn record(body: Body) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: TenantId::new("acme"),
        time_unix_nano: TIME,
        observed_time_unix_nano: Some(OBSERVED),
        severity_number: 9,
        body: Some(body),
        ..Default::default()
    }
}

fn structured_body() -> AnyValue {
    AnyValue {
        value: Some(Value::KvlistValue(KeyValueList {
            values: vec![kv("msg", LINE)],
        })),
    }
}

/// Scenario RFC0001.20 — masking a timestamp never touches the record's
/// time fields or body bytes (OTLP fidelity).
/// See `docs/rfcs/0001-template-miner.md` §5.
#[tokio::test]
async fn rfc0001_20_masked_timestamp_leaves_time_fields_and_body_bytes_untouched() {
    let bucket = TempDir::new().expect("temp");
    let audit = ParquetAuditSink::new(Store::local(bucket.path()).expect("store"));
    let records = SharedRecordSink::new();
    let mut miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit))
        .with_record_sink(Box::new(records.clone()));

    miner.ingest(&record(Body::String(LINE.to_owned())));
    miner.ingest(&record(Body::Structured(structured_body())));
    let stored = records.drain();
    write_all(bucket.path(), &stored);

    let [string, structured] = stored.as_slice() else {
        panic!("two records mined, got {}", stored.len());
    };
    assert_eq!(string.body_kind, BodyKind::String);
    let ts: Vec<_> = string
        .params
        .iter()
        .filter(|p| p.type_tag == ParamType::Ts)
        .map(|p| p.value.as_str())
        .collect();
    assert_eq!(
        ts,
        [BODY_TS],
        "the TS param is the token's original bytes, not a normalised instant",
    );
    assert_eq!(structured.body_kind, BodyKind::Structured);
    assert!(
        structured.params.is_empty(),
        "a structured body is not masked (RFC0001.9)",
    );

    let result = Querier::new(bucket.path())
        .run(QueryRequest {
            tenant: TenantId::new("acme"),
            time_range: None,
            template_id: None,
            severity_text: None,
            limit: Some(10),
        })
        .await
        .expect("query");
    assert_eq!(result.records.len(), 2, "both records read back");
    for row in &result.records {
        assert_eq!(row.time_unix_nano, TIME, "time_unix_nano is the wire value");
        assert_eq!(
            row.observed_time_unix_nano,
            Some(OBSERVED),
            "observed_time_unix_nano is the wire value",
        );
    }
    let bodies: Vec<_> = result.records.iter().map(|row| &row.body).collect();
    assert!(
        bodies.contains(&&LogBody::Rendered {
            line: LINE.as_bytes().to_vec(),
            reconstruction: Reconstruction::Faithful,
        }),
        "the string body reads back byte-identical and Faithful: {bodies:?}",
    );
    assert!(
        bodies.contains(&&LogBody::Structured(structured_body())),
        "the structured body reads back as the same AnyValue: {bodies:?}",
    );
}
