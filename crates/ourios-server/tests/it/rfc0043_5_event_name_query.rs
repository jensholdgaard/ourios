//! RFC0043.5 — the idiomatic query works end-to-end: an attr-only
//! source (the legacy `event.name` attribute, no wire `event_name`)
//! ingests through the real materialisation + miner, and
//! `event_name == "…"` returns exactly the matching records.
//!
//! See `docs/rfcs/0043-event-name-attribute-ingest.md` §5.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_config::MinerConfig;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_record;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{PartitionKey, Writer};

use crate::rfc0016_query_endpoint::post_for_equivalence;

/// One hour before the wall clock, in unix nanos — inside the
/// no-`range` look-back regardless of the machine's clock.
fn recent_ns() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(now.saturating_sub(60 * 60 * 1_000_000_000)).unwrap_or(0)
}

/// An attr-only event record: string body, no wire `event_name`, the
/// legacy `event.name` attribute carrying the identity — the Claude
/// Code / opencode shape.
fn attr_only_event(body: &str, event_name: &str) -> LogRecord {
    LogRecord {
        time_unix_nano: recent_ns(),
        severity_number: 9,
        body: Some(AnyValue {
            value: Some(Value::StringValue(body.to_owned())),
        }),
        attributes: vec![KeyValue {
            key: "event.name".to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(event_name.to_owned())),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn write_records(bucket: &Path, recs: &[MinedRecord]) {
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in recs {
        by_part
            .entry(PartitionKey::derive(r).expect("derive partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w = Writer::open(bucket, part).expect("open writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

/// Scenario RFC0043.5 — the idiomatic query works end-to-end.
/// See `docs/rfcs/0043-event-name-attribute-ingest.md` §5.
#[tokio::test]
async fn rfc0043_5_event_name_query_matches_attr_only_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bucket = dir.path();
    let tenant = TenantId::new("acme");

    // Ingest through the REAL derivation + miner — no hand-built
    // MinedRecords, so the seam under test is the one production runs.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let mut mined = Vec::new();
    for (body, event) in [
        ("api_request", "claude_code.api_request"),
        ("api_request", "claude_code.api_request"),
        ("tool_result", "claude_code.tool_result"),
    ] {
        let record = materialize_record(
            attr_only_event(body, event),
            &[],
            "",
            None,
            "",
            tenant.clone(),
        );
        let (_, captured) = cluster.ingest_mined(&record);
        mined.push(captured.expect("string-bodied record captures a MinedRecord"));
    }
    write_records(bucket, &mined);

    let (status, json) = post_for_equivalence(
        bucket,
        Some("acme"),
        r#"event_name == "claude_code.api_request""#,
    )
    .await;
    assert_eq!(status, 200, "query rejected: {json}");
    assert_eq!(
        json["rows"], 2,
        "exactly the two api_request records match: {json}"
    );
    let records = json["records"].as_array().expect("records array");
    for r in records {
        assert_eq!(
            r["event_name"], "claude_code.api_request",
            "every returned record carries the derived event_name"
        );
    }

    // The negative half: the other event type is not swept in.
    let (status, json) = post_for_equivalence(
        bucket,
        Some("acme"),
        r#"event_name == "claude_code.tool_result""#,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["rows"], 1, "only the tool_result record: {json}");
}
