//! Scenario RFC0005.7 — Audit-event stream is a separate file series
//! and round-trips every §3.7 row-level column.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Writes one [`AuditEvent`] of each [`AuditPayload`] variant
//! through [`AuditWriter`], reads them back through
//! [`AuditReader::open_partition`], and asserts full struct
//! equality.
//!
//! Two extra sub-tests pin the supplementary §3.7 properties:
//!
//! - audit files land at `audit/tenant_id=…/year/month/day/…` —
//!   not interleaved with the data file series at
//!   `data/tenant_id=…/year/month/day/hour/…`.
//! - the rejection variant's `would_be_template` /
//!   `would_be_positions` survive the round trip via the JSON-
//!   encoded `reason` column (see `audit_record_batch.rs`
//!   module-level note).

use std::path::Component;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ourios_core::alias::ActorId;
use ourios_core::audit::{
    AuditEvent, AuditPayload, ParamType, SlotExpansion, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_parquet::{AuditReader, AuditWriter, PartitionKey, audit_columns};
use tempfile::TempDir;

fn ts(offset_secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(offset_secs)
}

fn template_event(
    tenant: &str,
    template_id: u64,
    sample: Option<&str>,
    offset: u64,
    change: TemplateChange,
) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: ts(offset),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"trigger"),
            triggering_line_sample: sample.map(str::to_string),
            change,
        },
    }
}

/// Three events, one of each template variant, all in the same audit
/// partition (same tenant, same day). The widening + type-expand
/// share a `template_id`; the rejection lands on a different one.
fn three_variants(tenant: &str) -> Vec<AuditEvent> {
    vec![
        // 2026-04-02T10:58:00Z baseline (same day as the round-trip
        // test data fixtures).
        template_event(
            tenant,
            7,
            Some("user 42 in"),
            1_775_127_480,
            TemplateChange::Widened {
                old_version: 1,
                new_version: 2,
                old_template: "[\"user\",\"<*>\",\"in\"]".to_string(),
                new_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                positions_widened: vec![2],
            },
        ),
        template_event(
            tenant,
            7,
            None,
            1_775_127_490,
            TemplateChange::TypeExpanded {
                old_version: 2,
                new_version: 3,
                old_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                new_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                slots_expanded: vec![
                    SlotExpansion {
                        slot_index: 1,
                        added_types: vec![ParamType::Num, ParamType::Ip],
                    },
                    SlotExpansion {
                        slot_index: 2,
                        added_types: vec![ParamType::Str],
                    },
                ],
            },
        ),
        template_event(
            tenant,
            9,
            Some("zzz qqq"),
            1_775_127_500,
            TemplateChange::RejectedDegenerate {
                version: 5,
                current_template: "[\"only-literal\",\"<*>\"]".to_string(),
                would_be_template: "[\"<*>\",\"<*>\"]".to_string(),
                would_be_positions: vec![0],
            },
        ),
    ]
}

/// A compaction audit event in the same partition (RFC 0009 §3.6).
fn compaction_event(tenant: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: ts(1_775_127_510),
        payload: AuditPayload::Compaction {
            partition: "year=2026/month=04/day=02/hour=10".to_string(),
            input_files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
            output_file: "c.parquet".to_string(),
            generation: 7,
            rows: 100,
        },
    }
}

/// A quarantine audit event in the same partition (RFC 0025 §3.3).
fn quarantine_event(tenant: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: ts(1_775_127_520),
        payload: AuditPayload::RecordQuarantined {
            partition: "year=2026/month=04/day=02/hour=10".to_string(),
            error: "observed_time_unix_nano = 18446744073709551615 exceeds i64::MAX \
                    (RFC 0005 §3.2 u64→i64 overflow contract)"
                .to_string(),
        },
    }
}

/// An `ingest_denied` event (RFC 0026 §3.4): the envelope's tenant is
/// the offending derived tenant; the payload carries the token's audit
/// label only — never a token value.
fn denied_event(tenant: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: ts(1_775_127_530),
        payload: AuditPayload::IngestDenied {
            token_name: "edge-collector".to_string(),
        },
    }
}

fn audit_partition_for(event: &AuditEvent) -> PartitionKey {
    // Audit partitioning shares the data-side `PartitionKey`
    // shape; the writer/reader compare only tenant + year/month/
    // day (the hour field is populated but ignored). Derive via
    // a one-shot `MinedRecord` proxy so the test exercises the
    // same code path the production writer drives off the
    // event's timestamp.
    use ourios_core::record::{BodyKind, MinedRecord};
    let nanos = event
        .timestamp
        .duration_since(UNIX_EPOCH)
        .expect("test events are post-epoch")
        .as_nanos();
    let ns = u64::try_from(nanos).expect("test events fit u64");
    let proxy = MinedRecord {
        tenant_id: event.tenant_id.clone(),
        template_id: 0,
        template_version: 0,
        severity_number: 0,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ns,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new()],
        body: None,
        confidence: 0.0,
        lossy_flag: false,
    };
    PartitionKey::derive(&proxy).expect("derive")
}

/// Scenario RFC0005.7 — round-trip preserves every §3.7 row-level
/// column for each of the three event variants.
#[test]
fn rfc0005_7_audit_round_trip_one_of_each_variant() {
    let bucket = TempDir::new().unwrap();
    let events = three_variants("acme");
    let partition = audit_partition_for(&events[0]);

    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    assert_eq!(round_tripped.len(), events.len());
    for (i, (orig, rt)) in events.iter().zip(round_tripped.iter()).enumerate() {
        assert_eq!(
            rt, orig,
            "row {i} mismatch — full AuditEvent equality covers every row-level §3.7 column",
        );
    }
}

/// RFC 0026 §3.4 — the `ingest_denied` audit event round-trips: kind 8,
/// the offending tenant on the envelope, the token's audit label in its
/// column, every other payload column NULL.
#[test]
fn rfc0026_ingest_denied_audit_event_round_trips() {
    let bucket = TempDir::new().unwrap();
    let events = vec![
        denied_event("intruded-tenant"),
        quarantine_event("intruded-tenant"),
    ];
    let partition = audit_partition_for(&events[0]);

    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");
    assert_eq!(
        round_tripped, events,
        "denial + quarantine rows round-trip exactly"
    );
}

/// `AuditReader::open_bytes` reads an audit file from in-memory bytes — the
/// RFC 0019 `Store` read path the querier's audit scan moves onto (so the scan
/// reads through the object-storage seam, local or S3, rather than `std::fs`).
/// The events round-trip identically to the on-disk `open_file` path.
#[test]
fn audit_reader_open_bytes_round_trips() {
    let bucket = TempDir::new().unwrap();
    let events = three_variants("acme");
    let partition = audit_partition_for(&events[0]);

    let mut writer = AuditWriter::open(bucket.path(), partition).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    // Read the written object's raw bytes and parse them through `open_bytes`
    // (the bytes a `Store::get_blocking` returns on the migrated scan path).
    let bytes = std::fs::read(&written.path).expect("read file bytes");
    let round_tripped = AuditReader::open_bytes(bytes::Bytes::from(bytes))
        .expect("open_bytes")
        .read_all()
        .expect("read_all");

    assert_eq!(round_tripped, events, "open_bytes round-trips every event");
}

/// Scenario RFC0017.1 (storage round-trip) — the `template_created` variant
/// round-trips through the audit file series. Full `AuditEvent` equality
/// confirms `new_template` survives and the reader reconstructs `Created`
/// (which carries no version — a leaf is always v1 — and no `old_*`,
/// reflecting the NULL "not applicable" columns; the on-disk `new_version`
/// is the canonical `1`, RFC 0017 §3.1) — pins the writer/reader arms.
#[test]
fn rfc0017_1_template_created_round_trips() {
    let bucket = TempDir::new().unwrap();
    let event = template_event(
        "acme",
        11,
        Some("user 42 logged in"),
        1_775_127_480,
        TemplateChange::Created {
            new_template: "user <*> logged in".to_string(),
        },
    );
    let partition = audit_partition_for(&event);

    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer
        .append_events(std::slice::from_ref(&event))
        .expect("append");
    let written = writer.close().expect("close");

    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    assert_eq!(round_tripped.len(), 1);
    assert_eq!(
        round_tripped[0], event,
        "template_created round-trips with full AuditEvent equality",
    );
}

/// RFC0005.7 sub-test — audit files land under
/// `audit/tenant_id=…/year/month/day/<flush_uuid>.parquet`. There
/// is NO `hour=HH` segment (the audit partitioning is one axis
/// coarser than the data partitioning per §3.4).
#[test]
fn rfc0005_7_audit_file_path_stops_at_day_segment() {
    let bucket = TempDir::new().unwrap();
    let events = three_variants("acme");
    let partition = audit_partition_for(&events[0]);
    let mut writer = AuditWriter::open(bucket.path(), partition).expect("open");
    writer.append_events(&events[..1]).expect("append");
    let written = writer.close().expect("close");

    let rel = written
        .path
        .strip_prefix(bucket.path())
        .expect("file lives under bucket");
    let segments: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    assert_eq!(segments[0], "audit", "audit file series under `audit/`");
    assert!(segments[1].starts_with("tenant_id="));
    assert!(segments[2].starts_with("year="));
    assert!(segments[3].starts_with("month="));
    assert!(segments[4].starts_with("day="));
    // segments[5] is the UUIDv7 filename — no `hour=` between
    // day and the filename, distinguishing the audit layout from
    // the data layout.
    assert!(
        !segments[5].starts_with("hour="),
        "audit path must not carry an hour= segment, found {:?}",
        segments[5],
    );
    assert!(segments[5].ends_with(".parquet"));
}

/// RFC0005.7 sub-test — the rejection variant's
/// `would_be_template` / `would_be_positions` survive a round
/// trip through the JSON-encoded `reason` column. Pins the
/// encoding contract in `audit_record_batch.rs`'s module-level
/// note: without this round-trip the rejection variant would be
/// lossy.
#[test]
fn rfc0005_7_rejection_variant_round_trips_via_reason_column() {
    let bucket = TempDir::new().unwrap();
    let events = three_variants("acme");
    let partition = audit_partition_for(&events[0]);
    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    let rt_rejection = round_tripped
        .iter()
        .find(|e| {
            matches!(
                &e.payload,
                AuditPayload::Template {
                    change: TemplateChange::RejectedDegenerate { .. },
                    ..
                }
            )
        })
        .expect("rejection event round-tripped");

    let AuditPayload::Template {
        change:
            TemplateChange::RejectedDegenerate {
                version,
                current_template,
                would_be_template,
                would_be_positions,
            },
        ..
    } = &rt_rejection.payload
    else {
        unreachable!()
    };

    assert_eq!(*version, 5);
    assert_eq!(current_template, "[\"only-literal\",\"<*>\"]");
    assert_eq!(would_be_template, "[\"<*>\",\"<*>\"]");
    assert_eq!(would_be_positions, &vec![0]);
}

/// An `alias_asserted` audit event (RFC 0001 §6.7 / RFC 0005 §3.7
/// amendment 2026-06-12). `member_ids` deliberately carries a
/// duplicate and an unsorted order — the writer stores the list
/// verbatim, so the round trip must preserve it exactly.
fn alias_asserted_event(tenant: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: ts(1_775_127_520),
        payload: AuditPayload::AliasAsserted {
            representative_id: 30,
            member_ids: vec![20, 10, 20],
            actor: ActorId::new("op-alice").expect("non-empty actor"),
            reason: "deploy 2026-06 re-split the login template".to_string(),
        },
    }
}

/// An `alias_retracted` audit event with the empty member list (the
/// common single-id retraction) and no reason — exercising the §3.7
/// empty-list-vs-NULL distinction and the `"" ↔ NULL` reason rule.
fn alias_retracted_event(tenant: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: ts(1_775_127_530),
        payload: AuditPayload::AliasRetracted {
            representative_id: 20,
            member_ids: Vec::new(),
            actor: ActorId::new("op-bob").expect("non-empty actor"),
            reason: String::new(),
        },
    }
}

/// Scenario RFC0005.14 — alias audit events round-trip and back the
/// v1 map derivation (amendment 2026-06-12; the derivation half lives
/// in `ourios-querier`). Per the RFC0005.12 pattern: write an
/// `alias_asserted`, an `alias_retracted`, and a `template_widened`
/// event through `AuditWriter`, read back via `AuditReader`, and
/// assert each kind's columns populated / null per §3.7 — the full
/// asserted set verbatim, the empty-list retraction (≠ NULL), the
/// actor, and the `"" ↔ NULL` reason round trip.
#[test]
fn rfc0005_14_alias_audit_events_round_trip() {
    // Arrange — one of each alias kind plus a template event in the
    // same partition.
    let bucket = TempDir::new().unwrap();
    let template = three_variants("acme").remove(0);
    let asserted = alias_asserted_event("acme");
    let retracted = alias_retracted_event("acme");
    let events = vec![template.clone(), asserted.clone(), retracted.clone()];
    let partition = audit_partition_for(&events[0]);
    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    // Act
    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    // Assert — full equality: the member set verbatim (order and the
    // duplicate preserved), the actor, the non-empty reason; the
    // retraction's empty member list reads back as an empty list (the
    // decode requires non-NULL for alias kinds, so equality proves it
    // was stored as a list, not NULL); its on-disk NULL reason decodes
    // to the in-memory empty string.
    assert_eq!(round_tripped, events);

    // And the §3.8-rule-6 per-kind NULL discipline at the raw column
    // level: the template row's alias_* columns are NULL, the alias
    // rows' template / compaction columns are NULL.
    let file = std::fs::File::open(&written.path).expect("open raw");
    let batches: Vec<_> =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("builder")
            .build()
            .expect("reader")
            .collect::<Result<_, _>>()
            .expect("batches");
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    let null_at = |column: &str, row: usize| {
        use arrow_array::Array;
        let idx = batch.schema().index_of(column).expect("column exists");
        batch.column(idx).is_null(row)
    };
    // Row 0 is the template event; rows 1–2 are the alias events.
    for column in [
        audit_columns::ALIAS_REPRESENTATIVE_ID,
        audit_columns::ALIAS_MEMBER_IDS,
        audit_columns::ALIAS_ACTOR,
    ] {
        assert!(
            null_at(column, 0),
            "{column} must be NULL on a template row"
        );
        assert!(!null_at(column, 1), "{column} must be set on an alias row");
        assert!(!null_at(column, 2), "{column} must be set on an alias row");
    }
    for column in [
        audit_columns::TEMPLATE_ID,
        audit_columns::OLD_VERSION,
        audit_columns::NEW_VERSION,
        audit_columns::OLD_TEMPLATE,
        audit_columns::NEW_TEMPLATE,
        audit_columns::POSITIONS_WIDENED,
        audit_columns::SLOTS_EXPANDED,
        audit_columns::TRIGGERING_LINE_HASH,
        audit_columns::COMPACTION_PARTITION,
        audit_columns::COMPACTION_INPUT_FILES,
        audit_columns::COMPACTION_OUTPUT_FILE,
        audit_columns::COMPACTION_GENERATION,
        audit_columns::COMPACTION_ROWS,
    ] {
        assert!(null_at(column, 1), "{column} must be NULL on an alias row");
        assert!(null_at(column, 2), "{column} must be NULL on an alias row");
    }
    // The `"" ↔ NULL` reason rule on disk: non-empty reason stored,
    // empty reason stored as NULL.
    assert!(!null_at(audit_columns::REASON, 1));
    assert!(null_at(audit_columns::REASON, 2));
}

/// RFC0005.12 — a `compaction` audit event round-trips with its
/// `compaction_*` columns populated and the template columns NULL,
/// and a template event in the same file keeps the inverse (the
/// §3.8-rule-6 required-by-convention contract).
#[test]
fn rfc0005_12_compaction_audit_event_round_trips() {
    // Arrange — one template event + one compaction event, same
    // partition.
    let bucket = TempDir::new().unwrap();
    let template = three_variants("acme").remove(0);
    let compaction = compaction_event("acme");
    let events = vec![template.clone(), compaction.clone()];
    let partition = audit_partition_for(&events[0]);
    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    // Act
    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    // Assert — both events survive, byte-for-byte (the compaction
    // payload's columns and the template payload's columns are
    // mutually exclusive and both reconstruct).
    assert_eq!(round_tripped.len(), 2);
    assert!(
        round_tripped.contains(&compaction),
        "compaction event round-trips with compaction_* columns and NULL template columns",
    );
    assert!(
        round_tripped.contains(&template),
        "the template event in the same file is unaffected",
    );
}

/// RFC 0025 §3.3 — a `record_quarantined` audit event round-trips
/// with its `quarantine_*` columns populated and every other payload
/// group NULL, alongside template and compaction events in the same
/// file (the §3.8-rule-6 required-by-convention contract, extended
/// to the third system-scoped kind).
#[test]
fn rfc0025_record_quarantined_audit_event_round_trips() {
    let bucket = TempDir::new().unwrap();
    let template = three_variants("acme").remove(0);
    let compaction = compaction_event("acme");
    let quarantine = quarantine_event("acme");
    let events = vec![template.clone(), compaction.clone(), quarantine.clone()];
    let partition = audit_partition_for(&events[0]);
    let mut writer = AuditWriter::open(bucket.path(), partition.clone()).expect("open");
    writer.append_events(&events).expect("append");
    let written = writer.close().expect("close");

    let reader = AuditReader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    assert_eq!(round_tripped.len(), 3);
    assert!(
        round_tripped.contains(&quarantine),
        "quarantine event round-trips with quarantine_* columns and NULLs elsewhere",
    );
    assert!(
        round_tripped.contains(&compaction) && round_tripped.contains(&template),
        "the other kinds in the same file are unaffected",
    );
}
