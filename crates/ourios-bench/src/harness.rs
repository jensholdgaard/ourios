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

use ourios_config::MinerConfig;
use ourios_core::audit::{AuditEvent, NoOpAuditSink, SharedAuditSink};
use ourios_core::clock::TestClock;
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
    /// The miner's §3.1 diagnostic counters, snapshotted at end of the
    /// pass (the accessors are otherwise discarded — #446).
    pub miner_stats: crate::MinerStats,
}

/// Drive the miner over every line in `corpus`, snapshotting
/// per-`(template_id, template_version)` template tokens
/// once per unique pair, and invoking `on_record` per
/// ingested line with `(input, emitted, snapshot)`.
///
/// `snapshot` is `None` when the emitted record is lossy
/// (`record.lossy_flag = true`), carries the
/// [`NO_TEMPLATE`] sentinel (`template_id = 0`, the
/// parse-failure path per RFC 0001 §6.2), or has a non-
/// `BodyKind::String` body (e.g. `BodyKind::Structured`,
/// which uses the sentinel template id RFC 0001 §6.1 assigns
/// to `(severity, scope, BodyKind::Structured)` — that
/// sentinel isn't a Drain-tree leaf, so `templates_for()`
/// correctly returns nothing). **Non-lossy string** records
/// always receive `Some(template_tokens)`; a missing snapshot
/// for a real `(id, v)` pair on that path surfaces as
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
    run_streaming(
        corpus.lines.iter().map(Ok),
        capture_audit,
        /* capture_snapshots */ true,
        |input, record, snapshot| {
            on_record(input, record, snapshot);
            Ok(())
        },
    )
}

/// [`run`] over a lazy record stream — the memory-flat path the
/// query-store builds use (`corpus::stream` → mine → flush, no
/// `Vec<OtlpLogRecord>` of the whole corpus). An `Err` item — or an
/// `Err` from the callback (a Parquet write failure, say) — aborts
/// at the failing record rather than mining the remaining corpus:
/// at the 10–100 GiB scale this path exists for, finishing the mine
/// after a fatal error would burn many minutes of CPU for nothing.
///
/// `capture_snapshots` gates the per-`(template_id, version)`
/// template capture: `true` preserves [`run`]'s contract (non-lossy
/// string-body records get `Some(template)` in the callback's third
/// argument — what C1 consumes); `false` skips the
/// `templates_for` walk entirely and every callback gets `None`.
/// Store builds pass `false` — the walk clones the full template set
/// per new pair, which goes quadratic on widening-churny corpora
/// (measured ~3 KB/s on `LogHub` `HDFS_v2`).
pub(crate) fn run_streaming<T, I, F>(
    corpus: I,
    capture_audit: bool,
    capture_snapshots: bool,
    mut on_record: F,
) -> Result<HarnessResult, BenchError>
where
    // `Borrow` lets the eager gate path feed `&OtlpLogRecord` (no
    // per-record clone) and the streaming path feed owned records.
    T: std::borrow::Borrow<OtlpLogRecord>,
    I: IntoIterator<Item = Result<T, BenchError>>,
    F: FnMut(&OtlpLogRecord, &MinedRecord, Option<&[OwnedToken]>) -> Result<(), BenchError>,
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

    for input in corpus {
        let input = input?;
        let input = input.borrow();
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
        // `capture_snapshots = false` skips the whole capture: every new
        // `(template_id, template_version)` pair otherwise walks + clones
        // the full template set via `templates_for`. On corpora with heavy
        // widening churn (one version bump per few lines) that is
        // quadratic — measured at ~3 KB/s on LogHub HDFS_v2, a ~57-day
        // store build — and the query-store builds never read the
        // snapshots at all (their callbacks ignore the argument). Only C1
        // (reconstruction) needs them.
        let want_snapshot = capture_snapshots
            && !record.lossy_flag
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

        on_record(input, &record, template_snapshot)?;
    }

    // Snapshot the miner's §3.1 counters before the cluster drops — the
    // pass updates them line-by-line but nothing else reads them (#446).
    // Single bench tenant, so `template_count(&tenant)` is the corpus total.
    let miner_stats = crate::MinerStats {
        template_count: cluster.template_count(&tenant) as u64,
        merges_total: cluster.merges_total(),
        parse_failures_total: cluster.parse_failures_total(),
        body_retentions_total: cluster.body_retentions_total(),
        params_overflow_total: cluster.params_overflow_total(),
    };
    Ok(HarnessResult {
        audit_events: audit_sink.map(|s| s.drain()).unwrap_or_default(),
        miner_stats,
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

    /// `capture_snapshots = false` (the query-store builds) skips the
    /// per-`(id, version)` `templates_for` walk entirely: even a
    /// non-lossy, real-template, string-body record — exactly the shape
    /// that gets `Some(template)` under `run()` — receives `None`.
    #[test]
    fn store_build_mode_never_captures_snapshots() {
        let (_tmp, load) = load_lines(&["user 42 logged in", "user 43 logged in"]);
        let mut calls = 0usize;
        run_streaming(
            load.lines.iter().map(Ok),
            /* capture_audit */ false,
            /* capture_snapshots */ false,
            |_input, record, snap| {
                calls += 1;
                assert!(!record.lossy_flag, "fixture lines mine cleanly");
                assert_ne!(record.template_id, NO_TEMPLATE);
                assert!(
                    matches!(record.body_kind, BodyKind::String),
                    "fixture lines carry string bodies",
                );
                assert!(
                    snap.is_none(),
                    "capture_snapshots = false must skip the templates_for walk",
                );
                Ok(())
            },
        )
        .expect("harness runs");
        assert_eq!(calls, load.lines.len());
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

    /// Every non-lossy, non-`NO_TEMPLATE`, `BodyKind::String`
    /// callback gets `Some(template)`. Lossy / `NO_TEMPLATE` /
    /// `BodyKind::Structured` records get `None` without
    /// failing the harness. Pins the §3.4.2 lookup contract
    /// that C1 relies on. The plain-text fixture used here has
    /// only string bodies, so this test focuses on the
    /// lossy / `NO_TEMPLATE` exclusions; the
    /// `BodyKind::Structured` exclusion is documented in the
    /// `run` rustdoc and exercised end-to-end by an OTLP
    /// fixture run.
    #[test]
    fn non_lossy_string_callbacks_carry_a_template_snapshot() {
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
            if !record.lossy_flag
                && record.template_id != NO_TEMPLATE
                && matches!(record.body_kind, BodyKind::String)
            {
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
            "every non-lossy non-NO_TEMPLATE string-body record must receive a snapshot",
        );
    }

    /// `BodyKind::Structured` records reach the harness without
    /// triggering the `templates_for()` contract violation that
    /// the pre-fix path raised, AND the callback receives
    /// `snapshot = None` for them. End-to-end coverage of the
    /// production `BodyKind::String` guard the OTLP fixture
    /// surfaced — uses a synthetic OTLP/JSONL fixture with one
    /// `stringValue` body and one `kvlistValue` body so the
    /// harness exercises both the captured-snapshot and the
    /// skip-snapshot paths in one run.
    #[test]
    fn structured_body_record_skips_snapshot_lookup_without_error() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("structured.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        // Two LogsData lines: a string-body record (normal
        // template path) and a kvlist-body record (sentinel
        // template id — the path that pre-fix crashed).
        file.write_all(
            b"{\"resourceLogs\":[{\"scopeLogs\":[{\"logRecords\":\
              [{\"timeUnixNano\":\"1775127480000000000\",\
              \"severityNumber\":9,\
              \"body\":{\"stringValue\":\"user 1 logged in\"}}]}]}]}\n\
              {\"resourceLogs\":[{\"scopeLogs\":[{\"logRecords\":\
              [{\"timeUnixNano\":\"1775127481000000000\",\
              \"severityNumber\":9,\
              \"body\":{\"kvlistValue\":{\"values\":\
              [{\"key\":\"event\",\"value\":{\"stringValue\":\"startup\"}}]}}}]}]}]}\n",
        )
        .expect("write");
        drop(file);
        let load = corpus::load(tmp.path()).expect("OTLP fixture loads");

        let mut snapshots_present = 0usize;
        let mut snapshots_absent = 0usize;
        let mut structured_seen = 0usize;
        run(&load, false, |_input, record, snap| {
            if matches!(record.body_kind, BodyKind::Structured) {
                structured_seen += 1;
                assert!(
                    snap.is_none(),
                    "structured records must receive snapshot = None — the harness must skip the sentinel-id `templates_for()` lookup",
                );
            }
            if snap.is_some() {
                snapshots_present += 1;
            } else {
                snapshots_absent += 1;
            }
        })
        .expect("harness must not raise the pre-fix contract violation on structured bodies");
        assert_eq!(structured_seen, 1, "exactly one structured-body record");
        assert!(
            snapshots_present >= 1,
            "the string-body record gets a snapshot"
        );
        assert!(
            snapshots_absent >= 1,
            "the structured-body record's `None` is counted"
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

    /// The §3.1 miner counters are snapshotted into the harness result
    /// (#446): two lines that differ only in a masked number token merge
    /// into one clean template, so `template_count == 1` with no parse
    /// failures and no merges (a match is not a merge).
    #[test]
    fn harness_snapshots_miner_stats() {
        let (_tmp, load) = load_lines(&["user 42 logged in", "user 43 logged in"]);
        let result = run_streaming(
            load.lines.iter().map(Ok),
            /* capture_audit */ false,
            /* capture_snapshots */ false,
            |_input, _record, _snap| Ok(()),
        )
        .expect("harness runs");
        assert_eq!(
            result.miner_stats.template_count, 1,
            "the two lines share one `user <*> logged in` template",
        );
        assert_eq!(
            result.miner_stats.parse_failures_total, 0,
            "both mine cleanly"
        );
        assert_eq!(result.miner_stats.merges_total, 0, "a match is not a merge");
    }
}
