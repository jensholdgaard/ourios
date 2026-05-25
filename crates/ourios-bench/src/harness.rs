//! Per-line ingest harness.
//!
//! Drives a fresh `MinerCluster` over every line in a loaded
//! corpus, captures the emitted `MinedRecord`s, and snapshots
//! the per-`(template_id, template_version)` template tokens
//! so RFC0006.2's reconstruction comparison can look up the
//! template that was active at each record's emit-time
//! version. Mirrors the H7.1 property test pattern in
//! `crates/ourios-miner/tests/hazards.rs` — proven against
//! the seed corpus, the same shape the bench needs.
//!
//! Today the harness only produces the inputs C1 needs.
//! Future implementation PRs will extend it to:
//!
//! - Stream records into `ourios_parquet::Writer` per
//!   `BenchConfig.gates.a1`, so A1 can sum the bucket bytes.
//! - Sample `cluster.template_count()` at the §3.4.3 cadence
//!   per `BenchConfig.gates.c2`, so C2 can compute the
//!   convergence curve.
//!
//! Both extensions plug into the same per-line loop; this
//! module shrinks back to a thin adapter when A1/C2 land.

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::record::{MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::tree::OwnedToken;

use crate::BenchError;
use crate::corpus::{BENCH_TENANT, CorpusLine, CorpusLoad};

/// Output of [`run`]: every loaded line paired with the
/// emitted record, plus the per-version template snapshots
/// the C1 reconstruction compare needs.
pub(crate) struct HarnessOutput {
    /// Original lines (input bytes + the OTLP record fed to
    /// the miner). Indexed alongside `records`: line[i] is the
    /// input that produced records[i].
    pub lines: Vec<CorpusLine>,
    /// One emitted record per ingested line, in order.
    pub records: Vec<MinedRecord>,
    /// Template tokens snapshotted by emit-time
    /// `(template_id, template_version)`. The H7.1
    /// `or_insert_with` pattern: first observation wins, so a
    /// later widening that bumps to `(id, v+1)` doesn't
    /// clobber `(id, v)`'s snapshot.
    pub snapshots: HashMap<(u64, u32), Vec<OwnedToken>>,
}

/// Drive the miner over every line in `corpus` and return the
/// raw data C1 (and later A1 / C2) need.
///
/// # Errors
///
/// Returns [`BenchError::Pipeline`] when the miner's
/// post-ingest record count diverges from the input line
/// count. The miner's RFC 0001 §6.1 emit contract says "one
/// record per ingested line"; a mismatch is a contract
/// violation that the bench surfaces as a hard error rather
/// than letting C1 silently compute against misaligned data.
pub(crate) fn run(corpus: CorpusLoad) -> Result<HarnessOutput, BenchError> {
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let tenant = TenantId::new(BENCH_TENANT);
    let mut snapshots: HashMap<(u64, u32), Vec<OwnedToken>> = HashMap::new();

    for cline in &corpus.lines {
        cluster.ingest(&cline.record);
        // Snapshot every active template after each ingest
        // (per the H7.1 pattern). `or_insert_with` keeps the
        // first observation of each (id, version) pair — so a
        // later attach that widens the same leaf to
        // (id, version + 1) creates a new entry without
        // overwriting the original. C1's reconstruction looks
        // up the exact (id, version) the emitted record
        // carries.
        for snap in cluster.templates_for(&tenant) {
            snapshots
                .entry((snap.template_id, snap.template_version))
                .or_insert(snap.template);
        }
    }

    let records = sink.drain();
    if records.len() != corpus.lines.len() {
        return Err(BenchError::Pipeline {
            detail: format!(
                "miner emitted {} record(s) for {} ingested line(s) — RFC 0001 §6.1 \
                 pins one-record-per-line",
                records.len(),
                corpus.lines.len(),
            ),
        });
    }

    Ok(HarnessOutput {
        lines: corpus.lines,
        records,
        snapshots,
    })
}
