//! Build a queryable RFC 0005 Parquet store from a corpus.
//!
//! The A1 path measures the bytes a corpus compresses to; the B2
//! latency bench needs to *query* the same corpus, so it needs the
//! mined records laid down as a real partitioned Parquet store it
//! can point a [`ourios_querier::Querier`] at. This is the only
//! public store-builder the bench exposes — it reuses the same
//! corpus loader and miner harness the gates run on (so the store
//! matches what A1 measured), then writes every emitted record via
//! per-partition [`Writer`]s (the same streaming write A1 uses).

use std::collections::HashMap;
use std::path::Path;

use ourios_core::record::MinedRecord;
use ourios_parquet::{PartitionKey, Writer};

use crate::{BenchError, corpus, harness};

/// What [`build_query_store`] wrote, enough for the B2 bench to pick
/// a populated query and report the result-vs-corpus relationship.
#[derive(Debug, Clone, Copy)]
pub struct BuiltStore {
    /// Tenant every record was written under (the corpus loader is
    /// single-tenant — [`crate::corpus`]'s `BENCH_TENANT`). A query must
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
    /// Smallest non-zero `time_unix_nano` written (`0` if none) — the
    /// start of the corpus's time span, for picking a B2 query window.
    pub min_time_unix_nano: u64,
    /// Largest `time_unix_nano` written (`0` if none) — the end of the
    /// corpus's time span.
    pub max_time_unix_nano: u64,
}

/// Load the corpus at `corpus_dir`, mine it, and write the emitted
/// records as a partitioned RFC 0005 Parquet store under
/// `bucket_root` (which a [`ourios_querier::Querier`] can then be
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
pub fn build_query_store(corpus_dir: &Path, bucket_root: &Path) -> Result<BuiltStore, BenchError> {
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

    let load = corpus::load(corpus_dir)?;

    let mut writers: HashMap<PartitionKey, Writer> = HashMap::new();
    let mut counts: HashMap<u64, u64> = HashMap::new();
    let mut rows: u64 = 0;
    // Track the corpus's `time_unix_nano` span so the B2 bench can pick
    // a real time window for the partition-pruning measurement. Only
    // non-zero timestamps count (a `0` falls back to observed/epoch for
    // partitioning — not a meaningful window bound).
    let mut min_ts = u64::MAX;
    let mut max_ts = 0u64;
    // The harness callback returns `()`, so a write error is stashed
    // (first wins) and surfaced after the run — the same pattern
    // `a1::A1Accumulator` uses.
    let mut first_err: Option<BenchError> = None;

    harness::run(&load, false, |_input, emitted, _snap| {
        if first_err.is_some() {
            return;
        }
        match append_record(&mut writers, bucket_root, emitted) {
            Ok(()) => {
                rows += 1;
                *counts.entry(emitted.template_id).or_insert(0) += 1;
                if emitted.time_unix_nano != 0 {
                    min_ts = min_ts.min(emitted.time_unix_nano);
                    max_ts = max_ts.max(emitted.time_unix_nano);
                }
            }
            Err(e) => first_err = Some(e),
        }
    })?;

    if let Some(e) = first_err {
        return Err(e);
    }

    let files = u64::try_from(writers.len()).expect("usize fits in u64 on every supported target");
    for (_partition, writer) in writers {
        writer.close().map_err(|e| BenchError::Pipeline {
            detail: format!("parquet close: {e}"),
        })?;
    }

    let (busiest_template_id, busiest_template_rows) =
        counts.into_iter().max_by_key(|&(_, n)| n).unwrap_or((0, 0));
    // No non-zero timestamp seen ⇒ no meaningful span (report 0, 0).
    let (min_time_unix_nano, max_time_unix_nano) = if min_ts == u64::MAX {
        (0, 0)
    } else {
        (min_ts, max_ts)
    };

    Ok(BuiltStore {
        tenant: crate::corpus::BENCH_TENANT,
        rows,
        files,
        busiest_template_id,
        busiest_template_rows,
        min_time_unix_nano,
        max_time_unix_nano,
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

        let first = build_query_store(corpus.path(), bucket.path()).expect("first build");
        assert!(first.rows >= 1, "the one corpus line is written");

        let second = build_query_store(corpus.path(), bucket.path());
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

        let built = build_query_store(corpus.path(), bucket.path()).expect("build");

        assert_eq!(built.rows, 3, "one record per line");
        assert_eq!(built.min_time_unix_nano, TIME_BASELINE_NS, "span start");
        assert_eq!(
            built.max_time_unix_nano,
            TIME_BASELINE_NS + 2 * TIME_INCREMENT_NS,
            "span end (3rd line)",
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

        let built = build_query_store(corpus.path(), bucket.path()).expect("build");

        assert_eq!(built.rows, 1, "the one record is written");
        assert_eq!(built.min_time_unix_nano, 0, "no non-zero timestamp → 0");
        assert_eq!(built.max_time_unix_nano, 0, "no non-zero timestamp → 0");
    }
}
