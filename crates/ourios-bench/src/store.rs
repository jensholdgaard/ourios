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
    let load = corpus::load(corpus_dir)?;

    let mut writers: HashMap<PartitionKey, Writer> = HashMap::new();
    let mut counts: HashMap<u64, u64> = HashMap::new();
    let mut rows: u64 = 0;
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

    Ok(BuiltStore {
        rows,
        files,
        busiest_template_id,
        busiest_template_rows,
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
