//! RFC0043.6 — RFC 0037 keying engages, observably, for attr-only
//! sources. The structured template key is `(severity, scope,
//! event_name)` (RFC 0037 §3.1): without the RFC 0043 derivation every
//! attr-only structured record collapses into the one no-event
//! sentinel; with it, distinct derived event names take distinct
//! template ids, and same-event records share one id regardless of
//! their structured content.
//!
//! See `docs/rfcs/0043-event-name-attribute-ingest.md` §5.

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, KeyValueList};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_record;
use ourios_miner::cluster::MinerCluster;

fn kv(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}

/// An attr-only structured event: kvlist body, no wire `event_name`,
/// the legacy `event.name` attribute carrying the identity.
fn attr_only_structured(event_name: &str, body_field: KeyValue) -> LogRecord {
    LogRecord {
        severity_number: 9,
        body: Some(AnyValue {
            value: Some(Value::KvlistValue(KeyValueList {
                values: vec![body_field],
            })),
        }),
        attributes: vec![kv("event.name", Value::StringValue(event_name.to_owned()))],
        ..Default::default()
    }
}

/// Scenario RFC0043.6 — RFC 0037 keying engages, observably.
/// See `docs/rfcs/0043-event-name-attribute-ingest.md` §5.
#[test]
fn rfc0043_6_derived_event_name_drives_the_structured_template_key() {
    let tenant = TenantId::new("tenant-genai");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let materialize =
        |record: LogRecord| materialize_record(record, &[], "", None, "", tenant.clone());

    let inference_a = materialize(attr_only_structured(
        "gen_ai.client.inference.operation.details",
        kv("tokens", Value::IntValue(800)),
    ));
    let inference_b = materialize(attr_only_structured(
        "gen_ai.client.inference.operation.details",
        kv("model", Value::StringValue("claude-fable-5".to_owned())),
    ));
    let tool_call = materialize(attr_only_structured(
        "gen_ai.execute_tool",
        kv("tokens", Value::IntValue(800)),
    ));

    let id_a = cluster.ingest(&inference_a);
    let id_b = cluster.ingest(&inference_b);
    let id_tool = cluster.ingest(&tool_call);

    // The separating direction is the proof derivation reached the key:
    // without it, all three records share the one (severity, scope,
    // no-event) sentinel id.
    assert_ne!(
        id_a, id_tool,
        "distinct derived event names must take distinct template ids"
    );
    // The sharing direction: content does not participate in the key.
    assert_eq!(
        id_a, id_b,
        "same derived event name shares one template id across differing structured content"
    );
}
