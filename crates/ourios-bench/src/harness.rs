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
/// **Snapshot capture is O(N + W · T)** where N is the line
/// count, W the number of unique `(template_id,
/// template_version)` pairs observed across the corpus, and T
/// the current template count: we drain the emitted record
/// per-iteration and only walk `cluster.templates_for(...)`
/// when the record's `(template_id, template_version)` hasn't
/// been snapshotted yet. The H7.1 pattern walks every leaf
/// after every ingest (O(N · T)); for an RFC-sized
/// 1 M-line / 10⁴-template corpus that's roughly a 10⁴×
/// speed-up on the snapshot path. Semantics are equivalent —
/// version monotonicity (the miner never unmerges) means a
/// `(id, v)` we've already captured stays valid.
///
/// # Errors
///
/// Returns [`BenchError::Pipeline`] when the miner's
/// post-ingest record count diverges from the input line
/// count. The miner's RFC 0001 §6.1 emit contract says "one
/// record per ingested line"; a mismatch is a contract
/// violation that the bench surfaces as a hard error rather
/// than letting C1 silently compute against misaligned data.
/// Also returned when a snapshot can't be located for a
/// freshly-emitted `(id, v)` — that indicates the cluster's
/// `templates_for` returned a leaf list inconsistent with the
/// just-emitted record, which is a contract violation worth
/// surfacing.
pub(crate) fn run(corpus: CorpusLoad) -> Result<HarnessOutput, BenchError> {
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let tenant = TenantId::new(BENCH_TENANT);
    let mut snapshots: HashMap<(u64, u32), Vec<OwnedToken>> = HashMap::new();
    let mut records: Vec<MinedRecord> = Vec::with_capacity(corpus.lines.len());

    for cline in &corpus.lines {
        cluster.ingest(&cline.record);
        let mut emitted = sink.drain();
        if emitted.len() != 1 {
            return Err(BenchError::Pipeline {
                detail: format!(
                    "miner emitted {} record(s) for one ingested line — RFC 0001 §6.1 \
                     pins one-record-per-line",
                    emitted.len(),
                ),
            });
        }
        let record = emitted.pop().expect("len == 1 checked above");
        let key = (record.template_id, record.template_version);
        if let std::collections::hash_map::Entry::Vacant(slot) = snapshots.entry(key) {
            // First time we've seen this (id, version). Walk
            // `templates_for` once to capture the leaf's
            // template tokens. Subsequent emissions at the
            // same (id, version) skip the walk entirely —
            // see the O(N + W · T) note in the function
            // docstring.
            let snap = cluster
                .templates_for(&tenant)
                .into_iter()
                .find(|s| {
                    s.template_id == record.template_id
                        && s.template_version == record.template_version
                })
                .ok_or_else(|| BenchError::Pipeline {
                    detail: format!(
                        "miner emitted record at (template_id={}, template_version={}) \
                         but templates_for() returned no matching leaf — RFC 0001 §6.1 \
                         contract violation",
                        record.template_id, record.template_version,
                    ),
                })?;
            slot.insert(snap.template);
        }
        records.push(record);
    }

    // Sanity guard — even though we drained per-iteration,
    // a stray `MinerCluster::ingest` side-effect that pushed
    // more records would surface here too.
    if records.len() != corpus.lines.len() {
        return Err(BenchError::Pipeline {
            detail: format!(
                "miner emitted {} record(s) total for {} ingested line(s) — RFC 0001 §6.1 \
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

#[cfg(test)]
mod tests {
    //! Colocated harness invariants. The C1 integration test
    //! in `tests/c1.rs` exercises the full corpus → harness →
    //! C1 pipeline end-to-end; these unit tests pin the
    //! harness contracts (count parity + per-version
    //! snapshot capture) on small synthetic corpora so a
    //! refactor that breaks them surfaces before the slower
    //! integration suite runs.
    use super::*;
    use crate::corpus;
    use std::io::Write;

    /// Build a fixture corpus directory with `lines` written
    /// to a single `*.txt` file. Returns the `CorpusLoad`
    /// produced by `corpus::load` against it.
    fn load_lines(lines: &[&str]) -> (tempfile::TempDir, CorpusLoad) {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("fixture.txt");
        let mut file = std::fs::File::create(&path).expect("create fixture");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        drop(file);
        let load = corpus::load(tmp.path()).expect("fixture loads");
        (tmp, load)
    }

    /// One record emitted per ingested line — the RFC 0001
    /// §6.1 contract the harness asserts.
    #[test]
    fn count_parity_holds_on_a_small_corpus() {
        let (_tmp, load) = load_lines(&["user 42 logged in", "user 43 logged in"]);
        let line_count = load.lines.len();
        let out = run(load).expect("harness runs");
        assert_eq!(
            out.records.len(),
            line_count,
            "one MinedRecord per ingested line per RFC 0001 §6.1",
        );
        assert_eq!(
            out.lines.len(),
            line_count,
            "lines preserved alongside records"
        );
    }

    /// Every emitted `(template_id, template_version)` pair
    /// has a snapshot captured. Pins the §3.4.2 lookup
    /// contract — `c1::compute` panics if a snapshot is
    /// missing, so the harness must guarantee one per emitted
    /// (id, version) before c1 consumes its output.
    #[test]
    fn every_emitted_id_version_has_a_snapshot() {
        let (_tmp, load) = load_lines(&[
            "user 42 logged in",
            "user 43 logged in",
            "user 44 logged in",
            // A divergent line forces widening — leaf gains
            // a `<*>` slot and bumps `template_version`.
            "admin 99 logged out",
        ]);
        let out = run(load).expect("harness runs");

        for record in &out.records {
            let key = (record.template_id, record.template_version);
            assert!(
                out.snapshots.contains_key(&key),
                "snapshot missing for emitted record at {key:?}",
            );
        }
    }
}
