use super::*;

/// RFC0037.1 — a structured record's `event_name` participates in
/// the template key, so distinct event types in one
/// `(severity, scope)` get distinct `template_id`s (and identical
/// ones share) instead of collapsing to a single sentinel. This is
/// the mechanism behind `… | count by template_id` separating event
/// types (asserted at the query layer once the ids differ).
#[test]
fn rfc0037_1_event_name_distinguishes_structured_templates() {
    let tenant = TenantId::new("tenant-genai");
    let mut cluster = MinerCluster::new(MinerConfig::default());

    let mut inference = structured_record(&tenant, 9, Some("lib.agent"));
    inference.event_name = Some("gen_ai.client.inference.operation.details".to_string());
    let mut tool_call = structured_record(&tenant, 9, Some("lib.agent"));
    tool_call.event_name = Some("gen_ai.execute_tool".to_string());

    let id_inference = cluster.ingest(&inference);
    let id_tool = cluster.ingest(&tool_call);
    assert_ne!(
        id_inference, id_tool,
        "distinct event_name in one (severity, scope) must yield distinct template_ids"
    );

    // Same (severity, scope, event_name) shares its id.
    assert_eq!(
        id_inference,
        cluster.ingest(&inference),
        "identical structured key must share one template_id"
    );

    // A record with no event_name is its own class, distinct from both.
    let no_event = structured_record(&tenant, 9, Some("lib.agent"));
    let id_none = cluster.ingest(&no_event);
    assert_ne!(id_none, id_inference);
    assert_ne!(id_none, id_tool);
}

proptest! {
    /// RFC0037.1 (property) — a structured record's `template_id` is a
    /// pure function of exactly `(severity_number, scope_name,
    /// event_name)`: equal tuples reuse an id, distinct tuples receive
    /// distinct ids. Covers empty strings, `None`s, and repeated keys.
    #[test]
    fn rfc0037_1_structured_key_is_the_whole_template_identity(
        keys in prop::collection::vec(
            (
                any::<u8>(),
                prop::option::of("[a-z.]{0,8}"),
                prop::option::of("[a-z_.]{0,12}"),
            ),
            1..16,
        )
    ) {
        let tenant = TenantId::new("t");
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let mut ids: std::collections::HashMap<
            (u8, Option<String>, Option<String>),
            u64,
        > = std::collections::HashMap::new();
        for (severity, scope, event) in keys {
            let mut rec = structured_record(&tenant, severity, scope.as_deref());
            rec.event_name = event.clone();
            let id = cluster.ingest(&rec);
            let key = (severity, scope, event);
            if let Some(&prev) = ids.get(&key) {
                prop_assert_eq!(id, prev, "equal structured key must reuse its template_id");
            } else {
                prop_assert!(
                    !ids.values().any(|&existing| existing == id),
                    "a distinct structured key must receive a fresh template_id"
                );
                ids.insert(key, id);
            }
        }
    }
}

/// RFC0037.3 (unit) — the structured-body branch retains the body's
/// canonical JSON byte-for-byte and never flags it lossy (§3.2 fidelity),
/// colocated with `ingest_structured`. The per-service metric emission is
/// covered end-to-end in `tests/rfc0037_structured_body.rs`.
#[test]
fn rfc0037_3_structured_body_retained_byte_for_byte() {
    let tenant = TenantId::new("t");
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));

    let body_av = AnyValue {
        value: Some(AvValue::ArrayValue(ArrayValue {
            values: vec![
                AnyValue {
                    value: Some(AvValue::StringValue("user turn".to_string())),
                },
                AnyValue {
                    value: Some(AvValue::StringValue("assistant turn".to_string())),
                },
            ],
        })),
    };
    let expected = String::from_utf8(
        ourios_core::otlp::canonical::encode_any_value(&body_av)
            .expect("canonical encode is infallible"),
    )
    .expect("canonical JSON is UTF-8");

    let mut record = structured_record(&tenant, 9, Some("lib.agent"));
    record.event_name = Some("gen_ai.client.inference.operation.details".to_string());
    record.body = Some(Body::Structured(body_av));
    cluster.ingest(&record);

    let mined = sink.drain();
    assert_eq!(mined.len(), 1);
    assert_eq!(mined[0].body_kind, BodyKind::Structured);
    assert_eq!(
        mined[0].body.as_deref(),
        Some(expected.as_str()),
        "the structured body is retained as canonical JSON, byte-for-byte"
    );
    assert!(!mined[0].lossy_flag, "a structured body is never lossy");
}

// ---------- new behaviour from PR #28: body fork + structured short-circuit ----------

#[test]
fn ingest_returns_no_template_for_absent_body() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let r = OtlpLogRecord {
        tenant_id: t.clone(),
        body: None,
        ..Default::default()
    };

    let id = cluster.ingest(&r);

    assert_eq!(id, NO_TEMPLATE);
    assert_eq!(cluster.template_count(&t), 0);
}

#[test]
fn structured_body_short_circuit_allocates_one_template_per_severity_scope_tuple() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
    let id2 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
    let id3 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

    assert_eq!(id1, id2);
    assert_eq!(id2, id3);
    assert_eq!(cluster.template_count(&t), 1);
}

#[test]
fn structured_body_distinguishes_severity_within_one_scope() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id_info = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
    let id_error = cluster.ingest(&structured_record(&t, 17, Some("lib.auth")));

    assert_ne!(id_info, id_error);
    assert_eq!(cluster.template_count(&t), 2);
}

#[test]
fn structured_body_distinguishes_scope_within_one_severity() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id_a = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
    let id_b = cluster.ingest(&structured_record(&t, 9, Some("lib.payments")));

    assert_ne!(id_a, id_b);
    assert_eq!(cluster.template_count(&t), 2);
}

/// Pin the exact RFC 0005 §3.3 canonical-JSON bytes the
/// miner stores in `MinedRecord.body` for a structured
/// row. Catches a regression to debug formatting (the
/// prior `format!("{any_value:?}")` placeholder), AND
/// catches an `opentelemetry-proto` upgrade that breaks
/// the OTLP-JSON spec mapping (camelCase, string-encoded
/// `i64`, base64 bytes). A non-trivial `AnyValue` exercises
/// the recursive `KvlistValue` path through the encoder.
#[test]
fn structured_body_is_stored_as_otlp_canonical_json() {
    use ourios_core::otlp::{KeyValue as ProtoKv, KeyValueList};
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let av = AnyValue {
        value: Some(AvValue::KvlistValue(KeyValueList {
            values: vec![ProtoKv {
                key: "user.id".to_string(),
                value: Some(AnyValue {
                    value: Some(AvValue::IntValue(42)),
                }),
                ..Default::default()
            }],
        })),
    };
    let record = OtlpLogRecord {
        tenant_id: TenantId::new("tenant-x"),
        severity_number: 9,
        scope_name: Some("bench.scope".to_string()),
        body: Some(Body::Structured(av)),
        ..Default::default()
    };
    cluster.ingest(&record);
    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let body = emitted[0].body.as_deref().expect("structured body is Some");
    // Pinned canonical form per the proto3 JSON spec
    // mapping: camelCase keys, `i64` as a quoted string,
    // recursive `kvlistValue` shape. The opentelemetry-proto
    // `with-serde` derives emit fields in struct-definition
    // order, which is what serde_json::to_vec produces
    // deterministically — RFC0006.7 reproducibility relies
    // on this same byte stability.
    assert_eq!(
        body, r#"{"kvlistValue":{"values":[{"key":"user.id","value":{"intValue":"42"}}]}}"#,
        "miner must store RFC 0005 §3.3 canonical JSON, not a debug rendering",
    );
    assert!(
        !emitted[0].lossy_flag,
        "RFC 0001 §6.1: lossy_flag is always false on BodyKind::Structured",
    );
}

#[test]
fn structured_body_with_scope_none_is_its_own_bucket() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id_none = cluster.ingest(&structured_record(&t, 9, None));
    let id_some = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

    assert_ne!(id_none, id_some);
    assert_eq!(cluster.template_count(&t), 2);
}

#[test]
fn structured_body_isolates_template_ids_across_tenants() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let a = TenantId::new("tenant-a");
    let b = TenantId::new("tenant-b");

    let id_a = cluster.ingest(&structured_record(&a, 9, Some("lib.auth")));
    let id_b = cluster.ingest(&structured_record(&b, 9, Some("lib.auth")));

    assert_ne!(
        id_a, id_b,
        "structured records with identical key tuple must get distinct template_ids across tenants",
    );
    assert_eq!(cluster.template_count(&a), 1);
    assert_eq!(cluster.template_count(&b), 1);
}

#[test]
fn structured_and_string_share_no_template_ids_at_same_severity_scope() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id_struct = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
    let id_string = cluster.ingest(&OtlpLogRecord {
        tenant_id: t.clone(),
        severity_number: 9,
        scope_name: Some("lib.auth".to_string()),
        body: Some(Body::String("hello".to_string())),
        ..Default::default()
    });

    assert_ne!(id_struct, id_string);
    assert_eq!(cluster.template_count(&t), 2);
}

#[test]
fn string_body_distinguishes_severity_within_one_scope() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let info = OtlpLogRecord {
        tenant_id: t.clone(),
        severity_number: 9,
        body: Some(Body::String("user 42 logged in".to_string())),
        ..Default::default()
    };
    let error = OtlpLogRecord {
        severity_number: 17,
        ..info.clone()
    };

    let id_info = cluster.ingest(&info);
    let id_error = cluster.ingest(&error);

    assert_ne!(id_info, id_error);
    assert_eq!(cluster.template_count(&t), 2);
}
