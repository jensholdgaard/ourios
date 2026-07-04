//! Build a queryable RFC 0005 Parquet store from a corpus.
//!
//! The A1 path measures the bytes a corpus compresses to; the B1/B2
//! latency benches need to *query* the same corpus, so they need the
//! mined records laid down as a real partitioned Parquet store they
//! can point a `ourios_querier::Querier` at. Both public builders
//! reuse the same corpus loader and miner harness the gates run on
//! (so the store matches what A1 measured), then write every emitted
//! record via per-partition [`Writer`]s (the same streaming write A1
//! uses). [`build_b1_store`] additionally renders the flat-text
//! reference corpus B1's `zstdcat | grep` baseline scans and tracks
//! the severity distribution B1's predicate needs.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use ourios_core::otlp::{Body, OtlpLogRecord, canonical};
use ourios_core::record::MinedRecord;
use ourios_parquet::{PartitionKey, Writer};

use crate::reference::ReferenceCorpus;
use crate::{BenchError, corpus, harness};

const HOUR_NS: u64 = 3_600_000_000_000;

/// OTLP severity-number band for ERROR..ERROR4 — the `level='ERROR'`
/// class B1's query shape (`docs/benchmarks.md` §3 B1) filters on.
const ERROR_BAND: std::ops::RangeInclusive<u8> = 17..=20;

/// What [`build_query_store`] wrote, enough for the B2 bench to pick
/// a populated query and report the result-vs-corpus relationship.
#[derive(Debug, Clone, Copy)]
pub struct BuiltStore {
    /// Tenant every record was written under (the corpus loader is
    /// single-tenant — `crate::corpus`'s `BENCH_TENANT`). A query must
    /// use this tenant or it scans nothing (RFC0007.5 isolation).
    pub tenant: &'static str,
    /// Total rows written across all partitions.
    pub rows: u64,
    /// Number of partition files written (one per `*.parquet`).
    pub files: u64,
    /// The `template_id` with the most rows — a query for it is
    /// guaranteed to return a non-empty, representative result.
    pub busiest_template_id: u64,
    /// How many rows that busiest template has (the result size a
    /// `template_id = busiest_template_id` query returns).
    pub busiest_template_rows: u64,
    /// Smallest non-zero **effective** timestamp written (`0` if none) —
    /// the start of the corpus's time span, for picking a B2 query
    /// window. Effective per RFC 0005 §3.2 (amendment 2026-06-11):
    /// `time_unix_nano`, else `observed_time_unix_nano` — the value the
    /// query window actually filters, derived via the same
    /// `ourios_parquet::effective_time_unix_nano` the writer stores.
    pub min_effective_time_unix_nano: u64,
    /// Largest effective timestamp written (`0` if none) — the end of
    /// the corpus's time span.
    pub max_effective_time_unix_nano: u64,
}

/// Load the corpus at `corpus_dir`, mine it, and write the emitted
/// records as a partitioned RFC 0005 Parquet store under
/// `bucket_root` (which a `ourios_querier::Querier` can then be
/// rooted at). Returns a [`BuiltStore`] summary.
///
/// # Errors
///
/// - [`BenchError::Corpus`] if the corpus can't be loaded.
/// - [`BenchError::Pipeline`] if partition derivation or a Parquet
///   write/close fails.
///
/// # Panics
///
/// Panics if the partition count exceeds `u64` (`usize > u64`),
/// which can't happen on any supported target — same documented
/// assumption as [`crate::run`].
pub fn build_query_store(
    corpus_dir: &Path,
    bucket_root: &Path,
    txt_severity: corpus::TxtSeverity,
) -> Result<BuiltStore, BenchError> {
    let mut counts: HashMap<u64, u64> = HashMap::new();

    let core = build_store(
        corpus_dir,
        bucket_root,
        txt_severity,
        |_input, emitted, _effective| {
            *counts.entry(emitted.template_id).or_insert(0) += 1;
            Ok(())
        },
    )?;

    let (busiest_template_id, busiest_template_rows) =
        counts.into_iter().max_by_key(|&(_, n)| n).unwrap_or((0, 0));

    Ok(BuiltStore {
        tenant: crate::corpus::BENCH_TENANT,
        rows: core.rows,
        files: core.files,
        busiest_template_id,
        busiest_template_rows,
        min_effective_time_unix_nano: core.min_effective_time_unix_nano,
        max_effective_time_unix_nano: core.max_effective_time_unix_nano,
    })
}

/// What [`build_b1_store`] produced: the [`BuiltStore`]-style span
/// summary plus the severity bookkeeping and flat-text reference the
/// B1 latency arm needs. No `Debug` derive — [`ReferenceCorpus`]
/// holds opaque compressed blocks.
pub struct B1Store {
    /// Tenant every record was written under (see [`BuiltStore::tenant`]).
    pub tenant: &'static str,
    /// Total rows written across all partitions.
    pub rows: u64,
    /// Number of partition files written.
    pub files: u64,
    /// Smallest non-zero effective timestamp written (`0` if none) —
    /// see [`BuiltStore::min_effective_time_unix_nano`].
    pub min_effective_time_unix_nano: u64,
    /// Largest effective timestamp written (`0` if none).
    pub max_effective_time_unix_nano: u64,
    /// Rows whose **effective** timestamp is `0` — neither
    /// `time_unix_nano` nor `observed_time_unix_nano` carried a value
    /// (RFC 0005 §3.2 rule 7: the B1 guard keys off the effective
    /// span). B1 queries a real time window; such rows sit outside any
    /// window derived from the span above, so the bench skips the
    /// corpus when this is non-zero rather than benchmarking a
    /// mismatched result. Observed-only corpora (the ~15 % real-corpus
    /// case the amendment exists for) keep this at `0` and stay
    /// B1-eligible.
    pub zero_effective_ts_rows: u64,
    /// Distinct `severity_text` values seen. `< 2` means a severity
    /// predicate has no selectivity (the RFC 0006 §3.3 plain-text
    /// loader fixes every line at `INFO`, so plain-text corpora
    /// always land here) and B1 over this corpus is meaningless.
    pub distinct_severities: usize,
    /// The `severity_text` the B1 query should filter on and its
    /// exact row count (the expected query result). `"ERROR"` when
    /// that text appears at any severity number (the query filters on
    /// text); otherwise the busiest text in the OTLP error band
    /// (severity number 17..=20); `None` when neither yields a text.
    pub query_severity: Option<(String, u64)>,
    /// The `zstdcat | grep` baseline input: every record with a
    /// non-zero effective timestamp rendered as the flat-text line a
    /// traditional logger would have written
    /// (`<severity_text> <body>`), compressed one block per hour —
    /// the hour granularity mirrors the store's partitioning, i.e.
    /// the `*.zst` segments `files_in_range.zst` would name.
    /// Zero-effective-ts rows are excluded: they sit outside any
    /// window and the bench skips such corpora.
    pub reference: ReferenceCorpus,
}

/// Like [`build_query_store`], but with the extra bookkeeping the B1
/// predicate-pushdown arm needs: the severity distribution (B1
/// filters on severity) and the flat-text reference corpus
/// compressed at `reference_zstd_level`.
///
/// # Errors
///
/// Everything [`build_query_store`] can return, plus
/// [`BenchError::Pipeline`] when a structured body fails canonical
/// encoding or the reference compression fails.
///
/// # Panics
///
/// Same `usize → u64` documented assumption as [`build_query_store`].
pub fn build_b1_store(
    corpus_dir: &Path,
    bucket_root: &Path,
    reference_zstd_level: i32,
    txt_severity: corpus::TxtSeverity,
) -> Result<B1Store, BenchError> {
    let mut severity_rows: BTreeMap<String, u64> = BTreeMap::new();
    let mut error_band_rows: BTreeMap<String, u64> = BTreeMap::new();
    let mut spool = HourSpool::new().map_err(|e| BenchError::Pipeline {
        detail: format!("create B1 reference spool: {e}"),
    })?;
    let mut zero_effective_ts_rows = 0u64;

    let core = build_store(
        corpus_dir,
        bucket_root,
        txt_severity,
        |input, emitted, effective| {
            if let Some(text) = &emitted.severity_text {
                *severity_rows.entry(text.clone()).or_insert(0) += 1;
                if ERROR_BAND.contains(&emitted.severity_number) {
                    *error_band_rows.entry(text.clone()).or_insert(0) += 1;
                }
            }
            if effective == 0 {
                // Genuinely timeless (neither wire timestamp set) —
                // out-of-window by definition (the B1 arm skips any corpus
                // carrying zero-effective-ts rows); keep the reference
                // strictly in-window rather than spooling lines no query
                // scans.
                zero_effective_ts_rows += 1;
                return Ok(());
            }
            let line = reference_line(input)?;
            spool
                .append(effective / HOUR_NS, &line)
                .map_err(|e| BenchError::Pipeline {
                    detail: format!("spool B1 reference line: {e}"),
                })?;
            Ok(())
        },
    )?;

    let reference =
        spool
            .into_reference(reference_zstd_level)
            .map_err(|e| BenchError::Pipeline {
                detail: format!("compress B1 reference corpus: {e}"),
            })?;

    // Prefer the literal "ERROR" of the §3 B1 query shape wherever it
    // appears — the query filters on severity *text*, so a corpus
    // mapping "ERROR" to a nonstandard severity_number still
    // qualifies. Otherwise the busiest error-band text (real corpora
    // spell the level per-SDK: "Error", "error", …). BTreeMap
    // iteration + strict `>` make ties deterministic (first text in
    // lexicographic order).
    let query_severity = if severity_rows.contains_key("ERROR") {
        Some("ERROR".to_string())
    } else {
        let mut best: Option<(&String, u64)> = None;
        for (text, &n) in &error_band_rows {
            if best.is_none_or(|(_, m)| n > m) {
                best = Some((text, n));
            }
        }
        best.map(|(text, _)| text.clone())
    }
    .map(|text| {
        let rows = severity_rows.get(&text).copied().unwrap_or(0);
        (text, rows)
    });

    Ok(B1Store {
        tenant: crate::corpus::BENCH_TENANT,
        rows: core.rows,
        files: core.files,
        min_effective_time_unix_nano: core.min_effective_time_unix_nano,
        max_effective_time_unix_nano: core.max_effective_time_unix_nano,
        zero_effective_ts_rows,
        distinct_severities: severity_rows.len(),
        query_severity,
        reference,
    })
}

/// Render the flat-text line the B1 reference corpus stores for one
/// record — what a traditional logger writing plain files would have
/// emitted: the severity text, a space, the body. Structured bodies
/// use the RFC 0005 §3.3 canonical-JSON encoding (the same bytes the
/// store retains), so the reference does equivalent scan work rather
/// than skipping records the Ourios side has to carry.
fn reference_line(input: &OtlpLogRecord) -> Result<String, BenchError> {
    let body = match &input.body {
        Some(Body::String(s)) => s.clone(),
        Some(Body::Structured(v)) => {
            let bytes = canonical::encode_any_value(v).map_err(|e| BenchError::Pipeline {
                detail: format!("canonical-encode structured body for B1 reference: {e}"),
            })?;
            String::from_utf8(bytes).map_err(|e| BenchError::Pipeline {
                detail: format!("canonical JSON is not UTF-8: {e}"),
            })?
        }
        None => String::new(),
    };
    Ok(match &input.severity_text {
        Some(text) => format!("{text} {body}"),
        None => body,
    })
}

/// Spools B1 reference lines to one temp file per hour, so building
/// the reference stays memory-bounded at GiB corpus scale (buffering
/// every line as a `String` would hold the whole corpus — plus
/// allocator overhead — in RAM and OOM a CI runner). Compression
/// happens at the end, one hour at a time, with a single zstd
/// encoder alive at once. Open handles scale with *distinct hours*
/// (a capture-shaped corpus spans a handful), not with records.
struct HourSpool {
    dir: tempfile::TempDir,
    hours: BTreeMap<u64, std::io::BufWriter<std::fs::File>>,
}

impl HourSpool {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            dir: tempfile::TempDir::new()?,
            hours: BTreeMap::new(),
        })
    }

    fn append(&mut self, hour: u64, line: &str) -> std::io::Result<()> {
        use std::io::Write;

        let writer = match self.hours.entry(hour) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let file = std::fs::File::create(self.dir.path().join(hour.to_string()))?;
                e.insert(std::io::BufWriter::new(file))
            }
        };
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")
    }

    /// Flush every spool file and compress each into one reference
    /// block (hour order — `BTreeMap` keys — for determinism). The
    /// spool dir is dropped, and with it the uncompressed bytes.
    fn into_reference(self, level: i32) -> std::io::Result<ReferenceCorpus> {
        let mut blocks = Vec::with_capacity(self.hours.len());
        for (hour, writer) in self.hours {
            drop(
                writer
                    .into_inner()
                    .map_err(std::io::IntoInnerError::into_error)?,
            );
            let path = self.dir.path().join(hour.to_string());
            let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level)?;
            std::io::copy(&mut reader, &mut encoder)?;
            blocks.push(encoder.finish()?);
        }
        Ok(ReferenceCorpus::from_blocks(blocks))
    }
}

/// Span / size summary shared by both store builders.
struct StoreCore {
    rows: u64,
    files: u64,
    min_effective_time_unix_nano: u64,
    max_effective_time_unix_nano: u64,
}

/// The shared load → mine → write pipeline behind
/// [`build_query_store`] and [`build_b1_store`]. `observe` runs once
/// per successfully-appended record — its third argument is the
/// record's effective timestamp (the shared writer/partition
/// derivation, so the bookkeeping can never disagree with the
/// store) — and its first error aborts the build (surfaced after the
/// harness loop, same stash pattern as `a1::A1Accumulator`).
fn build_store(
    corpus_dir: &Path,
    bucket_root: &Path,
    txt_severity: corpus::TxtSeverity,
    mut observe: impl FnMut(&OtlpLogRecord, &MinedRecord, u64) -> Result<(), BenchError>,
) -> Result<StoreCore, BenchError> {
    // A reused bucket would let the querier enumerate a prior run's
    // Parquet too, mixing corpora and skewing both the row counts
    // and the latency measurement. Reject up front (the A1 path
    // guards the same way via `ensure_bucket_has_no_parquet`).
    if let Some(existing) = crate::find_published_parquet(bucket_root)? {
        return Err(BenchError::Pipeline {
            detail: format!(
                "bucket {} already contains a Parquet file ({}); build_query_store \
                 needs an empty bucket so the querier doesn't mix corpora",
                bucket_root.display(),
                existing.display(),
            ),
        });
    }

    // Stream rather than `corpus::load`: the eager `Vec<OtlpLogRecord>`
    // costs ~2-4x the raw corpus bytes, which caps the loadable corpus
    // well below the B1/B2 scale targets; mine + flush below are
    // per-record, so streaming keeps peak memory flat at any size.
    let (stream, files_meta) = corpus::stream(corpus_dir, txt_severity)?;

    let mut writers: HashMap<PartitionKey, Writer> = HashMap::new();
    let mut rows: u64 = 0;
    // Track the corpus's *effective*-timestamp span so the benches can
    // pick a real time window — the query window filters the effective
    // column (RFC 0002 §6.2 / RFC 0005 §3.2). Only non-zero values
    // count (`0` means genuinely timeless: the epoch partition, not a
    // meaningful window bound).
    let mut min_ts = u64::MAX;
    let mut max_ts = 0u64;
    // The harness callback returns `()`, so a write/observe error is
    // stashed (first wins) and surfaced after the run — the same
    // pattern `a1::A1Accumulator` uses.
    let mut first_err: Option<BenchError> = None;

    harness::run_streaming(stream, false, |input, emitted, _snap| {
        if first_err.is_some() {
            return;
        }
        let appended = effective_nanos(emitted).and_then(|effective| {
            append_record(&mut writers, bucket_root, emitted)?;
            observe(input, emitted, effective)?;
            Ok(effective)
        });
        match appended {
            Ok(effective) => {
                rows += 1;
                if effective != 0 {
                    min_ts = min_ts.min(effective);
                    max_ts = max_ts.max(effective);
                }
            }
            Err(e) => first_err = Some(e),
        }
    })?;

    if let Some(e) = first_err {
        return Err(e);
    }
    if rows == 0 {
        // Parity with `corpus::load`'s empty-corpus rejection: a store
        // with zero rows would make every query trivially instant.
        return Err(corpus::no_lines_error(corpus_dir, files_meta.total_files));
    }

    let files = u64::try_from(writers.len()).expect("usize fits in u64 on every supported target");
    for (_partition, writer) in writers {
        writer.close().map_err(|e| BenchError::Pipeline {
            detail: format!("parquet close: {e}"),
        })?;
    }

    // No non-zero effective timestamp ⇒ no meaningful span (0, 0).
    let (min_effective_time_unix_nano, max_effective_time_unix_nano) = if min_ts == u64::MAX {
        (0, 0)
    } else {
        (min_ts, max_ts)
    };

    Ok(StoreCore {
        rows,
        files,
        min_effective_time_unix_nano,
        max_effective_time_unix_nano,
    })
}

/// The record's RFC 0005 §3.2 effective timestamp in the `u64` wire
/// domain — `ourios_parquet::effective_time_unix_nano`, the same
/// derivation the writer stores and the partition tuple uses, so the
/// bench's span / eligibility bookkeeping can never disagree with
/// what the query window filters.
fn effective_nanos(emitted: &MinedRecord) -> Result<u64, BenchError> {
    let effective =
        ourios_parquet::effective_time_unix_nano(emitted).map_err(|e| BenchError::Pipeline {
            detail: format!("effective timestamp derive failed: {e}"),
        })?;
    // The derivation validates both candidates against the u64→i64
    // overflow contract, so the i64 is never negative; keep the
    // conversion total anyway rather than panicking.
    u64::try_from(effective).map_err(|_| BenchError::Pipeline {
        detail: format!("effective timestamp {effective} is negative"),
    })
}

/// Append one record into its partition's writer, opening one on the
/// first record for a partition (mirrors `a1::A1Accumulator`).
fn append_record(
    writers: &mut HashMap<PartitionKey, Writer>,
    bucket_root: &Path,
    emitted: &MinedRecord,
) -> Result<(), BenchError> {
    let partition = PartitionKey::derive(emitted).map_err(|e| BenchError::Pipeline {
        detail: format!("partition derive failed: {e}"),
    })?;
    if let Some(writer) = writers.get_mut(&partition) {
        return writer
            .append_records(std::slice::from_ref(emitted))
            .map_err(|e| BenchError::Pipeline {
                detail: format!("parquet append_records: {e}"),
            });
    }
    let mut writer =
        Writer::open(bucket_root, partition.clone()).map_err(|e| BenchError::Pipeline {
            detail: format!("parquet open: {e}"),
        })?;
    writer
        .append_records(std::slice::from_ref(emitted))
        .map_err(|e| BenchError::Pipeline {
            detail: format!("parquet append_records: {e}"),
        })?;
    writers.insert(partition, writer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bucket that already holds a published `*.parquet` is
    /// rejected, so a reused dir can't silently mix corpora into the
    /// B2 query (regression guard for the second-build case).
    #[test]
    fn rejects_a_non_empty_bucket() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(corpus.path().join("c.txt"), b"user 42 logged in\n").expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let first = build_query_store(corpus.path(), bucket.path(), corpus::TxtSeverity::Fixed)
            .expect("first build");
        assert!(first.rows >= 1, "the one corpus line is written");

        let second = build_query_store(corpus.path(), bucket.path(), corpus::TxtSeverity::Fixed);
        assert!(
            matches!(second, Err(BenchError::Pipeline { .. })),
            "a reused, non-empty bucket must be rejected, got {second:?}",
        );
    }

    /// The timestamp span the B2 windowed arm keys off: the text loader
    /// assigns `TIME_BASELINE_NS + i * TIME_INCREMENT_NS` per line, so a
    /// 3-line corpus spans `[baseline, baseline + 2·increment]`.
    #[test]
    fn tracks_the_timestamp_span() {
        use crate::corpus::{TIME_BASELINE_NS, TIME_INCREMENT_NS};

        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("c.txt"),
            b"login user 1\nlogout user 2\nerror code 3\n",
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_query_store(corpus.path(), bucket.path(), corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(built.rows, 3, "one record per line");
        assert_eq!(
            built.min_effective_time_unix_nano, TIME_BASELINE_NS,
            "span start"
        );
        assert_eq!(
            built.max_effective_time_unix_nano,
            TIME_BASELINE_NS + 2 * TIME_INCREMENT_NS,
            "span end (3rd line)",
        );
    }

    /// One `LogsData` line with `n` records at `severity_text` /
    /// `severity_number`, bodies `"<text> event <i>"`, timestamps
    /// `base + i` ns.
    fn logs_data_line(n: usize, text: &str, number: u8, base: u64) -> String {
        let records: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "{{\"timeUnixNano\":\"{}\",\"severityNumber\":{number},\
                     \"severityText\":\"{text}\",\
                     \"body\":{{\"stringValue\":\"{text} event {i}\"}}}}",
                    base + u64::try_from(i).expect("usize fits in u64"),
                )
            })
            .collect();
        format!(
            "{{\"resourceLogs\":[{{\"scopeLogs\":[{{\"logRecords\":[{}]}}]}}]}}",
            records.join(","),
        )
    }

    /// Like [`logs_data_line`], but **observed-only**: `timeUnixNano`
    /// is absent from the wire (the OTLP "source timestamp unknown"
    /// case), `observedTimeUnixNano` carries `base + i` ns.
    fn observed_only_logs_data_line(n: usize, text: &str, number: u8, base: u64) -> String {
        let records: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "{{\"observedTimeUnixNano\":\"{}\",\"severityNumber\":{number},\
                     \"severityText\":\"{text}\",\
                     \"body\":{{\"stringValue\":\"{text} event {i}\"}}}}",
                    base + u64::try_from(i).expect("usize fits in u64"),
                )
            })
            .collect();
        format!(
            "{{\"resourceLogs\":[{{\"scopeLogs\":[{{\"logRecords\":[{}]}}]}}]}}",
            records.join(","),
        )
    }

    /// RFC 0005 §3.2 rule 7 (the RFC0005.13 bench follow-up) — an
    /// observed-only corpus (`timeUnixNano` absent, ~15 % of real
    /// OTel-Demo records) is **B1-eligible**: the bookkeeping keys
    /// off the effective timestamp, so `zero_effective_ts_rows`
    /// stays 0, the span derives from the observed values, and every
    /// line lands in the reference corpus. These are exactly the
    /// outputs the `benches/b1.rs` `severity_query` guard checks.
    #[test]
    fn b1_store_with_observed_only_rows_is_eligible() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        let base = crate::corpus::TIME_BASELINE_NS;
        let jsonl = format!(
            "{}\n{}\n",
            observed_only_logs_data_line(5, "INFO", 9, base),
            observed_only_logs_data_line(3, "ERROR", 17, base + 1_000),
        );
        std::fs::write(corpus.path().join("c.jsonl"), jsonl).expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Fixed)
            .expect("build");

        // The b1 eligibility guard: a usable span and no
        // zero-effective-ts rows.
        assert_eq!(
            built.zero_effective_ts_rows, 0,
            "observed-only rows have a non-zero effective timestamp",
        );
        assert_eq!(
            built.min_effective_time_unix_nano, base,
            "span start derives from the observed fallback",
        );
        assert_eq!(
            built.max_effective_time_unix_nano,
            base + 1_002,
            "span end is the last ERROR record's observed instant",
        );
        // The query predicate and the reference corpus both see the
        // full row set — nothing was dropped as out-of-window.
        assert_eq!(
            built.query_severity,
            Some(("ERROR".to_string(), 3)),
            "the B1 predicate is unaffected by the timestamp source",
        );
        assert_eq!(
            built
                .reference
                .count_lines_containing("ERROR")
                .expect("reference grep"),
            3,
            "observed-only rows are spooled into the reference",
        );
    }

    /// B1 store over an OTLP corpus with a real severity mix: the
    /// "ERROR" text is preferred for the query predicate, its row
    /// count is exact, the severity distribution is visible (the
    /// selectivity guard's input), and the rendered reference corpus
    /// greps to at least the ERROR row count (severity-prefixed
    /// lines guarantee ≥; body text may add more).
    #[test]
    fn b1_store_prefers_error_and_renders_a_grep_consistent_reference() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        let base = crate::corpus::TIME_BASELINE_NS;
        let jsonl = format!(
            "{}\n{}\n",
            logs_data_line(5, "INFO", 9, base),
            logs_data_line(3, "ERROR", 17, base + 1_000),
        );
        std::fs::write(corpus.path().join("c.jsonl"), jsonl).expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(built.rows, 8);
        assert_eq!(built.zero_effective_ts_rows, 0);
        assert_eq!(built.distinct_severities, 2, "INFO + ERROR");
        assert_eq!(
            built.query_severity,
            Some(("ERROR".to_string(), 3)),
            "the literal ERROR text wins with its exact row count",
        );
        assert_eq!(
            built
                .reference
                .count_lines_containing("ERROR")
                .expect("reference grep"),
            3,
            "every ERROR record's reference line carries the token",
        );
        assert_eq!(built.min_effective_time_unix_nano, base, "span start");
    }

    /// Without a literal "ERROR" text, the busiest error-band
    /// (severity 17..=20) text is chosen — real SDKs spell the level
    /// per-language ("Error", "error", …).
    #[test]
    fn b1_store_falls_back_to_the_busiest_error_band_text() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        let base = crate::corpus::TIME_BASELINE_NS;
        let jsonl = format!(
            "{}\n{}\n{}\n",
            logs_data_line(4, "Information", 9, base),
            logs_data_line(2, "Error", 17, base + 1_000),
            logs_data_line(1, "Critical", 21, base + 2_000),
        );
        std::fs::write(corpus.path().join("c.jsonl"), jsonl).expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(
            built.query_severity,
            Some(("Error".to_string(), 2)),
            "the busiest error-band text is chosen; Critical (21) is outside the band",
        );
    }

    /// The query filters on severity *text*, so the literal "ERROR"
    /// wins even when the corpus maps it to a severity number outside
    /// the OTLP error band — it must not lose to a band text or make
    /// the corpus skip.
    #[test]
    fn b1_store_prefers_error_text_even_outside_the_band() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        let base = crate::corpus::TIME_BASELINE_NS;
        let jsonl = format!(
            "{}\n{}\n",
            logs_data_line(3, "ERROR", 9, base),
            logs_data_line(2, "Error", 17, base + 1_000),
        );
        std::fs::write(corpus.path().join("c.jsonl"), jsonl).expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(
            built.query_severity,
            Some(("ERROR".to_string(), 3)),
            "literal ERROR wins regardless of its severity number",
        );
    }

    /// Zero-ts rows sit outside any query window (the B1 arm skips
    /// corpora carrying them), so they must not leak into the
    /// reference corpus either.
    #[test]
    fn b1_reference_excludes_zero_effective_ts_rows() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        let base = crate::corpus::TIME_BASELINE_NS;
        let jsonl = format!(
            "{}\n{}\n",
            logs_data_line(1, "ERRZERO", 17, 0),
            logs_data_line(2, "ERROR", 17, base),
        );
        std::fs::write(corpus.path().join("c.jsonl"), jsonl).expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(built.zero_effective_ts_rows, 1);
        assert_eq!(
            built
                .reference
                .count_lines_containing("ERRZERO")
                .expect("reference grep"),
            0,
            "the zero-ts record's line is not spooled into the reference",
        );
        assert_eq!(
            built
                .reference
                .count_lines_containing("ERROR")
                .expect("reference grep"),
            2,
            "in-window rows are unaffected",
        );
    }

    /// A plain-text corpus collapses to a single severity (the
    /// RFC 0006 §3.3 loader fixes every line at `9` / `INFO`), so the
    /// B1 arm's selectivity guard sees `distinct_severities == 1` and
    /// `query_severity == None` (INFO is not in the error band) —
    /// the signals the bench uses to skip plain-text corpora.
    #[test]
    fn b1_store_over_plain_text_has_no_severity_selectivity() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("c.txt"),
            b"ERROR request failed id=1\nINFO request ok id=2\n",
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(built.distinct_severities, 1, "loader forces INFO on text");
        assert_eq!(
            built.query_severity, None,
            "INFO (9) is not in the error band — nothing to query",
        );
    }

    /// The opt-in `Log4j` severity mode gives a plain-text corpus real
    /// selectivity: level tokens map to distinct severities and the
    /// error band yields a B1 query predicate. The `Fixed` default
    /// above stays the §3.3 baseline.
    #[test]
    fn b1_store_over_log4j_text_gets_error_band_selectivity() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("c.txt"),
            b"081109 INFO dfs.DataNode: ok id=1\n081109 ERROR dfs.DataNode: failed id=2\n081109 WARN dfs.DataNode: slow id=3\n",
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_b1_store(corpus.path(), bucket.path(), 3, corpus::TxtSeverity::Log4j)
            .expect("build");

        assert_eq!(built.distinct_severities, 3, "INFO/WARN/ERROR extracted");
        assert_eq!(
            built.query_severity,
            Some(("ERROR".to_string(), 1)),
            "the error band yields the B1 predicate with its row count",
        );
    }

    /// When no record carries a non-zero `time_unix_nano` (an OTLP/JSON
    /// corpus with the field absent), the span is reported as `(0, 0)` —
    /// so the windowed B2 arm skips rather than picking a bogus window.
    #[test]
    fn reports_zero_span_when_all_timestamps_are_zero() {
        let corpus = tempfile::TempDir::new().expect("corpus dir");
        std::fs::write(
            corpus.path().join("c.jsonl"),
            b"{\"resourceLogs\":[{\"scopeLogs\":[{\"logRecords\":\
              [{\"body\":{\"stringValue\":\"no timestamp here\"}}]}]}]}\n",
        )
        .expect("write corpus");
        let bucket = tempfile::TempDir::new().expect("bucket dir");

        let built = build_query_store(corpus.path(), bucket.path(), corpus::TxtSeverity::Fixed)
            .expect("build");

        assert_eq!(built.rows, 1, "the one record is written");
        assert_eq!(
            built.min_effective_time_unix_nano, 0,
            "no non-zero timestamp → 0"
        );
        assert_eq!(
            built.max_effective_time_unix_nano, 0,
            "no non-zero timestamp → 0"
        );
    }
}
