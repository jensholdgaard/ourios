//! RFC 0031 — comparative evaluation vs Grafana Loki (§5 scenarios).
//!
//! One scenario per §5 acceptance criterion (RFC0031.1–.11). `.1`
//! (result-set equivalence) is **live**: a real Loki container run,
//! `#[ignore]`d because it needs Docker — the `loki-interop` CI job
//! executes it via `--ignored --exact` (the dex-oidc precedent).
//! `.2`/`.4`/`.7`/`.11` are green under the §7 partial freeze
//! (2026-07-13): locally-provable scenarios over the frozen gate math
//! and the published §9.13 record, with the dispatch run asserting the
//! same frozen gates on live measurements. The remaining six are
//! `#[ignore]`d red stubs, each discharged by its named green slice.
//!
//! Placement note: the comparative harness lives in `ourios-bench`
//! for now (extending the RFC 0006 harness) rather than a new crate,
//! keeping the §7 "new crate vs `bench/` harness" question open — a
//! new crate is a `CLAUDE.md` §7 architectural commitment and is not
//! made here.
//!
//! The primary gate metric throughout is **bytes read from object
//! storage** (RFC 0031 §2.5 / §3.6): the implementation-independent
//! expression of the pruning thesis. Latency is corroborating, not
//! sole-gating. See `docs/rfcs/0031-comparative-evaluation-loki.md`.

mod harness;
mod interop;
mod loki;
mod picker_tests;
mod pickers;

pub(crate) use harness::*;
pub(crate) use loki::*;
pub(crate) use pickers::*;

pub(crate) use std::collections::HashMap;

pub(crate) use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) use ourios_bench::{
    AggKey, FIXTURE_SERVICE, FIXTURE_SERVICE_B, FIXTURE_TRACE, FixtureRecord, LineKey,
    LokiFetchedBytes, comparative_fixture, compare_aggregations,
    compare_aggregations_within_margin, compare_lines, fixture_jsonl, fixture_logs_data,
    ourios_aggregate_answer, ourios_query_lines, parse_loki_bytes_processed,
    parse_loki_fetched_bytes, parse_loki_matrix, parse_loki_streams,
};

pub(crate) use ourios_core::tenant::TenantId;

pub(crate) use ourios_miner::tree::OwnedToken;

/// Scenario RFC0031.2 — L1 selective template lookup wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 partial freeze (2026-07-13): `M_L1 = 10` on the
/// storage-side bytes channel. Locally provable without a container —
/// the frozen gate math decides exactly at the margin boundary in the
/// must-win direction (above the ratio flips `pass = false`, the
/// pillar-level-finding arm), and the §9.13-recorded L1 measurements
/// (documented historical evidence from equivalence-verified runs
/// #15–#17, NOT a live measurement) clear the frozen margin. The live
/// assertion of the same gate is `rfc0031_indicative_comparative_run`;
/// latency stays recorded there as corroborating, non-gating.
#[test]
fn rfc0031_2_l1_template_lookup_bytes() {
    let m_l1 = ourios_bench::ComparativeMargins::default().m_l1;
    assert_eq!(m_l1, 10, "the §7-frozen L1 must-win margin");

    // The must-win rule at the frozen margin: pass at and below
    // ourios × 10 ≤ loki, fail one byte above — and a failure is a
    // Decided { pass: false, .. }, the reportable pillar-level arm.
    assert!(ourios_bench::bytes_must_win(100, 1_000, m_l1).passed());
    let over = ourios_bench::bytes_must_win(101, 1_000, m_l1);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));

    // §9.13 (runs #15/#16/#17): ourios 1,358,683 B vs Loki storage-side
    // 104.8–105.6 MB — 77.2–77.7×, clearing the frozen margin with
    // headroom in every counted run since the pair landed.
    for loki_storage in [104_825_428, 105_191_956, 105_579_510] {
        let outcome = ourios_bench::bytes_must_win(1_358_683, loki_storage, m_l1);
        assert!(
            outcome.passed(),
            "a recorded §9.13 L1 run must clear the frozen M_L1: {outcome:?}",
        );
    }
}

/// Scenario RFC0031.3 — L2 attribute predicate wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 `M_L2` unfreeze (2026-07-14, the named condition
/// met: RFC 0033's v2 artifact merged and measured warm at 187,904 B in
/// run #21, §9.15): `M_L2 = 10` on the processed channel as primary,
/// plus the 1.1× storage-side floor (`m_l2_storage_floor_tenths = 11`).
/// Locally provable without a container — both frozen gates decide
/// exactly at their boundaries in the must-win direction, and the
/// recorded measurements (documented historical evidence from
/// equivalence-verified runs, NOT a live measurement) decide correctly
/// on both sides: the post-artifact honest total (§9.13 run #10's
/// components with §9.15's warm registry: 0 + 2,035,267 + 187,904 =
/// 2,223,171 B) clears both channels against every §9.13 reproduction
/// row, while the pre-RFC-0033 total (2,549,129 B) correctly FAILS the
/// storage floor on the weakest recorded Loki row — the floor is not
/// vacuous. The live assertion of the same gates is
/// `rfc0031_indicative_comparative_run`.
#[test]
fn rfc0031_3_l2_attribute_predicate_bytes() {
    let margins = ourios_bench::ComparativeMargins::default();
    assert_eq!(
        margins.m_l2, 10,
        "the §7-frozen L2 processed-channel margin"
    );
    assert_eq!(
        margins.m_l2_storage_floor_tenths, 11,
        "the §7-frozen L2 storage-side floor (1.1×, in tenths)"
    );

    // The processed must-win rule at the frozen margin: pass at and
    // below ourios × 10 ≤ loki, fail one byte above — a Decided fail,
    // the reportable pillar-level arm.
    assert!(ourios_bench::bytes_must_win(100, 1_000, margins.m_l2).passed());
    let over = ourios_bench::bytes_must_win(101, 1_000, margins.m_l2);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));
    // The storage floor in tenths: pass at ourios × 11 == loki × 10,
    // fail one byte above.
    let tenths = margins.m_l2_storage_floor_tenths;
    assert!(ourios_bench::bytes_must_win_tenths(1_000, 1_100, tenths).passed());
    let over = ourios_bench::bytes_must_win_tenths(1_001, 1_100, tenths);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));

    // §9.13 (runs #10–#17 reproductions) with §9.15's warm registry
    // applied: the post-artifact total clears the processed margin
    // (37.3–45.1×) and the storage floor (1.20–1.51×) on every
    // recorded (storage, processed) row.
    let post_artifact = 2_223_171;
    for (loki_storage, loki_processed) in [
        (2_751_834, 85_261_718),
        (2_779_800, 86_255_901),
        (3_349_897, 98_253_343),
        (3_224_893, 100_044_070),
        (2_688_942, 83_216_895),
        (2_673_545, 82_919_233),
        (3_224_528, 100_198_466),
    ] {
        let processed = ourios_bench::bytes_must_win(post_artifact, loki_processed, margins.m_l2);
        assert!(
            processed.passed(),
            "a recorded §9.13 L2 run must clear the frozen processed M_L2: {processed:?}",
        );
        let storage = ourios_bench::bytes_must_win_tenths(post_artifact, loki_storage, tenths);
        assert!(
            storage.passed(),
            "a recorded §9.13 L2 run must clear the frozen storage floor: {storage:?}",
        );
    }

    // The pre-RFC-0033 total (2,549,129 B — the 513,862 B cold fold in
    // every query) fails the storage floor against run #16's Loki row
    // (1.05×): the frozen gate correctly refuses the configuration the
    // unfreeze condition existed to fix.
    let pre_artifact = ourios_bench::bytes_must_win_tenths(2_549_129, 2_673_545, tenths);
    assert!(!pre_artifact.passed(), "{pre_artifact:?}");
}

/// Scenario RFC0031.4 — L3 trace correlation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 partial freeze (2026-07-13): `M_L3 = 10` on the
/// storage-side bytes channel. Locally provable without a container:
/// the frozen gate decides correctly on BOTH sides of the margin using
/// the §9.13-recorded measurements as documented historical evidence
/// (not a live measurement) — the pre-bloom run #12 configuration
/// (1.41×) correctly FAILS the frozen margin, and the post-bloom runs
/// #14–#17 (21.2–21.9×) clear it. The live assertion is
/// `rfc0031_indicative_comparative_run`.
#[test]
fn rfc0031_4_l3_trace_correlation_bytes() {
    let m_l3 = ourios_bench::ComparativeMargins::default().m_l3;
    assert_eq!(m_l3, 10, "the §7-frozen L3 must-win margin");

    // §9.13 run #12 — before the trace_id/span_id blooms Ourios fetched
    // the trace_id column corpus-wide (72,935,984 B): the frozen gate
    // correctly fails that configuration rather than flattering it.
    let pre_bloom = ourios_bench::bytes_must_win(72_935_984, 102_835_803, m_l3);
    assert!(!pre_bloom.passed(), "{pre_bloom:?}");

    // §9.13 runs #14–#17 — with blooms the fetch collapses to
    // 4,812,668 B and clears the frozen margin four runs in a row.
    for loki_storage in [105_353_837, 102_133_866, 104_656_570, 105_251_547] {
        let outcome = ourios_bench::bytes_must_win(4_812_668, loki_storage, m_l3);
        assert!(
            outcome.passed(),
            "a recorded §9.13 L3 run must clear the frozen M_L3: {outcome:?}",
        );
    }
}

/// Scenario RFC0031.5 — L4 frequency aggregation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// `M_L4` stays **§7-DEFERRED** ("deferred until L4 is first measured") —
/// unlike RFC0031.2/.4 (`M_L1`/`M_L3` frozen on the storage channel) or
/// RFC0031.7 (`F_L6` frozen on the latency channel), there is no recorded
/// measurement to pin a gate against yet, so this slice cannot pin a
/// frozen-value bound the way those scenarios do. What it pins instead —
/// the un-stub for this slice — is the **equivalence machinery** and the
/// **`PairSpec` shape** an L4 pair takes: the picker
/// ([`pick_frequency_pair`]), the Ourios aggregation extraction
/// (`ourios_bench::ourios_aggregate_answer`), the Loki matrix parser
/// (`ourios_bench::parse_loki_matrix`, including its bucket-alignment
/// convention), and the `(bucket, group_key) -> count` equivalence check
/// (`ourios_bench::compare_aggregations`) — all wired correctly on the
/// deterministic fixture. No container: the Loki side is a HAND-BUILT
/// matrix response over the SAME grouped counts the Ourios side measured
/// (mirroring how `pick_template_pair_finds_a_validated_needle` validates
/// the L1 picker against the Ourios engine alone, with the live
/// cross-system round trip left to the dispatch run). The bytes figure is
/// PRINTED, never asserted, per the deferral.
#[test]
fn rfc0031_5_l4_frequency_aggregation_bytes() {
    // Two values ("10"/"11") of one template ("connection established to
    // peer <id>" — the same ≥10-char constant run
    // `pick_template_pair_finds_a_validated_needle` already validates),
    // spread over five 12-minute buckets so the picker's cardinality
    // (2..=50) and row-floor bounds are exercised, not merely satisfied
    // trivially. 1000x the original sub-3s timestamps (~580s average
    // spacing over the 100s-3000s span) so this candidate also clears
    // L4_MIN_AVG_INTERVAL_SECONDS — see
    // `pick_frequency_pair_finds_a_moderate_cardinality_group`.
    let records: Vec<(u64, &str)> = vec![
        (100_000_000_000, "connection established to peer 10"),
        (500_000_000_000, "connection established to peer 10"),
        (900_000_000_000, "connection established to peer 11"),
        (2_100_000_000_000, "connection established to peer 10"),
        (2_500_000_000_000, "connection established to peer 11"),
        (3_000_000_000_000, "connection established to peer 11"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    let pair = pick_frequency_pair(bucket.path(), &tenant, now, window)
        .expect("the peer template's trailing id validates as an L4 candidate");
    assert_eq!(pair.param, 0, "the template's only wildcard");
    assert_eq!(
        pair.bucket_width, "12m",
        "the ~2900s fixture span targets a 12-minute bucket width",
    );
    assert_eq!(pair.needle, "connection established to peer");
    assert_eq!(pair.capture_regex, "peer\\s+(?P<value>\\S+)");
    let distinct: std::collections::HashSet<&String> =
        pair.groups.keys().map(|k| &k.group_key).collect();
    assert_eq!(distinct.len(), 2, "exactly the two picked id values");
    assert_eq!(pair.groups.values().sum::<u64>(), 6, "all six rows counted");
    assert!(
        pair.bytes_read > 0,
        "the grouped-count scan reads real bytes from storage",
    );

    // The `PairSpec` shape the dispatch run would fill in with a live
    // Loki round trip (RFC 0031 §3.4: L4 is a must-win class) — pinned
    // here without wiring the container-based measurement loop, per this
    // slice's scope.
    let margins = ourios_bench::ComparativeMargins::default();
    let spec = l4_pair_spec(
        &pair,
        built.min_effective_time_unix_nano,
        built.max_effective_time_unix_nano,
        now,
        window,
        &margins,
    )
    .expect("the fixture's capture_regex carries no backtick");
    assert!(
        matches!(spec.class.gate(), GateKind::MustWin),
        "L4 is a must-win class (RFC 0031 §3.4), same disposition as L1/L2/L3",
    );
    assert_eq!(
        spec.margin, 10,
        "m_l4's current (§7-deferred) default value"
    );

    // The Ourios half, run for real through the exact `spec.dsl` (no
    // container needed) — and proved to match the picker's own
    // measurement.
    let ourios = ourios_aggregate_answer(bucket.path(), &tenant, &spec.dsl, spec.now, spec.window)
        .expect("l4 aggregate answer");
    assert_eq!(ourios.groups, pair.groups);

    // The Loki half: a HAND-BUILT matrix response over the SAME grouped
    // counts (no container — the live cross-system round trip is the
    // dispatch run's job), proving `parse_loki_matrix`'s bucket-alignment
    // convention (eval instant t = (k+1)*w, decoded bucket start = t - w)
    // decodes to EXACTLY the Ourios map.
    let width_ns = bucket_width_seconds(&pair.bucket_width)
        .checked_mul(1_000_000_000)
        .expect("bucket width fits u64 nanoseconds");
    let loki_json = synthetic_loki_matrix(&pair.groups, width_ns);
    let loki_groups =
        parse_loki_matrix(&loki_json, "value", width_ns).expect("parse synthetic loki matrix");

    let outcome = compare_aggregations(&ourios.groups, &loki_groups, 8);
    assert!(
        outcome.is_equal(),
        "RFC0031.5 — the two systems' grouped-count maps must be identical: {outcome:?}",
    );

    // The mismatch arm (RFC0031.1's L4 half): dropping one cell from the
    // Loki side must report Mismatch, not silently pass — mirrors
    // RFC0031.1's `loki_narrow` assertion for the line-returning classes.
    let mut short = loki_groups.clone();
    let any_key = short.keys().next().cloned().expect("non-empty map");
    short.remove(&any_key);
    assert!(
        !compare_aggregations(&ourios.groups, &short, 8).is_equal(),
        "a dropped aggregation cell must report Mismatch, not silently pass",
    );

    // PRINTED, never asserted — M_L4 stays §7-deferred until the dispatch
    // run's first live measurement.
    println!(
        "RFC0031.5 [{}] ourios bytes_read (grouped-count scan) = {} — M_L4 DEFERRED \
         (RFC 0031 §7): reported, not asserted, until first measured",
        spec.label, ourios.bytes_read,
    );
}

/// Scenario RFC0031.6 — L5 substring needle measured + published, loss permitted.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.6 stub — implemented in the L-gate + reporting green slice"]
fn rfc0031_6_l5_substring_needle_published() {
    todo!(
        "RFC0031.6 — a literal not captured by a template or a promoted \
         column (embedded in a param, nothing prunes it), RFC0031.1 \
         holding: both systems' bytes_read + latency recorded, \
         disposition 'acknowledged'. Run PASSES regardless of winner — \
         an Ourios loss does not fail the run and does not escalate, but \
         MUST appear in the benchmarks.md §9 table (a suppressed L5 loss \
         is a process violation)"
    );
}

/// Scenario RFC0031.7 — L6 broad scan stays within the floor.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 partial freeze (2026-07-13): `F_L6 = 3` FROZEN on
/// the LATENCY channel, exactly as the scenario is written
/// (`ourios.latency_p50 ≤ F_L6 × loki.latency_p50`); the window pairs'
/// bytes figures are reclassified to a published diagnostic (their
/// publication is RFC0031.11's ground). Locally provable: the floor
/// semantics at the frozen factor decide exactly at the boundary, and
/// the §9.13 run #18 window-pair p50s (documented historical evidence,
/// not a live measurement) all hold the floor. The live assertion —
/// loudly non-evaluable when latency is unmeasured, never a silent
/// pass or a spurious fail — is `rfc0031_indicative_comparative_run`.
#[test]
fn rfc0031_7_l6_broad_scan_floor() {
    let f_l6 = ourios_bench::ComparativeMargins::default().f_l6;
    assert_eq!(f_l6, 3, "the §7-frozen L6 latency floor factor");

    // The floor rule at the frozen factor: pass at ourios == 3 × loki,
    // fail one microsecond above — a Decided fail (the tuning-RFC
    // signal, not a pillar-level escalation), never an Invalid.
    let us = Duration::from_micros;
    assert!(latency_floor_gate(us(300), us(100), f_l6).passed());
    let over = latency_floor_gate(us(301), us(100), f_l6);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));

    // §9.13 run #18 (the program's first latency measurements): all
    // three window pairs hold the frozen floor — 40.2 vs 13.8 ms,
    // 85.9 vs 294.8 ms, 38.8 vs 51.2 ms.
    for (ourios_p50, loki_p50) in [(40_200, 13_800), (85_900, 294_800), (38_800, 51_200)] {
        let outcome = latency_floor_gate(us(ourios_p50), us(loki_p50), f_l6);
        assert!(
            outcome.passed(),
            "a recorded §9.13 run-#18 window pair must hold the frozen F_L6: {outcome:?}",
        );
    }
}

/// Scenario RFC0031.8 — L7 ingest throughput parity within a stated factor.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.8 stub — implemented in the ingest-parity green slice"]
fn rfc0031_8_l7_ingest_throughput_parity() {
    todo!(
        "RFC0031.8 — OTLP replay driver feeding both systems to steady \
         state on the same hardware: ourios.ingest_throughput >= \
         loki.ingest_throughput / F_L7. The WAL-before-ack invariant \
         (CLAUDE.md §3.4) is NOT relaxed to obtain the number — Ourios \
         throughput is measured with durable acks and the config \
         proving it is recorded"
    );
}

/// Scenario RFC0031.9 — storage footprint is a diagnostic, not a gate.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.9 stub — implemented in the reporting + escalation green slice"]
fn rfc0031_9_storage_footprint_diagnostic() {
    todo!(
        "RFC0031.9 — both systems' persisted bytes on the shared bucket \
         and their ratio written to benchmarks.md §9 as a DIAGNOSTIC \
         row; no pass/fail derived from it (parity with A1's RFC 0011 \
         demotion)"
    );
}

/// Scenario RFC0031.10 — Loki config committed, competent, machine-checked.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.10 stub — implemented in the config-check green slice"]
fn rfc0031_10_loki_config_machine_checked() {
    todo!(
        "RFC0031.10 — the exact Loki config (index, chunk target size, \
         S3 backend, retention, frozen label set), the OTLP-into-Loki \
         config, and the DSL<->LogQL query pairs are present under \
         bench/comparative/ and the comparison runs with one documented \
         command; a test asserts the label set is drawn from a declared \
         low-cardinality allowlist and that trace_id, span_id, and any \
         per-template id are ABSENT (no catch-all-forcing-full-scan and \
         no high-cardinality label smuggling Ourios's columns into \
         Loki's index); each §9 row links the config commit"
    );
}

/// Scenario RFC0031.11 — losses published and escalation follows §7.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green for the locally-testable half of the commitment: the published
/// record (`docs/benchmarks.md` §9.13) carries every class measured so
/// far — the L1/L3 wins, L2's honest short-of-margin disposition, AND
/// the window pairs' storage-channel loss figures (reclassified by the
/// §7 freeze from gated floor to published diagnostic — publishing them
/// honestly IS the commitment). A suppressed loss row is the process
/// violation the scenario forbids, so the loss figures' presence is
/// asserted against the committed doc. The escalation POLICY (an L1–L4
/// bytes loss is pillar-level; a latency-only loss with a bytes win is
/// a roadmap item) is prose in `benchmarks.md` §7 and the RFC; its
/// enforcement teeth are `rfc0031_indicative_comparative_run`'s
/// frozen-gate assertions.
#[test]
fn rfc0031_11_losses_published_and_escalation() {
    let text = std::fs::read_to_string(repo_root().join("docs/benchmarks.md"))
        .expect("read docs/benchmarks.md");
    let start = text
        .find("### 9.13 ")
        .expect("benchmarks.md carries the §9.13 comparative calibration record");
    let rest = &text[start..];
    let section = rest[1..].find("\n### ").map_or(rest, |i| &rest[..=i]);

    for (marker, why) in [
        // Unformatted substrings: a bold-only doc edit must not fail
        // this; deleting the figure itself must.
        ("77.2×", "the L1 flagship win (run #15) — wins publish too"),
        ("21.9×", "the L3 win (runs #14/#17)"),
        (
            "not a provisional 10× pass",
            "L2's honest short-of-margin disposition",
        ),
        (
            "Time-window browses (L6 floor family)",
            "the window-browse class heading",
        ),
        ("0.007 fail", "the k=2000 storage-channel loss row (run #8)"),
        (
            "0.016 fail",
            "the k=2000 storage-channel loss on current code (run #10)",
        ),
        (
            "0.018 fail",
            "the selective-resource diagnostic's storage loss (run #17)",
        ),
    ] {
        assert!(
            section.contains(marker),
            "§9.13 must publish {why} ({marker:?}) — deleting a loss figure \
             is the process violation RFC0031.11 forbids",
        );
    }
}

// ---------------------------------------------------------------------------
// Indicative comparative run (§7 calibration input) — dispatch-only.
// ---------------------------------------------------------------------------

/// The §7 calibration input: the indicative Ourios-vs-Loki bytes-read
/// comparison on a real corpus (`OURIOS_COMPARATIVE_CORPUS`, fetched by
/// the `comparative-bench` dispatch workflow — the `corpus/otel-demo-v*`
/// releases), measured across the [`build_pair_specs`] pair set — a
/// selectivity curve plus the L1/L3 must-win points — over one
/// container + one corpus replay.
///
/// **Equivalence is asserted per pair, and the §7-FROZEN gates
/// (partial freeze 2026-07-13; `M_L2` unfrozen and frozen 2026-07-14)
/// are asserted after the report prints**:
/// L1/L3 storage-channel must-win at margin 10, L2 processed-channel
/// must-win at 10 plus the 1.1× storage-side floor (11/10,
/// integer-exact), the RFC0031.7 latency floor at
/// factor 3 on the L6-family window pairs (loudly non-evaluable when
/// latency is unmeasured, never a silent pass or a spurious fail), and
/// the RFC0033.6 corpus acquisition gate (warm artifact GET ≤ half the
/// cold audit fold, whenever any pair ran warm). The
/// window pairs' bytes channel and the
/// selective-resource pair stay reported diagnostics per the §7
/// deferrals.
///
/// Loki runs the stock image config plus explicit, documented
/// ingest-side deviations (all in LOKI'S favour — the anti-strawman
/// direction): `-validation.reject-old-samples=false` (the frozen
/// captures carry their original timestamps, weeks old by run time),
/// raised ingest-rate limits so a 2.96 GB replay isn't throttled by
/// dev-scale defaults, and a raised internal gRPC message cap (see the
/// flag comment). The query side stays stock.
#[test]
#[ignore = "dispatch-only: needs Docker + a corpus via OURIOS_COMPARATIVE_CORPUS (comparative-bench workflow)"]
#[allow(clippy::too_many_lines)] // one linear dispatch-run script: pick → build → measure → gate
fn rfc0031_indicative_comparative_run() {
    let corpus_dir = std::path::PathBuf::from(
        std::env::var("OURIOS_COMPARATIVE_CORPUS")
            .expect("set OURIOS_COMPARATIVE_CORPUS to a corpus dir (the dispatch workflow does)"),
    );

    let pair = pick_selective_pair(&corpus_dir);
    eprintln!("pair: {pair:?}");
    let (clean_ts, poison_ts) = collect_service_timestamps(&corpus_dir, &pair.service);
    eprintln!(
        "service {}: {} clean timestamps, {} zero-time (poison)",
        pair.service,
        clean_ts.len(),
        poison_ts.len(),
    );
    let trace = pick_trace_pair(&corpus_dir);
    eprintln!("trace pair: {trace:?}");
    let rare_window = pick_rare_window_pair(&corpus_dir, &pair.service);
    eprintln!("selective-resource window: {rare_window:?}");

    // The (locally-proven) Ourios half, per pair. The RFC 0033
    // template-map artifact persists in the bucket across pairs BY
    // DESIGN — production reality: caches persist. The first
    // row-rendering query after the store build folds the audit stream
    // cold and write-through-publishes; every later query's registry
    // component is one small artifact GET (the L1 picker's rendering
    // validation query may itself be the publisher, so even the first
    // measured pair can be warm). Each pair's outcome is classified at
    // measurement time and printed in the report block.
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        &corpus_dir,
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let corpus_now = built
        .max_effective_time_unix_nano
        .checked_add(1)
        .expect("corpus max effective timestamp overflows now");
    let corpus_window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
    // The L1 picker runs against the BUILT store (template ids exist only
    // post-mining), unlike the corpus-scanning pickers above.
    let template = pick_template_pair(
        &corpus_dir,
        bucket.path(),
        &tenant,
        corpus_now,
        corpus_window,
    );
    eprintln!("template pair: {template:?}");
    // L4 (RFC 0031 §3.4/§3.5): picked the same way as L1's template pair
    // (post-store-build — group cardinality only exists once templates
    // are mined), but kept OUT of `Picks`/`specs` — purely additive to
    // the pair list, not woven into the line-returning classes' shared
    // machinery (see `run_l4_pair`'s doc for why: an aggregation's
    // `(bucket, group) -> count` map is not a `LineKey` multiset, and
    // forcing it through `OuriosAnswer`/`compare_lines` would misrepresent
    // the state rather than model it).
    // The dispatch's class filter (issue #538 item 4): a targeted
    // re-run measures only the requested classes. Skipping L4 skips
    // pick_frequency_pair too — the per-template enumeration is the
    // pickers' one genuinely expensive pass.
    let class_filter = ClassFilter::from_env();
    let l4_requested = class_filter.includes(PairClass::L4);
    let frequency = if l4_requested {
        pick_frequency_pair(bucket.path(), &tenant, corpus_now, corpus_window)
    } else {
        eprintln!("L4 SKIPPED by OURIOS_COMPARATIVE_CLASSES (not a must-win failure)");
        None
    };
    eprintln!("frequency pair: {frequency:?}");
    let margins = ourios_bench::ComparativeMargins::default();
    let l4_spec = frequency.as_ref().and_then(|pair| {
        l4_pair_spec(
            pair,
            built.min_effective_time_unix_nano,
            built.max_effective_time_unix_nano,
            corpus_now,
            corpus_window,
            &margins,
        )
    });
    if l4_requested && l4_spec.is_none() {
        eprintln!(
            "L4 PAIR MISSING (this WILL fail the dispatch — L4 is must-win): either no \
             template/param slot cleared pick_frequency_pair's bounds (moderate cardinality \
             {L4_CARDINALITY:?}, {L4_MIN_ROWS}..={L4_MAX_ROWS} rows, >= \
             {L4_MIN_AVG_INTERVAL_SECONDS}s average inter-arrival interval, a validated \
             needle + capture regex), or the picked candidate's capture regex contained a \
             backtick l4_pair_spec can't embed in LogQL's raw-string delimiter — the \
             rejection lines above say which"
        );
    }
    let picks = Picks {
        pair: &pair,
        clean_ts: &clean_ts,
        poison_ts: &poison_ts,
        trace: trace.as_ref(),
        template: template.as_ref(),
        rare_window: rare_window.as_ref(),
    };
    let mut specs = build_pair_specs(&picks, corpus_now, corpus_window);
    let before = specs.len();
    specs.retain(|spec| class_filter.includes(spec.class));
    if specs.len() < before {
        eprintln!(
            "class filter {:?}: measuring {} of {before} line-returning pairs",
            class_filter,
            specs.len(),
        );
    }
    let artifact_path = bucket
        .path()
        .join("audit")
        .join(format!(
            "tenant_id={}",
            ourios_parquet::percent_encode_tenant(built.tenant),
        ))
        .join(ourios_querier::TEMPLATE_MAP_FILENAME);
    let measured: Vec<(
        ourios_bench::OuriosAnswer,
        Option<Duration>,
        TemplateMapProbe,
    )> = specs
        .iter()
        .map(|spec| {
            let answer = ourios_bench::ourios_query_answer(
                bucket.path(),
                &tenant,
                &spec.dsl,
                spec.now,
                spec.window,
            )
            .expect("ourios answer");
            assert_eq!(
                answer.lines.len() as u64,
                spec.expected_rows,
                "Ourios must return exactly [{}]'s expected rows",
                spec.label,
            );
            let probe = TemplateMapProbe::observe(&artifact_path, answer.registry_bytes);
            // Timed reps only after the pair's Ourios correctness holds.
            let latency_p50 = ourios_latency_p50(bucket.path(), &tenant, spec);
            (answer, latency_p50, probe)
        })
        .collect();
    // Publish-outcome labels resolve AFTER every timed measurement (§3.2
    // amendment): reproducing the abstention decision costs a fold +
    // compress, so it runs once, off the measured path, and only labels.
    let absent_outcome = measured
        .iter()
        .any(|(_, _, probe)| matches!(probe, TemplateMapProbe::ColdAbsent { .. }))
        .then(|| reproduce_publish_decision(bucket.path(), &tenant));
    // The RFC0033.6 corpus acquisition gate's inputs, harvested before
    // the probes collapse into report labels: any warm pair's artifact
    // GET size, and — when a pair ran cold — the fold it measured.
    let warm_artifact = measured.iter().find_map(|(_, _, probe)| match probe {
        TemplateMapProbe::Warm { artifact_bytes } => Some(*artifact_bytes),
        _ => None,
    });
    let cold_fold = measured.iter().find_map(|(_, _, probe)| match probe {
        TemplateMapProbe::ColdPublished { registry_bytes, .. }
        | TemplateMapProbe::ColdAbsent { registry_bytes }
        | TemplateMapProbe::ColdUnreadable { registry_bytes, .. } => Some(*registry_bytes),
        TemplateMapProbe::Warm { .. } => None,
    });
    let acquisition_failure = template_map_acquisition_failure(warm_artifact, cold_fold, || {
        ourios_querier::derive_template_map(ourios_querier::StoreRef::Local(bucket.path()), &tenant)
            .map(|(_, fold_bytes)| fold_bytes)
            .map_err(|e| e.to_string())
    });
    let ourios: Vec<OuriosMeasured> = specs
        .iter()
        .zip(measured)
        .map(|(spec, (answer, latency_p50, probe))| {
            let template_map = probe.label(absent_outcome.as_deref());
            eprintln!(
                "[{}] template-map acquisition (RFC 0033): {template_map}",
                spec.label,
            );
            OuriosMeasured {
                answer,
                latency_p50,
                template_map,
            }
        })
        .collect();

    // The Loki half: one container (stock + documented ingest-side
    // flags), ONE full-corpus OTLP replay, all pairs measured against it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (loki, l4_loki): (Vec<_>, Option<Result<L4Measured, String>>) = runtime.block_on(async {
        let (container, base, http) = start_loki(LOKI_DISPATCH_FLAGS).await;
        push_corpus_to_loki(&http, &base, &corpus_dir).await;
        let mut measured = Vec::with_capacity(specs.len());
        for spec in &specs {
            let result = match loki_measure_pair(&http, &base, spec).await {
                Ok((lines, bytes, fetched)) => {
                    // Timed reps only after the poll proved ingest caught
                    // up — post-poll, so warm-up asymmetry is minimized.
                    let latency_p50 = loki_latency_p50(&http, &base, spec).await;
                    Ok((lines, bytes, fetched, latency_p50))
                }
                Err(detail) => Err(detail),
            };
            measured.push(result);
        }
        // L4, same container + corpus replay as the pairs above (one
        // Loki instance for the whole run) — but measured on its own
        // matrix query, not folded into the `for spec in &specs` loop.
        let l4 = match (&frequency, &l4_spec) {
            (Some(pair), Some(spec)) => {
                let bucket_width_ns = bucket_width_seconds(&pair.bucket_width)
                    .checked_mul(1_000_000_000)
                    .expect("bucket width fits u64 nanoseconds");
                Some(
                    loki_measure_frequency_pair(&http, &base, &container, spec, bucket_width_ns)
                        .await,
                )
            }
            _ => None,
        };
        (measured, l4)
    });

    // The machine-readable per-pair record (issue #538 item 3) writes
    // FIRST — before split/equivalence/gates can panic — so every run,
    // passing or failing, leaves a queryable completeness artifact.
    if let Ok(path) = std::env::var("OURIOS_COMPARATIVE_RESULTS") {
        let results = comparative_results_json(
            &specs,
            &ourios,
            &loki,
            L4ArtifactInput {
                spec: l4_spec.as_ref(),
                loki: l4_loki.as_ref(),
                ourios_bytes: frequency.as_ref().map(|pair| pair.bytes_read),
            },
            pair.total_records,
            &class_filter,
        );
        let rendered = serde_json::to_string_pretty(&results).expect("results serialize");
        match std::fs::write(&path, rendered) {
            Ok(()) => eprintln!("comparative results artifact written to {path}"),
            // Diagnostic-only output must never fail the run.
            Err(e) => eprintln!("(couldn't write the results artifact to {path}: {e})"),
        }
    }

    let (ok_specs, ok_ourios, ok_loki, mut failures) = split_measurements(&specs, &ourios, loki);

    // Equivalence gates every successful measurement (RFC0031.1).
    for ((spec, ours), (loki_lines, _, _, _)) in ok_specs.iter().zip(&ok_ourios).zip(&ok_loki) {
        let outcome = compare_lines(&ours.answer.lines, loki_lines, 8);
        assert!(
            outcome.is_equal(),
            "the two systems' answers must be multiset-identical on [{}]: {outcome:?}",
            spec.label,
        );
    }

    print_indicative_report(
        &corpus_dir,
        pair.total_records,
        &ok_specs,
        &ok_ourios,
        &ok_loki,
    );

    // L4 (RFC 0031 §3.4/§3.5) is purely additive to the pair list above,
    // measured and reported right after it — BEFORE the gate/failure
    // assertions below, not after: an L1–L3/L6 measurement failure (a
    // flake, e.g. the documented Loki low-volume-chunk race) must not
    // prevent L4 from ever being attempted or reported. A failed L4
    // measurement joins the SAME `failures` list those classes use, so
    // it still fails the run — just without silently starving L4 of a
    // chance to run at all. A missing candidate (the picker found
    // nothing eligible) is ALSO pushed into `failures` — L4 is a
    // must-win class (RFC 0031 §3.4), so a corpus that never produces a
    // viable candidate must fail the dispatch, not silently pass with
    // zero L4 evidence (the eprintln above is a diagnostic breadcrumb,
    // not a substitute for actually failing the gate).
    match (&l4_spec, l4_loki) {
        (Some(spec), Some(result)) => {
            run_l4_pair(bucket.path(), &tenant, spec, result, &mut failures);
        }
        // A filtered-out L4 is a requested skip, not a must-win failure
        // — the class filter's whole point is a targeted partial run.
        (None, _) if !l4_requested => {}
        (None, _) => failures.push(
            "L4: no viable frequency pair — either no template/param slot cleared \
             pick_frequency_pair's bounds, or the picked candidate's capture regex \
             contained a backtick l4_pair_spec can't embed in LogQL (see the L4 PAIR \
             MISSING diagnostic above for which) — L4 is a must-win class and cannot be \
             silently skipped"
                .to_string(),
        ),
        (Some(spec), None) => unreachable!(
            "l4_spec.is_some() implies l4_loki.is_some(): the async block's own match \
             (`(Some(pair), Some(spec)) => measure`) only skips measuring when EITHER is \
             None, and l4_spec.is_some() already implies frequency.is_some() (it's derived \
             via frequency.as_ref().and_then(l4_pair_spec)) — [{}] has a spec but no Loki \
             measurement (harness bug)",
            spec.label,
        ),
    }

    // The frozen gates run AFTER the report so a failed gate cannot
    // destroy the run's evidence (the run #11 salvage lesson).
    let mut gate_failures = frozen_gate_failures(&ok_specs, &ok_ourios, &ok_loki);
    gate_failures.extend(acquisition_failure);
    assert!(
        gate_failures.is_empty(),
        "{} asserting gate(s) failed — frozen §7 plus the RFC0033.6 corpus acquisition \
         gate (the report above carries the evidence): {gate_failures:#?}",
        gate_failures.len(),
    );
    assert!(
        failures.is_empty(),
        "{} pair(s) failed to measure (report above covers the rest, L4 included): \
         {failures:?}",
        failures.len(),
    );
}

/// One pair's bytes-channel lines, labeled per the §7 partial freeze
/// (2026-07-13) and the `M_L2` unfreeze (2026-07-14). Both Loki byte
/// figures are evaluated for every pair —
/// `totalBytesProcessed` is decompressed engine-side work, which
/// overstates Loki's storage reads by the chunk compression ratio; the
/// storage-side figure (compressed chunk bytes + memory-served
/// head-chunk bytes) is the apples-to-apples counterpart of Ourios's
/// fetched-compressed-Parquet bytes — but which line carries a gate
/// verdict depends on the class: L1/L3 gate on the frozen storage
/// channel (processed is context), L2 gates on the processed channel
/// (primary) plus the frozen 1.1× storage-side floor, L4 prints both
/// channels' ratio with no verdict (`M_L4` §7-deferred), and the window
/// pairs' bytes lines are a published diagnostic — a ratio, no verdict
/// — since the freeze reclassified them; their gate is the RFC0031.7
/// latency floor.
fn print_pair_bytes_gates(
    spec: &PairSpec,
    ourios_bytes: u64,
    loki_storage: u64,
    loki_processed: u64,
) {
    let gate = |loki_bytes: u64| {
        spec.class
            .gate()
            .evaluate(ourios_bytes, loki_bytes, spec.margin)
    };
    // Diagnostic bytes lines carry the gate math's advantage ratio (and
    // its zero-measurement honesty guards) WITHOUT a pass/fail verdict:
    // the §7 freeze reclassified the window pairs' bytes from gated
    // floor to published diagnostic.
    let diagnostic = |loki_bytes: u64| match gate(loki_bytes) {
        ourios_bench::BytesGateOutcome::Decided { advantage, .. } => {
            format!("ratio loki/ourios = {advantage:.3}")
        }
        outcome => format!("{outcome:?}"),
    };
    match spec.class {
        PairClass::L1 | PairClass::L3 => {
            println!(
                "gate vs storage-side (PRIMARY — §7 FROZEN, must-win margin {}): {:?}",
                spec.margin,
                gate(loki_storage),
            );
            println!(
                "gate vs bytes-processed (context, must-win margin {}): {:?}",
                spec.margin,
                gate(loki_processed),
            );
        }
        PairClass::L2 => {
            println!(
                "gate vs bytes-processed (PRIMARY — §7 FROZEN, must-win margin {}): {:?}",
                spec.margin,
                gate(loki_processed),
            );
            let tenths = ourios_bench::ComparativeMargins::default().m_l2_storage_floor_tenths;
            println!(
                "gate vs storage-side (§7 FROZEN floor, must-win at {tenths}/10): {:?}",
                ourios_bench::bytes_must_win_tenths(ourios_bytes, loki_storage, tenths),
            );
        }
        // L4 is must-win-shaped (RFC 0031 §3.4), but `M_L4` stays
        // §7-DEFERRED until first measured (§7): both channels print the
        // gate math's ratio for the record, never a pass/fail verdict —
        // `frozen_gate_failures` never evaluates this class.
        PairClass::L4 => {
            println!(
                "gate vs storage-side (must-win margin {}, §7 DEFERRED — reported only): {}",
                spec.margin,
                diagnostic(loki_storage),
            );
            println!(
                "gate vs bytes-processed (must-win margin {}, §7 DEFERRED — reported only): {}",
                spec.margin,
                diagnostic(loki_processed),
            );
        }
        PairClass::WindowFloor | PairClass::Diagnostic => {
            println!(
                "bytes vs storage-side: diagnostic (published, not gated); {}",
                diagnostic(loki_storage),
            );
            println!(
                "bytes vs bytes-processed: diagnostic (published, not gated); {}",
                diagnostic(loki_processed),
            );
        }
    }
}

/// The L4 pair's report block — the aggregation counterpart of
/// [`print_indicative_report`]'s per-pair section. An aggregation renders
/// no lines and acquires no template map (RFC 0002 §6.5: the aggregate
/// path's `materialize_bytes_read`/`registry_bytes_read` are structurally
/// zero, see [`ourios_bench::OuriosAggregateAnswer`]), so there is no
/// count-scan/materialize/registry breakdown to print, and this slice
/// does not wire an L4 latency channel (§7 DEFERRED already covers the
/// bytes verdict; latency is corroborating-only everywhere else too, so
/// this is a scope boundary, not a gap). Reuses [`print_pair_bytes_gates`]
/// for the shared bytes-channel labeling.
fn print_l4_report(
    spec: &PairSpec,
    ourios_bytes: u64,
    loki_fetched: &LokiFetchedBytes,
    loki_processed: u64,
) {
    let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
    println!("--- pair [{}] rows={} ---", spec.label, spec.expected_rows);
    println!("dsl: {}", spec.dsl);
    println!("logql: {}", spec.logql);
    println!("ourios bytes_read (grouped-count scan, compressed, fetched) = {ourios_bytes}");
    println!(
        "loki   storage-side bytes (conservative)  = {loki_storage} \
         (compressed={} + head_chunk={})",
        loki_fetched.compressed_bytes, loki_fetched.head_chunk_bytes,
    );
    println!("loki   totalBytesProcessed (decompressed) = {loki_processed}");
    print_pair_bytes_gates(spec, ourios_bytes, loki_storage, loki_processed);
}

/// Measure, equivalence-check, and report the L4 pair — the aggregation
/// counterpart of the line-returning pairs' report loop, kept separate
/// (see `rfc0031_indicative_comparative_run`'s call-site comment) rather
/// than folded into `OuriosMeasured`/`Measured`, which are built around
/// `LineKey` multisets. `l4_loki` is the Loki-side matrix measurement
/// already gathered inside the same container session the other pairs
/// share (the dispatch run's async block).
///
/// Called right after L1–L3/L6's evidence has PRINTED, but — unlike
/// those classes — BEFORE their frozen §7 gates are asserted (the call
/// site's own comment has the full reasoning): printing first means an
/// earlier pair's failure can never destroy already-printed evidence,
/// and running L4 before the frozen-gate assertions means an L4
/// failure can't prevent those gates from at least having their
/// evidence printed, even though — because this function can panic —
/// it does mean the frozen gates might not get a chance to formally
/// assert in the same run. A Loki-side MEASUREMENT failure
/// (timeout/flake — the SAME failure mode `split_measurements` salvages
/// for L1–L3/L6) is pushed onto `failures` and the pair is skipped,
/// exactly like a flaky L1–L3/L6 pair never reaching `ok_specs`. A
/// genuine equivalence MISMATCH (both sides measured, but disagree
/// beyond `L4_COMPLETENESS_MARGIN`) still hard-panics immediately —
/// RFC0031.1 equivalence is never optional, matching the L1–L3/L6
/// `compare_lines` assertion this mirrors — the failure modes are
/// deliberately not symmetric: a flake is salvageable, a mismatch is
/// not. `M_L4` itself stays §7-DEFERRED — [`print_l4_report`] prints
/// both bytes channels' ratio with no verdict, exactly like the
/// fixture-level proof (`rfc0031_5_l4_frequency_aggregation_bytes`).
fn run_l4_pair(
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    spec: &PairSpec,
    l4_loki: Result<L4Measured, String>,
    failures: &mut Vec<String>,
) {
    let (loki_groups, loki_processed, loki_fetched) = match l4_loki {
        Ok(measured) => measured,
        Err(detail) => {
            failures.push(format!(
                "L4 pair [{}] failed to measure: {detail}",
                spec.label
            ));
            return;
        }
    };
    let ourios_answer =
        ourios_aggregate_answer(bucket_root, tenant, &spec.dsl, spec.now, spec.window)
            .expect("l4 ourios aggregate answer");
    // RFC 0031 §7's completeness-margin decision (2026-07-17): exact
    // per-cell equality (`compare_aggregations`) is what the RFC0031.5
    // fixture-level test still holds a synthetic Loki answer to, but a
    // real dispatch's real Loki never reaches it — see
    // `L4_COMPLETENESS_MARGIN`'s documentation for the full evidence
    // trail. A phantom cell (one absent from Ourios's own answer) or
    // Loki's total exceeding Ourios's total still hard-fails here.
    let outcome = compare_aggregations_within_margin(
        &ourios_answer.groups,
        &loki_groups,
        L4_COMPLETENESS_MARGIN,
        8,
    );
    assert!(
        outcome.is_equal(),
        "RFC0031.5 — the two systems' grouped-count maps must be equivalent within the \
         {:.0}% completeness margin on [{}]: {outcome:?}",
        L4_COMPLETENESS_MARGIN * 100.0,
        spec.label,
    );
    print_l4_report(
        spec,
        ourios_answer.bytes_read,
        &loki_fetched,
        loki_processed,
    );
}

/// The indicative run's report block, one section per pair, labeled per
/// the §7 partial freeze (2026-07-13) — the bytes-channel labeling
/// itself is [`print_pair_bytes_gates`].
///
/// The latency lines carry the §3.6 corroborating channel: warm p50s
/// (median of [`LATENCY_REPS`] post-poll reps — see the asymmetry note
/// on [`ourios_latency_p50`]: Ourios in-process, Loki over localhost
/// HTTP), the `loki_p50/ourios_p50` ratio in the bytes gates'
/// above-1.0-means-Ourios-faster orientation, and — for Floor pairs — the
/// RFC0031.7 latency floor as written (`ourios_p50 ≤ F_L6 × loki_p50`).
fn print_indicative_report(
    corpus_dir: &std::path::Path,
    total_records: u64,
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: &[Measured],
) {
    println!("=== RFC 0031 indicative comparative run ===");
    println!("corpus: {} ({total_records} records)", corpus_dir.display());
    for ((spec, ours), (_, loki_processed, loki_fetched, loki_latency)) in
        specs.iter().zip(ourios).zip(loki)
    {
        let answer = &ours.answer;
        let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
        println!("--- pair [{}] rows={} ---", spec.label, spec.expected_rows);
        println!("dsl: {}", spec.dsl);
        // The Ourios figure is the honest TOTAL (§3.6 measurement-fidelity
        // amendment, 2026-07-12): count scan + row materialization +
        // template-map acquisition (RFC 0033: cold audit fold or warm
        // artifact GET) — the gates below ratio against it.
        println!(
            "ourios bytes_read (compressed, fetched)   = {} \
             (count_scan={} + materialize={} + registry={})",
            answer.bytes_read,
            answer.count_scan_bytes,
            answer.materialize_bytes,
            answer.registry_bytes,
        );
        // The registry component's RFC 0033 acquisition + publish
        // outcome (RFC0033.6's channel, §3.2 amendment): the artifact
        // persists across pairs by design, so a successful publish makes
        // the cold audit fold a once-per-run cost — while an abstention
        // (printed with its would-be size vs the folded bytes) leaves
        // every pair cold, run #20's finding.
        println!(
            "ourios template-map acquisition (RFC 0033) = {}",
            ours.template_map
        );
        println!(
            "loki   storage-side bytes (conservative)  = {loki_storage} \
             (compressed={} + head_chunk={})",
            loki_fetched.compressed_bytes, loki_fetched.head_chunk_bytes,
        );
        println!("loki   totalBytesProcessed (decompressed) = {loki_processed}");
        print_pair_bytes_gates(spec, answer.bytes_read, loki_storage, *loki_processed);
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        match ours.latency_p50 {
            Some(p50) => println!(
                "ourios latency_p50 (warm, in-process)      = {:.3} ms ({LATENCY_REPS} reps)",
                ms(p50),
            ),
            None => println!("ourios latency_p50                         = unmeasured"),
        }
        match loki_latency {
            Some(p50) => println!(
                "loki   latency_p50 (warm, localhost HTTP)  = {:.3} ms ({LATENCY_REPS} reps)",
                ms(*p50),
            ),
            None => println!("loki   latency_p50                         = unmeasured"),
        }
        if let (Some(ours_p50), Some(loki_p50)) = (ours.latency_p50, *loki_latency) {
            // A zero p50 (timer resolution coarser than the query) would
            // print inf/NaN — say "undefined" instead; the floor gate
            // below already treats zero as Invalid.
            if ours_p50.is_zero() || loki_p50.is_zero() {
                println!(
                    "latency ratio loki_p50/ourios_p50 = undefined (a p50 of 0 — \
                     timer resolution coarser than the query)"
                );
            } else {
                println!(
                    "latency ratio loki_p50/ourios_p50 = {:.2} (>1.0 = Ourios faster; \
                     corroborating only — Ourios is timed in-process, Loki over \
                     localhost HTTP, which favours Ourios on sub-millisecond answers)",
                    ms(loki_p50) / ms(ours_p50),
                );
            }
            match spec.class {
                PairClass::WindowFloor => println!(
                    "latency floor gate (RFC0031.7 — §7 FROZEN, floor factor {}): {:?}",
                    spec.margin,
                    latency_floor_gate(ours_p50, loki_p50, spec.margin),
                ),
                PairClass::Diagnostic => println!(
                    "latency floor (diagnostic reference, factor {}): {:?}",
                    spec.margin,
                    latency_floor_gate(ours_p50, loki_p50, spec.margin),
                ),
                PairClass::L1 | PairClass::L2 | PairClass::L3 | PairClass::L4 => {}
            }
        }
    }
}
