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
fn h1_2_lossy_zone_match_retains_body() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};

    // Arrange — two length-3 lines sharing the `user logged *`
    // prefix, differing at the final token. Neither token masks to
    // a tag (no NUM/UUID/IP match), so the templates are exactly
    // `["user", "logged", "in"]` and `["user", "logged", "out"]`.
    // sim_seq = 2/3 ≈ 0.667 lands in the §6.3 lossy zone for the
    // project defaults (floor 0.4 ≤ 0.667 < threshold 0.7), so the
    // second line must NOT force-merge into the first candidate:
    // it gets a fresh leaf, retains its body, and carries
    // `confidence = sim/threshold < 1.0` with `lossy_flag = false`
    // (`lossy_flag` marks non-reconstructable records — tokenizer or
    // parse failure — whereas a lossy-zone match is fully
    // reconstructable). This is the §3.1 "no silent merge"
    // boundary expressed as a positive: a sub-threshold match is
    // recorded losslessly, never coalesced.
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };
    let raw_first = "user logged in";
    let raw_lossy = "user logged out";

    // Act — the first line creates the candidate leaf; the second
    // scores in the lossy zone against it.
    let id_first = cluster.ingest(&make(raw_first));
    let id_lossy = cluster.ingest(&make(raw_lossy));

    // Assert — the lossy-zone line takes a fresh template_id rather
    // than the candidate's (no silent sub-threshold merge), and the
    // tenant now holds two templates.
    assert_ne!(
        id_first, NO_TEMPLATE,
        "the candidate line allocated a real template",
    );
    assert_ne!(
        id_lossy, NO_TEMPLATE,
        "the lossy-zone line allocates its own fresh leaf",
    );
    assert_ne!(
        id_first, id_lossy,
        "a lossy-zone match must create a fresh leaf, never coalesce into the candidate (§3.1)",
    );
    assert_eq!(
        cluster.template_count(&t),
        2,
        "lossy-zone fresh-leaf creation yields a distinct second template",
    );
    assert_eq!(
        cluster.merges_total(),
        0,
        "the lossy zone must not widen / merge — that would be a sub-threshold silent merge",
    );

    // Body retention — the heart of H1.2. The lossy-zone record's
    // `body` column carries the original line bytes verbatim (§3.3
    // / §6.3), and `lossy_flag` stays false because the line
    // tokenized cleanly and reconstruction still succeeds.
    let emitted = records.drain();
    assert_eq!(emitted.len(), 2, "one record per ingest");
    let first_rec = &emitted[0];
    let lossy_rec = &emitted[1];

    assert_eq!(
        lossy_rec.template_id, id_lossy,
        "the emitted lossy record carries its fresh template_id",
    );
    assert_eq!(
        lossy_rec.body.as_deref(),
        Some(raw_lossy),
        "the lossy zone retains the original line bytes in the body column (§6.3)",
    );
    assert!(
        !lossy_rec.lossy_flag,
        "lossy_flag marks non-reconstructable records (tokenizer/parse failure), not a reconstructable lossy-zone match",
    );

    // Confidence reflects the sub-threshold match: sim/threshold =
    // (2/3)/0.7 ≈ 0.952, strictly below the 1.0 clean-attach
    // boundary. Compared with a tolerance because the ratio is an
    // f32 with a repeating-decimal numerator.
    let expected_confidence = (2.0_f32 / 3.0) / 0.7;
    assert!(
        (lossy_rec.confidence - expected_confidence).abs() < 1e-6,
        "lossy-zone confidence must be sim/threshold, got {}",
        lossy_rec.confidence,
    );
    assert!(
        lossy_rec.confidence < 1.0,
        "a lossy-zone confidence is below the clean-attach boundary by construction",
    );

    // The candidate (first) line is a clean fresh leaf: confidence
    // 1.0 sentinel, no body retention. Pins that the lossy
    // semantics are specific to the second line, not a blanket
    // change to fresh-leaf creation.
    assert!(
        (first_rec.confidence - 1.0).abs() < f32::EPSILON,
        "the candidate fresh leaf carries the clean-attach confidence sentinel",
    );
    assert!(
        first_rec.body.is_none(),
        "a no-candidate fresh leaf does not retain body — only the lossy zone does",
    );

    // §3.1 telemetry: exactly one body-retention event (the lossy
    // line). The candidate fresh leaf and a clean attach do not
    // count; the §6.6 tokenizer-failure path is excluded by
    // contract.
    assert_eq!(
        cluster.body_retentions_total(),
        1,
        "the lossy zone is a §3.1 body_retention_ratio event; the candidate fresh leaf is not",
    );
    assert_eq!(
        cluster.parse_failures_total(),
        0,
        "the lossy zone is not a parse failure — a template was allocated",
    );
}

/// Scenario H1.3 — Every widening emits an audit event.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h1_3_every_widening_emits_an_audit_event() {
    use ourios_core::audit::{AuditPayload, SharedAuditSink, TemplateChange};
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
    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_template,
                new_template,
                ..
            },
        ..
    } = &e.payload
    else {
        panic!("expected Template/Widened, got {:?}", e.payload);
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
fn h1_4_severity_number_is_part_of_template_key() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — two records whose body text and scope are byte-for-
    // byte identical, differing only in `severity_number`: `9`
    // (INFO) vs `17` (ERROR). Per RFC §6.1 *Template-key
    // composition*, `severity_number` is part of the key, so the
    // two must never share a `template_id` — an INFO line surfacing
    // under an ERROR query (or vice versa) is the §3.1 / §4-hazard-1
    // silent-merge bug in disguise. The body text masks to no tag,
    // so a key that ignored severity would collapse them
    // (sim_seq = 1.0).
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |severity: u8| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String("user logged in".to_string())),
        severity_number: severity,
        scope_name: Some("lib.auth".to_string()),
        ..Default::default()
    };

    // Act
    let id_info = cluster.ingest(&make(9));
    let id_error = cluster.ingest(&make(17));

    // Assert — distinct ids, two templates, and no widening or
    // merge ever coalesced the two severity buckets.
    assert_ne!(
        id_info, id_error,
        "identical body+scope at different severities must not share a template_id",
    );
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(
        cluster.merges_total(),
        0,
        "no widening may coalesce the INFO and ERROR severity buckets",
    );
}

/// Scenario H1.5 — `scope_name` is part of the template key (no cross-scope silent merge).
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h1_5_scope_name_is_part_of_template_key() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — three records with identical body text and severity,
    // varying only in `scope_name`: `Some("lib.auth")`,
    // `Some("lib.payments")`, and `None`. Per RFC §6.1
    // *Template-key composition*, `scope_name` is part of the key
    // and `None` is its own bucket — so all three must land on
    // distinct `template_id`s. Collapsing two scopes onto one
    // template surfaces a `lib.payments` event under a `lib.auth`
    // query, the §3.1 / §4-hazard-1 silent merge.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |scope: Option<&str>| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String("user logged in".to_string())),
        severity_number: 9,
        scope_name: scope.map(str::to_string),
        ..Default::default()
    };

    // Act
    let id_auth = cluster.ingest(&make(Some("lib.auth")));
    let id_pay = cluster.ingest(&make(Some("lib.payments")));
    let id_none = cluster.ingest(&make(None));

    // Assert — all three distinct (incl. the `None` bucket), three
    // templates, no widening.
    assert_ne!(
        id_auth, id_pay,
        "identical body+severity across two named scopes must not share a template_id",
    );
    assert_ne!(
        id_auth, id_none,
        "a named scope must not share a template_id with the `None` scope bucket",
    );
    assert_ne!(
        id_pay, id_none,
        "a named scope must not share a template_id with the `None` scope bucket",
    );
    assert_eq!(cluster.template_count(&t), 3);
    assert_eq!(
        cluster.merges_total(),
        0,
        "no widening may coalesce two scopes onto one template",
    );
}

/// Scenario H2.1 — Oversized parameter triggers OVERFLOW marker and forced body retention.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h2_1_oversized_parameter_triggers_overflow_marker() {
    use ourios_core::audit::ParamType;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use ourios_miner::reconstruct::reconstruct;

    // Arrange — default `MinerConfig` (256-byte limit). One
    // numeric token longer than 256 bytes (300 ASCII digits)
    // forces the §6.5 overflow path: `mask()` classifies it as
    // `<NUM>`, `params_from_mask` runs the byte-limit check, and
    // the resulting `Param` carries `type_tag = Overflow` plus
    // the `(length, sha256_prefix)` marker. The emitted record's
    // `body` column is set to the original line per §6.5's
    // "overflow forces body retention" rule, and
    // `params_overflow_total` increments by exactly 1.
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");
    let big_num = "9".repeat(300);
    let raw = format!("user {big_num} logged in");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let _ = cluster.ingest(&make(&raw));

    // Assert — exactly one record emitted, with an Overflow
    // marker at the params slot for the oversized numeric.
    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let rec = &emitted[0];

    assert_eq!(rec.params.len(), 1, "one mask-emit slot (the big number)");
    assert_eq!(
        rec.params[0].type_tag,
        ParamType::Overflow,
        "oversized param must be tagged Overflow per RFC §6.5",
    );
    assert!(
        rec.params[0]
            .value
            .starts_with("OVERFLOW(length=300,sha256="),
        "marker must encode length and sha256 prefix, got: {}",
        rec.params[0].value,
    );

    // Body retention — RFC §6.5: "overflow forces body retention,
    // regardless of lossy_flag." The clean-attach path (this is
    // a fresh leaf) would normally leave body=None; the §6.5
    // path overrides.
    assert_eq!(
        rec.body.as_deref(),
        Some(raw.as_str()),
        "overflow forces body retention so reconstruct can fall back via the body column",
    );

    // Reconstruct — the Overflow branch in §6.6 returns body
    // verbatim. End-to-end round-trip.
    let snapshots = cluster.templates_for(&t);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        reconstruct(rec, &snapshots[0].template),
        raw.as_bytes().to_vec(),
        "reconstruct must round-trip the original bytes via the §6.6 Overflow body-fallback path",
    );

    // §6.8 counter contract: `params_overflow_total` bumped by
    // exactly the overflow count on this record (one oversized
    // param ⇒ one increment).
    assert_eq!(
        cluster.params_overflow_total(),
        1,
        "params_overflow_total must increment per Overflow-tagged param",
    );
}

/// Supplemental H2.1 coverage — overflow applied via the
/// **aligned-params path** (`build_record_params`), not the
/// fresh-leaf `params_from_mask` path the primary H2.1 test
/// exercises. A second ingest with `sim_seq` = 1.0 against an
/// existing wildcard slot routes through clean-reuse + the
/// per-slot byte-limit check inside `build_record_params`.
#[test]
fn h2_1_overflow_via_aligned_params_at_existing_wildcard() {
    use ourios_core::audit::ParamType;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use ourios_miner::reconstruct::reconstruct;

    // Arrange — L1 creates a fresh leaf with `<NUM>` mask emit
    // at position 1 (Wildcard slot, slot_types[0] = {Num}). L2
    // brings a *new* numeric value at the same slot — sim_seq
    // matches the existing Wildcard so the attach is clean and
    // params are rebuilt through `build_record_params`. The
    // §6.5 byte-limit check on that helper must catch a
    // >256-byte value at the slot.
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // L1: 4-token line, mask emits `<NUM>` at position 1 (the
    // `42`). Creates the wildcard slot.
    let _ = cluster.ingest(&make("user 42 logged in"));
    let _ = records.drain();
    assert_eq!(cluster.params_overflow_total(), 0);

    // L2: same shape, but the numeric at position 1 is 300 digits
    // — exceeds the 256-byte default limit. Routes through the
    // aligned-params path (clean-reuse, `sim_seq` = 1.0).
    let big_num = "9".repeat(300);
    let raw_l2 = format!("user {big_num} logged in");
    let _ = cluster.ingest(&make(&raw_l2));

    // Assert — emitted record (L2) carries the Overflow marker,
    // retained body, and the counter bumped.
    let emitted = records.drain();
    assert_eq!(
        emitted.len(),
        1,
        "L2 is the only ingest since the last drain"
    );
    let rec = &emitted[0];

    assert_eq!(rec.params.len(), 1, "one wildcard slot at position 1");
    assert_eq!(
        rec.params[0].type_tag,
        ParamType::Overflow,
        "the aligned-params builder must apply the §6.5 cap",
    );
    assert!(
        rec.params[0]
            .value
            .starts_with("OVERFLOW(length=300,sha256="),
        "unexpected marker on the aligned-params path: {}",
        rec.params[0].value,
    );
    assert_eq!(
        rec.body.as_deref(),
        Some(raw_l2.as_str()),
        "overflow on the aligned-params path must also force body retention",
    );

    // Reconstruct round-trips via the §6.6 Overflow body-fallback.
    let snapshots = cluster.templates_for(&t);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        reconstruct(rec, &snapshots[0].template),
        raw_l2.as_bytes().to_vec(),
        "reconstruct must round-trip the original line via the Overflow body fallback",
    );

    assert_eq!(
        cluster.params_overflow_total(),
        1,
        "params_overflow_total must increment via the aligned-params path too",
    );
}

/// Supplemental H2.1 coverage — the byte-limit decision follows
/// the **per-tenant `MinerConfig::param_byte_limit`** (RFC 0004
/// §3.4), not just the cluster default. Two tenants with
/// different limits ingest the same line; only the
/// stricter-limit tenant trips the marker.
#[test]
fn h2_1_per_tenant_byte_limit_override_honoured() {
    use ourios_core::audit::ParamType;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — cluster default = 256-byte limit; tenant B
    // overridden to 64. A 100-byte numeric should pass through
    // for tenant A (no overflow) and trip the marker for tenant B
    // (over the 64-byte limit). Both tenants see the same line
    // shape.
    let tenant_a = TenantId::new("tenant-a");
    let tenant_b = TenantId::new("tenant-b");
    let strict = MinerConfig::try_new_full(0.7, 0.4, 64).expect("64-byte limit is valid");
    let records = SharedRecordSink::new();
    let mut cluster = MinerCluster::new(MinerConfig::default())
        .with_record_sink(Box::new(records.clone()))
        .with_tenant_config(tenant_b.clone(), strict);
    let make = |tenant: &TenantId, text: &str| OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // 100-byte numeric value.
    let medium_num = "9".repeat(100);
    let raw = format!("user {medium_num} logged in");

    // Act — same line into both tenants.
    let _ = cluster.ingest(&make(&tenant_a, &raw));
    let _ = cluster.ingest(&make(&tenant_b, &raw));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 2);
    let rec_a = &emitted[0];
    let rec_b = &emitted[1];

    // Tenant A — under the default 256-byte limit, the param
    // passes through as `Num` with the original value. No
    // marker, no body retention, counter untouched.
    assert_eq!(rec_a.params[0].type_tag, ParamType::Num);
    assert_eq!(rec_a.params[0].value, medium_num);
    assert!(rec_a.body.is_none(), "tenant A's value is under its limit");

    // Tenant B — over the 64-byte override, so the marker fires
    // with retained body.
    assert_eq!(
        rec_b.params[0].type_tag,
        ParamType::Overflow,
        "tenant B's stricter byte_limit must trip the marker for the same value",
    );
    assert!(
        rec_b.params[0]
            .value
            .starts_with("OVERFLOW(length=100,sha256="),
    );
    assert_eq!(rec_b.body.as_deref(), Some(raw.as_str()));

    // Counter: exactly one overflow event (tenant B only).
    assert_eq!(
        cluster.params_overflow_total(),
        1,
        "params_overflow_total must follow per-tenant byte_limit",
    );
}

/// Scenario H2.2 — Per-service overflow rate above 1% raises an alert.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// "Alert" is the `params_overflow_ratio{tenant_id, service}`
/// gauge crossing the documented `0.01` threshold (Ourios ships the
/// metric + the alert rule, not an alerting engine). Ingests a
/// per-service line stream whose overflow rate exceeds 1%, collects
/// the exported stream, and asserts the gauge for the over-1%
/// service is above `0.01` while a clean sibling service stays at
/// `0.0` — the per-service isolation H2.2 hinges on.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn h2_2_per_service_overflow_rate_above_one_percent_alerts() {
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{AnyValue, Body, KeyValue, OtlpLogRecord, any_value};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    const ALERT_THRESHOLD: f64 = 0.01;

    // Arrange — in-memory provider, then the miner.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("acme");

    let svc_attr = |service: &str| {
        vec![KeyValue {
            key: "service.name".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(service.to_string())),
            }),
            ..Default::default()
        }]
    };
    let make = |service: &str, text: String| OtlpLogRecord {
        tenant_id: t.clone(),
        resource_attributes: svc_attr(service),
        body: Some(Body::String(text)),
        ..Default::default()
    };

    // A param value over the 256 B limit (§6.5) forces an Overflow
    // marker; each such line counts once toward the service's
    // overflow numerator.
    let big = "9".repeat(300);

    // `noisy`: 200 clean lines + 3 overflow lines → 3/203 ≈ 0.0148
    // > 1%. `quiet`: 100 clean lines, no overflow → 0.0.
    for i in 0..200 {
        cluster.ingest(&make("noisy", format!("user {i} logged in")));
    }
    for _ in 0..3 {
        cluster.ingest(&make("noisy", format!("user {big} logged in")));
    }
    for i in 0..100 {
        cluster.ingest(&make("quiet", format!("order {i} shipped")));
    }
    guard.force_flush().expect("force_flush succeeds");

    // Act — read the per-service ratio gauge.
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let ratio_for = |service: &str| -> f64 {
        let data = rms
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|m| m.name() == ourios_semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION)
            .expect("params_overflow_ratio missing from exported stream")
            .data();
        let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = data else {
            panic!("params_overflow_ratio should be an f64 gauge");
        };
        gauge
            .data_points()
            .find(|dp| {
                let mut tenant_ok = false;
                let mut service_ok = false;
                for kv in dp.attributes() {
                    match kv.key.as_str() {
                        k if k == ourios_semconv::OURIOS_TENANT && kv.value.as_str() == "acme" => {
                            tenant_ok = true;
                        }
                        k if k == ourios_semconv::OURIOS_SERVICE
                            && kv.value.as_str() == service =>
                        {
                            service_ok = true;
                        }
                        _ => {}
                    }
                }
                tenant_ok && service_ok
            })
            .unwrap_or_else(|| panic!("params_overflow_ratio missing the (acme, {service}) point"))
            .value()
    };

    // Assert — the over-1% service crosses the alert threshold; the
    // clean sibling does not (per-service isolation).
    let noisy = ratio_for("noisy");
    let quiet = ratio_for("quiet");
    assert!(
        noisy > ALERT_THRESHOLD,
        "noisy service overflow ratio {noisy} must exceed the {ALERT_THRESHOLD} alert threshold",
    );
    assert!(
        quiet <= ALERT_THRESHOLD,
        "quiet service overflow ratio {quiet} must stay at/under the alert threshold",
    );
}

/// Scenario H5.1 — Wildcard widening increments `template_version` and emits `template_widened`.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn h5_1_wildcard_widening_increments_version_and_emits_template_widened() {
    use ourios_core::audit::{AuditPayload, SharedAuditSink, TemplateChange};
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
    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version,
                new_version,
                positions_widened,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!("expected Template/Widened, got {:?}", events[0].payload);
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
///
/// This is the *type-expansion-only* case, distinct from H5.1's
/// structural widening: no `Fixed` token mismatches, so no
/// `template_widened` event fires. A pre-existing wildcard slot
/// whose `slot_types = {Num}` simply observes a `Str` value and
/// grows to `{Num, Str}`.
#[test]
fn h5_2_type_expansion_increments_version_and_emits_template_type_expanded() {
    use ourios_core::audit::{AuditPayload, ParamType, SharedAuditSink, SlotTypes, TemplateChange};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — L1 creates a 4-token leaf at template_version = 1.
    // The default `prefix_depth` is 2, so positions 0 and 1
    // (`request`, `id`) key the prefix path; the `<NUM>` mask emit
    // sits at position 2 (outside the prefix), giving the leaf a
    // single wildcard slot with `slot_types[0] = {Num}`. Keeping
    // the numeric slot outside the prefix is what lets L2 — which
    // carries a *string* at that position — still route to the
    // same leaf.
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    let _ = cluster.ingest(&make("request id 42 done"));
    // Discard L1: it created a fresh leaf, which emits no audit
    // event (RFC0001.1) — but drain anyway so the assert below
    // sees exactly the L2 trail.
    let _ = sink.drain();

    let pre = cluster.templates_for(&t);
    assert_eq!(pre.len(), 1, "L1 created exactly one leaf");
    assert_eq!(pre[0].template_version, 1, "fresh leaf starts at v1");
    assert_eq!(
        pre[0].slot_types,
        vec![SlotTypes::singleton(ParamType::Num)],
        "the position-2 wildcard slot starts as {{Num}}",
    );

    // Act — L2 matches every `Fixed` position (`request`, `id`,
    // `done`) exactly and carries the literal `abc` at position 2.
    // sim_seq = 1.0 (the wildcard slot matches by definition), so
    // there is no widening; the slot merely sees a new `ParamType`
    // (`Str`), triggering a pure type expansion.
    let _ = cluster.ingest(&make("request id abc done"));

    // Assert — exactly one audit event, a `TypeExpanded` (no
    // `Widened`), bumping the version 1 → 2 and naming slot 0 with
    // the newly-observed `Str`.
    let events = sink.drain();
    assert_eq!(
        events.len(),
        1,
        "type-expansion-only attach emits exactly one event (no widening)",
    );
    let AuditPayload::Template {
        change:
            TemplateChange::TypeExpanded {
                old_version,
                new_version,
                slots_expanded,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "expected Template/TypeExpanded, got {:?}",
            events[0].payload,
        );
    };
    assert_eq!(*old_version, 1, "leaf was at version 1 before the attach");
    assert_eq!(
        *new_version, 2,
        "type expansion bumps template_version by one",
    );
    assert_eq!(slots_expanded.len(), 1, "exactly one slot expanded");
    assert_eq!(
        slots_expanded[0].slot_index, 0,
        "the position-2 slot is wildcard ordinal 0",
    );
    assert_eq!(
        slots_expanded[0].added_types,
        vec![ParamType::Str],
        "the slot gained Str (the literal `abc` classified as Str)",
    );

    // The leaf's stored type set grew {Num} → {Num, Str}, and the
    // version reflects the single bump.
    let post = cluster.templates_for(&t);
    assert_eq!(
        post.len(),
        1,
        "type expansion reuses the leaf — no new template",
    );
    assert_eq!(post[0].template_version, 2);
    assert_eq!(
        post[0].slot_types,
        vec![SlotTypes::singleton(ParamType::Num).insert(ParamType::Str)],
        "slot_types[0] became {{Num, Str}}",
    );

    // RFC §6.4: `TemplateTypeExpanded` counts toward merges_total.
    assert_eq!(
        cluster.merges_total(),
        1,
        "the lone type expansion increments merges_total by one",
    );
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
///
/// Property: for every record `r` emitted by the miner across
/// the committed `testdata/corpus/` where `r.lossy_flag = false`,
/// `reconstruct(r, &template_at_emit_time) == r.ingested_bytes`
/// byte-for-byte (the §3.3 invariant — `CLAUDE.md` §3.3 /
/// RFC 0001 §6.6).
///
/// **Template-version snapshotting.** A widening (§6.4) mints a
/// new `template_version` and the prior version's literal shape
/// is sealed: once a record is emitted at `(id, v)`, the v's
/// token sequence never changes. So the test snapshots templates
/// into a `(template_id, template_version) -> tokens` map after
/// every ingest, using `or_insert_with` to preserve the
/// first-seen shape for each version. A record at `(id, v_emit)`
/// is then reconstructed against the v_emit-era template, even
/// when a later ingest in the same run pushed the leaf to v+1.
#[test]
fn h7_1_reconstruction_property_holds_across_corpus() {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use ourios_miner::reconstruct::reconstruct;
    use ourios_miner::tree::OwnedToken;

    // Arrange — load every `*.txt` file under `testdata/corpus/`.
    // Paths are resolved relative to the workspace root via
    // CARGO_MANIFEST_DIR so the test runs identically under
    // `cargo test` and `cargo test --workspace`.
    //
    // `MANIFEST_DIR` points at `crates/ourios-miner/`; the
    // corpus sits at the repo root.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let corpus_dir = Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above CARGO_MANIFEST_DIR")
        .join("testdata/corpus");

    let mut lines: Vec<String> = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(&corpus_dir)
        .unwrap_or_else(|e| panic!("read_dir({}): {e}", corpus_dir.display()))
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        })
        .collect();
    // Sort by file name so the test is deterministic across
    // platforms (directory iteration order is filesystem-
    // dependent).
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read_to_string({}): {e}", path.display()));
        for line in contents.lines() {
            let trimmed = line.trim_end_matches(['\r']);
            if trimmed.is_empty() {
                continue;
            }
            lines.push(trimmed.to_string());
        }
    }
    assert!(
        !lines.is_empty(),
        "corpus is empty — testdata/corpus/*.txt must contribute at least one line",
    );

    // Ingest every line through a fresh cluster. One ingest →
    // one emit, in order, so `emitted[i]` corresponds to
    // `lines[i]`. After each ingest, snapshot the current
    // template shape for every leaf into a version-keyed map
    // (`or_insert_with` so the first time a `(id, v)` pair is
    // observed wins — subsequent widenings produce `(id, v+1)`
    // entries without clobbering the earlier seal).
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("corpus");
    let mut version_snapshots: HashMap<(u64, u32), Vec<OwnedToken>> = HashMap::new();
    for line in &lines {
        cluster.ingest(&OtlpLogRecord {
            tenant_id: t.clone(),
            body: Some(Body::String(line.clone())),
            ..Default::default()
        });
        for snap in cluster.templates_for(&t) {
            version_snapshots
                .entry((snap.template_id, snap.template_version))
                .or_insert_with(|| snap.template.clone());
        }
    }
    let emitted = records.drain();
    assert_eq!(
        emitted.len(),
        lines.len(),
        "one record per ingested line (RFC §6.1 emit contract)",
    );

    // Property: every non-lossy record reconstructs byte-for-byte
    // against the template that was active at its emit-time
    // `template_version`.
    let mut non_lossy_count = 0usize;
    for (idx, (line, rec)) in lines.iter().zip(emitted.iter()).enumerate() {
        if rec.lossy_flag {
            continue;
        }
        non_lossy_count += 1;
        let template = version_snapshots
            .get(&(rec.template_id, rec.template_version))
            .unwrap_or_else(|| {
                panic!(
                    "corpus line {idx} (`{line}`): emitted record \
                     (template_id={}, template_version={}) has no \
                     matching versioned snapshot",
                    rec.template_id, rec.template_version,
                )
            });
        let recovered = reconstruct(rec, template);
        assert_eq!(
            recovered,
            line.as_bytes(),
            "H7.1 §3.3: corpus line {idx} (`{line}`) failed bit-identical reconstruction \
             (template_id {}, template_version {})",
            rec.template_id,
            rec.template_version,
        );
    }

    // Sanity guard: a future regression that flips every record
    // to `lossy_flag = true` would skip every assertion above
    // and pass silently. Require at least one non-lossy record
    // — the H7.1 property is vacuously true otherwise.
    assert!(
        non_lossy_count > 0,
        "H7.1 must exercise at least one non-lossy record; corpus produced zero",
    );
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
