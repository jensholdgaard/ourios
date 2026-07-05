//! RFC 0024 §5 — calibration extraction + calibrated-generator sanity
//! (`.1`/`.2`), plus the adversarial umbrella (`.7`). The per-property
//! scenarios live with the crates that own each invariant:
//! `.3` (P1 round-trip) in `ourios-parquet`, `.4`/`.5` (P2/P3) in
//! `ourios-miner`, `.6` (P4 query oracle) in `ourios-querier`.
//!
//! `.7` stays an `#[ignore]`d stub until the oracle green slice (the
//! umbrella runs last).

use std::path::{Path, PathBuf};

use ourios_bench::{TxtSeverity, extract_manifest};
use ourios_testgen::manifest::{CalibrationAccumulator, ExactHistogram, Log2Histogram};
use ourios_testgen::strategies;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;

/// The OTLP/JSON fixture the extraction scenarios measure (4 records:
/// 2× INFO, 1× WARN, 1× ERROR; 3 string bodies + 1 kvlist body).
fn otlp_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/otlp")
}

/// The repo root, resolved from the crate dir (same pattern as
/// `tests/a1.rs`) so the test is independent of the invocation cwd.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Scenario RFC0024.1 — calibration extraction.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
fn rfc0024_1_calibration_extraction_is_deterministic() {
    let first =
        extract_manifest(&otlp_fixture(), "rfc0024-fixture", TxtSeverity::Fixed).expect("extract");
    let second = extract_manifest(&otlp_fixture(), "rfc0024-fixture", TxtSeverity::Fixed)
        .expect("re-extract");
    assert_eq!(
        first.to_json_bytes().expect("serialize"),
        second.to_json_bytes().expect("serialize"),
        "rerunning --calibrate over the same corpus must be byte-identical",
    );
    assert_eq!(first.records, 4, "the fixture carries four records");

    // The committed-alongside arm: the manifest checked in for the
    // in-repo seed corpus must match regeneration exactly.
    let committed = std::fs::read(repo_root().join("testdata/calibration/seed.json"))
        .expect("committed seed manifest");
    let regenerated = extract_manifest(
        &repo_root().join("testdata/corpus"),
        "seed",
        TxtSeverity::Fixed,
    )
    .expect("regenerate seed manifest");
    assert_eq!(
        regenerated.to_json_bytes().expect("serialize"),
        committed,
        "testdata/calibration/seed.json drifted from its corpus — regenerate via \
         `cargo run -p ourios-bench -- --calibrate --corpus testdata/corpus --corpus-tag seed`",
    );
}

/// Mean of an exact histogram (`value → record count`).
fn exact_mean(histogram: &ExactHistogram) -> f64 {
    weighted_mean(histogram.iter().map(|(&v, &n)| (f64::from(v), n)))
}

/// Mean bucket index of a log2 histogram.
fn log2_mean(histogram: &Log2Histogram) -> f64 {
    weighted_mean(histogram.iter().map(|(&b, &n)| (f64::from(b), n)))
}

// Counts here are fixture / generation sizes (≪ 2^52), so the
// u64 → f64 conversion is exact.
#[allow(clippy::cast_precision_loss)]
fn weighted_mean(pairs: impl Iterator<Item = (f64, u64)>) -> f64 {
    let (mut sum, mut n) = (0.0_f64, 0.0_f64);
    for (value, count) in pairs {
        sum += value * count as f64;
        n += count as f64;
    }
    if n == 0.0 { 0.0 } else { sum / n }
}

// Same rationale as `weighted_mean`: counts are fixture / generation
// sizes (≪ 2^52), so the u64 → f64 conversion is exact.
#[allow(clippy::cast_precision_loss)]
fn share(count: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        count as f64 / total as f64
    }
}

/// Documented tolerance: means within ±(0.5 + 15 % of the
/// manifest's), shares within ±0.10 absolute — gross moments per
/// RFC 0024 §3.2 ("statistical, not exact"), wide enough for
/// N = 2000 sampling noise, tight enough to catch a mis-wired
/// distribution.
fn assert_mean_close(manifest: f64, generated: f64, what: &str) {
    let tolerance = 0.5 + 0.15 * manifest.abs();
    assert!(
        (manifest - generated).abs() <= tolerance,
        "{what}: manifest mean {manifest:.3} vs generated {generated:.3} (tolerance {tolerance:.3})",
    );
}

fn assert_share_close(
    manifest_count: u64,
    manifest_total: u64,
    generated_count: u64,
    generated_total: u64,
    what: &str,
) {
    let m = share(manifest_count, manifest_total);
    let g = share(generated_count, generated_total);
    assert!(
        (m - g).abs() <= 0.10,
        "{what}: manifest share {m:.3} vs generated {g:.3} (tolerance 0.10)",
    );
}

/// Scenario RFC0024.2 — calibrated generators are shaped by the
/// manifest. See `docs/rfcs/0024-otlp-envelope-property-testing.md`
/// §5 and `assert_mean_close` for the documented tolerances.
#[test]
fn rfc0024_2_calibrated_generators_match_manifest_moments() {
    const DRAWS: u64 = 2000;

    let manifest =
        extract_manifest(&otlp_fixture(), "rfc0024-fixture", TxtSeverity::Fixed).expect("extract");

    let strategy = strategies::calibrated(&manifest);
    // A fixed-seed runner: the scenario pins generator *shape*, not
    // proptest's exploration; a flaky sampling tail would be noise.
    let mut runner = TestRunner::deterministic();
    let mut accumulator = CalibrationAccumulator::new();
    for _ in 0..DRAWS {
        let tree = strategy.new_tree(&mut runner).expect("generate");
        accumulator.observe(&tree.current());
    }
    let generated = accumulator.finish("generated");
    assert_eq!(generated.records, DRAWS);

    assert_mean_close(
        exact_mean(&manifest.log_attribute_count),
        exact_mean(&generated.log_attribute_count),
        "mean log-attribute count",
    );
    assert_mean_close(
        exact_mean(&manifest.resource_attribute_count),
        exact_mean(&generated.resource_attribute_count),
        "mean resource-attribute count",
    );
    assert_mean_close(
        log2_mean(&manifest.string_body_len),
        log2_mean(&generated.string_body_len),
        "mean string-body length bucket",
    );

    for (what, manifest_count, generated_count) in [
        (
            "string bodies",
            manifest.body_kind.string,
            generated.body_kind.string,
        ),
        (
            "structured bodies",
            manifest.body_kind.structured,
            generated.body_kind.structured,
        ),
        (
            "absent bodies",
            manifest.body_kind.absent,
            generated.body_kind.absent,
        ),
    ] {
        assert_share_close(
            manifest_count,
            manifest.records,
            generated_count,
            generated.records,
            what,
        );
    }

    for bucket in &manifest.severity {
        let generated_count = generated
            .severity
            .iter()
            .find(|g| g.number == bucket.number && g.text == bucket.text)
            .map_or(0, |g| g.count);
        assert_share_close(
            bucket.count,
            manifest.records,
            generated_count,
            generated.records,
            &format!("severity ({}, {:?})", bucket.number, bucket.text),
        );
    }
    for bucket in &generated.severity {
        assert!(
            manifest
                .severity
                .iter()
                .any(|m| m.number == bucket.number && m.text == bucket.text),
            "generated severity ({}, {:?}) is outside the manifest's support",
            bucket.number,
            bucket.text,
        );
    }
}

// ---- RFC0024.7 — the adversarial umbrella ---------------------------
//
// One end-to-end harness per case: mine (P2 fidelity) → write / read
// (P1 equality) → query vs the naive oracle (P4) → a second stream
// under tiny RFC 0023 bounds (P3). The per-property suites in the
// owning crates carry the fine-grained assertions; the umbrella's job
// is the *composition* at an elevated case count, adversarial mode
// only — any failure here shrinks to a minimal reproducer by
// construction.

mod umbrella {
    use std::collections::HashMap;

    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord, canonical};
    use ourios_core::record::{BodyKind, MinedRecord, SharedRecordSink};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
    use ourios_miner::reconstruct::reconstruct;
    use ourios_miner::tree::OwnedToken;
    use ourios_parquet::{PartitionKey, Reader, Writer};
    use ourios_testgen::strategies;
    use proptest::prelude::*;
    use tempfile::TempDir;

    const NOW: u64 = strategies::TIME_BASE_UNIX_NANO + 24 * 3_600_000_000_000;
    const WINDOW_NS: u64 = 30 * 24 * 3_600_000_000_000;

    fn fail(what: &str, e: impl std::fmt::Display) -> TestCaseError {
        TestCaseError::fail(format!("{what}: {e}"))
    }

    fn effective(r: &MinedRecord) -> u64 {
        if r.time_unix_nano != 0 {
            r.time_unix_nano
        } else {
            r.observed_time_unix_nano.unwrap_or(0)
        }
    }

    fn writable(r: &MinedRecord) -> bool {
        r.body_kind != BodyKind::Absent
            && i64::try_from(r.time_unix_nano).is_ok()
            && r.observed_time_unix_nano
                .is_none_or(|t| i64::try_from(t).is_ok())
    }

    /// P2: every emitted row renders back to its own record's body.
    fn assert_fidelity(
        batch: &[OtlpLogRecord],
        emitted: &[MinedRecord],
        templates: &HashMap<u64, Vec<OwnedToken>>,
    ) -> Result<(), TestCaseError> {
        let no_template: Vec<OwnedToken> = Vec::new();
        for (i, (original, mined)) in batch.iter().zip(emitted).enumerate() {
            let template = templates.get(&mined.template_id).unwrap_or(&no_template);
            let rebuilt = reconstruct(mined, template);
            match &original.body {
                Some(Body::String(s)) => {
                    prop_assert_eq!(rebuilt.as_slice(), s.as_bytes(), "record {} body", i);
                }
                Some(Body::Structured(av)) => {
                    let back = canonical::decode_any_value(&rebuilt)
                        .map_err(|e| fail(&format!("record {i} structured decode"), e))?;
                    prop_assert_eq!(&back, av, "record {} structured body", i);
                }
                None => prop_assert!(rebuilt.is_empty(), "record {} absent body", i),
            }
        }
        Ok(())
    }

    /// P1: writable rows round-trip storage struct-equal. Returns the
    /// bucket (kept alive) and the stored rows.
    fn assert_storage_round_trip(rows: &[MinedRecord]) -> Result<TempDir, TestCaseError> {
        let bucket = TempDir::new().map_err(|e| fail("temp dir", e))?;
        let mut groups: Vec<(PartitionKey, Vec<MinedRecord>)> = Vec::new();
        for r in rows {
            let key = PartitionKey::derive(r).map_err(|e| fail("derive", e))?;
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push(r.clone()),
                None => groups.push((key, vec![r.clone()])),
            }
        }
        for (key, originals) in groups {
            let mut w =
                Writer::open(bucket.path(), key.clone()).map_err(|e| fail("open writer", e))?;
            w.append_records(&originals)
                .map_err(|e| fail("append", e))?;
            let written = w.close().map_err(|e| fail("close", e))?;
            let back = Reader::open_partition(&written.path, key)
                .map_err(|e| fail("open_partition", e))?
                .read_all()
                .map_err(|e| fail("read_all", e))?;
            prop_assert_eq!(&back, &originals, "storage round-trip");
        }
        Ok(bucket)
    }

    /// P4-composition: three fixed-shape queries vs the naive oracle.
    fn assert_query_oracle(bucket: &TempDir, rows: &[MinedRecord]) -> Result<(), TestCaseError> {
        let querier = ourios_querier::Querier::new(bucket.path());
        let tenant = TenantId::new(strategies::TESTGEN_TENANT);
        let aliases = ourios_core::alias::AliasMap::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| fail("runtime", e))?;

        let mut probes = vec![
            (
                "severity >= 17".to_string(),
                Box::new(|r: &MinedRecord| r.severity_number >= 17)
                    as Box<dyn Fn(&MinedRecord) -> bool>,
            ),
            (
                "severity >= 0".to_string(),
                Box::new(|_: &MinedRecord| true),
            ),
        ];
        if let Some(r) = rows.iter().find(|r| r.template_id != NO_TEMPLATE) {
            let id = r.template_id;
            probes.push((
                format!("template_id == {id}"),
                Box::new(move |r: &MinedRecord| r.template_id == id),
            ));
        }
        for (dsl, pred) in probes {
            let expected = rows
                .iter()
                .filter(|r| {
                    let t = effective(r);
                    (NOW - WINDOW_NS..NOW).contains(&t) && pred(r)
                })
                .count() as u64;
            let query =
                ourios_querier::dsl::parse(&dsl).map_err(|e| fail(&format!("parse `{dsl}`"), e))?;
            let got = runtime
                .block_on(querier.run_query(&query, &tenant, NOW, WINDOW_NS, Some(&aliases)))
                .map_err(|e| fail(&format!("run `{dsl}`"), e))?
                .rows;
            prop_assert_eq!(got, expected, "oracle disagreement on `{}`", dsl);
        }
        Ok(())
    }

    /// P3: a second stream under tiny bounds — ceiling holds
    /// mid-stream, over-long lines divert.
    fn assert_bounds(batch: &[OtlpLogRecord]) -> Result<(), TestCaseError> {
        let config = MinerConfig::default()
            .with_max_templates(3)
            .expect("non-zero")
            .with_max_node_children(2)
            .expect("non-zero")
            .with_max_line_tokens(8)
            .expect("non-zero");
        let mut cluster = MinerCluster::new(config);
        let tenant = TenantId::new(strategies::TESTGEN_TENANT);
        for (i, record) in batch.iter().enumerate() {
            let id = cluster.ingest(record);
            prop_assert!(
                cluster.templates_for(&tenant).len() <= 3,
                "record {}: ceiling breached",
                i
            );
            if let Some(Body::String(s)) = &record.body
                && s.split_whitespace().count() > 8
            {
                prop_assert_eq!(id, NO_TEMPLATE, "record {}: over-long line attached", i);
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: ourios_testgen::proptest_cases(48), ..ProptestConfig::default() })]

        /// Scenario RFC0024.7 — adversarial mode finds nothing today.
        /// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
        #[test]
        fn rfc0024_7_adversarial_mode_passes_the_full_property_set(
            batch in proptest::collection::vec(strategies::adversarial(), 1..16),
        ) {
            let sink = SharedRecordSink::new();
            let mut cluster = MinerCluster::new(MinerConfig::default())
                .with_record_sink(Box::new(sink.clone()));
            for record in &batch {
                cluster.ingest(record);
            }
            let emitted = sink.drain();
            prop_assert_eq!(emitted.len(), batch.len());

            let tenant = TenantId::new(strategies::TESTGEN_TENANT);
            let templates: HashMap<u64, Vec<OwnedToken>> = cluster
                .templates_for(&tenant)
                .into_iter()
                .map(|leaf| (leaf.template_id, leaf.template))
                .collect();

            assert_fidelity(&batch, &emitted, &templates)?;
            let stored: Vec<MinedRecord> =
                emitted.into_iter().filter(writable).collect();
            let bucket = assert_storage_round_trip(&stored)?;
            assert_query_oracle(&bucket, &stored)?;
            assert_bounds(&batch)?;
        }
    }
}
