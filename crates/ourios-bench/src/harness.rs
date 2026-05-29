//! Per-line ingest harness.
//!
//! Drives a fresh `MinerCluster` over every line in a loaded
//! corpus and yields one `(input, emitted, template)` triple
//! per ingested line to a caller-supplied callback. The
//! callback shape lets each gate compute its result *while*
//! the loop runs, without the harness buffering every emitted
//! `MinedRecord` in memory — a `Vec<MinedRecord>` for an
//! RFC-sized 1 M-line corpus is a real OOM risk given each
//! record's `Vec` / `String` payload.
//!
//! Mirrors the H7.1 property test pattern in
//! `crates/ourios-miner/tests/hazards.rs` for the snapshot
//! capture (per-`(template_id, template_version)` template
//! tokens captured once per unique pair via
//! `HashMap::entry(...).Vacant.insert`); the streaming
//! surface is the bench-specific shape.
//!
//! Current consumers (`lib::run` plugs in whichever are
//! enabled):
//!
//! - C1 (PR-I1): checks each emitted record's reconstruction
//!   against the input bytes.
//! - A1 (PR-I2): streams each `MinedRecord` into its
//!   partition's `ourios_parquet::Writer` and, after the loop,
//!   writes the drained audit events (returned in
//!   [`HarnessResult`]) so it can sum the full bucket bytes.
//!
//! C2 (template-count convergence) is the remaining future
//! consumer — a counter-accumulator that samples the template
//! count at the §3.4.3 cadence.

use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{AuditEvent, NoOpAuditSink, SharedAuditSink};
use ourios_core::clock::TestClock;
use ourios_core::config::MinerConfig;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::{BodyKind, MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_miner::tree::OwnedToken;

use crate::BenchError;
use crate::corpus::{BENCH_TENANT, CorpusLoad, TIME_BASELINE_NS};

/// What [`run`] returns after draining the miner: the
/// audit-event stream A1 writes into the `audit/...` partition
/// series for its `bytes(ourios_output)` term. The
/// per-version template snapshots stay local to [`run`] — they
/// feed the per-line callback but have no consumer past the
/// loop, so they aren't surfaced until a gate needs them.
pub(crate) struct HarnessResult {
    pub audit_events: Vec<AuditEvent>,
}

/// Drive the miner over every line in `corpus`, snapshotting
/// per-`(template_id, template_version)` template tokens
/// once per unique pair, and invoking `on_record` per
/// ingested line with `(input, emitted, snapshot)`.
///
/// `snapshot` is `None` when the emitted record is lossy
/// (`record.lossy_flag = true`) or carries the
/// [`NO_TEMPLATE`] sentinel (`template_id = 0`, the
/// parse-failure path per RFC 0001 §6.2). Non-lossy records
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
/// `capture_audit` controls audit-event collection: A1 needs
/// the stream (it writes it into the `audit/...` series),
/// other gates don't. When `false` the cluster gets a
/// [`NoOpAuditSink`] and [`HarnessResult::audit_events`] is
/// empty — important because [`SharedAuditSink`] is unbounded,
/// so buffering it on a C1-only run would retain the whole
/// event stream for nothing.
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
    capture_audit: bool,
    mut on_record: F,
) -> Result<HarnessResult, BenchError>
where
    F: FnMut(&OtlpLogRecord, &MinedRecord, Option<&[OwnedToken]>),
{
    let sink = SharedRecordSink::new();
    let audit_sink = capture_audit.then(SharedAuditSink::new);
    // `with_audit_sink` is the constructor that takes the
    // audit sink; `with_record_sink` is the chainable setter
    // for the record sink. A `NoOpAuditSink` is installed when
    // the caller isn't capturing, so the miner's emissions are
    // dropped instead of buffered.
    let audit_box: Box<dyn ourios_core::audit::AuditSink> = match &audit_sink {
        Some(s) => Box::new(s.clone()),
        None => Box::new(NoOpAuditSink::new()),
    };
    // Pin the miner's clock to a fixed instant (the §3.3
    // corpus baseline) so audit-event timestamps are
    // deterministic. The default `SystemClock` would stamp
    // each run's audit events with wall-clock time, making
    // A1's audit-stream bytes — and therefore the compression
    // ratio — differ run-to-run; RFC0006.7 requires bit-
    // identical measurements across reruns. A bench audit
    // timestamp isn't a meaningful measurement, only its
    // reproducibility is.
    let bench_clock = TestClock::new(UNIX_EPOCH + Duration::from_nanos(TIME_BASELINE_NS));
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), audit_box)
        .with_record_sink(Box::new(sink.clone()))
        .with_clock(Box::new(bench_clock));
    let tenant = TenantId::new(BENCH_TENANT);
    let mut snapshots: HashMap<(u64, u32), Vec<OwnedToken>> = HashMap::new();

    for input in &corpus.lines {
        cluster.ingest(input);
        let record = require_single(sink.drain())?;

        // Skip the snapshot capture for lossy / parse-failure /
        // structured-body records. Lossy rows are excluded from
        // C1's numerator and denominator per §3.4.2, a
        // `template_id == NO_TEMPLATE` (0) emit has no leaf by
        // construction, and structured-body records use the
        // sentinel template id RFC 0001 §6.1 assigns to
        // `(severity, scope, BodyKind::Structured)` — that
        // sentinel is *not* a Drain-tree leaf, so
        // `templates_for()` correctly returns nothing for it.
        // Per RFC 0001 §6.4 / RFC 0003 §6.4, reconstruction for
        // structured bodies is a storage-layer round-trip
        // (decode the stored `AnyValue` bytes) rather than
        // template + params, so the bench's C1 (which measures
        // template-based reconstruction) doesn't apply.
        let want_snapshot = !record.lossy_flag
            && record.template_id != NO_TEMPLATE
            && matches!(record.body_kind, BodyKind::String);
        if want_snapshot {
            let key = (record.template_id, record.template_version);
            if let std::collections::hash_map::Entry::Vacant(slot) = snapshots.entry(key) {
                let template = snapshot_for(
                    cluster.templates_for(&tenant),
                    record.template_id,
                    record.template_version,
                )?;
                slot.insert(template);
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

    Ok(HarnessResult {
        audit_events: audit_sink.map(|s| s.drain()).unwrap_or_default(),
    })
}

/// Take the single record the miner must emit for one
/// ingested line (RFC 0001 §6.1). Returns
/// [`BenchError::Pipeline`] for any count other than 1.
///
/// Generic over the element type so the count guard is unit-
/// testable without constructing a full `MinedRecord` (which
/// has no `Default`); `run` instantiates it at
/// `T = MinedRecord`.
fn require_single<T>(mut emitted: Vec<T>) -> Result<T, BenchError> {
    if emitted.len() != 1 {
        return Err(BenchError::Pipeline {
            detail: format!(
                "miner emitted {} record(s) for one ingested line — RFC 0001 §6.1 \
                 pins one-record-per-line",
                emitted.len(),
            ),
        });
    }
    Ok(emitted.pop().expect("len == 1 checked above"))
}

/// Find the template tokens for the emit-time `(template_id,
/// template_version)` among the leaves the cluster reports.
/// Returns [`BenchError::Pipeline`] when no leaf matches — by
/// construction the miner always reports the leaf for a
/// freshly-emitted non-lossy record, so an absence is an
/// RFC 0001 §6.1 contract violation worth surfacing rather
/// than a `panic` deep in C1's snapshot lookup.
fn snapshot_for(
    leaves: Vec<ourios_miner::cluster::LeafSnapshot>,
    template_id: u64,
    template_version: u32,
) -> Result<Vec<OwnedToken>, BenchError> {
    leaves
        .into_iter()
        .find(|s| s.template_id == template_id && s.template_version == template_version)
        .map(|s| s.template)
        .ok_or_else(|| BenchError::Pipeline {
            detail: format!(
                "miner emitted record at (template_id={template_id}, \
                 template_version={template_version}) but templates_for() returned no \
                 matching leaf — RFC 0001 §6.1 contract violation",
            ),
        })
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
        run(&load, false, |_input, _record, _snap| count += 1).expect("harness runs");
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
        run(&load, false, |_input, record, snap| {
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

    /// A snapshot is captured at most once per unique
    /// `(template_id, template_version)` — re-emitting the
    /// same template (the steady-state common case) reuses the
    /// stored tokens and the same `&[OwnedToken]` reaches the
    /// callback. Two structurally-identical lines share one
    /// template, so both callbacks must see byte-identical
    /// snapshot slices.
    #[test]
    fn repeated_template_reuses_one_snapshot() {
        let (_tmp, load) = load_lines(&["user 42 logged in", "user 99 logged in"]);
        let mut snapshots_seen: Vec<Option<Vec<OwnedToken>>> = Vec::new();
        run(&load, false, |_input, _record, snap| {
            snapshots_seen.push(snap.map(<[OwnedToken]>::to_vec));
        })
        .expect("harness runs");
        assert_eq!(snapshots_seen.len(), 2);
        assert_eq!(
            snapshots_seen[0], snapshots_seen[1],
            "two structurally-identical lines share one template snapshot",
        );
        assert!(
            snapshots_seen[0].as_ref().is_some_and(|s| !s.is_empty()),
            "non-lossy lines carry a non-empty template snapshot",
        );
    }

    /// `require_single` is the RFC 0001 §6.1 one-record-per-
    /// line guard. The real miner always emits exactly one
    /// record per ingest, so the error branches are
    /// unreachable through `run`; the guard is generic over
    /// `T` precisely so the count logic is testable here
    /// without constructing a `MinedRecord` (which has no
    /// `Default`).
    #[test]
    fn require_single_accepts_exactly_one() {
        assert_eq!(require_single(vec![42u32]).expect("one element"), 42);
    }

    #[test]
    fn require_single_rejects_zero_and_many() {
        let zero = require_single::<u32>(vec![]).expect_err("empty must error");
        assert!(
            matches!(&zero, BenchError::Pipeline { detail } if detail.contains("emitted 0")),
            "expected Pipeline mentioning a 0 count, got {zero:?}",
        );
        let many = require_single(vec![1u32, 2, 3]).expect_err("3 elements must error");
        assert!(
            matches!(&many, BenchError::Pipeline { detail } if detail.contains("emitted 3")),
            "expected Pipeline mentioning a 3 count, got {many:?}",
        );
    }

    /// `snapshot_for` is the RFC 0001 §6.1 leaf-lookup guard.
    /// The success path is exercised end-to-end by every
    /// `run`-based test above; the error path (no leaf matches
    /// the emitted `(id, version)`) can't be reached through
    /// the real miner, so we pin it here with an empty leaf
    /// list — no `LeafSnapshot` construction required.
    #[test]
    fn snapshot_for_errors_when_no_leaf_matches() {
        let err = snapshot_for(Vec::new(), 7, 3).expect_err("no leaf must error");
        assert!(
            matches!(
                &err,
                BenchError::Pipeline { detail }
                    if detail.contains("template_id=7") && detail.contains("template_version=3")
            ),
            "expected Pipeline naming the missing (id, version), got {err:?}",
        );
    }
}
