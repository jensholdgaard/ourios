//! RFC 0001 §5.1 — Hazards. Acceptance criteria stubs for H1, H2,
//! H5, H7. Each `#[test]` carries the scenario id in its doc
//! comment so `grep -R "H1.1" .` resolves bidirectionally between
//! the RFC and the tests (`docs/verification.md` §2.3).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! invocation skips them (outer loop / CI stays green). The Red
//! signal lives at the inner loop: an implementor working on a
//! stub runs `cargo test <name> -- --ignored` and watches the
//! `todo!()` panic. See `docs/verification.md` §3.

/// Scenario H1.1 — Semantically distinct templates do not silently merge.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h1_1_login_and_logout_remain_distinct_at_default_threshold() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — the canonical `hazards.md` H1 horror: two
    // length-3 lines differing at one position. After masking
    // neither token gets a tag (no NUM/UUID/IP match) so the
    // templates are exactly `["user", "logged", "in"]` vs
    // `["user", "logged", "out"]`. sim_seq = 2/3 ≈ 0.667,
    // strictly below the default 0.7 threshold, so the §6.2
    // step-4 candidate selection falls through to fresh-leaf
    // creation — no widening, no audit, no silent merge.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let id_in = cluster.ingest(&make("user logged in"));
    let id_out = cluster.ingest(&make("user logged out"));

    // Assert — distinct ids, two templates, zero widenings.
    assert_ne!(
        id_in, id_out,
        "the two semantically distinct lines must not silently merge \
         at the default threshold",
    );
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.merges_total(), 0);
}

/// Scenario H1.2 — Lossy-zone match retains body.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_2_lossy_zone_match_retains_body() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H1.3 — Every widening emits an audit event.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h1_3_every_widening_emits_an_audit_event() {
    use ourios_core::audit::{AuditEventKind, SharedAuditSink};
    use ourios_core::clock::TestClock;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use std::time::{Duration, SystemTime};

    // Arrange — two length-6 lines differing at one position.
    // After masking they share the `user <NUM> logged * from
    // <IP>` shape; sim_seq = 5/6 ≈ 0.833 ≥ 0.7 → widens
    // position 3. The §3.1 invariant requires this widening to
    // emit an audit event naming the old + new templates, the
    // tenant, the kind, and a timestamp.
    //
    // A `TestClock` pins the timestamp deterministically so the
    // assertion is `==` rather than `<= SystemTime::now()`, which
    // would flake under NTP step / leap seconds / VM pause.
    let pinned = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()))
        .with_clock(Box::new(TestClock::new(pinned)));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let _ = cluster.ingest(&make("user 42 logged in from 10.0.0.1"));
    let _ = cluster.ingest(&make("user 42 logged out from 10.0.0.1"));

    // Assert — exactly one audit event, schema fields populated.
    let events = sink.drain();
    assert_eq!(events.len(), 1, "exactly one widening occurred");
    let e = &events[0];
    assert_eq!(e.tenant_id, t);
    let AuditEventKind::TemplateWidened {
        old_template,
        new_template,
        ..
    } = &e.kind
    else {
        panic!("expected TemplateWidened, got {:?}", e.kind);
    };
    assert!(!old_template.is_empty(), "old_template must be recorded");
    assert!(!new_template.is_empty(), "new_template must be recorded");
    assert_ne!(
        old_template, new_template,
        "old and new templates must differ — the event records the change",
    );
    // The `event_type` clause from the scenario is satisfied by
    // the `TemplateWidened` variant match above; the timestamp
    // clause is satisfied by the `TestClock`-pinned value.
    assert_eq!(
        e.timestamp, pinned,
        "timestamp must be the value the cluster's clock returned at emit time",
    );
}

/// Scenario H1.4 — `severity_number` is part of the template key (no INFO/ERROR silent merge).
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_4_severity_number_is_part_of_template_key() {
    todo!("RFC 0001 §6.1 (Template-key composition), §6.2");
}

/// Scenario H1.5 — `scope_name` is part of the template key (no cross-scope silent merge).
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_5_scope_name_is_part_of_template_key() {
    todo!("RFC 0001 §6.1 (Template-key composition), §6.2");
}

/// Scenario H2.1 — Oversized parameter triggers OVERFLOW marker and forced body retention.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h2_1_oversized_parameter_triggers_overflow_marker() {
    todo!("RFC 0001 §6.5");
}

/// Scenario H2.2 — Per-service overflow rate above 1% raises an alert.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h2_2_per_service_overflow_rate_above_one_percent_alerts() {
    todo!("RFC 0001 §6.5");
}

/// Scenario H5.1 — Wildcard widening increments `template_version` and emits `template_widened`.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h5_1_wildcard_widening_increments_version_and_emits_template_widened() {
    use ourios_core::audit::{AuditEventKind, SharedAuditSink};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — set up a single leaf at template_version = 1,
    // then trigger one widening. Per RFC §6.4 the audit event
    // must be a `TemplateWidened` carrying the new wildcard
    // position(s) and the version bump (old_version = 1,
    // new_version = 2).
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let _ = cluster.ingest(&make("user 42 logged in from 10.0.0.1"));
    let _ = cluster.ingest(&make("user 42 logged out from 10.0.0.1"));

    // Assert
    let events = sink.drain();
    assert_eq!(events.len(), 1);
    let AuditEventKind::TemplateWidened {
        old_version,
        new_version,
        positions_widened,
        ..
    } = &events[0].kind
    else {
        panic!("expected TemplateWidened, got {:?}", events[0].kind);
    };
    assert_eq!(*old_version, 1, "leaf was at version 1 before this attach");
    assert_eq!(
        *new_version, 2,
        "wildcard widening bumps template_version by one",
    );
    assert_eq!(
        *positions_widened,
        vec![3],
        "position 3 (`in` → `<*>`) is the only mismatched fixed position",
    );
}

/// Scenario H5.2 — Type expansion increments `template_version` and emits `template_type_expanded`.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h5_2_type_expansion_increments_version_and_emits_template_type_expanded() {
    todo!("RFC 0001 §6.4");
}

/// Scenario H5.3 — Drift query returns templates that gained a version in window.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h5_3_drift_query_returns_templates_that_gained_a_version() {
    todo!("RFC 0001 §6.7");
}

/// Scenario H7.1 — Reconstruction property holds across the corpus.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h7_1_reconstruction_property_holds_across_corpus() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H7.2 — Tokenizer failure sets `lossy_flag = true` and retains body.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h7_2_tokenizer_failure_sets_lossy_flag_and_retains_body() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::{BodyKind, SharedRecordSink};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};

    // Arrange — a line carrying an embedded NUL byte (RFC §6.2
    // step 1's canonical tokenizer-failure mode). The miner must
    // route the line to the parse-failure path with the original
    // bytes retained in `body` and `lossy_flag = true`.
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");
    let raw = "user 42\u{0000}secret";
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let id = cluster.ingest(&make(raw));

    // Assert — NO_TEMPLATE sentinel returned, parse-failures
    // counter ticked, and the emitted record carries body=raw,
    // lossy_flag=true, no template id. `body_retentions_total`
    // stays at zero: the §6.6 tokenizer-failure path is
    // orthogonal to the §6.3 body-retention paths the gauge
    // counts (the body IS retained on the emitted record, but
    // not as a §3.1 `body_retention_ratio` event).
    assert_eq!(id, NO_TEMPLATE);
    assert_eq!(cluster.parse_failures_total(), 1);
    assert_eq!(
        cluster.body_retentions_total(),
        0,
        "tokenizer-failure retention is orthogonal to §6.3 retentions and must not inflate the ratio",
    );

    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let rec = &emitted[0];
    assert_eq!(rec.body_kind, BodyKind::String);
    assert_eq!(rec.template_id, NO_TEMPLATE);
    assert_eq!(rec.template_version, 0);
    assert!(rec.lossy_flag, "tokenizer failure must set lossy_flag=true");
    assert_eq!(
        rec.body.as_deref(),
        Some(raw),
        "body must carry the original line bytes verbatim, NUL included",
    );
}

/// Scenario H7.3 — Reader emits body verbatim when `lossy_flag` is true.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h7_3_reader_emits_body_verbatim_when_lossy_flag_is_true() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H7.4 — Widened literal slot reconstructs via STR fallback.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h7_4_widened_literal_slot_reconstructs_via_str_fallback() {
    use ourios_core::audit::ParamType;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use ourios_miner::reconstruct::reconstruct;

    // Arrange — drive a widening of a literal-token position
    // (RFC §6.2 step 5: a `<*>` slot opens via the widening of an
    // originally-literal token). The triggering line then has a
    // literal at that slot; the §6.2 "build params" rule says
    // that position's params entry must be
    // `{ type_tag: STR, value: L_tok[pos] }` so reconstruct can
    // restore the literal.
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // L1 creates a fresh leaf with Wildcard at position 1
    // (`<NUM>`) and Fixed at positions 0, 2, 3, 4.
    let _ = cluster.ingest(&make("user 42 logged in NOW"));
    // L2 (the triggering line for the widening test) mismatches
    // position 4 ("NOW" → "LATER"). sim_seq = 4/5 = 0.8 ≥ 0.7,
    // so this attaches with a widening at position 4. The new
    // wildcard at slot ordinal 1 (the literal-widened one) must
    // carry params[1] = { Str, "LATER" } for reconstruct.
    let raw_l2 = "user 42 logged in LATER";
    let _ = cluster.ingest(&make(raw_l2));

    // Look up the leaf's template so we can pass it to
    // reconstruct.
    let snapshots = cluster.templates_for(&t);
    assert_eq!(snapshots.len(), 1, "single leaf after widening");
    let snap = &snapshots[0];

    // The post-widen leaf template has 2 wildcards: ordinal 0 at
    // position 1 (`<NUM>` from creation), ordinal 1 at position 4
    // (the literal-widened slot).
    let emitted = records.drain();
    assert_eq!(emitted.len(), 2, "one record per ingest");
    let l2_record = &emitted[1];

    // Pin the STR-fallback contract: params[1] = { Str, "LATER" }.
    assert_eq!(l2_record.params.len(), 2);
    assert_eq!(l2_record.params[1].type_tag, ParamType::Str);
    assert_eq!(l2_record.params[1].value, "LATER");

    // And the headline property: reconstruct(L2) == raw_l2 byte
    // for byte.
    assert_eq!(
        reconstruct(l2_record, &snap.template),
        raw_l2.as_bytes().to_vec(),
        "reconstruction must round-trip the literal-widened position",
    );
}
