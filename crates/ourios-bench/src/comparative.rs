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

use opentelemetry_proto::tonic::logs::v1::LogsData;
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
/// RFC0031.1 equivalence check. Thin wrapper over [`ourios_query_answer`]
/// for callers that need only the lines.
///
/// # Errors
///
/// Exactly [`ourios_query_answer`]'s.
pub fn ourios_query_lines(
    bucket_root: &Path,
    tenant: &TenantId,
    dsl: &str,
    now_unix_nano: u64,
    default_window_nanos: u64,
) -> Result<Vec<LineKey>, BenchError> {
    ourios_query_answer(
        bucket_root,
        tenant,
        dsl,
        now_unix_nano,
        default_window_nanos,
    )
    .map(|answer| answer.lines)
}

/// The Ourios side of a comparative query: the matching rows as
/// [`LineKey`]s plus the **total bytes read from storage** to answer it —
/// the RFC 0031 §3.6 primary gate metric (measurement-fidelity amendment,
/// 2026-07-12: the total, not just the count scan).
#[derive(Debug, Clone)]
pub struct OuriosAnswer {
    /// The matching rows, keyed for [`compare_lines`].
    pub lines: Vec<LineKey>,
    /// **Total** bytes read from object storage to answer the query — the
    /// sum of the three components below, so the figure the L-gates ratio
    /// against Loki's `totalBytesProcessed`
    /// ([`parse_loki_bytes_processed`]) / storage-side bytes counts
    /// everything Ourios fetched to deliver the answer, not only the
    /// count/pruning scan (Loki's counterpart includes delivering
    /// results, so the §3.7 anti-strawman discipline requires ours to).
    pub bytes_read: u64,
    /// The count/pruning-scan component (`QueryStats::bytes_read`). `0`
    /// whenever the count scan was **elided** (`QueryOptions::single_pass`):
    /// the completeness this harness enforces — every matching row rendered,
    /// i.e. the limit was never hit — is exactly the condition under which
    /// the querier derives the count from the materialize pass and skips
    /// the count scan, so the query genuinely read zero bytes for it. The
    /// zero is the honest figure, not an accounting gap. It is non-zero on
    /// the success path only in the exact-limit edge (matches == limit),
    /// where the querier falls back to the count scan to prove the result
    /// wasn't truncated.
    pub count_scan_bytes: u64,
    /// The row-materialization component: the extra scan that fetched the
    /// ≤ `limit` rendered rows (`QueryResult::materialize_bytes_read`).
    pub materialize_bytes: u64,
    /// The template-map acquisition component (RFC 0033): the bytes that
    /// obtained the registry that reconstructs string bodies — a cold
    /// audit fold or a warm artifact GET
    /// (`QueryResult::registry_bytes_read`).
    pub registry_bytes: u64,
}

/// Run a logs-DSL query against the Ourios store at `bucket_root` and
/// return the matching rows **and** the bytes-read measurement — the
/// Ourios half of both the RFC0031.1 equivalence check and the
/// RFC0031.2–.5 bytes-read gates.
///
/// Runs the querier **in-process** (RFC 0031 §7: no served binary) with
/// `QueryOptions::single_pass`, so the measured bytes are what one
/// answer-delivering query actually reads: when the materialized result
/// is complete the count scan is elided rather than re-reading the same
/// row groups for a count already in hand (see
/// [`OuriosAnswer::count_scan_bytes`]). The query MUST carry a `limit`
/// large enough to render **every** matching row — the querier renders
/// rows only when a limit is set, and caps them at it. An equivalence
/// check over a truncated (or empty, limit-less) result is meaningless,
/// so this **enforces completeness**: it errors unless the rendered row
/// count equals the total match count.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the DSL fails to parse, the tokio runtime
/// can't be built, the query fails, the rendered rows don't cover every
/// matching row (missing or too-small `limit`), or a returned row carries
/// a body kind the equivalence extraction does not yet lower (a structured
/// or absent body — the string-body case always lowers).
pub fn ourios_query_answer(
    bucket_root: &Path,
    tenant: &TenantId,
    dsl: &str,
    now_unix_nano: u64,
    default_window_nanos: u64,
) -> Result<OuriosAnswer, BenchError> {
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
        .block_on(querier.run_query_with(
            &query,
            tenant,
            now_unix_nano,
            default_window_nanos,
            None,
            ourios_querier::QueryOptions::single_pass(),
        ))
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
    let count_scan_bytes = result.stats.bytes_read;
    let materialize_bytes = result.materialize_bytes_read;
    let registry_bytes = result.registry_bytes_read;
    // A wrapped sum would silently corrupt the primary gate metric —
    // fail loudly instead (the same rule as the L-gates' checked_mul).
    let bytes_read = count_scan_bytes
        .checked_add(materialize_bytes)
        .and_then(|sum| sum.checked_add(registry_bytes))
        .ok_or_else(|| BenchError::Pipeline {
            detail: format!(
                "total bytes_read overflows u64 (count_scan={count_scan_bytes}, \
                 materialize={materialize_bytes}, registry={registry_bytes})"
            ),
        })?;
    result
        .records
        .iter()
        .map(|row| {
            Ok(LineKey {
                timestamp_unix_nanos: row.time_unix_nano,
                body: body_bytes(&row.body)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| OuriosAnswer {
            lines,
            bytes_read,
            count_scan_bytes,
            materialize_bytes,
            registry_bytes,
        })
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

/// The Ourios side of an L4 (frequency-aggregation) comparative pair (RFC
/// 0031 §3.4/§3.5): the executed `count by param(n), bucket(w)` stage's
/// grouped-count map, keyed the way [`compare_aggregations`] expects, plus
/// the bytes read to answer it.
#[derive(Debug, Clone)]
pub struct OuriosAggregateAnswer {
    /// `(bucket, group_key) -> count`, per RFC0031.1's L4 equivalence shape.
    pub groups: HashMap<AggKey, u64>,
    /// Bytes read from storage to answer the aggregation. An aggregation
    /// renders no rows and acquires no template map — RFC 0002 §6.5's
    /// "zero row materialization and zero template-map acquisition"
    /// contract holds `materialize_bytes_read`/`registry_bytes_read` at
    /// `0` on this path — so the grouped-count scan
    /// (`QueryStats::bytes_read`) is already the honest total.
    pub bytes_read: u64,
}

/// Run a `count by param(n), bucket(w)` logs-DSL query against the Ourios
/// store and return its grouped-count map plus bytes read — the Ourios
/// half of the RFC0031.5 (L4) comparative pair.
///
/// Assumes this harness's own `by`-list convention (`count by param(N),
/// bucket(W)`, in exactly that order — the comparative-harness callers
/// never emit any other shape): the zeroth key cell is the extracted
/// param (the group), the first is the bucket window start.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the DSL fails to parse, the tokio runtime
/// can't be built, the query fails, the query did not compile to an
/// aggregation (`QueryResult::aggregate` is `None` — the `dsl` argument
/// must carry a `count by …` stage), or a group key does not carry
/// exactly the `(param, bucket)` two cells this harness's convention
/// requires.
pub fn ourios_aggregate_answer(
    bucket_root: &Path,
    tenant: &TenantId,
    dsl: &str,
    now_unix_nano: u64,
    default_window_nanos: u64,
) -> Result<OuriosAggregateAnswer, BenchError> {
    let query = ourios_querier::dsl::parse(dsl).map_err(|e| BenchError::Pipeline {
        detail: format!("comparative aggregate DSL parse `{dsl}`: {e}"),
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
            detail: format!("comparative aggregate query `{dsl}`: {e}"),
        })?;
    let raw_groups = result.aggregate.ok_or_else(|| BenchError::Pipeline {
        detail: format!(
            "comparative aggregate query `{dsl}` did not compile to an aggregation \
             (QueryResult::aggregate is None) — the dsl must carry a `count by …` stage"
        ),
    })?;
    let mut groups = HashMap::with_capacity(raw_groups.len());
    for group in raw_groups {
        let cell_count = group.key.len();
        let [group_key, bucket]: [String; 2] =
            group.key.try_into().map_err(|_| BenchError::Pipeline {
                detail: format!(
                    "comparative aggregate query `{dsl}` produced a {cell_count}-cell group \
                     key — this harness's convention is exactly `param(n), bucket(w)` (2 cells)"
                ),
            })?;
        let bucket_start_unix_nanos =
            rfc3339_to_unix_nanos(&bucket).map_err(|detail| BenchError::Pipeline {
                detail: format!("comparative aggregate query `{dsl}` bucket key: {detail}"),
            })?;
        // Groups decode from `decode_aggregate`'s already-deduplicated,
        // sorted-by-key output (one group per distinct key), so no key
        // collides with another here.
        groups.insert(
            AggKey {
                bucket_start_unix_nanos,
                group_key,
            },
            group.count,
        );
    }
    Ok(OuriosAggregateAnswer {
        groups,
        bytes_read: result.stats.bytes_read,
    })
}

/// Parse an RFC 3339 UTC instant — the `bucket(width)` group-key rendering
/// (`ourios-querier`'s `group_key_string` `Timestamp` arm) — to Unix
/// nanoseconds.
fn rfc3339_to_unix_nanos(s: &str) -> Result<u64, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|e| format!("bucket key {s:?} is not a resolvable RFC 3339 instant: {e}"))?;
    let ns = dt
        .timestamp_nanos_opt()
        .ok_or_else(|| format!("bucket key {s:?} is out of the representable range"))?;
    u64::try_from(ns).map_err(|_| format!("bucket key {s:?} predates the epoch"))
}

/// Parse a Loki `query_range` **matrix** response (a metric query: `sum by
/// (<label>) (count_over_time(...))`) into the `(bucket, group_key) ->
/// count` map [`compare_aggregations`] expects — the Loki half of the
/// RFC0031.5 (L4) equivalence check.
///
/// Loki (Prometheus-compatible) returns `data.result[]` entries shaped
/// `{"metric": {"<label>": "<value>"}, "values": [[<unix-seconds>,
/// "<count>"], ...]}`. **Bucket alignment** (RFC 0031 §7's L4 open
/// question): each sample at evaluation instant `t` is
/// `count_over_time(range[w])`'s count over the half-open window `(t-w,
/// t]`. Querying with `start`/`step` pinned so every evaluation instant is
/// `t = (k+1)·w` for a desired Ourios bucket `k` makes that window exactly
/// the `bucket(w)` window `[k·w, (k+1)·w)` — so a sample's decoded bucket
/// **start**, matching [`AggKey::bucket_start_unix_nanos`], is `t - w`
/// (`bucket_width_ns`). Choosing that `start`/`step` is the caller's job
/// (the harness's pair builder); this function only decodes under the
/// convention, it does not choose them.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the response isn't JSON; is a Loki error
/// response; is not `resultType: "matrix"`; is missing `data.result`; a
/// result entry is missing its `<label>` metric or its `values` array; a
/// sample isn't a `[number, string]` pair; a sample's timestamp is not a
/// non-negative whole-second instant (this harness never emits a
/// sub-second bucket width, so a fractional-second sample means a
/// misaligned query, not lost precision); a sample's timestamp is before
/// `bucket_width_ns` (would underflow the bucket start); a sample's count
/// string doesn't parse to a `u64`; or two samples land in the same
/// `(bucket, group_key)` cell and their counts would overflow `u64`
/// summed together.
pub fn parse_loki_matrix(
    response_json: &str,
    label_name: &str,
    bucket_width_ns: u64,
) -> Result<HashMap<AggKey, u64>, BenchError> {
    let root = parse_loki_root(response_json)?;
    let result_type = root
        .get("data")
        .and_then(|d| d.get("resultType"))
        .and_then(serde_json::Value::as_str);
    if result_type != Some("matrix") {
        return Err(BenchError::Pipeline {
            detail: format!(
                "Loki response resultType is {result_type:?}, not \"matrix\" — an L4 \
                 metric query must return a matrix"
            ),
        });
    }
    let result = root
        .get("data")
        .and_then(|d| d.get("result"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| BenchError::Pipeline {
            detail: "Loki response missing `data.result` array".to_string(),
        })?;

    let mut groups: HashMap<AggKey, u64> = HashMap::new();
    for (ri, series) in result.iter().enumerate() {
        let group_key = series
            .get("metric")
            .and_then(|m| m.get(label_name))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BenchError::Pipeline {
                detail: format!("Loki matrix result {ri} metric is missing label `{label_name}`"),
            })?
            .to_string();
        let values = series
            .get("values")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| BenchError::Pipeline {
                detail: format!("Loki matrix result {ri} missing `values` array"),
            })?;
        for (vi, pair) in values.iter().enumerate() {
            let (bucket_start_unix_nanos, count) =
                decode_matrix_sample(pair, bucket_width_ns, ri, vi)?;
            let key = AggKey {
                bucket_start_unix_nanos,
                group_key: group_key.clone(),
            };
            let existing = groups.entry(key).or_insert(0);
            *existing = existing
                .checked_add(count)
                .ok_or_else(|| BenchError::Pipeline {
                    detail: format!(
                        "Loki matrix result {ri} value {vi}: summing count {count} into an \
                     existing cell overflows u64"
                    ),
                })?;
        }
    }
    Ok(groups)
}

/// Decode one `[<unix-seconds>, "<count>"]` matrix sample to `(bucket
/// start, count)`, per [`parse_loki_matrix`]'s bucket-alignment
/// convention. `ri`/`vi` (result/value index) are for error messages
/// only.
fn decode_matrix_sample(
    sample: &serde_json::Value,
    bucket_width_ns: u64,
    ri: usize,
    vi: usize,
) -> Result<(u64, u64), BenchError> {
    let entry = sample
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| BenchError::Pipeline {
            detail: format!("Loki matrix result {ri} value {vi} is not a [timestamp, count] pair"),
        })?;
    let t_seconds = entry[0].as_f64().ok_or_else(|| BenchError::Pipeline {
        detail: format!("Loki matrix result {ri} value {vi} timestamp is not a number"),
    })?;
    if !t_seconds.is_finite() || t_seconds < 0.0 || t_seconds.fract() != 0.0 {
        return Err(BenchError::Pipeline {
            detail: format!(
                "Loki matrix result {ri} value {vi} timestamp {t_seconds} is not a \
                 non-negative whole-second instant — this harness never queries a \
                 sub-second bucket width, so a fractional sample means a misaligned \
                 query, not a precision loss"
            ),
        });
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // Checked above: finite, non-negative, integral — an exact conversion
    // (unix seconds are far under f64's 2^53 exact-integer range).
    let seconds = t_seconds as u64;
    let t_ns = seconds
        .checked_mul(1_000_000_000)
        .ok_or_else(|| BenchError::Pipeline {
            detail: format!(
                "Loki matrix result {ri} value {vi} timestamp {t_seconds}s overflows \
                 u64 nanoseconds"
            ),
        })?;
    let bucket_start_unix_nanos =
        t_ns.checked_sub(bucket_width_ns)
            .ok_or_else(|| BenchError::Pipeline {
                detail: format!(
                    "Loki matrix result {ri} value {vi} timestamp {t_seconds}s is before \
                     the bucket width ({bucket_width_ns} ns) — cannot compute a bucket start"
                ),
            })?;
    let count_str = entry[1].as_str().ok_or_else(|| BenchError::Pipeline {
        detail: format!("Loki matrix result {ri} value {vi} count is not a string"),
    })?;
    let count: u64 = count_str.parse().map_err(|e| BenchError::Pipeline {
        detail: format!("Loki matrix result {ri} value {vi} count `{count_str}` is not a u64: {e}"),
    })?;
    Ok((bucket_start_unix_nanos, count))
}

/// Parse a Loki `query_range` **streams** response into [`LineKey`]s — the
/// Loki half of the RFC0031.1 equivalence check.
///
/// Loki returns matching lines under `data.result[].values[]`, each a
/// `["<ns-timestamp-string>", "<log line>"]` pair. Each becomes a
/// `LineKey` keyed the same way as the Ourios side (`(timestamp, body)`),
/// so the two feed [`compare_lines`]. The timestamp is Loki's nanosecond
/// string; the body is the log line bytes.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the response isn't JSON; is a Loki **error**
/// response (`status == "error"` — surfaces Loki's `errorType` / `error`);
/// is missing the `data.result` array; has a stream missing its `values`
/// array; has a `values` entry that isn't a two-element `[string, string]`
/// pair; has a timestamp or log line that isn't a string; or has a
/// timestamp string that isn't a `u64`. Malformed-entry errors carry the
/// stream + value indices for debugging against real Loki responses.
pub fn parse_loki_streams(response_json: &str) -> Result<Vec<LineKey>, BenchError> {
    let root = parse_loki_root(response_json)?;
    let result = root
        .get("data")
        .and_then(|d| d.get("result"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| BenchError::Pipeline {
            detail: "Loki response missing `data.result` array".to_string(),
        })?;

    let mut lines = Vec::new();
    for (si, stream) in result.iter().enumerate() {
        let values = stream
            .get("values")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| BenchError::Pipeline {
                detail: format!("Loki stream {si} missing `values` array"),
            })?;
        for (vi, pair) in values.iter().enumerate() {
            let entry =
                pair.as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or_else(|| BenchError::Pipeline {
                        detail: format!(
                            "Loki stream {si} value {vi} is not a [timestamp, line] pair"
                        ),
                    })?;
            let ts_str = entry[0].as_str().ok_or_else(|| BenchError::Pipeline {
                detail: format!("Loki stream {si} value {vi} timestamp is not a string"),
            })?;
            let timestamp_unix_nanos = ts_str.parse::<u64>().map_err(|e| BenchError::Pipeline {
                detail: format!(
                    "Loki stream {si} value {vi} timestamp `{ts_str}` is not a u64: {e}"
                ),
            })?;
            let body = entry[1].as_str().ok_or_else(|| BenchError::Pipeline {
                detail: format!("Loki stream {si} value {vi} log line is not a string"),
            })?;
            lines.push(LineKey {
                timestamp_unix_nanos,
                body: body.as_bytes().to_vec(),
            });
        }
    }
    Ok(lines)
}

/// Parse a Loki `query_range` response's **bytes-processed** measurement
/// (`data.stats.summary.totalBytesProcessed`) — the Loki half of the
/// RFC 0031 §3.6 bytes-read gate metric, ratioed against
/// [`OuriosAnswer::bytes_read`]. Loki attaches the stats block to every
/// successful query response; its absence is an error rather than a
/// silent `0`, because a zero would fake a perfect pruning ratio.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the response isn't JSON, is a Loki error
/// response (surfaces Loki's `errorType` / `error`), or carries no
/// numeric `data.stats.summary.totalBytesProcessed`.
pub fn parse_loki_bytes_processed(response_json: &str) -> Result<u64, BenchError> {
    let root = parse_loki_root(response_json)?;
    root.get("data")
        .and_then(|d| d.get("stats"))
        .and_then(|s| s.get("summary"))
        .and_then(|s| s.get("totalBytesProcessed"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| BenchError::Pipeline {
            detail: "Loki response missing numeric `data.stats.summary.totalBytesProcessed` \
                     — refusing to record 0 bytes for a query that ran"
                .to_string(),
        })
}

/// Loki's **storage-side** byte figures for one query — the
/// apples-to-apples counterpart of [`OuriosAnswer::bytes_read`] (which
/// counts compressed Parquet bytes fetched from storage).
///
/// `summary.totalBytesProcessed` counts **decompressed** bytes the query
/// engine processed, which overstates Loki's storage reads by the chunk
/// compression ratio. The stats tree's per-section chunk figures carry
/// the storage-side view; both are reported so the recorded ratio is the
/// conservative, defensible one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LokiFetchedBytes {
    /// Sum of every `compressedBytes` in the stats tree (querier +
    /// ingester store sections): compressed chunk bytes the query
    /// touched — the closest analog of bytes fetched from storage.
    pub compressed_bytes: u64,
    /// Sum of every `headChunkBytes`: bytes served from the ingester's
    /// in-memory (uncompressed, never-fetched) head chunks. Reported
    /// separately — memory-served bytes are not storage reads, but
    /// ignoring them entirely would understate Loki's data touched when
    /// a query is served mostly from the head.
    pub head_chunk_bytes: u64,
}

/// Parse Loki's storage-side byte figures from a `query_range` response
/// by summing every `compressedBytes` / `headChunkBytes` field under
/// `data.stats` — recursive, so it is resilient to which sections
/// (querier / ingester, store / head) the figures land in across Loki
/// versions. Zero values are legitimate here (a query served purely from
/// head chunks has no compressed reads); a missing stats block is not.
///
/// # Errors
///
/// [`BenchError::Pipeline`] if the response isn't JSON, is a Loki error
/// response (surfaces Loki's `errorType` / `error`), or has no
/// `data.stats` object at all.
pub fn parse_loki_fetched_bytes(response_json: &str) -> Result<LokiFetchedBytes, BenchError> {
    let root = parse_loki_root(response_json)?;
    let stats = root
        .get("data")
        .and_then(|d| d.get("stats"))
        .filter(|s| s.is_object())
        .ok_or_else(|| BenchError::Pipeline {
            detail: "Loki response missing `data.stats` — refusing to record 0 storage \
                     bytes for a query that ran"
                .to_string(),
        })?;
    let mut fetched = LokiFetchedBytes {
        compressed_bytes: 0,
        head_chunk_bytes: 0,
    };
    sum_stats_fields(stats, &mut fetched);
    Ok(fetched)
}

/// Recursively sum `compressedBytes` / `headChunkBytes` leaves under a
/// Loki stats subtree.
fn sum_stats_fields(node: &serde_json::Value, acc: &mut LokiFetchedBytes) {
    let Some(map) = node.as_object() else {
        return;
    };
    for (key, value) in map {
        match (key.as_str(), value.as_u64()) {
            ("compressedBytes", Some(n)) => acc.compressed_bytes += n,
            ("headChunkBytes", Some(n)) => acc.head_chunk_bytes += n,
            _ => sum_stats_fields(value, acc),
        }
    }
}

/// Parse a Loki response to its JSON root, surfacing a Loki **error**
/// response (`status == "error"`) as Loki's own diagnostic — shared by
/// [`parse_loki_streams`] and [`parse_loki_bytes_processed`] so an error
/// body never reads as a confusing structural parse failure.
fn parse_loki_root(response_json: &str) -> Result<serde_json::Value, BenchError> {
    let root: serde_json::Value =
        serde_json::from_str(response_json).map_err(|e| BenchError::Pipeline {
            detail: format!("Loki response is not JSON: {e}"),
        })?;
    if root.get("status").and_then(serde_json::Value::as_str) == Some("error") {
        let error_type = root
            .get("errorType")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let error = root
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no message)");
        return Err(BenchError::Pipeline {
            detail: format!("Loki query error [{error_type}]: {error}"),
        });
    }
    Ok(root)
}

/// The trace three fixture records share — two in [`FIXTURE_SERVICE`],
/// one in [`FIXTURE_SERVICE_B`], so it spans service streams — the
/// RFC0031.1 L3 (trace-correlation) equivalence arm queries for exactly
/// these three lines on both systems.
pub const FIXTURE_TRACE: &str = "00112233445566778899aabbccddeeff";

/// The `service.name` most comparative-fixture records carry — the
/// resource identity both systems key on (Ourios: promoted service
/// column; Loki: the `service_name` stream label its OTLP ingest derives).
pub const FIXTURE_SERVICE: &str = "comparative-fixture";

/// The second fixture service: one [`FIXTURE_TRACE`] record lives here so
/// the shared trace genuinely SPANS services — the L3 equivalence arm
/// then fails if the `LogQL` side is ever accidentally narrowed to a
/// single stream.
pub const FIXTURE_SERVICE_B: &str = "comparative-fixture-b";

/// One comparative-fixture log record: the **single source of truth** for
/// the RFC0031.1 equivalence check. The same records are rendered as the
/// Ourios corpus (OTLP/JSON Lines via [`fixture_jsonl`]) and as the Loki
/// OTLP payload (the [`fixture_logs_data`] wire shape), so the two
/// systems ingest byte-identical `(timestamp, body)` pairs and
/// [`LineKey`]s align by construction.
#[derive(Debug, Clone)]
pub struct FixtureRecord {
    /// Wire `time_unix_nano` (both systems' returned timestamp).
    pub time_unix_nano: u64,
    /// OTLP severity number (1–24).
    pub severity_number: i32,
    /// OTLP severity text.
    pub severity_text: &'static str,
    /// The log line (string body).
    pub body: &'static str,
    /// Optional trace context, as 32 hex digits (the DSL's `trace_id`
    /// literal shape). `None` leaves the wire field empty — the common
    /// case for uninstrumented lines.
    pub trace_id: Option<&'static str>,
    /// The record's `service.name` (its `ResourceLogs` stream identity).
    pub service: &'static str,
}

/// The deterministic comparative fixture, timestamped from `base_ns`.
///
/// `base_ns` is a parameter — not a baked-in constant — because Loki's
/// default `reject_old_samples` refuses lines older than its window, so
/// the container test stamps the fixture near *now*, while local tests
/// may use any base.
#[must_use]
pub fn comparative_fixture(base_ns: u64) -> Vec<FixtureRecord> {
    // Distinct timestamps (LineKey identity is (timestamp, body)) and a
    // severity mix so severity-filtered pairs have selective results.
    // Three records share FIXTURE_TRACE — two in FIXTURE_SERVICE, one in
    // FIXTURE_SERVICE_B, so the trace genuinely spans service streams
    // (the L3 arm's structural point) — while one record carries a
    // different trace the L3 arm must NOT return. The service-B record
    // mirrors the ERROR band so the severity picker's deterministic
    // service tiebreak still selects FIXTURE_SERVICE.
    [
        (
            0,
            9,
            "INFO",
            "user 1 logged in",
            Some(FIXTURE_TRACE),
            FIXTURE_SERVICE,
        ),
        (
            1_000,
            9,
            "INFO",
            "user 2 logged in",
            Some("ffeeddccbbaa99887766554433221100"),
            FIXTURE_SERVICE,
        ),
        (
            2_000,
            17,
            "ERROR",
            "payment 7 failed",
            Some(FIXTURE_TRACE),
            FIXTURE_SERVICE,
        ),
        (
            3_000,
            17,
            "ERROR",
            "payment 7 retried",
            Some(FIXTURE_TRACE),
            FIXTURE_SERVICE_B,
        ),
    ]
    .into_iter()
    .map(|(off, num, text, body, trace_id, service)| FixtureRecord {
        time_unix_nano: base_ns + off,
        severity_number: num,
        severity_text: text,
        body,
        trace_id,
        service,
    })
    .collect()
}

/// The fixture as the OTLP `LogsData` wire shape: one `ResourceLogs`
/// per distinct `service.name` (in first-appearance order), one scope
/// each, one `LogRecord` per fixture record. The Ourios corpus line and
/// the Loki OTLP push are both derived from this one value.
#[must_use]
pub fn fixture_logs_data(records: &[FixtureRecord]) -> LogsData {
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    // One ResourceLogs per distinct service, in first-appearance order —
    // deterministic wire shape, and a multi-service fixture lands as the
    // multiple streams the L3 arm exists to cross.
    let mut by_service: Vec<(&str, Vec<LogRecord>)> = Vec::new();
    for r in records {
        let record = LogRecord {
            time_unix_nano: r.time_unix_nano,
            severity_number: r.severity_number,
            severity_text: r.severity_text.to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(r.body.to_string())),
            }),
            trace_id: r.trace_id.map(hex_to_bytes).unwrap_or_default(),
            ..LogRecord::default()
        };
        match by_service.iter_mut().find(|(svc, _)| *svc == r.service) {
            Some((_, records)) => records.push(record),
            None => by_service.push((r.service, vec![record])),
        }
    }
    LogsData {
        resource_logs: by_service
            .into_iter()
            .map(|(service, log_records)| ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(service.to_string())),
                        }),
                        ..KeyValue::default()
                    }],
                    ..Resource::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records,
                    ..ScopeLogs::default()
                }],
                ..ResourceLogs::default()
            })
            .collect(),
    }
}

/// Fixture-side hex decode (exactly 32 hex digits → 16 bytes). Panics on a bad
/// literal — fixture constants are compile-time authored, so a typo
/// should fail the test loudly, not ship a silently-empty trace.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert!(
        hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        "fixture trace_id {hex:?} is not exactly 32 hex digits",
    );
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked hex digits"))
        .collect()
}

/// The fixture rendered as one OTLP/JSON Lines corpus line — the format
/// `corpus::ingest_otlp_jsonl` parses (`serde_json` against the same
/// `with-serde` `LogsData` type, so it round-trips by construction).
///
/// # Errors
///
/// [`BenchError::Pipeline`] if serde serialization fails (structurally
/// impossible for these types; surfaced rather than unwrapped).
pub fn fixture_jsonl(records: &[FixtureRecord]) -> Result<String, BenchError> {
    serde_json::to_string(&fixture_logs_data(records)).map_err(|e| BenchError::Pipeline {
        detail: format!("fixture LogsData serialization: {e}"),
    })
}

/// The [`LineKey`]s both systems must return for a query matching every
/// fixture record — the expected value of the equivalence check.
#[must_use]
pub fn fixture_line_keys(records: &[FixtureRecord]) -> Vec<LineKey> {
    records
        .iter()
        .map(|r| LineKey {
            timestamp_unix_nanos: r.time_unix_nano,
            body: r.body.as_bytes().to_vec(),
        })
        .collect()
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
    fn parse_loki_streams_keys_compatibly_with_the_ourios_side() {
        // A synthetic Loki `query_range` streams response — three lines
        // across two streams.
        let response = r#"{
            "status": "success",
            "data": {
                "resultType": "streams",
                "result": [
                    { "stream": {"service_name": "a"}, "values": [
                        ["1775127480000000000", "user 1 logged in"],
                        ["1775127480000000001", "user 2 logged in"]
                    ]},
                    { "stream": {"service_name": "b"}, "values": [
                        ["1775127480000000002", "user 3 logged in"]
                    ]}
                ]
            }
        }"#;
        let loki = parse_loki_streams(response).expect("parse loki streams");
        assert_eq!(loki.len(), 3);

        // The parsed keys must be byte-for-byte what the Ourios side would
        // produce for the same lines — otherwise the equivalence check is
        // comparing incompatibly-keyed sets.
        let ourios = vec![
            LineKey {
                timestamp_unix_nanos: 1_775_127_480_000_000_000,
                body: b"user 1 logged in".to_vec(),
            },
            LineKey {
                timestamp_unix_nanos: 1_775_127_480_000_000_001,
                body: b"user 2 logged in".to_vec(),
            },
            LineKey {
                timestamp_unix_nanos: 1_775_127_480_000_000_002,
                body: b"user 3 logged in".to_vec(),
            },
        ];
        assert!(
            compare_lines(&ourios, &loki, 8).is_equal(),
            "Loki-parsed lines must key-match the Ourios side",
        );
    }

    #[test]
    fn parse_loki_streams_rejects_malformed_responses() {
        // Missing data.result, a non-pair value, and a non-numeric
        // timestamp all error rather than silently drop rows.
        assert!(parse_loki_streams(r#"{"status":"success"}"#).is_err());
        assert!(
            parse_loki_streams(r#"{"data":{"result":[{"values":[["1","a","extra"]]}]}}"#).is_err()
        );
        assert!(
            parse_loki_streams(r#"{"data":{"result":[{"values":[["notanum","a"]]}]}}"#).is_err()
        );

        // A Loki error response surfaces Loki's own diagnostic, not the
        // misleading "missing data.result".
        let err = parse_loki_streams(
            r#"{"status":"error","errorType":"parse error","error":"unexpected token"}"#,
        )
        .expect_err("Loki error response must error");
        let BenchError::Pipeline { detail } = err else {
            panic!("expected a pipeline error");
        };
        assert!(detail.contains("parse error"), "{detail}");
        assert!(detail.contains("unexpected token"), "{detail}");
    }

    #[test]
    fn fixture_round_trips_through_the_ourios_side() {
        // The single-source-of-truth proof, Ourios half: the fixture
        // rendered as an OTLP/JSON Lines corpus, ingested through the
        // registry-bearing comparative store and queried in-process, comes
        // back as exactly `fixture_line_keys` — the same expected keys the
        // Loki container run is compared against. Local, no container.
        let base_ns = crate::corpus::TIME_BASELINE_NS;
        let records = comparative_fixture(base_ns);

        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("fixture.jsonl"),
            fixture_jsonl(&records).expect("fixture jsonl"),
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built =
            crate::build_comparative_store(corpus.path(), bucket.path(), crate::TxtSeverity::Fixed)
                .expect("build store");
        assert_eq!(built.rows, 4, "one stored row per fixture record");

        let tenant = TenantId::new(built.tenant);
        let now = built.max_effective_time_unix_nano + 1;
        let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
        let answer = ourios_query_answer(
            bucket.path(),
            &tenant,
            "severity >= 0 | limit 1000",
            now,
            window,
        )
        .expect("query answer");

        assert!(
            compare_lines(&answer.lines, &fixture_line_keys(&records), 8).is_equal(),
            "Ourios round-trip of the fixture must equal the expected LineKeys",
        );
        // The measurement channel: answering the query read real bytes from
        // storage (the RFC 0031 §3.6 primary gate metric must never be a
        // silent 0 for a query that scanned data).
        assert!(
            answer.bytes_read > 0,
            "bytes_read must be non-zero for a query that scanned the store",
        );
    }

    #[test]
    fn honest_total_bytes_breaks_down_additively() {
        // The §3.6 measurement-fidelity amendment (2026-07-12): the figure
        // the L-gates ratio is the TOTAL bytes fetched to deliver the answer
        // — count scan + row materialization + template-map acquisition.
        // Since the single-pass amendment the harness elides the count scan
        // whenever the result is complete (which the completeness guard
        // requires anyway), so on this path the count-scan component is an
        // honest 0 — the query never read those bytes. The registry-bearing
        // comparative store makes the other two components real: rendered
        // rows force the materialization scan, and body reconstruction
        // forces the RFC 0017 audit-stream read.
        let records = comparative_fixture(crate::corpus::TIME_BASELINE_NS);
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("fixture.jsonl"),
            fixture_jsonl(&records).expect("fixture jsonl"),
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");
        let built =
            crate::build_comparative_store(corpus.path(), bucket.path(), crate::TxtSeverity::Fixed)
                .expect("build store");

        let tenant = TenantId::new(built.tenant);
        let now = built.max_effective_time_unix_nano + 1;
        let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
        let answer = ourios_query_answer(
            bucket.path(),
            &tenant,
            "severity >= 0 | limit 1000",
            now,
            window,
        )
        .expect("query answer");
        assert_eq!(answer.lines.len(), 4, "every fixture row rendered");

        assert_eq!(
            answer.bytes_read,
            answer.count_scan_bytes + answer.materialize_bytes + answer.registry_bytes,
            "bytes_read is exactly the sum of its three components",
        );
        assert_eq!(
            answer.count_scan_bytes, 0,
            "4 matches < limit 1000 ⇒ the result is complete ⇒ the count \
             scan was elided and its component is an honest 0",
        );
        assert!(
            answer.materialize_bytes > 0,
            "rendering rows reads the data files — it must be counted",
        );
        assert!(
            answer.registry_bytes > 0,
            "body reconstruction reads the audit stream — it must be counted",
        );

        // The exact-limit edge: 4 matches with `limit 4` looks truncated
        // (returned == limit), so the querier must fall back to the count
        // scan to prove completeness — the component is real bytes again,
        // and the completeness guard still passes because rows == rendered.
        let exact = ourios_query_answer(
            bucket.path(),
            &tenant,
            "severity >= 0 | limit 4",
            now,
            window,
        )
        .expect("exact-limit query answer");
        assert_eq!(exact.lines.len(), 4, "still the complete result");
        assert!(
            exact.count_scan_bytes > 0,
            "returned == limit ⇒ fell back to the count scan",
        );
        assert_eq!(
            exact.bytes_read,
            exact.count_scan_bytes + exact.materialize_bytes + exact.registry_bytes,
            "the fallback path sums the same three components",
        );
    }

    #[test]
    fn parse_loki_bytes_processed_reads_the_summary() {
        let response = r#"{
            "status": "success",
            "data": {
                "resultType": "streams",
                "result": [],
                "stats": { "summary": { "totalBytesProcessed": 4096 } }
            }
        }"#;
        assert_eq!(
            parse_loki_bytes_processed(response).expect("parse bytes"),
            4096,
        );
    }

    #[test]
    fn parse_loki_fetched_bytes_sums_across_sections() {
        // compressedBytes/headChunkBytes are summed wherever they appear
        // in the stats tree (querier + ingester, store + head).
        let response = r#"{
            "status": "success",
            "data": {
                "result": [],
                "stats": {
                    "summary": { "totalBytesProcessed": 89550249 },
                    "querier": { "store": { "chunk": {
                        "compressedBytes": 1000, "headChunkBytes": 10,
                        "decompressedBytes": 8000
                    }}},
                    "ingester": { "store": { "chunk": {
                        "compressedBytes": 2000, "headChunkBytes": 30
                    }}}
                }
            }
        }"#;
        let fetched = parse_loki_fetched_bytes(response).expect("parse fetched");
        assert_eq!(fetched.compressed_bytes, 3000);
        assert_eq!(fetched.head_chunk_bytes, 40);

        // All-head-chunk service is legitimate zeros...
        let head_only = r#"{"data":{"result":[],"stats":{"summary":{}}}}"#;
        let fetched = parse_loki_fetched_bytes(head_only).expect("empty stats parse");
        assert_eq!((fetched.compressed_bytes, fetched.head_chunk_bytes), (0, 0));

        // ...but a missing or non-object stats block is an error, same
        // honesty rule as the processed-bytes parser.
        assert!(parse_loki_fetched_bytes(r#"{"data":{"result":[]}}"#).is_err());
        assert!(parse_loki_fetched_bytes(r#"{"data":{"result":[],"stats":null}}"#).is_err());
    }

    #[test]
    fn parse_loki_bytes_processed_refuses_a_missing_summary() {
        // Absent stats must be an error, not a silent 0 — a zero would fake
        // a perfect pruning ratio in the L-gates.
        let no_stats = r#"{"status":"success","data":{"result":[]}}"#;
        assert!(parse_loki_bytes_processed(no_stats).is_err());

        // And a Loki error response surfaces Loki's own diagnostic.
        let err = parse_loki_bytes_processed(
            r#"{"status":"error","errorType":"too_many_requests","error":"throttled"}"#,
        )
        .expect_err("error response must error");
        let BenchError::Pipeline { detail } = err else {
            panic!("expected a pipeline error");
        };
        assert!(detail.contains("throttled"), "{detail}");
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
        // Sort the expected lines too: extraction order is unspecified, so
        // the comparison must be order-independent (not rely on the fixture
        // happening to be lexicographically ordered).
        let mut expected: Vec<&str> = lines.to_vec();
        expected.sort_unstable();
        assert_eq!(
            got, expected,
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

    /// Hour width and shape of the late-materialization corpus, shared
    /// between the generator and the test's expected-row computation.
    const LM_HOUR_NS: u64 = 3_600_000_000_000;
    const LM_HOT_ROWS: u64 = 60_000;
    const LM_FILLER_ROWS: u64 = 5_000;
    const LM_ERROR_INDEX: u64 = 13_337;

    /// The per-record pseudo-random ids (an LCG keeps the corpus
    /// deterministic and dependency-free) whose entropy makes the heavy
    /// columns compress poorly, the way real request logs do.
    fn synthetic_ids(n: u64) -> (u64, u64) {
        let a = n
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let b = a.wrapping_mul(6_364_136_223_846_793_005) >> 32;
        (a, b)
    }

    fn synthetic_body(n: u64, error: bool) -> String {
        let (a, b) = synthetic_ids(n);
        let verb = if error { "failed" } else { "handled" };
        format!("{verb} request {a:016x} for user {b:08x} path /api/v1/orders/{n}")
    }

    /// One `LogRecord` of the late-materialization corpus.
    fn synthetic_record(
        time_unix_nano: u64,
        n: u64,
        error: bool,
    ) -> opentelemetry_proto::tonic::logs::v1::LogRecord {
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
        use opentelemetry_proto::tonic::logs::v1::LogRecord;

        let (a, b) = synthetic_ids(n);
        let (severity_number, severity_text) = if error { (17, "ERROR") } else { (9, "INFO") };
        let body = synthetic_body(n, error);
        let attr = |key: &str, value: String| KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value)),
            }),
            ..KeyValue::default()
        };
        LogRecord {
            time_unix_nano,
            severity_number,
            severity_text: severity_text.to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body)),
            }),
            attributes: vec![
                attr("http.request.id", format!("{a:016x}")),
                attr("session.id", format!("{a:016x}{b:016x}")),
                attr("url.path", format!("/api/v1/orders/{n}/items/{b:x}")),
                attr(
                    "client.address",
                    format!("10.{}.{}.{}", a % 256, b % 256, n % 256),
                ),
                attr(
                    "user_agent.original",
                    format!("Mozilla/5.0 (build {b:08x}) ourios-bench/{}", n % 97),
                ),
            ],
            ..LogRecord::default()
        }
    }

    /// The synthetic multi-hour store the late-materialization regression
    /// measures: a hot hour of 60 000 rows holding exactly one ERROR row,
    /// plus two 5 000-row INFO-only filler hours. Hour-aligned so the hot
    /// hour lands in a single partition; sized past parquet-rs's 20 000-row
    /// default page limit so each hot-hour column chunk spans several pages
    /// (page-granular reads need >1 page per chunk to be observable).
    fn late_materialization_corpus() -> String {
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
        use opentelemetry_proto::tonic::logs::v1::{ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        let base_ns = (crate::corpus::TIME_BASELINE_NS / LM_HOUR_NS) * LM_HOUR_NS;

        let mut lines = Vec::new();
        for (hour, rows) in [
            (0u64, LM_HOT_ROWS),
            (1, LM_FILLER_ROWS),
            (2, LM_FILLER_ROWS),
        ] {
            let hour_base = base_ns + hour * LM_HOUR_NS;
            let spacing = LM_HOUR_NS / (rows + 1);
            let log_records = (0..rows)
                .map(|i| {
                    let error = hour == 0 && i == LM_ERROR_INDEX;
                    synthetic_record(hour_base + i * spacing, hour * 1_000_000 + i, error)
                })
                .collect();
            let logs = LogsData {
                resource_logs: vec![ResourceLogs {
                    resource: Some(Resource {
                        attributes: vec![KeyValue {
                            key: "service.name".to_string(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::StringValue(
                                    FIXTURE_SERVICE.to_string(),
                                )),
                            }),
                            ..KeyValue::default()
                        }],
                        ..Resource::default()
                    }),
                    scope_logs: vec![ScopeLogs {
                        log_records,
                        ..ScopeLogs::default()
                    }],
                    ..ResourceLogs::default()
                }],
            };
            lines.push(serde_json::to_string(&logs).expect("synthetic LogsData serializes"));
        }
        lines.join("\n")
    }

    /// The late-materialization regression guard (RFC 0031 run #8
    /// decomposition): rendering even ONE matching row used to re-read the
    /// hot partition's page-index-matched window of every column chunk
    /// whole — with filter pushdown on the querier's scans, the
    /// materialize pass fetches only the pages the selected row needs.
    ///
    /// Empirical basis for the ceiling (this exact store, measured
    /// 2026-07-12, `DataFusion` 54 / parquet 58): the pre-pushdown
    /// whole-window materialize scan read 1 664 654 B; the page-selective
    /// scan reads 742 036 B. The 1 MiB ceiling sits between with
    /// comfortable margin both ways — the old behavior violates it by
    /// ~1.6×, the new behavior clears it with ~1.4× headroom. The store is
    /// fully deterministic (LCG corpus, fixed timestamps), so drift toward
    /// the ceiling means a real regression in scan selectivity, not noise.
    #[test]
    fn one_row_materialization_reads_pages_not_whole_chunks() {
        const MATERIALIZE_CEILING_BYTES: u64 = 1_048_576;

        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("synthetic.jsonl"),
            late_materialization_corpus(),
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");
        let built =
            crate::build_comparative_store(corpus.path(), bucket.path(), crate::TxtSeverity::Fixed)
                .expect("build store");
        assert_eq!(
            built.rows,
            LM_HOT_ROWS + 2 * LM_FILLER_ROWS,
            "the whole synthetic corpus is stored",
        );

        let tenant = TenantId::new(built.tenant);
        let now = built.max_effective_time_unix_nano + 1;
        let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
        let answer = ourios_query_answer(
            bucket.path(),
            &tenant,
            "severity >= 17 | limit 10",
            now,
            window,
        )
        .expect("query answer");

        // Correctness first: pushdown must not change the answer. The one
        // ERROR row comes back with its exact timestamp and bit-identical
        // reconstructed body.
        let base_ns = (crate::corpus::TIME_BASELINE_NS / LM_HOUR_NS) * LM_HOUR_NS;
        let expected = LineKey {
            timestamp_unix_nanos: base_ns + LM_ERROR_INDEX * (LM_HOUR_NS / (LM_HOT_ROWS + 1)),
            body: synthetic_body(LM_ERROR_INDEX, true).into_bytes(),
        };
        assert_eq!(
            answer.lines,
            vec![expected],
            "exactly the one ERROR row matches, rendered bit-identically",
        );

        assert!(
            answer.materialize_bytes < MATERIALIZE_CEILING_BYTES,
            "materializing 1 row must read pages, not the whole page-index \
             window of every chunk: materialize={} (ceiling {}, count_scan={}, \
             registry={})",
            answer.materialize_bytes,
            MATERIALIZE_CEILING_BYTES,
            answer.count_scan_bytes,
            answer.registry_bytes,
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

    #[test]
    fn rfc3339_to_unix_nanos_parses_and_rejects() {
        assert_eq!(
            rfc3339_to_unix_nanos("1970-01-01T00:00:01Z").unwrap(),
            1_000_000_000,
        );
        assert_eq!(rfc3339_to_unix_nanos("1970-01-01T00:00:00Z").unwrap(), 0);
        assert!(rfc3339_to_unix_nanos("not a timestamp").is_err());
    }

    #[test]
    fn ourios_aggregate_answer_decodes_the_group_map() {
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        // One template ("connection established to peer <id>" — the same
        // ≥10-char constant run the L1 picker validates elsewhere), two
        // distinct trailing ids across two 1-second buckets:
        // TIME_BASELINE_NS lands exactly on a whole-second boundary, so
        // the expected bucket starts are hand-computable.
        let base_ns = crate::corpus::TIME_BASELINE_NS;
        let record = |offset_ns: u64, body: &str| LogRecord {
            time_unix_nano: base_ns + offset_ns,
            severity_number: 9,
            severity_text: "INFO".to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body.to_string())),
            }),
            ..LogRecord::default()
        };
        let logs = LogsData {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(FIXTURE_SERVICE.to_string())),
                        }),
                        ..KeyValue::default()
                    }],
                    ..Resource::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![
                        record(0, "connection established to peer 10"),
                        record(1_000_000_000, "connection established to peer 10"),
                        record(1_000_000_000, "connection established to peer 11"),
                    ],
                    ..ScopeLogs::default()
                }],
                ..ResourceLogs::default()
            }],
        };
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("agg.jsonl"),
            serde_json::to_string(&logs).expect("serialize LogsData"),
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");
        let built =
            crate::build_comparative_store(corpus.path(), bucket.path(), crate::TxtSeverity::Fixed)
                .expect("build store");
        let tenant = TenantId::new(built.tenant);
        let now = built.max_effective_time_unix_nano + 1;
        let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

        let querier = ourios_querier::Querier::new(bucket.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let registry = runtime
            .block_on(querier.template_registry(&tenant))
            .expect("derive template registry");
        let template_id = registry
            .keys()
            .map(|&(id, _)| id)
            .find(|&id| id != ourios_miner::cluster::NO_TEMPLATE)
            .expect("exactly one mined template");

        let dsl = format!("template_id == {template_id} | count by param(0), bucket(1s)");
        let answer = ourios_aggregate_answer(bucket.path(), &tenant, &dsl, now, window)
            .expect("aggregate answer");

        assert_eq!(
            answer.groups,
            HashMap::from([
                (agg(base_ns, "10"), 1),
                (agg(base_ns + 1_000_000_000, "10"), 1),
                (agg(base_ns + 1_000_000_000, "11"), 1),
            ]),
        );
        assert!(
            answer.bytes_read > 0,
            "the grouped-count scan reads real bytes from storage",
        );
    }

    #[test]
    fn ourios_aggregate_answer_rejects_a_non_aggregating_dsl() {
        let records = comparative_fixture(crate::corpus::TIME_BASELINE_NS);
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("fixture.jsonl"),
            fixture_jsonl(&records).expect("fixture jsonl"),
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");
        let built =
            crate::build_comparative_store(corpus.path(), bucket.path(), crate::TxtSeverity::Fixed)
                .expect("build store");
        let tenant = TenantId::new(built.tenant);
        let now = built.max_effective_time_unix_nano + 1;
        let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

        assert!(
            ourios_aggregate_answer(
                bucket.path(),
                &tenant,
                "severity >= 0 | limit 10",
                now,
                window
            )
            .is_err(),
            "a query with no `count by …` stage must not silently report an empty aggregate",
        );
    }

    #[test]
    fn parse_loki_matrix_decodes_bucket_aligned_samples() {
        // Bucket alignment (RFC 0031 §7's L4 open question): a 1s bucket
        // width, evaluated at t = (k+1)·w for k = 0 and k = 2, decodes
        // to bucket starts 0 and 2_000_000_000 — exactly `t - w`.
        let response = r#"{
            "status": "success",
            "data": {
                "resultType": "matrix",
                "result": [
                    { "metric": {"value": "10"}, "values": [[1, "1"], [3, "1"]] },
                    { "metric": {"value": "11"}, "values": [[1, "2"]] }
                ]
            }
        }"#;
        let groups = parse_loki_matrix(response, "value", 1_000_000_000).expect("parse matrix");
        assert_eq!(
            groups,
            HashMap::from([
                (agg(0, "10"), 1),
                (agg(2_000_000_000, "10"), 1),
                (agg(0, "11"), 2),
            ]),
        );
    }

    #[test]
    fn parse_loki_matrix_rejects_non_matrix_result_types() {
        let response = r#"{"status":"success","data":{"resultType":"streams","result":[]}}"#;
        assert!(parse_loki_matrix(response, "value", 1_000_000_000).is_err());
    }

    #[test]
    fn parse_loki_matrix_rejects_a_missing_label() {
        let response = r#"{"status":"success","data":{"resultType":"matrix",
            "result":[{"metric":{},"values":[[1,"1"]]}]}}"#;
        assert!(parse_loki_matrix(response, "value", 1_000_000_000).is_err());
    }

    #[test]
    fn parse_loki_matrix_rejects_sub_second_and_pre_width_samples() {
        // A fractional-second sample means a misaligned query (this
        // harness never emits a sub-second bucket width), not lost
        // precision — refuse rather than truncate.
        let fractional = r#"{"status":"success","data":{"resultType":"matrix",
            "result":[{"metric":{"value":"10"},"values":[[1.5,"1"]]}]}}"#;
        assert!(parse_loki_matrix(fractional, "value", 1_000_000_000).is_err());

        // t = 0 with a 1s bucket width underflows the bucket start.
        let too_early = r#"{"status":"success","data":{"resultType":"matrix",
            "result":[{"metric":{"value":"10"},"values":[[0,"1"]]}]}}"#;
        assert!(parse_loki_matrix(too_early, "value", 1_000_000_000).is_err());
    }

    #[test]
    fn parse_loki_matrix_surfaces_a_loki_error_response() {
        let err = parse_loki_matrix(
            r#"{"status":"error","errorType":"parse error","error":"unexpected token"}"#,
            "value",
            1_000_000_000,
        )
        .expect_err("a Loki error response must error");
        let BenchError::Pipeline { detail } = err else {
            panic!("expected a pipeline error");
        };
        assert!(detail.contains("unexpected token"), "{detail}");
    }
}
