//! Corpus scanners + pair pickers (selective / trace / template /
//! frequency / windows) and the shared corpus fixtures.

use crate::*;

/// The repo root, resolved from the crate dir (the `rfc0024_calibration`
/// pattern) so the docs-presence scenario is cwd-independent.
pub(crate) fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// The dynamically-picked first query pair: a `(service, severity
/// threshold, severity text)` whose rows form a small, exactly-equivalent
/// result set on both systems.
#[derive(Debug)]
pub(crate) struct SelectivePair {
    pub(crate) service: String,
    /// The DSL threshold: the pair selects rows with
    /// `severity_number ≥ threshold`.
    pub(crate) threshold: i32,
    /// The single `severity_text` those rows all carry (the `LogQL` side of
    /// the pair) — the picker's text-consistency guarantee.
    pub(crate) text: String,
    /// How many rows the pair selects.
    pub(crate) rows: u64,
    /// Corpus record count (for the report).
    pub(crate) total_records: u64,
    /// Corpus `time_unix_nano` span (the Loki query window).
    pub(crate) min_ts: u64,
    pub(crate) max_ts: u64,
}

/// Scan an OTLP/JSON Lines corpus and pick the query pair for the
/// indicative run: a `(service, threshold T, text t)` where **every** row
/// of the service with `severity_number ≥ T` carries the single
/// `severity_text == t`, and their count is `1..=4000` (under Loki's
/// 5000-line query cap, so the complete result fits one page). The
/// consistency requirement makes DSL `severity >= T` and `LogQL`
/// `severity_text="t"` express the same question, so the equivalence
/// check is meaningful. Zero-`time_unix_nano` rows are tallied
/// separately as POISON bands and disqualify any candidate whose
/// predicate could select them (see [`select_pair_candidates`]).
/// Generalised from a hardcoded ERROR band because
/// real captures vary — otel-demo v8 carries no ERROR logs at all (its
/// failure flags surface in traces/metrics), only INFO/Information and
/// four WARNs. Candidate thresholds are the service's **observed**
/// severity numbers — which is complete, because a gap threshold (say 16
/// when only 17 occurs) selects exactly the same rows as the next
/// observed number above it, adding no new candidates. Picks the FEWEST
/// rows; ties break to the lowest threshold then the lexicographically
/// smallest service, for deterministic reruns.
pub(crate) fn pick_selective_pair(corpus_dir: &std::path::Path) -> SelectivePair {
    use std::collections::HashMap;
    use std::io::BufRead as _;

    // service -> (clean bands, poison bands)
    let mut per_service: HashMap<String, (SeverityBands, SeverityBands)> = HashMap::new();
    let (mut total, mut min_ts, mut max_ts) = (0u64, u64::MAX, 0u64);

    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .and_then(|v| match v {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                // One entry lookup per ResourceLogs group (moving the
                // service string in), not one clone per record — the scan
                // walks multi-million-record corpora. Severity texts are
                // likewise cloned only on their FIRST occurrence per
                // (service, number): get_mut hits the existing key for the
                // millions of repeats.
                let entry = per_service.entry(service).or_default();
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        total += 1;
                        // Zero-`time_unix_nano` records go into POISON
                        // bands, not candidate bands: both systems could
                        // still return them (Ourios windows the RFC 0005
                        // §3.2 EFFECTIVE timestamp, falling back to
                        // observed; Loki's OTLP ingest falls back to
                        // observed too), but with DIFFERENT answer
                        // timestamps — Ourios's row keeps time_unix_nano
                        // = 0, Loki stamps its stored (observed) time —
                        // so their LineKeys can never match and any
                        // candidate whose predicate could select such a
                        // row is a guaranteed equivalence mismatch.
                        let bands = if lr.time_unix_nano == 0 {
                            &mut entry.1
                        } else {
                            min_ts = min_ts.min(lr.time_unix_nano);
                            max_ts = max_ts.max(lr.time_unix_nano);
                            &mut entry.0
                        };
                        let texts = bands.entry(lr.severity_number).or_default();
                        if let Some(count) = texts.get_mut(lr.severity_text.as_str()) {
                            *count += 1;
                        } else {
                            texts.insert(lr.severity_text.clone(), 1);
                        }
                    }
                }
            }
        }
    }
    assert!(
        min_ts <= max_ts,
        "corpus has no record with a non-zero time_unix_nano — no query window derivable",
    );

    let mut candidates = select_pair_candidates(&per_service);
    candidates.sort();
    let Some(&(rows, threshold, service, text)) = candidates.first() else {
        panic!(
            "no (service, severity-threshold) with a single text and 1..=4000 rows; \
             per-service severity bands: {per_service:#?}"
        );
    };
    SelectivePair {
        service: service.clone(),
        threshold,
        text: text.clone(),
        rows,
        total_records: total,
        min_ts,
        max_ts,
    }
}

/// The corpus's `*.jsonl` files, sorted for deterministic scans.
pub(crate) fn corpus_jsonl_paths(corpus_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(corpus_dir)
        .expect("read corpus dir")
        .filter_map(|e| {
            let p = e.expect("dir entry").path();
            (p.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(p)
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no *.jsonl in {}", corpus_dir.display());
    paths
}

/// Second streaming pass over the corpus, for the selectivity-curve
/// window pairs: every log record of `service`, split into CLEAN
/// timestamps (non-zero `time_unix_nano` — rows both systems answer
/// with identical keys) and POISON timestamps (the observed-fallback
/// effective time of zero-`time_unix_nano` rows: both systems would
/// RETURN such a row if a window covered its fallback time, but with
/// DIFFERENT answer timestamps — Ourios keeps `time_unix_nano = 0`,
/// Loki stamps its stored observed time — so any window containing one
/// is a guaranteed equivalence mismatch). Both sorted ascending.
pub(crate) fn collect_service_timestamps(
    corpus_dir: &std::path::Path,
    service: &str,
) -> (Vec<u64>, Vec<u64>) {
    use std::io::BufRead as _;

    let (mut clean, mut poison) = (Vec::new(), Vec::new());
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let matches_service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .is_some_and(|v| {
                        matches!(
                            v,
                            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s)
                                if s == service
                        )
                    });
                if !matches_service {
                    continue;
                }
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        if lr.time_unix_nano != 0 {
                            clean.push(lr.time_unix_nano);
                        } else if lr.observed_time_unix_nano != 0 {
                            poison.push(lr.observed_time_unix_nano);
                        }
                    }
                }
            }
        }
    }
    clean.sort_unstable();
    poison.sort_unstable();
    (clean, poison)
}

/// Per-trace tally for [`pick_trace_pair`]. Allocation-free by design:
/// v8-scale corpora carry millions of mostly-singleton traces, so the
/// per-entry state holds an interned service id, not a `String`.
#[derive(Default)]
pub(crate) struct TraceTally {
    pub(crate) rows: u64,
    pub(crate) has_zero_ts: bool,
    pub(crate) has_empty_service: bool,
    pub(crate) first_service: Option<u32>,
    pub(crate) multi_service: bool,
}

/// Third streaming pass, for the L3 (trace-correlation) pair: tally
/// every 16-byte `trace_id` in the corpus and pick the one to query.
/// Eligibility mirrors the other pickers' honesty rules: no
/// zero-`time_unix_nano` row (the two systems would answer with
/// different keys — a guaranteed mismatch) and no empty-service row
/// (the `LogQL` side's `{service_name=~".+"}` selector could not see it),
/// with `2..=100` rows so the result is a meaningful multiset that fits
/// one page. Preference order: multi-service traces first (the class's
/// structural point — Loki cannot pre-narrow a cross-service trace to
/// one stream), then the most rows, then the lexicographically smallest
/// id for deterministic reruns. Returns `(hex_id, rows)`.
pub(crate) fn pick_trace_pair(corpus_dir: &std::path::Path) -> Option<(String, u64)> {
    use std::collections::HashMap;
    use std::io::BufRead as _;

    let mut traces: HashMap<[u8; 16], TraceTally> = HashMap::new();
    // Service names interned to u32 ids — one small map over the handful
    // of distinct services, zero per-trace allocations.
    let mut service_ids: HashMap<String, u32> = HashMap::new();
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .and_then(|v| match v {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                // get-then-insert (not entry) so the millions of repeat
                // ResourceLogs never allocate a lookup key.
                let service_id = if let Some(&id) = service_ids.get(service) {
                    id
                } else {
                    let id = u32::try_from(service_ids.len()).expect("service count fits u32");
                    service_ids.insert(service.to_string(), id);
                    id
                };
                let service_is_empty = service.is_empty();
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        let Ok(id) = <[u8; 16]>::try_from(lr.trace_id.as_slice()) else {
                            continue;
                        };
                        let tally = traces.entry(id).or_default();
                        tally.rows += 1;
                        tally.has_zero_ts |= lr.time_unix_nano == 0;
                        tally.has_empty_service |= service_is_empty;
                        match tally.first_service {
                            None => tally.first_service = Some(service_id),
                            Some(first) if first != service_id => tally.multi_service = true,
                            Some(_) => {}
                        }
                    }
                }
            }
        }
    }
    traces
        .into_iter()
        .filter(|(_, t)| !t.has_zero_ts && !t.has_empty_service && (2..=100).contains(&t.rows))
        .max_by(|(a_id, a), (b_id, b)| {
            (a.multi_service, a.rows, std::cmp::Reverse(a_id)).cmp(&(
                b.multi_service,
                b.rows,
                std::cmp::Reverse(b_id),
            ))
        })
        .map(|(id, t)| {
            use std::fmt::Write as _;
            let hex = id.iter().fold(String::new(), |mut out, b| {
                let _ = write!(out, "{b:02x}");
                out
            });
            (hex, t.rows)
        })
}

/// The dynamically-picked L1 (template-exact lookup) pair: DSL
/// `template_id == N` — riding the writer's existing bloom filter on
/// `template_id` — against `LogQL` `{service_name=~".+"} |= "<needle>"`.
/// Loki has no template concept, so its honest equivalent is a line
/// filter over every stream.
#[derive(Debug)]
pub(crate) struct TemplatePair {
    pub(crate) template_id: u64,
    /// The template's constant text: contained in every one of the
    /// template's rows (bit-identical reconstruction, `CLAUDE.md` §3.3)
    /// and — validated against the corpus — in NO other line, so the two
    /// queries select identical row sets.
    pub(crate) needle: String,
    /// How many rows the pair selects (the validated count).
    pub(crate) rows: u64,
}

/// A template's candidate needle: the longest run of consecutive
/// [`OwnedToken::Fixed`] tokens joined with single spaces, kept only at
/// ≥ 10 chars. Runs split at wildcards AND at tokens outside the safe
/// charset ([`select_pair_candidates`]' rule — the needle lands inside a
/// quoted `LogQL` string literal). Separators are per-ROW state, not
/// template state, so the single-space join is an assumption about the
/// common case; [`pick_template_pair`]'s containment validation rejects
/// any candidate it fails for. Length ties break to the
/// lexicographically smallest run, for deterministic reruns.
pub(crate) fn template_needle(tokens: &[OwnedToken]) -> Option<String> {
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ' '))
    };
    let mut runs: Vec<String> = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    for token in tokens {
        match token {
            OwnedToken::Fixed(s) if safe(s) => run.push(s),
            _ => {
                if !run.is_empty() {
                    runs.push(run.join(" "));
                    run.clear();
                }
            }
        }
    }
    if !run.is_empty() {
        runs.push(run.join(" "));
    }
    runs.retain(|needle| needle.len() >= 10);
    runs.into_iter().min_by(|a, b| {
        (std::cmp::Reverse(a.len()), a.as_str()).cmp(&(std::cmp::Reverse(b.len()), b.as_str()))
    })
}

/// Per-needle tally from [`tally_needles`]' corpus pass.
#[derive(Default)]
pub(crate) struct NeedleTally {
    /// Corpus lines whose string body contains the needle.
    pub(crate) matches: u64,
    /// A matching line has `time_unix_nano == 0` — the established
    /// key-mismatch poison rule (the two systems answer with different
    /// keys).
    pub(crate) has_zero_ts: bool,
    /// A matching line has no `service.name` — invisible to the `LogQL`
    /// side's `{service_name=~".+"}` selector.
    pub(crate) has_empty_service: bool,
}

/// One streaming corpus pass for the L1 picker: for each candidate
/// needle, count the corpus lines whose string body CONTAINS it, and
/// record whether any matching line is poisoned. Non-string bodies never
/// match (a needle can only select the string-body lines whose bytes
/// both systems return identically).
pub(crate) fn tally_needles(
    corpus_dir: &std::path::Path,
    candidates: &[(u64, String)],
) -> Vec<NeedleTally> {
    use std::io::BufRead as _;

    let mut tallies: Vec<NeedleTally> = candidates.iter().map(|_| NeedleTally::default()).collect();
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service_is_empty = !rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .is_some_and(|v| {
                        matches!(
                            v,
                            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s)
                                if !s.is_empty()
                        )
                    });
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        let Some(
                            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                                body,
                            ),
                        ) = lr.body.as_ref().and_then(|b| b.value.as_ref())
                        else {
                            continue;
                        };
                        for ((_, needle), tally) in candidates.iter().zip(&mut tallies) {
                            if body.contains(needle.as_str()) {
                                tally.matches += 1;
                                tally.has_zero_ts |= lr.time_unix_nano == 0;
                                tally.has_empty_service |= service_is_empty;
                            }
                        }
                    }
                }
            }
        }
    }
    tallies
}

/// The L1 candidates worth validating, ordered most-selective first:
/// needle-match count in `2..=4000` (a meaningful multiset that fits one
/// Loki page) and no poisoned matching line. Sorted by fewest matches,
/// then the longest needle, then the lexicographically smallest needle,
/// then the smallest template id — deterministic reruns.
pub(crate) fn eligible_template_candidates<'a>(
    candidates: &'a [(u64, String)],
    tallies: &[NeedleTally],
) -> Vec<(u64, &'a str, u64)> {
    let mut eligible: Vec<(u64, &str, u64)> = candidates
        .iter()
        .zip(tallies)
        .filter(|(_, t)| !t.has_zero_ts && !t.has_empty_service && (2..=4000).contains(&t.matches))
        .map(|((id, needle), t)| (*id, needle.as_str(), t.matches))
        .collect();
    eligible.sort_by(|a, b| {
        (a.2, std::cmp::Reverse(a.1.len()), a.1, a.0).cmp(&(
            b.2,
            std::cmp::Reverse(b.1.len()),
            b.1,
            b.0,
        ))
    });
    eligible
}

/// Fourth picker, for the L1 (template-exact lookup) pair — runs AFTER
/// `build_comparative_store`, because template ids exist only
/// post-mining. Derives the tenant's template registry via the querier
/// (RFC 0017 §3.2), takes each template's longest constant run as its
/// candidate needle ([`template_needle`]), and VALIDATES equivalence
/// before selection. A candidate survives iff:
///
/// - the count of corpus lines containing the needle equals the
///   template's row count (an Ourios count query, `template_id == N`,
///   no limit) — every template row contains its constant text
///   (bit-identical reconstruction, `CLAUDE.md` §3.3), so count-equality
///   proves the needle selects exactly the template's rows and nothing
///   else;
/// - every RENDERED template row actually contains the needle — the
///   containment direction count-equality alone cannot prove for a
///   multi-token needle (separators are per-row state; the single-space
///   join is only the common case);
/// - the shared honesty rules ([`eligible_template_candidates`]):
///   `2..=4000` rows, no zero-`time_unix_nano` and no empty-service line
///   among the needle matches.
///
/// Rejections are LOUD (stderr), and so is the no-valid-template skip
/// (the [`build_pair_specs`] `None` arm).
pub(crate) fn pick_template_pair(
    corpus_dir: &std::path::Path,
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    now: u64,
    window: u64,
) -> Option<TemplatePair> {
    use std::collections::HashMap;
    use std::collections::hash_map::Entry;

    let querier = ourios_querier::Querier::new(bucket_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let registry = runtime
        .block_on(querier.template_registry(tenant))
        .expect("derive template registry");

    // Highest version per template id: widening only ever turns Fixed
    // positions into Wildcards (RFC 0001 §6.2 step 5), so the latest
    // version's constants are constant in every earlier version's rows
    // too — and the DSL `template_id == N` selects rows of EVERY version.
    let mut latest: HashMap<u64, (u32, Vec<OwnedToken>)> = HashMap::new();
    for ((id, version), tokens) in registry {
        match latest.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert((version, tokens));
            }
            Entry::Occupied(mut entry) => {
                if version > entry.get().0 {
                    entry.insert((version, tokens));
                }
            }
        }
    }
    let mut candidates: Vec<(u64, String)> = latest
        .into_iter()
        .filter(|&(id, _)| id != ourios_miner::cluster::NO_TEMPLATE)
        .filter_map(|(id, (_, tokens))| template_needle(&tokens).map(|needle| (id, needle)))
        .collect();
    // Deterministic tally/validation order regardless of map iteration.
    candidates.sort_unstable();

    let tallies = tally_needles(corpus_dir, &candidates);
    for (template_id, needle, matches) in eligible_template_candidates(&candidates, &tallies) {
        let dsl = format!("template_id == {template_id}");
        let query = ourios_querier::dsl::parse(&dsl).expect("L1 count DSL parses");
        let rows = runtime
            .block_on(querier.run_query(&query, tenant, now, window, None))
            .expect("L1 count query")
            .rows;
        if rows != matches {
            eprintln!(
                "L1 candidate template_id={template_id} needle={needle:?} rejected: \
                 {rows} template rows vs {matches} needle-matching corpus lines",
            );
            continue;
        }
        match ourios_bench::ourios_query_answer(
            bucket_root,
            tenant,
            &format!("{dsl} | limit 5000"),
            now,
            window,
        ) {
            Ok(answer) => {
                if answer.lines.iter().all(|line| {
                    line.body
                        .windows(needle.len())
                        .any(|w| w == needle.as_bytes())
                }) {
                    return Some(TemplatePair {
                        template_id,
                        needle: needle.to_string(),
                        rows,
                    });
                }
                eprintln!(
                    "L1 candidate template_id={template_id} needle={needle:?} rejected: a \
                     rendered row does not contain the needle (per-row separators broke \
                     the single-space join)",
                );
            }
            Err(e) => eprintln!(
                "L1 candidate template_id={template_id} needle={needle:?} rejected: row \
                 materialization failed ({e:?})",
            ),
        }
    }
    None
}

// ---------------------------------------------------------------------------
// L4 (frequency-aggregation) pair — the fourth post-store-build picker.
// ---------------------------------------------------------------------------

/// The lowest floor on total matching rows a [`pick_frequency_pair`]
/// candidate must clear — enough for the grouped-count map to be a
/// meaningful multi-cell answer, not a trivially-passing edge case.
pub(crate) const L4_MIN_ROWS: u64 = 4;

/// The highest ceiling on total matching rows a [`pick_frequency_pair`]
/// candidate may have — a smaller candidate proves the same
/// `(bucket, group_key) → count` equivalence just as validly as a
/// larger one, and [`loki_measure_frequency_pair`]'s poll shares the
/// same 300 s deadline every other class's [`loki_measure_pair`] uses.
/// A ~971K-row candidate (a service's dominant, near-catch-all
/// template) genuinely could not finish ingesting/processing within
/// that window on a real dispatch — 100K rows leaves comfortable
/// margin at the observed ~2.7K rows/s Loki throughput (~37 s of the
/// 300 s budget) while still being a large, meaningful multiset.
pub(crate) const L4_MAX_ROWS: u64 = 100_000;

/// The distinct-param-value cardinality band a [`pick_frequency_pair`]
/// candidate must fall within: `2` (a single value is not a grouping
/// question) through `50` (moderate cardinality — a low-cardinality
/// `GROUP BY` an operator would actually run, per the RFC 0031 §2.3 L4
/// motivation; this is also what naturally rejects a per-line-unique slot
/// like kafka's, without a special case for it).
pub(crate) const L4_CARDINALITY: std::ops::RangeInclusive<usize> = 2..=50;

/// The floor on a candidate's average inter-arrival interval
/// (`bucket_width_seconds / (total_rows / distinct_buckets)`) a
/// [`pick_frequency_pair`] candidate must clear. A real dispatch (RFC
/// 0031 L4 workstream, runs #6-#12) picked kafka's `template_id=16`
/// "Wrote producer snapshot" needle — a ~15 s average cadence — and
/// measured a stable ~93-97% completeness ceiling against Loki
/// regardless of poll deadline, results caching,
/// `-validation.max-entries-limit`, or bucket-aligned query windows.
/// Loki's documented same-`(timestamp, body)` ingester dedup was the
/// leading theory, but runs #13+ disproved it directly: a corpus-side
/// check found ZERO exact `(timestamp, body)` collisions for either
/// candidate template's matching records. The true mechanism is still
/// uncharacterized (it matches an open, unresolved upstream Loki issue
/// — silent small-percentage loss on wide-time-range queries, no
/// maintainer-identified root cause; see [`L4_COMPLETENESS_MARGIN`]),
/// but empirically, lower frequency DOES help: switching from the ~15 s
/// candidate to a ~144 s one cut the loss from ~17.5% to ~4%. This
/// floor is deliberately generous — 100 s, comfortably above the
/// observed-bad 15 s and with real headroom below the two candidates a
/// corpus exploration run found clear of it (60 s and ~144 s cadences).
pub(crate) const L4_MIN_AVG_INTERVAL_SECONDS: f64 = 100.0;

/// The minimum fraction of a picked L4 candidate's `expected_rows` Loki
/// must return before the harness accepts its answer as equivalent
/// (RFC 0031 §7's completeness-margin decision, dated 2026-07-17).
///
/// Even after [`L4_MIN_AVG_INTERVAL_SECONDS`] cut the loss from ~17.5%
/// to ~4% (runs #13/#14/#16: 95.6%/95.8%/96.1% complete), a real
/// dispatch NEVER reached exact completeness for any L4 candidate
/// tried. Runs #13-#16 exhausted every mechanism checkable from this
/// side: a plain unaggregated line-filter count came back exactly as
/// short as the aggregation path (ruling out anything query-shape
/// specific); a corpus-side check found zero exact `(timestamp, body)`
/// collisions (ruling out Loki's documented dedup rule); the two kafka
/// service-instance periods (a genuine mid-corpus container restart)
/// are cleanly sequential with no interleaving; `push_corpus_to_loki`
/// was read end to end with no drop path found; Loki's own container
/// stderr carries zero `level=warn`/`level=error` lines (bar one
/// harmless startup "empty ring" transient); and Loki's own
/// `loki_discarded_samples_total`/`loki_discarded_bytes_total`
/// Prometheus counters — its dedicated accounting for silent/expected
/// discards — never appear in `/metrics` at all, meaning zero discards
/// of ANY kind were recorded for ANY reason.
///
/// This matches a known, OPEN, unresolved upstream Loki issue
/// (grafana/loki#10658 and related): wide-time-range queries silently
/// missing a small, consistent percentage of lines, with no error, no
/// discard accounting, and no maintainer-identified root cause as of
/// this writing. It is not a defect in Ourios, this harness's query
/// construction, or the corpus — it is a documented, external,
/// currently-unfixable characteristic of the comparison partner.
///
/// 90% (tolerating up to 10% loss) is chosen with real headroom over
/// the observed 3.9-4.4% band (roughly 2.3x), not tuned to the exact
/// number — a margin this loose stays meaningful because
/// [`compare_aggregations_within_margin`] still hard-fails on the
/// invariants that WOULD indicate a genuine Ourios-side or query-side
/// bug: a `(bucket, group_key)` cell Loki reports that Ourios's own
/// answer doesn't contain at all (a phantom cell — the signal of a
/// wrong regex or wrong bucket math), or any `group_key` where Loki's
/// total across all its buckets exceeds Ourios's. The check went
/// through two rounds of PR #536 review hardening after this margin was
/// first written: run #17 showed a single cell landing 1 row over
/// Ourios's own count while the SAME key's total across its own buckets
/// stayed a solid under-count (step-grid boundary imprecision, not
/// fabrication), which a naive per-cell check flagged too strictly —
/// fixed to check per-`group_key`, not per-cell or a single grand
/// total (a pure grand-total check would let an overcount on one key
/// silently compensate for another key's loss). Run #19 then found a
/// cardinality-1 key that legitimately lost its only row — a pure
/// percentage margin can't express that at `n = 1` — fixed by
/// converting the per-key tolerance to an absolute row count, floored
/// at 1 only for `n = 1`. See [`compare_aggregations_within_margin`]'s
/// own documentation for the full, current design.
pub(crate) const L4_COMPLETENESS_MARGIN: f64 = 0.90;

/// Choose a `bucket(width)` for the L4 pair from a query's time span: the
/// largest whole DSL duration unit (`w`/`d`/`h`/`m`/`s`) that divides the
/// span into roughly [`L4_TARGET_BUCKETS`] windows, floored at the DSL's
/// finest unit (`1s`) so a short capture window still splits into more
/// than one bucket. RFC 0031 §7 leaves "which bucket width" open pending
/// v8-scale tuning at dispatch time; this is a documented, deterministic
/// default, not a frozen value.
pub(crate) fn pick_bucket_width(span_ns: u64) -> String {
    const L4_TARGET_BUCKETS: u64 = 4;
    const MIN_BUCKET_NS: u64 = 1_000_000_000;
    const UNITS: [(&str, u64); 5] = [
        ("w", 7 * 86_400 * 1_000_000_000),
        ("d", 86_400 * 1_000_000_000),
        ("h", 3_600 * 1_000_000_000),
        ("m", 60 * 1_000_000_000),
        ("s", 1_000_000_000),
    ];
    let target = (span_ns / L4_TARGET_BUCKETS).max(MIN_BUCKET_NS);
    for (suffix, unit_ns) in UNITS {
        if target >= unit_ns {
            return format!("{}{suffix}", target / unit_ns);
        }
    }
    unreachable!("target is floored at MIN_BUCKET_NS, the \"s\" unit's own width")
}

/// Convert a `bucket(width)` lexeme this harness emits (`pick_bucket_width`'s
/// output — `s`/`m`/`h`/`d`/`w`, never sub-second) to whole seconds, the
/// unit Loki's `query_range` `start`/`end`/`step` parameters take.
pub(crate) fn bucket_width_seconds(width: &str) -> u64 {
    let (digits, unit) = width.split_at(width.len() - 1);
    let n: u64 = digits
        .parse()
        .unwrap_or_else(|e| panic!("bucket width {width:?} digits: {e}"));
    let per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        other => panic!("bucket width {width:?} has unknown unit {other:?}"),
    };
    n * per_unit
}

/// Escape Go RE2 metacharacters (Loki's `regexp` stage dialect) in a
/// literal template constant.
pub(crate) fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '|' | '(' | ')' | '[' | ']' | '{' | '}' | '*' | '+' | '?' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build a Loki `regexp` pattern (Go RE2 syntax) capturing one template
/// parameter as the named group `value` — the RFC 0031 §5 L4 "`LogQL`
/// pattern/regexp extraction" (§2.3). Anchors on the wildcard's immediate
/// fixed-token neighbours, not the whole line: a narrower pattern is more
/// robust to separator variance elsewhere in the line (per-row separators
/// are not template state — the same known limitation [`template_needle`]
/// documents) and unambiguous whenever at least one neighbour is fixed.
/// Returns `None` if `target_param` is not a wildcard position in
/// `tokens`, **or** if neither neighbour is a fixed token — an
/// unanchored `(?P<value>\S+)` would match any token and silently feed
/// wrong equivalence input to `compare_aggregations`, so the caller must
/// reject the candidate instead of measuring it.
pub(crate) fn param_capture_regex(tokens: &[OwnedToken], target_param: u32) -> Option<String> {
    let mut wildcard_idx = 0u32;
    let pos = tokens.iter().position(|t| {
        if matches!(t, OwnedToken::Wildcard) {
            let is_target = wildcard_idx == target_param;
            wildcard_idx += 1;
            is_target
        } else {
            false
        }
    })?;
    let fixed_at = |i: usize| match tokens.get(i) {
        Some(OwnedToken::Fixed(s)) => Some(s.as_str()),
        _ => None,
    };
    let prev = pos.checked_sub(1).and_then(fixed_at);
    let next = fixed_at(pos + 1);
    if prev.is_none() && next.is_none() {
        return None;
    }
    let mut pattern = String::new();
    if let Some(prev) = prev {
        pattern.push_str(&regex_escape(prev));
        pattern.push_str(r"\s+");
    }
    pattern.push_str(r"(?P<value>\S+)");
    if let Some(next) = next {
        pattern.push_str(r"\s+");
        pattern.push_str(&regex_escape(next));
    }
    Some(pattern)
}

/// The dynamically-picked L4 (frequency-aggregation) pair: a
/// `(template_id, param position, bucket width)` whose grouped-count map
/// is a moderate-cardinality, multi-row answer.
#[derive(Debug)]
pub(crate) struct FrequencyPair {
    pub(crate) template_id: u64,
    /// The wildcard position within the template extracted as the group
    /// key (`param(n)`, 0-based over wildcards only).
    pub(crate) param: u32,
    /// The template's constant needle for the Loki `|=` stream-selector
    /// pre-filter ([`template_needle`] — the same machinery the L1 pair
    /// uses).
    pub(crate) needle: String,
    /// The Loki `regexp` pattern extracting `param` as the named group
    /// `value` ([`param_capture_regex`]).
    pub(crate) capture_regex: String,
    /// The `bucket(width)` lexeme ([`pick_bucket_width`]).
    pub(crate) bucket_width: String,
    /// The validated grouped-count map (from the real Ourios aggregate
    /// query — the picker's own selection criterion).
    pub(crate) groups: HashMap<AggKey, u64>,
    /// The grouped-count scan's bytes read (RFC 0031 §3.6).
    pub(crate) bytes_read: u64,
}

/// Fourth picker (post-store-build, like [`pick_template_pair`]): a
/// per-template, per-param-slot cardinality tally over the corpus,
/// choosing the first `(template_id, param position)` whose grouped-count
/// map (at a bucket width derived from the query's own time span,
/// [`pick_bucket_width`]) has [`L4_CARDINALITY`] distinct group values and
/// at least [`L4_MIN_ROWS`] total matching rows — moderate cardinality,
/// enough rows to be a meaningful multiset, and (naturally, via the
/// cardinality bound) never a per-line-unique slot like kafka's. Iterates
/// templates by ascending id and, within a template, param slots by
/// ascending index — a deterministic pick. Every rejected candidate logs
/// its reason to stderr (the run-#16 "no silent fallthrough" lesson); a
/// candidate that fails the SAME query bounds check but has no usable
/// needle or capture regex is rejected too, loudly.
/// Whether an aggregate candidate's grouped-count map meets the L4
/// picker's shape floors/ceiling — moderate cardinality
/// ([`L4_CARDINALITY`]), a row-count band (at least [`L4_MIN_ROWS`],
/// at most [`L4_MAX_ROWS`] — too many rows makes
/// [`loki_measure_frequency_pair`]'s poll unable to finish within its
/// shared 300 s deadline before Loki has ingested/processed them
/// all), and at least 2 distinct bucket starts (otherwise the
/// candidate never exercises the bucket dimension of the `(bucket,
/// group_key) → count` equivalence shape, leaving
/// [`parse_loki_matrix`]'s bucket-alignment convention untested on
/// this corpus). Returns the rejection reason, or `None` if the
/// candidate passes every bound.
pub(crate) fn frequency_shape_rejection(
    groups: &HashMap<AggKey, u64>,
    bucket_width_seconds: u64,
) -> Option<String> {
    let distinct_values: std::collections::HashSet<&String> =
        groups.keys().map(|k| &k.group_key).collect();
    if !L4_CARDINALITY.contains(&distinct_values.len()) {
        return Some(format!(
            "{} distinct param values (need {L4_CARDINALITY:?})",
            distinct_values.len(),
        ));
    }
    let total_rows: u64 = groups.values().sum();
    if total_rows < L4_MIN_ROWS {
        return Some(format!("{total_rows} rows (need >= {L4_MIN_ROWS})"));
    }
    if total_rows > L4_MAX_ROWS {
        return Some(format!(
            "{total_rows} rows (need <= {L4_MAX_ROWS}, so the Loki poll can finish within \
             its shared 300s deadline)"
        ));
    }
    let distinct_buckets: std::collections::HashSet<u64> =
        groups.keys().map(|k| k.bucket_start_unix_nanos).collect();
    if distinct_buckets.len() < 2 {
        return Some(format!(
            "all rows land in {} bucket window(s) (need >= 2, to exercise the bucket dimension)",
            distinct_buckets.len(),
        ));
    }
    // Row/bucket counts and a bucket width in seconds never approach
    // f64's 2^53 exact-integer range in practice — this is an
    // approximate picker heuristic, not a value stored or compared bit
    // for bit.
    #[allow(clippy::cast_precision_loss)]
    let avg_rows_per_bucket = total_rows as f64 / distinct_buckets.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let avg_interval_s = bucket_width_seconds as f64 / avg_rows_per_bucket;
    if avg_interval_s < L4_MIN_AVG_INTERVAL_SECONDS {
        return Some(format!(
            "~{avg_interval_s:.1}s average inter-arrival interval (need >= \
             {L4_MIN_AVG_INTERVAL_SECONDS}s — a shorter cadence correlates with Loki's \
             uncharacterized wide-time-range completeness shortfall; exact \
             (timestamp, body) collision was checked and ruled out directly, see \
             L4_COMPLETENESS_MARGIN's documentation)"
        ));
    }
    None
}

pub(crate) fn pick_frequency_pair(
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    now: u64,
    window: u64,
) -> Option<FrequencyPair> {
    use std::collections::HashMap as StdHashMap;
    use std::collections::hash_map::Entry;

    let querier = ourios_querier::Querier::new(bucket_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let registry = runtime
        .block_on(querier.template_registry(tenant))
        .expect("derive template registry");

    // Highest version per template id — mirrors `pick_template_pair`'s
    // rationale: widening only ever turns Fixed positions into Wildcards,
    // so the latest version's wildcard positions are wildcards in every
    // earlier version's rows too.
    let mut latest: StdHashMap<u64, (u32, Vec<OwnedToken>)> = StdHashMap::new();
    for ((id, version), tokens) in registry {
        match latest.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert((version, tokens));
            }
            Entry::Occupied(mut entry) => {
                if version > entry.get().0 {
                    entry.insert((version, tokens));
                }
            }
        }
    }
    let mut ids: Vec<u64> = latest
        .keys()
        .copied()
        .filter(|&id| id != ourios_miner::cluster::NO_TEMPLATE)
        .collect();
    ids.sort_unstable();

    let bucket_width = pick_bucket_width(window);

    for id in ids {
        let tokens = &latest[&id].1;
        let wildcard_count = tokens
            .iter()
            .filter(|t| matches!(t, OwnedToken::Wildcard))
            .count();
        for param in 0..u32::try_from(wildcard_count).expect("wildcard count fits u32") {
            let dsl =
                format!("template_id == {id} | count by param({param}), bucket({bucket_width})");
            let answer = match ourios_aggregate_answer(bucket_root, tenant, &dsl, now, window) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "L4 candidate template_id={id} param({param}): aggregate query \
                         failed: {e:?}",
                    );
                    continue;
                }
            };
            if let Some(reason) =
                frequency_shape_rejection(&answer.groups, bucket_width_seconds(&bucket_width))
            {
                eprintln!("L4 candidate template_id={id} param({param}): rejected — {reason}");
                continue;
            }
            let Some(needle) = template_needle(tokens) else {
                eprintln!(
                    "L4 candidate template_id={id} param({param}): rejected — no ≥10-char \
                     constant needle for the Loki stream-selector filter",
                );
                continue;
            };
            let Some(capture_regex) = param_capture_regex(tokens, param) else {
                eprintln!(
                    "L4 candidate template_id={id} param({param}): rejected — no fixed \
                     neighbour token to anchor the Loki capture regex on (an unanchored \
                     pattern would match any token)",
                );
                continue;
            };
            return Some(FrequencyPair {
                template_id: id,
                param,
                needle,
                capture_regex,
                bucket_width,
                groups: answer.groups,
                bytes_read: answer.bytes_read,
            });
        }
    }
    None
}

/// Render a synthetic Loki matrix `query_range` response over the SAME
/// grouped counts an [`ourios_aggregate_answer`] call measured — the
/// no-container proof that [`parse_loki_matrix`]'s bucket-alignment
/// convention (evaluation instant `t = (k+1)*width`, decoded bucket start
/// `t - width`) round-trips exactly. Samples are grouped by
/// [`AggKey::group_key`] under the `value` metric label and sorted by
/// timestamp ascending within each series (cosmetic — `parse_loki_matrix`
/// does not require sample order).
pub(crate) fn synthetic_loki_matrix(groups: &HashMap<AggKey, u64>, bucket_width_ns: u64) -> String {
    let mut by_label: std::collections::BTreeMap<String, Vec<(u64, u64)>> =
        std::collections::BTreeMap::new();
    for (key, &count) in groups {
        let t_ns = key
            .bucket_start_unix_nanos
            .checked_add(bucket_width_ns)
            .expect("bucket end fits u64 nanoseconds");
        by_label
            .entry(key.group_key.clone())
            .or_default()
            .push((t_ns, count));
    }
    let result: Vec<serde_json::Value> = by_label
        .into_iter()
        .map(|(label, mut samples)| {
            samples.sort_unstable_by_key(|&(t, _)| t);
            let values: Vec<serde_json::Value> = samples
                .into_iter()
                .map(|(t_ns, count)| {
                    let seconds = t_ns / 1_000_000_000;
                    serde_json::json!([seconds, count.to_string()])
                })
                .collect();
            serde_json::json!({ "metric": { "value": label }, "values": values })
        })
        .collect();
    serde_json::json!({
        "status": "success",
        "data": { "resultType": "matrix", "result": result },
    })
    .to_string()
}

/// The selective-resource diagnostic's service pick: the corpus's
/// LOWEST-volume safe-named service that still yields a clean window
/// (maintainer's positioning point, 2026-07-12: an enriched
/// resource-scoped browse should prune via the promoted `service.name`
/// bloom — the measured L6 windows used the HIGHEST-volume service,
/// which is present in every row group and is the bloom's worst case
/// by construction, so they bound the UNSCOPED-browse cost, not the
/// enriched one). `exclude` is the service the L6 window pairs already
/// use: falling through to it would duplicate an existing measurement,
/// which is a vacuous diagnostic (run #16 measured exactly that before
/// this guard existed). One corpus pass collects every service's
/// timestamps at once — the per-service rescan cost run #16 ~70
/// minutes. Every skipped candidate logs WHY, so the run log records
/// the corpus's enrichment reality even when the pick falls through.
pub(crate) type ServiceTimestamps = (Vec<u64>, Vec<u64>);

pub(crate) fn pick_rare_window_pair(
    corpus_dir: &std::path::Path,
    exclude: &str,
) -> Option<(String, u64, u64, u64)> {
    use std::collections::HashMap;
    use std::io::BufRead as _;

    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ' '))
    };
    // service -> (clean ts, poison ts), both filled in ONE corpus pass.
    let mut per_service: HashMap<String, ServiceTimestamps> = HashMap::new();
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .and_then(|v| match v {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                if !safe(service) || service == exclude {
                    continue;
                }
                // get-then-insert (the file's established pattern) so
                // repeat ResourceLogs never allocate a lookup key.
                let entry = if let Some(entry) = per_service.get_mut(service) {
                    entry
                } else {
                    per_service.entry(service.to_string()).or_default()
                };
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        if lr.time_unix_nano != 0 {
                            entry.0.push(lr.time_unix_nano);
                        } else if lr.observed_time_unix_nano != 0 {
                            entry.1.push(lr.observed_time_unix_nano);
                        }
                    }
                }
            }
        }
    }
    let mut by_volume: Vec<(String, ServiceTimestamps)> = per_service.into_iter().collect();
    by_volume.sort_by(|a, b| {
        let (av, bv) = (a.1.0.len() + a.1.1.len(), b.1.0.len() + b.1.1.len());
        av.cmp(&bv).then_with(|| a.0.cmp(&b.0))
    });
    for (service, (mut clean, mut poison)) in by_volume {
        if clean.len() < 2 {
            eprintln!(
                "selective-resource candidate {service}: skipped — {} clean timestamps \
                 ({} zero-time rows); a source-timestamp-quality gap in the corpus, \
                 not a picker gap",
                clean.len(),
                poison.len(),
            );
            continue;
        }
        clean.sort_unstable();
        poison.sort_unstable();
        // Descending window-size ladder — a deliberate SAMPLING of the
        // 2..=100 range, not an exhaustive search: the largest k may
        // have no clean edges even when smaller ones do, and a
        // lower-volume service should win at any sampled size before a
        // busier one is tried. A window valid only at an unsampled
        // intermediate k falls through — acceptable for a diagnostic,
        // where 99 validity passes per service buys no measurement
        // value. Clamped duplicates are skipped.
        let mut tried = 0usize;
        for k in [100usize, 50, 20, 10, 5, 2] {
            let k = k.min(clean.len());
            if k < 2 {
                break;
            }
            if k == tried {
                continue;
            }
            tried = k;
            if let Some((start, end)) = pick_window_pair(&clean, &poison, k) {
                return Some((service, start, end, k as u64));
            }
        }
        eprintln!(
            "selective-resource candidate {service}: skipped — {} clean timestamps but \
             no clean-edged window at any sampled k",
            clean.len(),
        );
    }
    None
}

/// Pick a `k`-row time-window slice `[a, b)` of a service's records with
/// CLEAN EDGES, preferring mid-corpus. `clean_ts` and `poison_ts` come
/// sorted from [`collect_service_timestamps`]. A candidate start index
/// `i` is valid iff:
///
/// - **start edge**: no earlier record shares `clean_ts[i]`, so `a =
///   clean_ts[i]` admits exactly the window's rows;
/// - **end edge**: the next record (if any) sits ≥ 2 ns past the last
///   in-window one, so `b = last + 1` selects the same rows whether a
///   system treats its range end as inclusive or exclusive;
/// - no poison timestamp falls in `[a, b)`.
///
/// The window then contains exactly `k` rows on both systems. Returns
/// `(a, b)`, or `None` if no valid window exists.
pub(crate) fn pick_window_pair(
    clean_ts: &[u64],
    poison_ts: &[u64],
    k: usize,
) -> Option<(u64, u64)> {
    if k == 0 || clean_ts.len() < k {
        return None;
    }
    let valid = |i: usize| {
        let (first, last) = (clean_ts[i], clean_ts[i + k - 1]);
        if i > 0 && clean_ts[i - 1] == first {
            return false;
        }
        let Some(b) = last.checked_add(1) else {
            return false;
        };
        if i + k < clean_ts.len() && clean_ts[i + k] <= b {
            return false;
        }
        let at = poison_ts.partition_point(|&t| t < first);
        poison_ts.get(at).is_none_or(|&t| t >= b)
    };
    let centre = (clean_ts.len() - k) / 2;
    let i = (0..=clean_ts.len() - k)
        .filter(|&i| valid(i))
        .min_by_key(|&i| (i.abs_diff(centre), i))?;
    Some((clean_ts[i], clean_ts[i + k - 1].checked_add(1)?))
}

/// `severity_number -> severity_text -> row count`, one map per service
/// for clean (non-zero-time) rows and one for poison (zero-time) rows.
pub(crate) type SeverityBands =
    std::collections::HashMap<i32, std::collections::HashMap<String, u64>>;

/// The candidate `(rows, threshold, service, text)` tuples for
/// [`pick_selective_pair`]: for every service and every DISTINCT severity
/// number T in its clean bands (the nested map dedupes them), the rows
/// with `number ≥ T` are a candidate iff (a) they all share ONE text t,
/// (b) the count is `1..=4000`, (c) — the reverse direction — NO row with
/// text t sits below T, so `LogQL`'s text filter selects exactly the same
/// rows as the DSL's number threshold, and (d) no POISON row could be
/// selected by EITHER side's predicate: none with `number ≥ T` (the DSL
/// side) and none carrying text t at any number (the `LogQL` side) —
/// the severity pair queries the full corpus window, so a selectable
/// zero-time row is a guaranteed equivalence mismatch. No-service
/// (empty-key) records are scanned — they show in the failure diagnostic
/// — but never form candidates (an empty service can't make a valid
/// DSL/LogQL pair).
pub(crate) fn select_pair_candidates(
    per_service: &std::collections::HashMap<String, (SeverityBands, SeverityBands)>,
) -> Vec<(u64, i32, &String, &String)> {
    // Both names are interpolated into quoted DSL and LogQL string
    // literals; a `"` or `\` (legal in OTLP attributes) would break or
    // change either query. Rather than implement escaping for two query
    // languages, a pair whose names fall outside this conservative set is
    // simply not a candidate.
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ' '))
    };
    let mut candidates = Vec::new();
    for (svc, (bands, poison)) in per_service {
        if !safe(svc) {
            continue;
        }
        for &threshold in bands.keys() {
            let selected: Vec<(&String, u64)> = bands
                .iter()
                .filter(|&(&n, _)| n >= threshold)
                .flat_map(|(_, texts)| texts.iter().map(|(t, c)| (t, *c)))
                .collect();
            let rows: u64 = selected.iter().map(|&(_, c)| c).sum();
            let mut texts: Vec<&String> = selected.iter().map(|&(t, _)| t).collect();
            texts.sort();
            texts.dedup();
            let [text] = texts.as_slice() else {
                continue;
            };
            if !safe(text) || !(1..=4000).contains(&rows) {
                continue;
            }
            let text_total: u64 = bands
                .values()
                .filter_map(|texts| texts.get(text.as_str()))
                .sum();
            if text_total != rows {
                // Rows with this text exist below the threshold — the
                // LogQL side would return more than the DSL side.
                continue;
            }
            if poison
                .iter()
                .any(|(&n, texts)| n >= threshold || texts.contains_key(text.as_str()))
            {
                continue;
            }
            candidates.push((rows, threshold, svc, *text));
        }
    }
    candidates
}

/// One OTLP `LogsData` line for `service`, one INFO record per
/// `(time_unix_nano, body)` pair — the hand-rolled two-service analogue
/// of `fixture_logs_data` (which is fixed to the single
/// [`FIXTURE_SERVICE`] resource).
pub(crate) fn service_logs_data(
    service: &str,
    records: &[(u64, &str)],
) -> opentelemetry_proto::tonic::logs::v1::LogsData {
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, LogsData, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    let log_records = records
        .iter()
        .map(|&(ts, body)| LogRecord {
            time_unix_nano: ts,
            severity_number: 9,
            severity_text: "INFO".to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body.to_string())),
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
        }],
    }
}

/// A fixed in-range instant for the two-service corpus (2026-04-02 UTC;
/// local tests don't face Loki's reject-old-samples window, so any base
/// works — fixed for deterministic partition paths).
pub(crate) const TWO_SERVICE_BASE_NS: u64 = 1_775_127_480_000_000_000;

pub(crate) const HOUR_NS: u64 = 3_600_000_000_000;

/// The `(timestamp, body)` records each service of the two-service
/// corpus carries. `svc-a` sits two hour-partitions before `svc-b`, so
/// each service lands in its own file with service-homogeneous row-group
/// stats — the layout a `service ==` predicate can actually prune
/// against (a row group holding both services has min/max spanning both,
/// and nothing to skip).
pub(crate) fn two_service_records(service: &str) -> Vec<(u64, &'static str)> {
    let (base, bodies): (u64, [&'static str; 3]) = match service {
        "svc-a" => (
            TWO_SERVICE_BASE_NS,
            ["alpha one", "alpha two", "alpha three"],
        ),
        "svc-b" => (
            TWO_SERVICE_BASE_NS + 2 * HOUR_NS,
            ["beta one", "beta two", "beta three"],
        ),
        other => panic!("two_service_records knows svc-a and svc-b, not {other:?}"),
    };
    bodies
        .into_iter()
        .zip([0u64, 1_000, 2_000])
        .map(|(body, off)| (base + off, body))
        .collect()
}

/// Build the registry-bearing comparative store over the two-service
/// corpus. Returns the live bucket dir plus the build summary.
pub(crate) fn build_two_service_store() -> (tempfile::TempDir, ourios_bench::BuiltStore) {
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    let jsonl = ["svc-a", "svc-b"]
        .map(|svc| {
            serde_json::to_string(&service_logs_data(svc, &two_service_records(svc)))
                .expect("serialize LogsData")
        })
        .join("\n");
    std::fs::write(corpus.path().join("two-service.jsonl"), jsonl).expect("write corpus");

    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    (bucket, built)
}

pub(crate) fn synthetic_frequency_pair(capture_regex: &str) -> FrequencyPair {
    FrequencyPair {
        template_id: 1,
        param: 0,
        needle: "connection established to peer".to_string(),
        capture_regex: capture_regex.to_string(),
        bucket_width: "1s".to_string(),
        groups: HashMap::from([(
            AggKey {
                bucket_start_unix_nanos: 0,
                group_key: "10".to_string(),
            },
            1,
        )]),
        bytes_read: 1,
    }
}
