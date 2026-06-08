//! RFC 0001 §5.2 — Invariants. Acceptance criteria stubs for
//! CLAUDE.md §3.1, §3.2, §3.3, §3.5, §3.7. Each `#[test]` carries
//! the scenario id in its doc comment so `grep -R "§3.1.1" .`
//! resolves bidirectionally between the RFC and the tests
//! (`docs/verification.md` §2.3).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! invocation skips them (outer loop / CI stays green). The Red
//! signal lives at the inner loop: an implementor working on a
//! stub runs `cargo test <name> -- --ignored` and watches the
//! `todo!()` panic. See `docs/verification.md` §3.

/// Scenario §3.1.1 — Default similarity threshold is 0.7.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_1_1_default_threshold_is_0_7() {
    use ourios_core::config::MinerConfig;

    // Arrange — no override; tenant config is left at defaults.

    // Act
    let cfg = MinerConfig::default();

    // Assert
    assert!(
        (cfg.similarity_threshold - 0.7_f32).abs() < f32::EPSILON,
        "default similarity_threshold must be 0.7, got {}",
        cfg.similarity_threshold,
    );
}

/// Scenario §3.1.2 — Mandatory metric set is exposed.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// Installs an SDK in-memory meter provider, constructs a miner
/// (which registers the §6.8 instrument set on the `ourios.miner`
/// meter and init-seeds it), collects the exported stream at zero
/// traffic, and asserts every mandatory §6.8 metric name is present
/// via its init-seeded data point.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn invariant_3_1_2_mandatory_metric_set_is_exposed() {
    use opentelemetry_sdk::metrics::data::{ResourceMetrics, ScopeMetrics};

    use ourios_core::config::MinerConfig;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — in-memory provider, then the miner (so its
    // instruments resolve against the global meter the provider
    // installs). No ingest: §3.1.2 is the zero-traffic guarantee.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let _cluster = MinerCluster::new(MinerConfig::default());

    // Act — collect without any traffic.
    guard.force_flush().expect("force_flush succeeds");

    // Assert — every mandatory §6.8 metric name is in the stream.
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let names: Vec<String> = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .map(|m| m.name().to_string())
        .collect();

    for expected in [
        ourios_semconv::OURIOS_MINER_TEMPLATE_COUNT,
        ourios_semconv::OURIOS_MINER_MERGES,
        ourios_semconv::OURIOS_MINER_CONFIDENCE,
        ourios_semconv::OURIOS_MINER_CONFIDENCE_P50,
        ourios_semconv::OURIOS_MINER_CONFIDENCE_P01,
        ourios_semconv::OURIOS_MINER_BODY_RETENTION_UTILIZATION,
        ourios_semconv::OURIOS_MINER_PARSE_FAILURES,
        ourios_semconv::OURIOS_MINER_PARAMS_OVERFLOW,
        ourios_semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION,
        ourios_semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES,
        ourios_semconv::OURIOS_MINER_DURATION,
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "exported stream missing mandatory §6.8 metric `{expected}`, got {names:?}",
        );
    }
}

/// Scenario §3.2.1 — Default per-parameter byte limit is 256.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_2_1_default_param_byte_limit_is_256() {
    use ourios_core::config::MinerConfig;

    // Arrange — no override; tenant config is left at defaults.

    // Act
    let cfg = MinerConfig::default();

    // Assert
    assert_eq!(cfg.param_byte_limit, 256);
}

/// Default `similarity_floor` is `0.4` per RFC 0001 §6.3
/// *Defaults*. Sibling of `invariant_3_1_1_default_threshold_is_0_7`
/// — together they pin the three-zone boundaries production runs
/// with. The two-arg shorthand `MinerConfig::try_new(0.7, 256)`
/// produces the same triple as `MinerConfig::default()`, so the
/// invariant is asserted on both shapes.
///
/// (Not a §5 scenario id today — the floor is a tuning knob, not
/// a §3 invariant — but locked in as a regression pin so a future
/// config refactor cannot silently slide the boundary.)
#[test]
fn invariant_default_similarity_floor_is_0_4() {
    use ourios_core::config::MinerConfig;

    let from_default = MinerConfig::default();
    assert!(
        (from_default.similarity_floor - 0.4_f32).abs() < f32::EPSILON,
        "Default::default() similarity_floor must be 0.4, got {}",
        from_default.similarity_floor,
    );

    let from_try_new = MinerConfig::try_new(0.7, 256).expect("project defaults are valid");
    assert!(
        (from_try_new.similarity_floor - 0.4_f32).abs() < f32::EPSILON,
        "try_new(0.7, 256) must produce the same floor as Default::default(), got {}",
        from_try_new.similarity_floor,
    );
}

/// Scenario §3.2.2 — Configured limit above 1 KiB is rejected at startup.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_2_2_param_limit_above_1_kib_rejected_at_startup() {
    use ourios_core::config::{MinerConfig, MinerConfigError};

    // Arrange — explicit attempt to set the limit one byte above
    // the §3.2 ceiling (1024 B).
    let over_ceiling: u32 = 1025;

    // Act
    let r = MinerConfig::try_new(0.7, over_ceiling);

    // Assert — variant equality pins the failure mode and the
    // offending value.
    assert_eq!(
        r,
        Err(MinerConfigError::ParamByteLimitTooLarge(over_ceiling)),
    );

    // And the rendered message pins §3.2.2's "with an error
    // citing the §3.2 ceiling" clause (PR #11 review): a
    // regression that drops the citation from the Display impl
    // would still pass the variant assertion above but fail
    // these. Two separate asserts so each pin gets its own
    // diagnostic on failure.
    let rendered = r.unwrap_err().to_string();
    assert!(
        rendered.contains("§3.2"),
        "error must cite the §3.2 ceiling, got: {rendered}",
    );
    assert!(
        rendered.contains("1024"),
        "error must cite the 1024-byte limit, got: {rendered}",
    );

    // §3.2.2's "the process refuses to start serving that tenant"
    // is the consequence of try_new returning Err; refusal is the
    // call site's responsibility (future ingester PR), not a
    // property of MinerConfig itself.
}

/// Scenario §3.3.1 — Separators array captured on every successful tokenization.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_3_1_separators_captured_on_every_tokenization() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::record::{BodyKind, SharedRecordSink};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — a mixed batch covering the §6.6 "capture, always"
    // contract on every `BodyKind::String` exit:
    //   L1 = "user 42 logged in"           — clean fresh leaf
    //   L2 = "user 17 logged in"           — clean reuse (same shape)
    //   L3 = "user 42 logged out from 10.0.0.1" — new bucket (length 6)
    //   L4 = "user 42 logged out from 10.0.0.2" — clean reuse
    //   L5 = ""                            — empty / parse-failure path
    //   L6 = "  \t  "                      — whitespace-only / parse-failure
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    let lines = [
        "user 42 logged in",
        "user 17 logged in",
        "user 42 logged out from 10.0.0.1",
        "user 42 logged out from 10.0.0.2",
        "",
        "  \t  ",
    ];
    let mut expected_token_counts = Vec::new();
    for line in lines {
        // ourios_miner::tokenize is the source of truth for how
        // many tokens a line should yield; we capture the count
        // here so the assertion below can compare against it
        // independently of the cluster's own bookkeeping.
        let r = ourios_miner::tokenize::tokenize(line).expect("nul-free test input");
        expected_token_counts.push(r.tokens.len());
        cluster.ingest(&make(line));
    }

    let records = sink.drain();
    assert_eq!(
        records.len(),
        lines.len(),
        "one record per ingested line on every `BodyKind::String` path",
    );

    // Act + Assert — for every emitted `BodyKind::String` record,
    // the §6.6 capture invariant holds: `separators.len()
    // == tokens.len() + 1`. Empty-input rows still satisfy this
    // (tokens.len() = 0 → separators.len() = 1).
    for (idx, (rec, expected_tokens)) in
        records.iter().zip(expected_token_counts.iter()).enumerate()
    {
        assert_eq!(
            rec.body_kind,
            BodyKind::String,
            "record {idx} should carry BodyKind::String",
        );
        assert_eq!(
            rec.separators.len(),
            expected_tokens + 1,
            "record {idx}: separators.len() must equal tokens.len() + 1 \
             (got separators={}, expected tokens+1={})",
            rec.separators.len(),
            expected_tokens + 1,
        );
    }
}

/// Scenario §3.5.1 — Snapshot format carries a leading version byte.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_5_1_snapshot_format_carries_leading_version_byte() {
    todo!("RFC 0001 §6.9");
}

/// Scenario §3.5.2 — Unknown snapshot version triggers full WAL replay.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_5_2_unknown_snapshot_version_triggers_wal_replay() {
    todo!("RFC 0001 §6.9");
}

/// Scenario §3.7.1 — Tenants' template trees never cross-pollinate.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_7_1_tenant_trees_never_cross_pollinate() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — two tenants emitting *different* template
    // shapes so the cross-pollination question is testable. A's
    // lines exercise the "user <NUM> logged in" shape; B's
    // lines exercise the "GET <PATH> <NUM>" shape (the path
    // differs between B's two lines so each B line lands in its
    // own template — sim 2/3 < 0.7 keeps them distinct rather
    // than widening — but neither matches anything in A's set).
    // Both tenants use default severity (UNSPECIFIED) and scope
    // (None), so the §6.1 template-key tuple's discriminator is
    // constant — what varies (and what the cross-pollination
    // question pivots on) is only the masked tokens.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let a = TenantId::new("tenant-a");
    let b = TenantId::new("tenant-b");
    let a_lines = ["user 42 logged in", "user 17 logged in"];
    let b_lines = ["GET /home 200", "GET /api 200"];

    let make = |tenant: &TenantId, text: &str| OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act — interleave the two streams to make any tree
    // sharing observable: a single shared store would
    // accumulate all four shapes regardless of which tenant
    // emitted which line.
    for (la, lb) in a_lines.iter().zip(b_lines.iter()) {
        cluster.ingest(&make(&a, la));
        cluster.ingest(&make(&b, lb));
    }

    // Assert — A's templates contain only A-shaped tokens
    // (`user`, `<NUM>`, `logged`, `in`); B's contain only
    // B-shaped tokens (`GET`, the literal paths, `<NUM>`).
    // Cross-pollination would mean either set contained tokens
    // that originated in the other tenant's input.
    let a_templates = cluster.templates_for(&a);
    let b_templates = cluster.templates_for(&b);

    // Extract the literal-token side of each template; the
    // cross-pollination question is about Fixed tokens (Wildcard
    // positions have no token string to compare). `templates_for`
    // returns `Vec<LeafSnapshot>` post-widening — the wildcard
    // distinction stays typed end-to-end.
    let literal_tokens = |templates: &[ourios_miner::cluster::LeafSnapshot]| {
        templates
            .iter()
            .flat_map(|snap| {
                snap.template.iter().filter_map(|tok| match tok {
                    ourios_miner::tree::OwnedToken::Fixed(s) => Some(s.clone()),
                    ourios_miner::tree::OwnedToken::Wildcard => None,
                })
            })
            .collect::<std::collections::HashSet<String>>()
    };
    let a_token_set = literal_tokens(&a_templates);
    let b_token_set = literal_tokens(&b_templates);

    assert!(
        a_token_set.contains("user") && a_token_set.contains("logged"),
        "A's tree must hold the A-shape tokens, got {a_token_set:?}",
    );
    assert!(
        b_token_set.contains("GET"),
        "B's tree must hold the B-shape tokens, got {b_token_set:?}",
    );
    assert!(
        !a_token_set.contains("GET"),
        "A's tree must NOT contain B-shape tokens (cross-pollination), got {a_token_set:?}",
    );
    assert!(
        !b_token_set.contains("user") && !b_token_set.contains("logged"),
        "B's tree must NOT contain A-shape tokens (cross-pollination), got {b_token_set:?}",
    );
}

/// Scenario §3.7.2 — Same structural template in two tenants gets distinct `template_id`s.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_7_2_same_template_two_tenants_distinct_template_ids() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — two tenants emit the structurally identical
    // line. After masking they produce the same token sequence
    // (`user <NUM> logged in from <IP>`). Default severity
    // (UNSPECIFIED) and scope (None) for both — the question is
    // about cross-tenant id distinctness, not about the
    // severity/scope discriminator.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let a = TenantId::new("tenant-a");
    let b = TenantId::new("tenant-b");
    let line = "user 42 logged in from 10.0.0.1";
    let record_for = |tenant: &TenantId| OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(line.to_string())),
        ..Default::default()
    };

    // Act — same line, different tenants.
    let id_a = cluster.ingest(&record_for(&a));
    let id_b = cluster.ingest(&record_for(&b));

    // Assert — RFC 0001 §6.1's `template_id` allocator is
    // cluster-wide unique (the id space is shared across tenants
    // so the same `u64` value never refers to two different
    // leaves), so even when two tenants ingest the same masked
    // shape the second call pulls the *next* monotonic id rather
    // than reusing the first tenant's id.
    assert_ne!(
        id_a, id_b,
        "structurally identical templates must get distinct template_ids across tenants",
    );
}

/// Scenario §3.7.3 — Tenant derivation runs per `ResourceLogs`, not per export batch.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// This is the **miner-side** stub: it asserts that when records
/// from two distinct tenants arrive in the same ingest sequence,
/// each record lands in its derived tenant's tree. The
/// **receiver-side** stub — that the wire-decode layer actually
/// derives `tenant_id` per `ResourceLogs.resource` rather than
/// per `ExportLogsServiceRequest` — lives with RFC 0003 (see
/// RFC 0003 §6.3) once the receiver crate exists.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_7_3_tenant_derivation_runs_per_resource_logs() {
    todo!("RFC 0001 §6.1 (Tenant derivation)");
}
