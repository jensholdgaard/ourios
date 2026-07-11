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
/// [`LineKey`]s plus the **bytes read from storage** to answer it — the
/// RFC 0031 §3.6 primary gate metric (`QueryStats::bytes_read`, folded
/// from the engine's `bytes_scanned` scan metric on the RFC 0016 path).
#[derive(Debug, Clone)]
pub struct OuriosAnswer {
    /// The matching rows, keyed for [`compare_lines`].
    pub lines: Vec<LineKey>,
    /// Bytes read from object storage to answer the query — the
    /// measurement the L-gates ratio against Loki's
    /// `totalBytesProcessed` ([`parse_loki_bytes_processed`]).
    pub bytes_read: u64,
}

/// Run a logs-DSL query against the Ourios store at `bucket_root` and
/// return the matching rows **and** the bytes-read measurement — the
/// Ourios half of both the RFC0031.1 equivalence check and the
/// RFC0031.2–.5 bytes-read gates.
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
    let bytes_read = result.stats.bytes_read;
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
        .map(|lines| OuriosAnswer { lines, bytes_read })
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

/// The `service.name` every comparative-fixture record carries — the
/// resource identity both systems key on (Ourios: promoted service
/// column; Loki: the `service_name` stream label its OTLP ingest derives).
pub const FIXTURE_SERVICE: &str = "comparative-fixture";

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
    [
        (0, 9, "INFO", "user 1 logged in"),
        (1_000, 9, "INFO", "user 2 logged in"),
        (2_000, 17, "ERROR", "payment 7 failed"),
    ]
    .into_iter()
    .map(|(off, num, text, body)| FixtureRecord {
        time_unix_nano: base_ns + off,
        severity_number: num,
        severity_text: text,
        body,
    })
    .collect()
}

/// The fixture as the OTLP `LogsData` wire shape: one resource whose
/// `service.name` is [`FIXTURE_SERVICE`], one scope, one `LogRecord` per
/// fixture record. The Ourios corpus line and the Loki OTLP push are both
/// derived from this one value.
#[must_use]
pub fn fixture_logs_data(records: &[FixtureRecord]) -> LogsData {
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    let log_records = records
        .iter()
        .map(|r| LogRecord {
            time_unix_nano: r.time_unix_nano,
            severity_number: r.severity_number,
            severity_text: r.severity_text.to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(r.body.to_string())),
            }),
            ..LogRecord::default()
        })
        .collect();
    LogsData {
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
                log_records,
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
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
        assert_eq!(built.rows, 3, "one stored row per fixture record");

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
