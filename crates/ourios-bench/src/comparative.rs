//! RFC 0031 — comparative evaluation vs Grafana Loki.
//!
//! This module is the harness that measures Ourios against Loki on the
//! same OTLP corpus (see `docs/rfcs/0031-comparative-evaluation-loki.md`).
//! It lives in `ourios-bench` — extending the RFC 0006 harness — rather
//! than a new crate, keeping the §7 "new crate vs `bench/`" question open
//! (a new crate is a `CLAUDE.md` §7 architectural commitment).
//!
//! Landed so far (the equivalence-harness slice, RFC0031.1 logic half):
//! the **result-set equivalence** comparator. A latency or bytes
//! comparison between two queries that return different answers is
//! meaningless, so every gate is fenced behind this check — it must
//! confirm both systems answer the *same* question before any of their
//! numbers are trusted (RFC0031.1). The Loki-container integration that
//! drives real answers into it is the next slice.
//!
//! The **Ourios side** of that check — [`ourios_query_lines`] — runs a
//! logs-DSL query against an Ourios store in-process (the querier, no
//! served binary — RFC 0031 §7) and lowers the rendered rows to
//! [`LineKey`]s. The Loki side (a container fed the same OTLP corpus,
//! queried with the equivalent `LogQL`) mirrors it, and the two feed
//! [`compare_lines`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use ourios_core::tenant::TenantId;

use crate::BenchError;

/// Stable identity of one returned log line, per RFC0031.1:
/// `(timestamp_unix_nanos, body_bytes)`. Two lines are the same datum
/// iff both fields match — the round-trip identity that survives each
/// system's ingest/return path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LineKey {
    /// Log-record timestamp in Unix nanoseconds (matching the
    /// workspace `time_unix_nano` representation).
    pub timestamp_unix_nanos: u64,
    /// The record body, byte-exact.
    pub body: Vec<u8>,
}

/// Stable identity of one aggregation cell (RFC0031.1, L4 class):
/// a `(time-bucket, group-key)` pair whose value is a count.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AggKey {
    /// Inclusive start of the time bucket, Unix nanoseconds.
    pub bucket_start_unix_nanos: u64,
    /// The `GROUP BY` key value (e.g. an extracted template param).
    pub group_key: String,
}

/// The result of comparing two systems' answers to one query.
///
/// [`Equal`](EquivalenceOutcome::Equal) is the only outcome that lets a
/// gate record its metric; a [`Mismatch`](EquivalenceOutcome::Mismatch)
/// means the two DSLs did not express the same question, so the harness
/// writes the summary + examples and skips that class (RFC0031.1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EquivalenceOutcome {
    /// The two answers are multiset-identical (or, for aggregations,
    /// map-identical).
    Equal,
    /// The answers differ; carries a human summary and bounded examples.
    Mismatch(Mismatch),
}

/// Detail of an equivalence mismatch — enough to write the RFC0031.1
/// stderr report without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// One-line count-delta summary.
    pub summary: String,
    /// Up to `examples_cap` rendered example keys that differ.
    pub examples: Vec<String>,
}

impl EquivalenceOutcome {
    /// `true` iff the two answers matched.
    #[must_use]
    pub fn is_equal(&self) -> bool {
        matches!(self, Self::Equal)
    }
}

/// Compare two line-returning result sets as **multisets** (RFC0031.1):
/// each key's *count* must match, so a system returning three identical
/// duplicate lines where the other returns two is a mismatch, not a
/// silent pass. `examples_cap` bounds the example keys surfaced.
#[must_use]
pub fn compare_lines(
    ourios: &[LineKey],
    loki: &[LineKey],
    examples_cap: usize,
) -> EquivalenceOutcome {
    let ourios_counts = tally(ourios);
    let loki_counts = tally(loki);

    // Walk the union of keys once; a differing count on either side is a
    // mismatch. Sorting the differing keys keeps the report deterministic
    // (map iteration order is not).
    let mut differing: Vec<(&LineKey, u64, u64)> = ourios_counts
        .keys()
        .chain(loki_counts.keys())
        .filter_map(|k| {
            let o = ourios_counts.get(k).copied().unwrap_or(0);
            let l = loki_counts.get(k).copied().unwrap_or(0);
            (o != l).then_some((*k, o, l))
        })
        .collect();
    if differing.is_empty() {
        return EquivalenceOutcome::Equal;
    }
    // De-dup (the chain visited shared keys twice) and order stably.
    differing.sort_by(|a, b| {
        (a.0.timestamp_unix_nanos, &a.0.body).cmp(&(b.0.timestamp_unix_nanos, &b.0.body))
    });
    differing.dedup_by(|a, b| a.0 == b.0);

    let only_ourios = differing
        .iter()
        .filter(|(_, o, l)| *l == 0 && *o > 0)
        .count();
    let only_loki = differing.iter().filter(|(_, o, _)| *o == 0).count();
    let unequal = differing.len() - only_ourios - only_loki;
    let summary = format!(
        "{} line-keys differ ({only_ourios} only in ourios, {only_loki} only in loki, \
         {unequal} with unequal counts)",
        differing.len(),
    );
    let examples = differing
        .iter()
        .take(examples_cap)
        .map(|(k, o, l)| {
            let mut s = String::new();
            // Bodies can be arbitrary bytes and arbitrarily large; render
            // a truncated, lossy preview so a mismatch report can't blow
            // up stderr.
            let _ = write!(
                s,
                "ts={} body={:?} ourios={o} loki={l}",
                k.timestamp_unix_nanos,
                body_preview(&k.body),
            );
            s
        })
        .collect();
    EquivalenceOutcome::Mismatch(Mismatch { summary, examples })
}

/// Compare two aggregation results as `(bucket, group_key) -> count`
/// maps (RFC0031.1 for the L4 class): every cell must match exactly.
#[must_use]
pub fn compare_aggregations<S1: std::hash::BuildHasher, S2: std::hash::BuildHasher>(
    ourios: &HashMap<AggKey, u64, S1>,
    loki: &HashMap<AggKey, u64, S2>,
    examples_cap: usize,
) -> EquivalenceOutcome {
    let mut differing: Vec<(&AggKey, u64, u64)> = ourios
        .keys()
        .chain(loki.keys())
        .filter_map(|k| {
            let o = ourios.get(k).copied().unwrap_or(0);
            let l = loki.get(k).copied().unwrap_or(0);
            (o != l).then_some((k, o, l))
        })
        .collect();
    if differing.is_empty() {
        return EquivalenceOutcome::Equal;
    }
    differing.sort_by(|a, b| {
        (a.0.bucket_start_unix_nanos, &a.0.group_key)
            .cmp(&(b.0.bucket_start_unix_nanos, &b.0.group_key))
    });
    differing.dedup_by(|a, b| a.0 == b.0);

    let summary = format!("{} aggregation cells differ", differing.len());
    let examples = differing
        .iter()
        .take(examples_cap)
        .map(|(k, o, l)| {
            format!(
                "bucket={} group={:?} ourios={o} loki={l}",
                k.bucket_start_unix_nanos, k.group_key,
            )
        })
        .collect();
    EquivalenceOutcome::Mismatch(Mismatch { summary, examples })
}

/// Count occurrences of each key — the multiset the comparison walks.
fn tally(lines: &[LineKey]) -> HashMap<&LineKey, u64> {
    let mut counts: HashMap<&LineKey, u64> = HashMap::with_capacity(lines.len());
    for line in lines {
        *counts.entry(line).or_insert(0) += 1;
    }
    counts
}

/// Run a logs-DSL query against the Ourios store at `bucket_root` and
/// return the matching rows as [`LineKey`]s — the Ourios half of the
/// RFC0031.1 equivalence check.
///
/// Runs the querier **in-process** (RFC 0031 §7: no served binary). The
/// query MUST carry a `limit` large enough to render **every** matching
/// row — the querier renders rows only when a limit is set, and caps them
/// at it. An equivalence check over a truncated (or empty, limit-less)
/// result is meaningless, so this **enforces completeness**: it errors
/// unless the rendered row count equals the total match count.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the DSL fails to parse, the tokio runtime
/// can't be built, the query fails, the rendered rows don't cover every
/// matching row (missing or too-small `limit`), or a returned row carries
/// a body kind the equivalence extraction does not yet lower (a structured
/// or absent body — the string-body case always lowers).
pub fn ourios_query_lines(
    bucket_root: &Path,
    tenant: &TenantId,
    dsl: &str,
    now_unix_nano: u64,
    default_window_nanos: u64,
) -> Result<Vec<LineKey>, BenchError> {
    let query = ourios_querier::dsl::parse(dsl).map_err(|e| BenchError::Pipeline {
        detail: format!("comparative DSL parse `{dsl}`: {e}"),
    })?;
    let querier = ourios_querier::Querier::new(bucket_root);
    // A current-thread runtime is enough: `run_query` offloads its own
    // blocking IO, and the comparative harness drives one query at a time.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| BenchError::Pipeline {
            detail: format!("comparative tokio runtime: {e}"),
        })?;
    let result = runtime
        .block_on(querier.run_query(&query, tenant, now_unix_nano, default_window_nanos, None))
        .map_err(|e| BenchError::Pipeline {
            detail: format!("comparative query `{dsl}`: {e}"),
        })?;
    // Completeness guard: `records` is the rendered rows (capped at the
    // query's limit); `rows` is the total match count. If they differ the
    // result is truncated (or the query had no limit, so nothing rendered),
    // and comparing an incomplete set would silently pass a false match.
    if result.records.len() as u64 != result.rows {
        return Err(BenchError::Pipeline {
            detail: format!(
                "comparative query `{dsl}` matched {} rows but rendered {} — the \
                 equivalence check needs the complete result; raise the `| limit`",
                result.rows,
                result.records.len(),
            ),
        });
    }
    result
        .records
        .iter()
        .map(|row| {
            Ok(LineKey {
                timestamp_unix_nanos: row.time_unix_nano,
                body: body_bytes(&row.body)?,
            })
        })
        .collect()
}

/// Canonical byte identity of a query row's body for equivalence keying.
///
/// The string-body (`Rendered`) case — the only kind the first
/// severity-predicate query pair over a text corpus produces — lowers to
/// its reconstructed bytes. Structured and absent bodies belong to the
/// OTLP-native gates (L2/L3/L4) and are deferred: they return an error
/// rather than a lossy encoding, because RFC 0025's absent-vs-empty
/// distinction (and structured-vs-string) must be represented in the key
/// deliberately, not collapsed — the follow-up slice that lands those
/// gates extends [`LineKey`] to carry the body-kind discriminator.
fn body_bytes(body: &ourios_querier::LogBody) -> Result<Vec<u8>, BenchError> {
    use ourios_querier::LogBody;
    // Name the body *kind*, never `Debug`-dump the content: a structured
    // body can be large and carry sensitive payload.
    let kind = match body {
        LogBody::Rendered { line, .. } => return Ok(line.clone()),
        LogBody::Structured(_) => "structured",
        LogBody::Absent => "absent",
        _ => "unknown",
    };
    Err(BenchError::Pipeline {
        detail: format!(
            "comparative equivalence extraction does not yet lower {kind} bodies \
             (they land with the OTLP-native L2/L3/L4 gates)"
        ),
    })
}

/// A truncated, lossy preview of a body for a mismatch report — bounded
/// so an arbitrarily large body can't blow up the stderr summary.
fn body_preview(body: &[u8]) -> String {
    const CAP: usize = 96;
    let shown = &body[..body.len().min(CAP)];
    let mut preview = String::from_utf8_lossy(shown).into_owned();
    if body.len() > CAP {
        let _ = write!(preview, "…(+{} bytes)", body.len() - CAP);
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: u64, body: &str) -> LineKey {
        LineKey {
            timestamp_unix_nanos: ts,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn identical_multisets_are_equal() {
        let a = vec![line(1, "a"), line(2, "b"), line(3, "c")];
        // Same keys, different order — multiset equality is order-independent.
        let b = vec![line(3, "c"), line(1, "a"), line(2, "b")];
        assert!(compare_lines(&a, &b, 8).is_equal());
    }

    #[test]
    fn duplicate_count_difference_is_a_mismatch() {
        // The whole point of multiset (not set) comparison: 3 copies vs 2.
        let a = vec![line(1, "dup"), line(1, "dup"), line(1, "dup")];
        let b = vec![line(1, "dup"), line(1, "dup")];
        let out = compare_lines(&a, &b, 8);
        let EquivalenceOutcome::Mismatch(m) = out else {
            panic!("expected mismatch on unequal duplicate counts");
        };
        assert!(m.summary.contains("unequal counts"), "{}", m.summary);
        assert_eq!(m.examples.len(), 1);
        assert!(m.examples[0].contains("ourios=3"));
        assert!(m.examples[0].contains("loki=2"));
    }

    #[test]
    fn key_only_on_one_side_is_a_mismatch() {
        let a = vec![line(1, "a"), line(2, "only-ourios")];
        let b = vec![line(1, "a")];
        let EquivalenceOutcome::Mismatch(m) = compare_lines(&a, &b, 8) else {
            panic!("expected mismatch");
        };
        assert!(m.summary.contains("1 only in ourios"), "{}", m.summary);
    }

    #[test]
    fn empty_on_both_sides_is_equal() {
        assert!(compare_lines(&[], &[], 8).is_equal());
    }

    #[test]
    fn examples_are_capped() {
        let a: Vec<LineKey> = (0..10).map(|i| line(i, "x")).collect();
        let b: Vec<LineKey> = Vec::new();
        let EquivalenceOutcome::Mismatch(m) = compare_lines(&a, &b, 3) else {
            panic!("expected mismatch");
        };
        assert_eq!(m.examples.len(), 3, "examples must respect the cap");
        assert!(m.summary.contains("10 line-keys differ"), "{}", m.summary);
    }

    #[test]
    fn aggregation_maps_equal_and_mismatch() {
        let mut o = HashMap::new();
        o.insert(agg(0, "svcA"), 5);
        o.insert(agg(0, "svcB"), 2);
        let mut l = o.clone();
        assert!(compare_aggregations(&o, &l, 8).is_equal());

        // One cell's count diverges.
        l.insert(agg(0, "svcB"), 3);
        let EquivalenceOutcome::Mismatch(m) = compare_aggregations(&o, &l, 8) else {
            panic!("expected aggregation mismatch");
        };
        assert!(
            m.summary.contains("1 aggregation cells differ"),
            "{}",
            m.summary
        );
        assert!(m.examples[0].contains("ourios=2"));
        assert!(m.examples[0].contains("loki=3"));
    }

    #[test]
    fn body_bytes_defers_non_string_kinds() {
        // An absent body (RFC 0025) must NOT be silently lowered to an
        // empty string — that would collapse a legally-distinct record
        // into an empty-string match. It errors until the OTLP-native
        // gates extend LineKey with a body-kind discriminator.
        assert!(
            body_bytes(&ourios_querier::LogBody::Absent).is_err(),
            "absent body must not be silently lowered to bytes",
        );
    }

    #[test]
    fn ourios_query_lines_extracts_bit_identical_bodies() {
        // Build a registry-bearing comparative store from a text corpus
        // (build_comparative_store persists the audit stream, so the querier
        // derives the RFC 0017 template registry and reconstructs bodies),
        // then prove the Ourios-side extraction returns one LineKey per
        // stored row with in-span timestamps and **bit-identical bodies** —
        // the Ourios half of RFC0031.1, locally verifiable without Loki.
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        let lines = ["user 1 logged in", "user 2 logged in", "user 3 logged in"];
        std::fs::write(corpus.path().join("fixture.txt"), lines.join("\n")).expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built =
            crate::build_comparative_store(corpus.path(), bucket.path(), crate::TxtSeverity::Fixed)
                .expect("build store");
        assert_eq!(built.rows, 3, "one stored row per corpus line");

        let tenant = TenantId::new(built.tenant);
        // Bracket the whole corpus time span with the default window.
        let now = built.max_effective_time_unix_nano + 1;
        let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
        let extracted = ourios_query_lines(
            bucket.path(),
            &tenant,
            "severity >= 0 | limit 100",
            now,
            window,
        )
        .expect("extract lines");

        assert_eq!(extracted.len(), 3, "one LineKey per stored row");
        for key in &extracted {
            assert!(
                (built.min_effective_time_unix_nano..=built.max_effective_time_unix_nano)
                    .contains(&key.timestamp_unix_nanos),
                "extracted timestamp is within the corpus span",
            );
        }
        // Bodies reconstruct bit-identically (RFC 0001 §6 / the C1
        // invariant), now that the registry is persisted.
        let mut bodies: Vec<String> = extracted
            .iter()
            .map(|k| String::from_utf8(k.body.clone()).expect("utf8 body"))
            .collect();
        bodies.sort();
        let got: Vec<&str> = bodies.iter().map(String::as_str).collect();
        assert_eq!(
            got, lines,
            "extracted bodies are the corpus lines, bit for bit"
        );

        // The extraction feeds the comparator: a result compared to itself
        // is Equal (the multiset round-trips through `compare_lines`).
        assert!(
            compare_lines(&extracted, &extracted, 8).is_equal(),
            "self-equivalence must hold",
        );

        // Completeness guard: a limit-less query matches rows but renders
        // none, so it must error rather than silently compare an empty set.
        assert!(
            ourios_query_lines(bucket.path(), &tenant, "severity >= 0", now, window).is_err(),
            "a limit-less query (rows matched, none rendered) must error",
        );
    }

    #[test]
    fn body_preview_bounds_large_bodies() {
        // Small body is shown whole.
        assert_eq!(body_preview(b"short"), "short");
        // A large body is truncated with a byte-count suffix (96-byte cap).
        let big = vec![b'x'; 500];
        let preview = body_preview(&big);
        assert!(
            preview.len() < 200,
            "preview must be bounded: {}",
            preview.len()
        );
        assert!(preview.contains("+404 bytes"), "{preview}");
    }

    fn agg(bucket: u64, group: &str) -> AggKey {
        AggKey {
            bucket_start_unix_nanos: bucket,
            group_key: group.to_string(),
        }
    }
}
