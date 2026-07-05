//! RFC 0024 §5 — the querier-owned pipeline property: P4 (the query
//! oracle, `.6`) over generated OTLP batches and generated predicates
//! from `ourios-testgen`. See
//! `crates/ourios-bench/tests/rfc0024_calibration.rs` for the
//! scenario placement map.
//!
//! Batches enter through the real miner, land in a real RFC 0005
//! store (with a promoted set covering *some* of the generator's
//! attribute key pool, so both RFC 0022 compile arms run), and every
//! generated predicate's count is checked against [`oracle_count`] —
//! a deliberately naive linear scan whose correctness is reviewable
//! by eye (RFC 0024 §3.3).

mod common;

use common::{no_aliases, write_all_with_promoted};
use ourios_core::config::MinerConfig;
use ourios_core::otlp::{OtlpLogRecord, any_value};
use ourios_core::record::{BodyKind, MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::PromotedAttributes;
use ourios_querier::Querier;
use ourios_testgen::manifest::{AnyValueShapes, BodyKindMix, CalibrationManifest, SeverityBucket};
use ourios_testgen::strategies;
use proptest::prelude::*;
use tempfile::TempDir;

/// Query "now": one day past the generators' plausible-timestamp
/// base, so calibrated rows straddle every generated window edge.
const NOW: u64 = strategies::TIME_BASE_UNIX_NANO + 24 * 3_600_000_000_000;

/// Default window wide enough to cover the whole generated timestamp
/// region when no `range(...)` stage is present.
const DEFAULT_WINDOW_NS: u64 = 30 * 24 * 3_600_000_000_000;

/// Keys `log.k00` / `log.k01` are promoted; the rest of the
/// calibrated pool stays JSON-arm only — .6 requires both compile
/// arms exercised.
fn promoted_set() -> PromotedAttributes {
    PromotedAttributes::new(
        Vec::new(),
        vec!["log.k00".to_string(), "log.k01".to_string()],
    )
}

/// The calibrated manifest: 4 distinct attribute keys (the promoted
/// pair plus two JSON-arm keys), a realistic severity mix, and a
/// week of timestamps around [`NOW`].
fn synthetic_manifest() -> CalibrationManifest {
    CalibrationManifest {
        corpus_tag: "rfc0024-oracle".to_string(),
        records: 100,
        log_attribute_count: [(0, 20), (1, 40), (3, 40)].into_iter().collect(),
        resource_attribute_count: [(1, 100)].into_iter().collect(),
        body_kind: BodyKindMix {
            string: 90,
            structured: 10,
            absent: 0,
        },
        string_body_len: [(4, 30), (5, 70)].into_iter().collect(),
        severity: vec![
            SeverityBucket {
                number: 9,
                text: Some("INFO".to_string()),
                count: 70,
            },
            SeverityBucket {
                number: 17,
                text: Some("ERROR".to_string()),
                count: 30,
            },
        ],
        any_value_shapes: AnyValueShapes {
            string: 80,
            int: 20,
            ..Default::default()
        },
        any_value_max_depth: 1,
        distinct_attribute_keys: 4,
    }
}

/// One generated predicate: its DSL text and its naive evaluation.
#[derive(Debug, Clone)]
enum Pred {
    /// `template_id == n` — template-exact equality.
    TemplateEq(u64),
    /// `severity == n` — numeric equality.
    SeverityEq(u8),
    /// `severity >= n` — the ordering class.
    SeverityGe(u8),
    /// `attr.<key> == "<value>"` — attribute equality, promoted or
    /// JSON arm depending on the key.
    AttrEq { key: &'static str, value: String },
    /// `severity >= 0 | range(-<h>h, now)` — the time-window class
    /// (the match-all filter isolates the window).
    Window { hours_back: u32 },
}

/// The generator's attribute key pool: first two promoted, last two
/// JSON-arm only.
const KEY_POOL: [&str; 4] = ["log.k00", "log.k01", "log.k02", "log.k03"];

fn pred_strategy() -> impl Strategy<Value = Pred> {
    prop_oneof![
        (0u64..8).prop_map(Pred::TemplateEq),
        (0u8..=24).prop_map(Pred::SeverityEq),
        (0u8..=24).prop_map(Pred::SeverityGe),
        (0usize..KEY_POOL.len(), "[a-z0-9 ]{1,12}").prop_map(|(k, value)| Pred::AttrEq {
            key: KEY_POOL[k],
            value
        }),
        (1u32..72).prop_map(|hours_back| Pred::Window { hours_back }),
    ]
}

impl Pred {
    fn dsl(&self) -> String {
        match self {
            Self::TemplateEq(n) => format!("template_id == {n}"),
            Self::SeverityEq(n) => format!("severity == {n}"),
            Self::SeverityGe(n) => format!("severity >= {n}"),
            Self::AttrEq { key, value } => format!(r#"attr.{key} == "{value}""#),
            Self::Window { hours_back } => {
                format!("severity >= 0 | range(-{hours_back}h, now)")
            }
        }
    }

    /// The window this predicate's query runs under (`resolve_window`
    /// picks the bounds — the `range` stage wins, otherwise the
    /// default lookback — and the compiled filter applies them
    /// half-open: `[start, end)`).
    fn window(&self) -> (u64, u64) {
        match self {
            Self::Window { hours_back } => (NOW - u64::from(*hours_back) * 3_600_000_000_000, NOW),
            _ => (NOW - DEFAULT_WINDOW_NS, NOW),
        }
    }

    fn matches(&self, r: &MinedRecord) -> bool {
        match self {
            Self::TemplateEq(n) => r.template_id == *n,
            Self::SeverityEq(n) => r.severity_number == *n,
            Self::SeverityGe(n) => r.severity_number >= *n,
            Self::AttrEq { key, value } => attr_first_string(r, key) == Some(value.as_str()),
            Self::Window { .. } => true,
        }
    }
}

/// First-occurrence string projection — the documented semantics of
/// both RFC 0022 arms (`ourios_parquet::promoted::project_string_value`).
fn attr_first_string<'a>(r: &'a MinedRecord, key: &str) -> Option<&'a str> {
    r.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| match &kv.value {
            Some(av) => match &av.value {
                Some(any_value::Value::StringValue(s)) => Some(s.as_str()),
                _ => None,
            },
            None => None,
        })
}

/// The stored effective timestamp (RFC 0005 §3.2: `time_unix_nano`,
/// else `observed_time_unix_nano`).
fn effective(r: &MinedRecord) -> u64 {
    if r.time_unix_nano != 0 {
        r.time_unix_nano
    } else {
        r.observed_time_unix_nano.unwrap_or(0)
    }
}

/// The reviewable-by-eye reference evaluator: linear scan, no
/// `DataFusion` (RFC 0024 §3.3).
fn oracle_count(rows: &[MinedRecord], pred: &Pred) -> u64 {
    let (start, end) = pred.window();
    rows.iter()
        .filter(|r| {
            let t = effective(r);
            t >= start && t < end && pred.matches(r)
        })
        .count() as u64
}

/// Mine the batch and return the rows the writer accepts (the two
/// documented rejections — absent body, timestamp overflow — are
/// P1's concern; the oracle compares over what is actually stored).
fn mine_writable(batch: &[OtlpLogRecord]) -> Vec<MinedRecord> {
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    for record in batch {
        cluster.ingest(record);
    }
    sink.drain()
        .into_iter()
        .filter(|r| {
            r.body_kind != BodyKind::Absent
                && i64::try_from(r.time_unix_nano).is_ok()
                && r.observed_time_unix_nano
                    .is_none_or(|t| i64::try_from(t).is_ok())
        })
        .collect()
}

async fn querier_count(q: &Querier, dsl: &str) -> u64 {
    let query = ourios_querier::dsl::parse(dsl).expect("generated DSL must parse");
    q.run_query(
        &query,
        &TenantId::new(strategies::TESTGEN_TENANT),
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&no_aliases()),
    )
    .await
    .expect("run_query")
    .rows
}

/// Predicates harvested from the stored rows themselves, so the
/// suite always exercises non-zero match counts (independently
/// generated values almost never collide with generated data): one
/// promoted-key equality and one JSON-arm equality when the data
/// offers them.
fn harvested_preds(rows: &[MinedRecord]) -> Vec<Pred> {
    let mut preds = Vec::new();
    for keys in [&KEY_POOL[..2], &KEY_POOL[2..]] {
        if let Some(pred) = rows.iter().find_map(|r| {
            keys.iter().find_map(|key| {
                attr_first_string(r, key).map(|value| Pred::AttrEq {
                    key,
                    value: value.to_string(),
                })
            })
        }) {
            preds.push(pred);
        }
    }
    if let Some(r) = rows.iter().find(|r| r.template_id != 0) {
        preds.push(Pred::TemplateEq(r.template_id));
    }
    preds
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    /// Scenario RFC0024.6 — P4: the query oracle.
    /// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
    ///
    /// Covered operator classes per field kind: `template_id`
    /// equality, `severity` equality + ordering, `attr.<key>`
    /// equality on a promoted key (typed-column arm) and a
    /// non-promoted key (JSON arm), and the time window (both the
    /// implicit default window and an explicit `range` stage).
    /// Generated predicates pin the no-false-positives direction;
    /// harvested predicates (values taken from the stored rows) pin
    /// no-false-negatives.
    #[test]
    fn rfc0024_6_querier_agrees_with_the_naive_oracle(
        calibrated in proptest::collection::vec(
            strategies::calibrated(&synthetic_manifest()), 4..24),
        adversarial in proptest::collection::vec(strategies::adversarial(), 0..8),
        preds in proptest::collection::vec(pred_strategy(), 1..6),
    ) {
        let mut batch = calibrated;
        batch.extend(adversarial);
        let rows = mine_writable(&batch);
        prop_assume!(!rows.is_empty());

        let bucket = TempDir::new().expect("temp dir");
        write_all_with_promoted(bucket.path(), &rows, &promoted_set());
        let querier = Querier::new(bucket.path());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        for pred in preds.iter().chain(harvested_preds(&rows).iter()) {
            let expected = oracle_count(&rows, pred);
            let got = runtime.block_on(querier_count(&querier, &pred.dsl()));
            prop_assert_eq!(
                got,
                expected,
                "querier disagrees with the naive oracle on `{}`",
                pred.dsl()
            );
        }
    }
}
