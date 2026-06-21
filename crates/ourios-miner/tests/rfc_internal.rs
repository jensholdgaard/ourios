//! RFC 0001 §5.3 — RFC-internal design commitments. Acceptance
//! criteria stubs for `RFC0001.x`. Each `#[test]` carries the
//! scenario id in its doc comment so `grep -R "RFC0001.1" .`
//! resolves bidirectionally between the RFC and the tests
//! (`docs/verification.md` §2.3).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! invocation skips them (outer loop / CI stays green). The Red
//! signal lives at the inner loop: an implementor working on a
//! stub runs `cargo test <name> -- --ignored` and watches the
//! `todo!()` panic. See `docs/verification.md` §3.

/// Scenario RFC0001.1 — **superseded by RFC 0017 §3.1.** The original
/// RFC0001.1 contract was "fresh-leaf creation does not emit an audit
/// event". RFC 0017 §3.1 overturns it: a read-time template registry must
/// be able to recover a leaf's version-1 tokens after the originating rows
/// age out, so leaf creation now emits a `template_created` audit event.
/// This test asserts the amended contract — still *not* a merge.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §3.1 / §5
/// (and `docs/rfcs/0001-template-miner.md` §5 for the original scenario).
#[test]
fn rfc0001_1_fresh_leaf_creation_emits_template_created() {
    use ourios_core::audit::{AuditPayload, SharedAuditSink, TemplateChange};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — ingest two structurally distinct lines, both
    // creating fresh leaves.
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let _ = cluster.ingest(&make("user 42 logged in"));
    let _ = cluster.ingest(&make("GET /home 200"));

    // Assert — both lines created fresh leaves, each emitting one
    // `template_created` event; creation is not a merge.
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.merges_total(), 0, "creation is not a merge");

    let events = sink.drain();
    assert_eq!(events.len(), 2, "one template_created event per fresh leaf");
    assert!(
        events.iter().all(|e| matches!(
            &e.payload,
            AuditPayload::Template {
                change: TemplateChange::Created { new_version: 1, .. },
                ..
            }
        )),
        "every fresh leaf emits a version-1 template_created event",
    );
}

/// Scenario RFC0001.2 — Degenerate-template guard rejects fully-wildcard widening.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_2_degenerate_template_guard_rejects_fully_wildcard_widening() {
    use ourios_core::audit::{AuditPayload, SharedAuditSink, TemplateChange};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};

    // Arrange — force a degenerate-widening attempt.
    //
    // The default prefix-tree shape (`prefix_depth = 2`) makes
    // this scenario structurally unreachable: the first two
    // tokens of any line are baked into the descend path, so
    // any leaf reachable as a widening target has those two
    // positions Fixed by construction. To test the §6.4 guard
    // we drop `prefix_depth` to 0 (all length-N lines share one
    // leaf list) so widenings can reach every position. The
    // tunable is exposed precisely for this kind of guard
    // exercise.
    //
    // Threshold of 0.3 so a 1/3-similar attach triggers widening
    // rather than fresh-leaf creation.
    //   L1 = ["alpha", "beta", "gamma"]  — v=1, all-Fixed.
    //   L2 = ["alpha", "xxx", "yyy"]     — sim 1/3 → widens
    //                                       positions 1, 2 →
    //                                       template
    //                                       ["alpha", <*>, <*>],
    //                                       v=2. NOT degenerate.
    //   L3 = ["zzz", "qqq", "rrr"]       — sim 2/3 (two wildcard
    //                                       matches) → would
    //                                       widen position 0 →
    //                                       fully degenerate →
    //                                       rejected.
    let config = MinerConfig::try_new(0.3, 256).expect("valid config");
    let sink = SharedAuditSink::new();
    let mut cluster =
        MinerCluster::with_audit_sink(config, Box::new(sink.clone())).with_prefix_depth(0);
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let l1 = cluster.ingest(&make("alpha beta gamma"));
    let _l2 = cluster.ingest(&make("alpha xxx yyy"));
    let l3 = cluster.ingest(&make("zzz qqq rrr"));

    // Assert — L3 was rejected (NO_TEMPLATE), the rejection
    // was audited but did not increment `merges_total`, and
    // `parse_failures_total` ticked.
    assert_ne!(l1, NO_TEMPLATE, "L1 created the leaf");
    assert_eq!(
        l3, NO_TEMPLATE,
        "L3's widening would be fully wildcard → rejected"
    );
    assert_eq!(
        cluster.merges_total(),
        1,
        "only L2's (non-degenerate) widening counts toward merges_total",
    );
    assert_eq!(
        cluster.parse_failures_total(),
        1,
        "L3's rejection is a parse failure",
    );

    // Filter out the leading `Created` events (RFC 0017 §3.1 audits leaf
    // creation); this scenario asserts the widening + rejection pair.
    let events: Vec<_> = sink
        .drain()
        .into_iter()
        .filter(|e| {
            !matches!(
                &e.payload,
                AuditPayload::Template {
                    change: TemplateChange::Created { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(events.len(), 2);
    assert!(
        matches!(
            events[0].payload,
            AuditPayload::Template {
                change: TemplateChange::Widened { .. },
                ..
            }
        ),
        "event 0: expected Template/Widened, got {:?}",
        events[0].payload,
    );
    // Rejection audit's `would_be_template` records what the
    // widening *would* have produced, so an operator inspecting
    // the event sees the degenerate shape that was avoided.
    let AuditPayload::Template {
        change: TemplateChange::RejectedDegenerate {
            would_be_template, ..
        },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1: expected Template/RejectedDegenerate, got {:?}",
            events[1].payload,
        );
    };
    assert_eq!(would_be_template, "<*> <*> <*>");
}

/// Scenario RFC0001.3 — Tokenizer is Unicode whitespace only; punctuation stays in tokens.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_3_tokenizer_is_unicode_whitespace_only() {
    use ourios_miner::tokenize::tokenize;

    let r = tokenize("key=value, other=42").expect("nul-free test input");
    assert_eq!(r.tokens, vec!["key=value,", "other=42"]);
    assert_eq!(
        r.separators.len(),
        r.tokens.len() + 1,
        "separators contract: len == tokens.len() + 1",
    );

    // Each listed punctuation must stay inside a single token AND
    // the token must equal the input verbatim — asserting only
    // tokens.len() == 1 would miss a buggy tokenizer that *drops*
    // the punctuation char (PR #8 review, comment 3178317220).
    for line in ["a=b", "a:b", "a,b", "a;b", "a[b", "a]b", "a(b", "a)b"] {
        let r = tokenize(line).expect("nul-free test input");
        assert_eq!(
            r.tokens.len(),
            1,
            "punctuation in {line:?} introduced a token boundary",
        );
        assert_eq!(
            r.tokens[0], line,
            "punctuation char dropped or altered in {line:?}",
        );
    }

    let r = tokenize("hello world").expect("nul-free test input");
    assert_eq!(r.tokens, vec!["hello", "world"]);

    let r = tokenize("hello\u{00A0}world").expect("nul-free test input");
    assert_eq!(
        r.tokens,
        vec!["hello", "world"],
        "U+00A0 (non-breaking space) is Unicode whitespace and must split",
    );
}

/// Regression for RFC0001.3 — the full ASCII whitespace set (every
/// byte `char::is_whitespace` recognises) must split tokens. The
/// scenario only names "Unicode whitespace"; this test locks in the
/// VT (U+000B) and FF (U+000C) cases that the doc comment originally
/// omitted (PR #8 review, comment 3178317199).
#[test]
fn rfc0001_3_regression_vt_and_ff_split_tokens() {
    use ourios_miner::tokenize::tokenize;

    let r = tokenize("hello\u{000B}world").expect("nul-free test input");
    assert_eq!(
        r.tokens,
        vec!["hello", "world"],
        "U+000B (vertical tab) must split",
    );

    let r = tokenize("hello\u{000C}world").expect("nul-free test input");
    assert_eq!(
        r.tokens,
        vec!["hello", "world"],
        "U+000C (form feed) must split",
    );
}

/// Regression for `Tokenized<'a>` — every separator must be a
/// borrowed slice of the input, including the empty trailing
/// separator after a line ending in a token. This locks in the
/// "borrows from the input" guarantee against an implementation
/// that pushes a literal `""` (which is `&'static str`) and would
/// silently violate the lifetime story (PR #8 review, comment
/// 3178317213).
#[test]
fn rfc0001_3_regression_separators_always_borrow_from_input() {
    use ourios_miner::tokenize::tokenize;

    for line in ["", "hello", "  hello  world  ", "hello\nworld\n"] {
        let r = tokenize(line).expect("nul-free test input");
        let bounds = line.as_bytes().as_ptr_range();
        for (idx, sep) in r.separators.iter().enumerate() {
            let sep_ptr = sep.as_ptr();
            assert!(
                sep_ptr >= bounds.start && sep_ptr <= bounds.end,
                "separator[{idx}] = {sep:?} in line {line:?} \
                 does not borrow from the input ({sep_ptr:p} \
                 outside [{:p}, {:p}])",
                bounds.start,
                bounds.end,
            );
        }
    }
}

/// Scenario RFC0001.4 — Confidence ratio = simSeq / threshold; decision boundary at 1.0.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_4_confidence_ratio_decision_boundary_at_one() {
    use ourios_core::confidence::ConfidenceZone;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use ourios_miner::sim_seq::confidence_ratio;

    // The math: confidence = simSeq / threshold.
    assert!(
        (confidence_ratio(0.7, 0.7) - 1.0).abs() < f32::EPSILON,
        "simSeq == threshold → confidence == 1.0",
    );
    assert!(
        (confidence_ratio(0.7, 0.5) - 1.4).abs() < f32::EPSILON,
        "same simSeq under a lower threshold reframes confidence \
         scale-invariantly (sim 0.7 / thresh 0.5 = 1.4)",
    );

    // The decision boundary at confidence == 1.0 maps to the
    // Clean zone (inclusive); confidence < 1.0 (i.e.
    // simSeq < threshold) drops to Lossy or below.
    assert_eq!(
        ConfidenceZone::classify(0.7, 0.7, 0.5),
        ConfidenceZone::Clean,
        "sim == threshold (confidence == 1.0) is on the clean-attach side",
    );
    assert_eq!(
        ConfidenceZone::classify(0.6999, 0.7, 0.5),
        ConfidenceZone::Lossy,
        "sim just below threshold (confidence < 1.0) is lossy",
    );

    // Behavioural check: a line whose sim_seq against the
    // candidate is exactly the threshold takes the clean-attach
    // branch (widening if positions differ; body NOT retained).
    //
    // Construction: two 10-token lines sharing the first 7
    // post-mask tokens. Single-letter tokens dodge every mask
    // rule, so masked == raw and sim_seq = 7/10 = 0.7 exactly.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };
    cluster.ingest(&make("a b c d e f g h i j"));
    cluster.ingest(&make("a b c d e f g x y z"));

    assert_eq!(
        cluster.template_count(&t),
        1,
        "sim == threshold is clean → reuses the existing leaf",
    );
    assert_eq!(
        cluster.merges_total(),
        1,
        "clean attach widens the 3 mismatched positions → one merge event",
    );
    assert_eq!(
        cluster.body_retentions_total(),
        0,
        "clean zone does not retain body (ConfidenceZone::Clean.retains_body() == false)",
    );
    assert_eq!(cluster.parse_failures_total(), 0);
}

// RFC0001.5 and RFC0001.6 are querier query-semantics (what `where
// template_id = X` returns over a written store), not miner behaviour — the
// miner crate cannot run queries. The real AAA tests live in
// `crates/ourios-querier/tests/rfc0001_query_semantics.rs`; the greppable
// `RFC0001.5` / `RFC0001.6` scenario ids resolve there.

/// Scenario RFC0001.7 — Combined widening + type-expansion increments version twice and emits two events in order.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_7_combined_widening_and_type_expansion_emits_two_events_in_order() {
    use ourios_core::audit::{AuditPayload, ParamType, SharedAuditSink, TemplateChange};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — a leaf with one pre-existing wildcard slot whose
    // slot_types = {Str} (seeded via a literal widening at
    // position 4). The third line then triggers a Fixed mismatch
    // at position 3 (literal "hour" → "minute" → widening) AND
    // brings a `<NUM>` token at position 4 (the existing
    // wildcard, whose slot_types[0] = {Str}, doesn't contain
    // Num → type expansion).
    //
    // Per RFC §6.2 the third line's attach must emit
    // `TemplateWidened` first, then `TemplateTypeExpanded`,
    // bumping `template_version` twice.
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // L1: fresh 5-token leaf, v=1. Shared prefix ["user","logged"].
    let _ = cluster.ingest(&make("user logged at hour NOW"));
    // L2: literal widening at position 4 ("NOW" → "LATER").
    //     sim_seq = 4/5 ≥ default threshold 0.7. v=2, slot_types
    //     = [{Str}].
    let _ = cluster.ingest(&make("user logged at hour LATER"));
    // Discard the L2 event so we can assert exactly the L3 trail.
    let _ = sink.drain();

    // Act — L3: Fixed mismatch at position 3 ("hour" → "minute")
    // AND `<NUM>` (from "13") at position 4's pre-existing
    // wildcard. sim_seq = 4/5 → Clean. Combined attach.
    let _ = cluster.ingest(&make("user logged at minute 13"));

    // Assert — exactly two events in widening-then-expansion
    // order, with the version pair carrying the two-bump trail
    // (2 → 3 from widening, 3 → 4 from expansion).
    let events = sink.drain();
    assert_eq!(
        events.len(),
        2,
        "combined attach must emit exactly two audit events",
    );

    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version: w_old,
                new_version: w_new,
                positions_widened,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "event 0 must be Template/Widened (widening fires before expansion), got {:?}",
            events[0].payload,
        );
    };
    assert_eq!((*w_old, *w_new), (2, 3), "widening bumps version 2 → 3");
    assert_eq!(*positions_widened, vec![3]);

    let AuditPayload::Template {
        change:
            TemplateChange::TypeExpanded {
                old_version: e_old,
                new_version: e_new,
                slots_expanded,
                ..
            },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1 must be Template/TypeExpanded, got {:?}",
            events[1].payload,
        );
    };
    assert_eq!(
        (*e_old, *e_new),
        (3, 4),
        "expansion bumps version 3 → 4 in the same attach",
    );
    // The existing wildcard at template position 4 is ordinal 1
    // in the post-widen template (the freshly-widened slot at
    // position 3 takes ordinal 0). Expansion adds Num.
    assert_eq!(slots_expanded.len(), 1);
    assert_eq!(slots_expanded[0].slot_index, 1);
    assert_eq!(slots_expanded[0].added_types, vec![ParamType::Num]);

    // RFC §6.4: both events count toward merges_total via
    // `TemplateChange::counts_as_merge`, so the counter
    // increments by exactly 2 across the combined attach.
    assert_eq!(
        cluster.merges_total(),
        3,
        "merges_total: 1 (L2 widen) + 1 (L3 widen) + 1 (L3 expand)",
    );
}

/// Scenario RFC0001.8 — `ourios.miner.confidence.p50` and
/// `ourios.miner.confidence.p01` are emitted as gauges.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// Ingests a controlled confidence spread for one `(ourios.tenant,
/// ourios.service)`, collects the exported stream, and asserts the
/// `ourios.miner.confidence.p50` / `.p01` gauges are present for that
/// attribute pair with values equal to the nearest-rank quantile of
/// the same samples the `confidence` histogram saw (the in-process
/// reservoir per §6.8).
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0001_8_confidence_p50_and_p01_are_emitted_as_gauges() {
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{AnyValue, Body, KeyValue, OtlpLogRecord, any_value};
    use ourios_core::record::SharedRecordSink;
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Nearest-rank quantile, mirroring `crate::metrics::Reservoir`
    // (rank = ceil(q * n), clamped to [1, n]). The gauge reads the
    // same per-line confidences the record sink captures, so
    // recomputing here is the cross-check RFC0001.8 demands.
    fn nearest_rank(samples: &[f64], q: f64) -> f64 {
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
        sorted[rank.min(sorted.len()) - 1]
    }

    // Arrange — in-memory provider, then the miner with a record
    // sink so the per-line confidences feeding the gauge are
    // observable for an independent quantile cross-check.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let t = TenantId::new("acme");
    let service = "checkout";
    let svc_attr = vec![KeyValue {
        key: "service.name".to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(service.to_string())),
        }),
        ..Default::default()
    }];
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        resource_attributes: svc_attr.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act — a mix of distinct shapes (fresh leaves, confidence
    // sentinel 1.0) and a low-similarity line that lands in a zone
    // with a sub-1.0 confidence, so the p50/p01 spread is
    // non-degenerate. The exact per-line confidences are read back
    // from the record sink rather than assumed.
    cluster.ingest(&make("alpha beta gamma delta epsilon"));
    cluster.ingest(&make("one two three four five"));
    cluster.ingest(&make("red green blue cyan magenta"));
    cluster.ingest(&make("alpha beta gamma rho sigma"));
    cluster.ingest(&make("alpha beta phi rho sigma omega"));
    guard.force_flush().expect("force_flush succeeds");

    let samples: Vec<f64> = sink
        .drain()
        .iter()
        .map(|r| f64::from(r.confidence))
        .collect();
    assert!(!samples.is_empty(), "the miner must emit a record per line");
    let expected_p50 = nearest_rank(&samples, 0.50);
    let expected_p01 = nearest_rank(&samples, 0.01);
    // Guard against a degenerate (all-equal) confidence stream so
    // the quantile cross-check below is genuinely exercised: a
    // p50 == p01 single-value distribution would pass trivially.
    assert!(
        (expected_p50 - expected_p01).abs() > 1e-9,
        "test setup must produce a non-degenerate confidence spread (p50={expected_p50}, p01={expected_p01})",
    );

    // Assert — both gauges present for (ourios.tenant=acme,
    // ourios.service=checkout) with the expected nearest-rank values.
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let gauge_value = |name: &str| -> f64 {
        let data = rms
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("{name} missing from exported stream"))
            .data();
        let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = data else {
            panic!("{name} should be an f64 gauge");
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
            .unwrap_or_else(|| panic!("{name} missing the (acme, checkout) data point"))
            .value()
    };

    assert!(
        (gauge_value(ourios_semconv::OURIOS_MINER_CONFIDENCE_P50) - expected_p50).abs() < 1e-6,
        "ourios.miner.confidence.p50 must match the in-process p50 quantile",
    );
    assert!(
        (gauge_value(ourios_semconv::OURIOS_MINER_CONFIDENCE_P01) - expected_p01).abs() < 1e-6,
        "ourios.miner.confidence.p01 must match the in-process p01 quantile",
    );
}

/// Scenario RFC0001.9 — `body_kind = Structured` short-circuits to a structured-template id.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_9_structured_body_short_circuits_to_structured_template_id() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{
        AnyValue, Body, KeyValue, KeyValueList, OtlpLogRecord, any_value, canonical,
    };
    use ourios_core::record::{BodyKind, SharedRecordSink};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — a non-String `AnyValue`: a KvlistValue with two keys
    // in insertion order `zeta, alpha` so the structured branch
    // exercises a real structured body whose canonical encoding
    // preserves received order. RFC §6.2 step 0 must skip
    // tokenize/mask/descend and route this to a structured-template id
    // keyed on `(severity_number, scope_name, BodyKind::Structured)`.
    let int_av = |n: i64| AnyValue {
        value: Some(any_value::Value::IntValue(n)),
    };
    let kvlist = || AnyValue {
        value: Some(any_value::Value::KvlistValue(KeyValueList {
            values: vec![
                KeyValue {
                    key: "zeta".to_string(),
                    value: Some(int_av(1)),
                    ..Default::default()
                },
                KeyValue {
                    key: "alpha".to_string(),
                    value: Some(int_av(2)),
                    ..Default::default()
                },
            ],
        })),
    };
    let t = TenantId::new("tenant-x");
    let make = |severity: u8, scope: Option<&str>| OtlpLogRecord {
        tenant_id: t.clone(),
        severity_number: severity,
        scope_name: scope.map(str::to_string),
        body: Some(Body::Structured(kvlist())),
        ..Default::default()
    };

    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));

    // Act — two records sharing `(9, "lib.auth")` and one differing
    // only in scope. The structured-template id semantics: same
    // (severity, scope) reuses the id, a different (severity, scope)
    // allocates a fresh one.
    let id_a1 = cluster.ingest(&make(9, Some("lib.auth")));
    let id_a2 = cluster.ingest(&make(9, Some("lib.auth")));
    let id_b = cluster.ingest(&make(9, Some("lib.payments")));

    // Assert — structured-template id semantics.
    assert_eq!(
        id_a1, id_a2,
        "two structured records with the same (severity, scope) must share one structured-template id",
    );
    assert_ne!(
        id_a1, id_b,
        "a structured record with a different (severity, scope) must get a distinct structured-template id",
    );

    // Assert — the emitted record shape per §6.1 / §6.2 step 0.
    let emitted = records.drain();
    assert_eq!(emitted.len(), 3, "one record emitted per ingest");
    let rec = &emitted[0];
    assert_eq!(
        rec.body_kind,
        BodyKind::Structured,
        "non-String AnyValue body must emit BodyKind::Structured",
    );
    assert_eq!(
        rec.template_id, id_a1,
        "the emitted record carries the structured-template id ingest returned",
    );
    let body = rec.body.as_deref().expect("structured body is Some");
    let decoded = canonical::decode_any_value(body.as_bytes()).expect("canonical body decodes");
    assert_eq!(
        decoded,
        kvlist(),
        "the canonical body round-trips to the original AnyValue",
    );
    assert!(
        !rec.lossy_flag,
        "RFC §6.1: lossy_flag is always false on BodyKind::Structured",
    );
    assert!(
        rec.params.is_empty(),
        "RFC §6.1: params is empty on BodyKind::Structured",
    );
    assert!(
        rec.separators.is_empty(),
        "RFC §6.1: separators is empty on BodyKind::Structured",
    );
    assert!(
        (rec.confidence - 1.0).abs() < f32::EPSILON,
        "RFC §6.1: confidence is the 1.0 sentinel on BodyKind::Structured",
    );
}

// Scenario RFC0001.10 (`time_unix_nano` preserved verbatim) is an end-to-end
// ingest → Parquet → query scenario the miner crate cannot run; it lives in
// `ourios-querier/tests/rfc0001_time_preserved.rs` (same relocation as
// RFC0001.5/.6).

/// Scenario RFC0001.11 — `severity_number = 0` and `scope_name = None` are distinct key buckets.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_11_severity_zero_and_scope_none_are_distinct_key_buckets() {
    use std::collections::HashSet;

    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — four records with identical body text, varying only
    // across the full `(severity_number, scope_name)` cross-product
    // of the two edge-case key positions the RFC locks: severity `0`
    // (`UNSPECIFIED`, a valid OTLP value) and scope `None` (absent).
    // Per RFC §6.1, `0` is its own severity bucket and `None` is its
    // own scope bucket — never coalesced with a specified severity
    // or named scope. The four `(severity, scope)` combinations must
    // therefore yield four distinct `template_id`s.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |severity: u8, scope: Option<&str>| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String("user logged in".to_string())),
        severity_number: severity,
        scope_name: scope.map(str::to_string),
        ..Default::default()
    };

    // Act — (0, None), (0, Some), (9, None), (9, Some).
    let id_0_none = cluster.ingest(&make(0, None));
    let id_0_some = cluster.ingest(&make(0, Some("lib.x")));
    let id_9_none = cluster.ingest(&make(9, None));
    let id_9_some = cluster.ingest(&make(9, Some("lib.x")));

    // Assert — four distinct ids (one per key bucket), four
    // templates, no widening coalescing any pair of buckets.
    let ids = HashSet::from([id_0_none, id_0_some, id_9_none, id_9_some]);
    assert_eq!(
        ids.len(),
        4,
        "each (severity, scope) bucket — including (0, None) — must get its own template_id; got {ids:?}",
    );
    assert_eq!(cluster.template_count(&t), 4);
    assert_eq!(
        cluster.merges_total(),
        0,
        "the UNSPECIFIED-severity / None-scope buckets must never coalesce with specified ones",
    );
}
