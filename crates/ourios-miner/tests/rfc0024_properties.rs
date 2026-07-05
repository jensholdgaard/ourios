//! RFC 0024 §5 — the miner-owned pipeline properties: P2 (no silent
//! merge, `.4`) and P3 (RFC 0023 bounds, `.5`) over generated OTLP
//! batches from `ourios-testgen`. See
//! `crates/ourios-bench/tests/rfc0024_calibration.rs` for the scenario
//! placement map.

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord, canonical};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_miner::reconstruct::reconstruct;
use ourios_miner::tree::OwnedToken;
use ourios_testgen::manifest::{AnyValueShapes, BodyKindMix, CalibrationManifest, SeverityBucket};
use ourios_testgen::strategies;
use proptest::prelude::*;

/// A small hand-shaped manifest so the calibrated arm has a real
/// severity / body / attribute mix to draw from (no corpus file
/// dependency in this crate).
fn synthetic_manifest() -> CalibrationManifest {
    CalibrationManifest {
        corpus_tag: "rfc0024-synthetic".to_string(),
        records: 100,
        log_attribute_count: [(0, 30), (1, 40), (3, 30)].into_iter().collect(),
        resource_attribute_count: [(2, 100)].into_iter().collect(),
        body_kind: BodyKindMix {
            string: 85,
            structured: 10,
            absent: 5,
        },
        string_body_len: [(4, 20), (5, 50), (6, 30)].into_iter().collect(),
        severity: vec![
            SeverityBucket {
                number: 9,
                text: Some("INFO".to_string()),
                count: 70,
            },
            SeverityBucket {
                number: 13,
                text: Some("WARN".to_string()),
                count: 20,
            },
            SeverityBucket {
                number: 17,
                text: Some("ERROR".to_string()),
                count: 10,
            },
        ],
        any_value_shapes: AnyValueShapes {
            string: 60,
            int: 20,
            double: 5,
            boolean: 5,
            array: 5,
            kvlist: 5,
            ..Default::default()
        },
        any_value_max_depth: 3,
        distinct_attribute_keys: 12,
    }
}

/// The tenant's `template_id → template` map, for reconstruction.
fn template_map(cluster: &MinerCluster, tenant: &TenantId) -> HashMap<u64, Vec<OwnedToken>> {
    cluster
        .templates_for(tenant)
        .into_iter()
        .map(|leaf| (leaf.template_id, leaf.template))
        .collect()
}

/// The P2 oracle: mine a batch and assert every emitted row renders
/// back to *its own* record's body. A row that silently attached to
/// another line's template cannot reconstruct its original bytes
/// (clean attaches store no body, so a wrong template renders wrong
/// bytes; a lossy/diverted row must carry the original verbatim) —
/// so byte-fidelity here *is* the §3.1 no-silent-merge invariant
/// under arbitrary input.
fn assert_no_silent_merge(batch: &[OtlpLogRecord]) -> Result<(), TestCaseError> {
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    for record in batch {
        cluster.ingest(record);
    }
    let emitted = sink.drain();
    prop_assert_eq!(emitted.len(), batch.len(), "one emitted row per record");

    let tenant = TenantId::new(strategies::TESTGEN_TENANT);
    let templates = template_map(&cluster, &tenant);
    let no_template: Vec<OwnedToken> = Vec::new();

    for (i, (original, mined)) in batch.iter().zip(&emitted).enumerate() {
        let template = templates.get(&mined.template_id).unwrap_or(&no_template);
        let rebuilt = reconstruct(mined, template);
        match &original.body {
            Some(Body::String(s)) => {
                prop_assert_eq!(
                    rebuilt.as_slice(),
                    s.as_bytes(),
                    "record {}: reconstruction must yield the record's own line",
                    i
                );
                if mined.template_id == NO_TEMPLATE {
                    prop_assert!(mined.lossy_flag, "record {}: diverted rows are lossy", i);
                    prop_assert_eq!(
                        mined.body.as_deref(),
                        Some(s.as_str()),
                        "record {}: diverted rows retain the body verbatim",
                        i
                    );
                }
            }
            Some(Body::Structured(av)) => {
                let back = canonical::decode_any_value(&rebuilt).map_err(|e| {
                    TestCaseError::fail(format!("record {i}: structured body decode: {e}"))
                })?;
                prop_assert_eq!(
                    &back,
                    av,
                    "record {}: structured body must round-trip canonical-JSON equal",
                    i
                );
            }
            None => {
                prop_assert!(
                    rebuilt.is_empty(),
                    "record {}: an absent body reconstructs to nothing",
                    i
                );
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: ourios_testgen::proptest_cases(24), ..ProptestConfig::default() })]

    /// Scenario RFC0024.4 — P2: no silent merge over generated
    /// batches, both modes.
    /// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
    #[test]
    fn rfc0024_4_no_silent_merge_over_generated_batches(
        adversarial in proptest::collection::vec(strategies::adversarial(), 1..12),
        calibrated in proptest::collection::vec(
            strategies::calibrated(&synthetic_manifest()), 1..12),
    ) {
        assert_no_silent_merge(&adversarial)?;
        assert_no_silent_merge(&calibrated)?;
    }

    /// Scenario RFC0024.5 — P3: RFC 0023 bounds hold over generated
    /// streams with deliberately tiny configured bounds, checked
    /// after *every* ingest (mid-stream, not just at the end). The
    /// third bound, per-node fan-out, has no cluster-level
    /// observation surface — it is pinned at the tree level by the
    /// fan-out property in `ourios-miner::tree` and behaviorally by
    /// RFC0023.3; this stream still runs under the tiny fan-out cap
    /// so the wildcard routing path is exercised under arbitrary
    /// input.
    /// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
    #[test]
    fn rfc0024_5_bounds_hold_over_generated_streams(
        batch in proptest::collection::vec(strategies::adversarial(), 1..24),
    ) {
        const CEILING: u32 = 3;
        const TOKEN_CAP: u16 = 8;
        let config = MinerConfig::default()
            .with_max_templates(CEILING).expect("non-zero ceiling")
            .with_max_node_children(2).expect("non-zero cap")
            .with_max_line_tokens(TOKEN_CAP).expect("non-zero cap");
        let sink = SharedRecordSink::new();
        let mut cluster = MinerCluster::new(config).with_record_sink(Box::new(sink.clone()));
        let tenant = TenantId::new(strategies::TESTGEN_TENANT);

        for (i, record) in batch.iter().enumerate() {
            let id = cluster.ingest(record);
            // Deliberately `templates_for` (leaves), not
            // `template_count` (leaves + structured entries):
            // RFC 0023 §3.1 bound 2 is a ceiling on Drain-tree
            // *leaves* — `at_template_ceiling` gates on `leaf_count`,
            // which only `create_new_leaf` increments; structured
            // templates are outside the bound by design.
            prop_assert!(
                cluster.templates_for(&tenant).len() <= CEILING as usize,
                "record {}: leaf count exceeded the ceiling mid-stream",
                i
            );
            if let Some(Body::String(s)) = &record.body
                && s.split_whitespace().count() > usize::from(TOKEN_CAP)
            {
                prop_assert_eq!(
                    id,
                    NO_TEMPLATE,
                    "record {}: an over-long line must divert, never mint or attach",
                    i
                );
            }
        }

        // Every diverted string row kept its body (§6.3) — the
        // ceiling / caps must never cost data.
        for (original, mined) in batch.iter().zip(sink.drain()) {
            if let Some(Body::String(s)) = &original.body
                && mined.template_id == NO_TEMPLATE
            {
                prop_assert!(mined.lossy_flag);
                prop_assert_eq!(mined.body.as_deref(), Some(s.as_str()));
            }
        }
    }
}
