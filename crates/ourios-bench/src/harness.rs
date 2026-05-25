//! Per-line ingest harness.
//!
//! Drives a fresh `MinerCluster` over every line in a loaded
//! corpus and yields one `(input, emitted, template)` triple
//! per ingested line to a caller-supplied callback. The
//! callback shape lets each gate (C1 today, A1 / C2 in
//! follow-ups) compute its result *while* the loop runs,
//! without the harness buffering every emitted `MinedRecord`
//! in memory — a `Vec<MinedRecord>` for an RFC-sized 1 M-line
//! corpus is a real OOM risk given each record's `Vec` /
//! `String` payload.
//!
//! Mirrors the H7.1 property test pattern in
//! `crates/ourios-miner/tests/hazards.rs` for the snapshot
//! capture (per-`(template_id, template_version)` token
//! tokens via `or_insert`); the streaming surface is the
//! bench-specific shape.
//!
//! Future implementation PRs plug additional gate
//! accumulators into the same callback signature:
//!
//! - A1: a writer-accumulator that streams `MinedRecord`s
//!   into `ourios_parquet::Writer` and sums the bucket bytes.
//! - C2: a counter-accumulator that samples the template
//!   count at the §3.4.3 cadence.

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::{MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_miner::tree::OwnedToken;

use crate::BenchError;
use crate::corpus::{BENCH_TENANT, CorpusLoad};

/// Drive the miner over every line in `corpus`, snapshotting
/// per-`(template_id, template_version)` template tokens
/// once per unique pair, and invoking `on_record` per
/// ingested line with `(input, emitted, snapshot)`.
///
/// `snapshot` is `None` when the emitted record is lossy
/// (`record.lossy_flag = true`) or carries the
/// [`NO_TEMPLATE`] sentinel (`template_id = 0`, the
/// parse-failure path per RFC 0001 §6.6). Non-lossy records
/// always receive `Some(template_tokens)`; a missing snapshot
/// for a real `(id, v)` pair surfaces as
/// [`BenchError::Pipeline`] (the cluster's `templates_for()`
/// returned a leaf list inconsistent with the just-emitted
/// record, an RFC 0001 §6.1 contract violation).
///
/// **Snapshot capture is O(N + W · T)** where N is the line
/// count, W the number of unique non-lossy `(id, v)` pairs
/// observed, and T the current template count. We only walk
/// `cluster.templates_for(...)` when a new `(id, v)` shows
/// up; subsequent emissions at the same pair reuse the
/// snapshot. Version monotonicity (the miner never unmerges)
/// means a `(id, v)` we've already captured stays valid.
///
/// # Errors
///
/// - [`BenchError::Pipeline`] when the miner's emit count for
///   one ingested line is anything other than 1 (RFC 0001
///   §6.1 one-record-per-line violation).
/// - [`BenchError::Pipeline`] when a non-lossy emitted record
///   carries a `(template_id, template_version)` that the
///   cluster's `templates_for()` doesn't return a leaf for
///   (impossible by construction; surfaces a future miner
///   bug rather than crashing C1's snapshot lookup).
pub(crate) fn run<F>(
    corpus: &CorpusLoad,
    mut on_record: F,
) -> Result<HashMap<(u64, u32), Vec<OwnedToken>>, BenchError>
where
    F: FnMut(&OtlpLogRecord, &MinedRecord, Option<&[OwnedToken]>),
{
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let tenant = TenantId::new(BENCH_TENANT);
    let mut snapshots: HashMap<(u64, u32), Vec<OwnedToken>> = HashMap::new();

    for input in &corpus.lines {
        cluster.ingest(input);
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

        // Skip the snapshot capture for lossy / parse-failure
        // records. Lossy rows are excluded from C1's
        // numerator and denominator per §3.4.2, and a
        // `template_id == NO_TEMPLATE` (0) emit has no leaf
        // by construction — `templates_for()` won't find it,
        // and we don't need it either.
        let want_snapshot = !record.lossy_flag && record.template_id != NO_TEMPLATE;
        if want_snapshot {
            let key = (record.template_id, record.template_version);
            if let std::collections::hash_map::Entry::Vacant(slot) = snapshots.entry(key) {
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
        }

        let template_snapshot = if want_snapshot {
            snapshots
                .get(&(record.template_id, record.template_version))
                .map(Vec::as_slice)
        } else {
            None
        };

        on_record(input, &record, template_snapshot);
    }

    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    //! Colocated harness invariants. The C1 integration test
    //! in `tests/c1.rs` exercises the full corpus → harness →
    //! C1 pipeline end-to-end; these unit tests pin the
    //! harness contracts (count parity + per-version
    //! snapshot capture + the parse-failure carve-out) on
    //! small synthetic corpora so a refactor that breaks them
    //! surfaces before the slower integration suite runs.
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

    /// One callback invocation per ingested line — the
    /// RFC 0001 §6.1 emit contract the harness asserts.
    #[test]
    fn callback_fires_once_per_ingested_line() {
        let (_tmp, load) = load_lines(&["user 42 logged in", "user 43 logged in"]);
        let line_count = load.lines.len();
        let mut count = 0usize;
        run(&load, |_input, _record, _snap| count += 1).expect("harness runs");
        assert_eq!(count, line_count);
    }

    /// Every non-lossy emitted record's callback gets
    /// `Some(template)`; lossy / `NO_TEMPLATE` records get
    /// `None` without failing the harness. Pins the §3.4.2
    /// lookup contract that c1 relies on: non-lossy needs a
    /// snapshot, lossy doesn't.
    #[test]
    fn non_lossy_callbacks_carry_a_template_snapshot() {
        let (_tmp, load) = load_lines(&[
            "user 42 logged in",
            "user 43 logged in",
            "user 44 logged in",
            // Divergent line forces widening — leaf bumps
            // `template_version`.
            "admin 99 logged out",
        ]);
        let mut non_lossy_seen = 0usize;
        let mut non_lossy_with_snapshot = 0usize;
        run(&load, |_input, record, snap| {
            if !record.lossy_flag && record.template_id != NO_TEMPLATE {
                non_lossy_seen += 1;
                if snap.is_some() {
                    non_lossy_with_snapshot += 1;
                }
            }
        })
        .expect("harness runs");
        assert!(non_lossy_seen > 0);
        assert_eq!(
            non_lossy_seen, non_lossy_with_snapshot,
            "every non-lossy non-NO_TEMPLATE record must receive a snapshot",
        );
    }

    /// Returned snapshot map carries entries keyed by
    /// `(template_id, template_version)` for every distinct
    /// non-lossy record the harness saw — the C1 path doesn't
    /// strictly need this (the streaming callback already had
    /// the snapshot), but it's a useful diagnostic + lets
    /// future gates inspect the alphabet without re-running.
    #[test]
    fn snapshot_map_covers_every_non_lossy_id_version_observed() {
        let (_tmp, load) = load_lines(&["user 42 logged in", "user 43 logged in"]);
        let mut emitted_keys = std::collections::HashSet::new();
        let snapshots = run(&load, |_input, record, _snap| {
            if !record.lossy_flag && record.template_id != NO_TEMPLATE {
                emitted_keys.insert((record.template_id, record.template_version));
            }
        })
        .expect("harness runs");
        for key in emitted_keys {
            assert!(
                snapshots.contains_key(&key),
                "snapshot map missing entry for emitted non-lossy key {key:?}",
            );
        }
    }
}
