//! Corpus loader for the RFC 0006 bench harness.
//!
//! Two file formats are consumed:
//!
//! - **Plain text** (`*.txt`) — RFC 0006 §3.3 v1 form. One line
//!   per record, UTF-8. Each non-empty line becomes one
//!   [`OtlpLogRecord`] with `Body::String(line)`, default tenant
//!   (`bench-tenant`), severity `9` / `INFO`, and
//!   `scope = (None, None)`. Timestamps advance
//!   deterministically — `time_unix_nano` starts at the §3.3
//!   baseline (`1_775_127_480_000_000_000`, i.e.
//!   2026-04-02T10:58:00 UTC) and ticks `1_000_000` ns (1 ms)
//!   per line.
//!
//! - **OTLP/JSON Lines** (`*.jsonl`, `*.json`) — RFC 0006 §3.1's
//!   OTLP-LogsData migration. One OTLP `LogsData` per line
//!   (the [OTel File Exporter] format the collector emits). Each
//!   wire `LogRecord`'s envelope maps 1:1 onto the
//!   [`OtlpLogRecord`] per the RFC 0003 §6.6 shape — severity,
//!   scope, attributes, resource attributes, trace context, body.
//!   `time_unix_nano` is taken from the wire (file-static =
//!   run-reproducible). String bodies become `Body::String`;
//!   any other `AnyValue` becomes `Body::Structured`. The RFC
//!   0005 §3.3 canonical-JSON writer (landed in PR #62) lets
//!   the full envelope survive to disk; PR-K4's earlier strip
//!   workaround is gone.
//!
//!   This is the path RFC 0003 §6.5 itself names as the MVP bench
//!   route — "the MVP bench reads OTLP from the on-disk corpus,
//!   bypassing this component entirely" — pending the
//!   `ourios-wal` crate (without which a live receiver would
//!   violate CLAUDE.md §3.4 WAL-before-ack).
//!
//! Both formats may coexist in the same directory; the walker
//! dispatches by extension. `total_files` and `raw_bytes` cover
//! every consumed file regardless of format — §3.4.1's
//! `bytes(raw_corpus)` is "the bytes the bench actually read,"
//! whichever encoding. For OTLP/JSON corpora that includes the
//! envelope (camelCase keys, base64 bytes), which inflates the
//! denominator A1 divides into; downstream comparisons across
//! corpus formats need to account for this.
//!
//! Inspiration for the parse strategy
//! (`serde_json::from_str::<LogsData>` against
//! [`opentelemetry-proto`] types with the `with-serde` feature)
//! is [rotel]'s OTLP HTTP receiver — the same pattern it uses on
//! `ExportLogsServiceRequest`. Keeps the spec mapping
//! single-sourced in `opentelemetry-proto` rather than a
//! hand-rolled struct that could drift from the OTLP/JSON spec.
//!
//! The walk is **recursive** — same shape as
//! `tests/a1.rs::sum_txt_bytes` after the second round of
//! review. The committed seed corpus is flat today but a future
//! nested `testdata/corpus/<archetype>/...` layout would silently
//! undercount with a single-level loader, and §3.4.1's
//! `bytes(raw_corpus)` is naturally recursive.
//!
//! [OTel File Exporter]: https://opentelemetry.io/docs/specs/otel/protocol/file-exporter/
//! [`opentelemetry-proto`]: https://docs.rs/opentelemetry-proto
//! [rotel]: https://github.com/streamfold/rotel

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, LogsData};
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;

use crate::BenchError;

/// §3.3 baseline timestamp — 2026-04-02T10:58:00 UTC. Matches
/// the test fixture baseline used by `crates/ourios-parquet/
/// tests/round_trip.rs` so the bench writes timestamps the
/// existing readers already exercise.
pub(crate) const TIME_BASELINE_NS: u64 = 1_775_127_480_000_000_000;

/// §3.3 inter-line delta. One millisecond keeps timestamps
/// monotonically increasing across the corpus without
/// crossing hour / day boundaries within a single bench run
/// (a corpus of `≤ 3.6_000_000` lines fits inside one hour at
/// 1 ms / line). Hour-spanning corpora exercise the writer's
/// partition rotation; that's a separate bench concern.
pub(crate) const TIME_INCREMENT_NS: u64 = 1_000_000;

/// Default tenant per §3.3. All bench records land in the same
/// partition; multi-tenant scenarios are a future RFC.
pub(crate) const BENCH_TENANT: &str = "bench-tenant";

/// Default severity number per §3.3 — `9` (INFO). Severity
/// participates in the template key per RFC 0001 §6.1, so
/// keeping every bench record on the same severity means
/// they all share the same key bucket regardless of body
/// content. The RFC pins INFO specifically so a future
/// multi-severity bench corpus has a documented baseline to
/// diverge from.
pub(crate) const BENCH_SEVERITY_NUMBER: u8 = 9;

/// Default severity text per §3.3 — `"INFO"` (the canonical
/// `OTel` name for severity 9). Round-trips through the
/// writer / reader as the §3.2 `severity_text` column.
pub(crate) const BENCH_SEVERITY_TEXT: &str = "INFO";

/// Aggregate output of [`load`]: per-line OTLP records plus the
/// `corpus` metadata fields the §3.6 results JSON requires.
///
/// Each [`OtlpLogRecord`] carries the original line bytes
/// inside `body = Some(Body::String(line))`. The earlier shape
/// of this struct also stored the line as a separate `String`
/// field on a `CorpusLine` wrapper, which doubled the
/// per-line memory footprint on multi-million-line corpora;
/// the §3.4.2 reconstruction compare and any other consumer
/// pulls the bytes from the OTLP body via a small helper
/// like [`line_bytes`].
#[derive(Debug)]
pub(crate) struct CorpusLoad {
    pub lines: Vec<OtlpLogRecord>,
    /// Number of corpus files the loader consumed under the
    /// directory (recursive) — every extension the walker
    /// dispatches on (`*.txt`, `*.jsonl`, `*.json`). Empty
    /// files count: the §3.4.1 A1 formula sums
    /// `metadata.len()` over every consumed file, not just
    /// non-empty ones, so the diagnostic surfaces the input
    /// the formula actually consumed.
    pub total_files: u32,
    /// Sum of `fs::metadata(p).len()` over every consumed
    /// corpus file (`*.txt` + `*.jsonl` + `*.json`, empty
    /// files included). Matches the §3.4.1 `bytes(raw_corpus)`
    /// formula 1:1.
    pub raw_bytes: u64,
    /// User-facing directory path string for the results JSON.
    pub directory: String,
}

/// Walk `dir` recursively and load every recognised corpus
/// file (`*.txt`, `*.jsonl`, `*.json`) into a [`CorpusLoad`].
/// Errors with [`BenchError::Corpus`] when the directory is
/// unreadable, contains no recognised corpus files, or every
/// contributing file is empty.
pub(crate) fn load(dir: &Path) -> Result<CorpusLoad, BenchError> {
    let mut total_files = 0u32;
    let mut raw_bytes = 0u64;
    let mut lines = Vec::new();
    let tenant = TenantId::new(BENCH_TENANT);
    let mut next_ns = TIME_BASELINE_NS;
    let mut visited = HashSet::new();

    walk(
        dir,
        &mut total_files,
        &mut raw_bytes,
        &mut lines,
        &tenant,
        &mut next_ns,
        &mut visited,
    )?;

    if lines.is_empty() {
        return Err(BenchError::Corpus {
            detail: format!(
                "no non-empty corpus lines under {} (read {} file(s); supported extensions: \
                 `*.txt`, `*.jsonl`, `*.json`)",
                dir.display(),
                total_files,
            ),
        });
    }

    Ok(CorpusLoad {
        lines,
        total_files,
        raw_bytes,
        directory: dir.display().to_string(),
    })
}

/// Borrow the original line bytes from an OTLP record's body.
/// Returns `None` for non-`Body::String` records (e.g. wire-
/// absent bodies the bench corpus never produces). C1's
/// reconstruction compare uses this rather than holding a
/// separate `String` per line.
pub(crate) fn line_bytes(record: &OtlpLogRecord) -> Option<&[u8]> {
    match &record.body {
        Some(Body::String(s)) => Some(s.as_bytes()),
        _ => None,
    }
}

fn walk(
    dir: &Path,
    total_files: &mut u32,
    raw_bytes: &mut u64,
    lines: &mut Vec<OtlpLogRecord>,
    tenant: &TenantId,
    next_ns: &mut u64,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), BenchError> {
    // Cycle / alias guard. `fs::metadata` follows symlinks,
    // so a symlinked subdirectory pointing back at an
    // ancestor would recurse until the stack overflows.
    // Canonicalize each directory and refuse to descend into
    // one we've already visited — `HashSet::insert` returns
    // `false` when the resolved path is a repeat. That covers
    // both a true symlink loop and a benign alias (two paths
    // resolving to the same real directory); both are
    // rejected because re-walking the same directory would
    // double-count its lines / bytes and skew A1 / C1, not
    // only because of the unbounded-recursion risk.
    let canonical = fs::canonicalize(dir).map_err(|e| BenchError::Corpus {
        detail: format!("canonicalize({}): {e}", dir.display()),
    })?;
    if !visited.insert(canonical.clone()) {
        return Err(BenchError::Corpus {
            detail: format!(
                "corpus directory visited twice: {} resolves to {}, already seen — a symlink \
                 cycle (which would recurse indefinitely) or an alias (two paths to the same \
                 directory, which would double-count)",
                dir.display(),
                canonical.display(),
            ),
        });
    }

    // Sort entries by file name so the bench is deterministic
    // across platforms — `read_dir` order is filesystem-
    // dependent. Same pattern as
    // `crates/ourios-miner/tests/hazards.rs::h7_1`. Per-entry
    // errors are surfaced explicitly (no `filter_map(ok)`) so
    // an unreadable directory entry undercounts loudly rather
    // than silently.
    let read_dir = fs::read_dir(dir).map_err(|e| BenchError::Corpus {
        detail: format!("read_dir({}): {e}", dir.display()),
    })?;
    let mut entries: Vec<fs::DirEntry> =
        read_dir
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| BenchError::Corpus {
                detail: format!("read_dir entry under {}: {e}", dir.display()),
            })?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        // Use `fs::metadata` (fallible, symlink-following)
        // rather than `Path::is_dir` (which returns `false`
        // on metadata errors and would silently skip
        // unreadable subdirectories). A permission-denied
        // subdir now surfaces as `BenchError::Corpus`
        // instead of disappearing from the corpus count.
        let meta = fs::metadata(&path).map_err(|e| BenchError::Corpus {
            detail: format!("metadata({}): {e}", path.display()),
        })?;
        if meta.is_dir() {
            walk(
                &path,
                total_files,
                raw_bytes,
                lines,
                tenant,
                next_ns,
                visited,
            )?;
            continue;
        }
        // Dispatch on extension. `.txt` → plain-text loader
        // (RFC 0006 §3.3 v1). `.jsonl` / `.json` → OTLP/JSON
        // Lines loader (the §3.1 OTLP-LogsData path that RFC
        // 0003 §6.5 itself names as the MVP bench source).
        // Any other extension is silently skipped — corpora
        // often sit next to `README.md`, `.gitignore`, sample
        // configs, etc., and treating those as input would
        // poison the §3.4.1 byte count.
        //
        // `eq_ignore_ascii_case` is allocation-free; a
        // `to_ascii_lowercase` would allocate a fresh `String`
        // per directory entry on this hot loader path.
        let ext = path.extension().and_then(std::ffi::OsStr::to_str);
        let format = match ext {
            Some(e) if e.eq_ignore_ascii_case("txt") => CorpusFormat::Txt,
            Some(e) if e.eq_ignore_ascii_case("jsonl") || e.eq_ignore_ascii_case("json") => {
                CorpusFormat::OtlpJsonl
            }
            _ => continue,
        };
        *total_files += 1;
        *raw_bytes += meta.len();
        match format {
            CorpusFormat::Txt => ingest_txt(&path, lines, tenant, next_ns)?,
            CorpusFormat::OtlpJsonl => ingest_otlp_jsonl(&path, lines, tenant)?,
        }
    }
    Ok(())
}

/// Which on-disk encoding a corpus file uses.
enum CorpusFormat {
    /// RFC 0006 §3.3 plain text, one line per record.
    Txt,
    /// RFC 0006 §3.1 OTLP JSON Lines, one `LogsData` per line
    /// (the `OTel` File Exporter format).
    OtlpJsonl,
}

/// Plain-text ingest (RFC 0006 §3.3). Streams via `BufReader`
/// rather than `read_to_string` — corpora can reach the 100
/// MiB class per file, and slurping would spike memory
/// unnecessarily.
fn ingest_txt(
    path: &Path,
    lines: &mut Vec<OtlpLogRecord>,
    tenant: &TenantId,
    next_ns: &mut u64,
) -> Result<(), BenchError> {
    let file = File::open(path).map_err(|e| BenchError::Corpus {
        detail: format!("open({}): {e}", path.display()),
    })?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        // `lines()` allocates one `String` per line and strips
        // the trailing `\n` but not `\r`. Pop the CR(s) a
        // CRLF file leaves in place rather than
        // `trim_end_matches('\r').to_string()` (which would
        // allocate a second copy on every line), then move
        // the `String` straight into `Body::String` — one
        // allocation per line on the hot loader path.
        let mut raw = line.map_err(|e| BenchError::Corpus {
            detail: format!("read line in {}: {e}", path.display()),
        })?;
        while raw.ends_with('\r') {
            raw.pop();
        }
        if raw.is_empty() {
            continue;
        }
        lines.push(OtlpLogRecord {
            tenant_id: tenant.clone(),
            body: Some(Body::String(raw)),
            time_unix_nano: *next_ns,
            severity_number: BENCH_SEVERITY_NUMBER,
            severity_text: Some(BENCH_SEVERITY_TEXT.to_string()),
            ..Default::default()
        });
        *next_ns = next_ns.saturating_add(TIME_INCREMENT_NS);
    }
    Ok(())
}

/// OTLP JSON Lines ingest (RFC 0006 §3.1, the `OTel` File
/// Exporter format). Each non-blank line is parsed as one
/// `LogsData` via `serde_json::from_str` against the
/// `opentelemetry-proto` types (with the `with-serde`
/// feature). Walks `resource_logs[].scope_logs[].log_records[]`
/// and emits one [`OtlpLogRecord`] per wire `LogRecord` —
/// envelope mapped 1:1 per the RFC 0003 §6.6 in-memory shape;
/// the RFC 0005 §3.3 canonical-JSON writer landed in PR #62
/// carries attributes and structured bodies through to disk.
/// Tenant is the bench default — multi-tenant corpora are a
/// future RFC.
fn ingest_otlp_jsonl(
    path: &Path,
    lines: &mut Vec<OtlpLogRecord>,
    tenant: &TenantId,
) -> Result<(), BenchError> {
    let file = File::open(path).map_err(|e| BenchError::Corpus {
        detail: format!("open({}): {e}", path.display()),
    })?;
    // 1-based line number for parse-error diagnostics —
    // matches editor / jq output and what an operator would
    // type to `sed -n '<n>p'` to inspect the failing line.
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let raw = line.map_err(|e| BenchError::Corpus {
            detail: format!("read line in {}: {e}", path.display()),
        })?;
        // Skip blank / whitespace-only lines (trailing newline
        // at EOF, accidental blank separators between
        // batches). Real `LogsData` JSON always starts with
        // `{`, so a trimmed-empty check is sufficient and
        // doesn't require parsing.
        if raw.trim().is_empty() {
            continue;
        }
        let logs_data: LogsData = serde_json::from_str(&raw).map_err(|e| BenchError::Corpus {
            detail: format!("parse OTLP/JSON at {}:{}: {e}", path.display(), idx + 1),
        })?;
        for rl in logs_data.resource_logs {
            // Resource attributes are copied onto every record
            // in the `ResourceLogs` group per the
            // `OtlpLogRecord` doc-comment ("inherited from
            // `Resource.attributes` and copied onto every
            // record under that `ResourceLogs` group"). Hoist
            // the borrow once so the per-record map doesn't
            // re-extract.
            let resource_attrs: Vec<KeyValue> =
                rl.resource.map(|r| r.attributes).unwrap_or_default();
            for sl in rl.scope_logs {
                let scope = sl.scope;
                for lr in sl.log_records {
                    lines.push(map_log_record(
                        tenant.clone(),
                        &resource_attrs,
                        scope.as_ref(),
                        lr,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Map one wire `LogRecord` (plus its inherited resource
/// attributes and `InstrumentationScope`) into the
/// [`OtlpLogRecord`] shape RFC 0003 §6.6 pins. Severity
/// clamps wire `i32` into the OTLP-defined `0..=24` band
/// (`0` = UNSPECIFIED, `1..=24` = TRACE..FATAL with sub-
/// levels), matching the receiver-boundary narrowing the
/// `OtlpLogRecord` doc-comment describes ("narrowed from
/// proto's unbounded `i32` at the receiver boundary"). Empty
/// strings on optional fields collapse to `None` so
/// downstream code sees a single absence signal regardless of
/// whether the wire delivered `""` or omitted the field.
fn map_log_record(
    tenant: TenantId,
    resource_attrs: &[KeyValue],
    scope: Option<&InstrumentationScope>,
    lr: LogRecord,
) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: tenant,
        time_unix_nano: lr.time_unix_nano,
        observed_time_unix_nano: (lr.observed_time_unix_nano != 0)
            .then_some(lr.observed_time_unix_nano),
        severity_number: u8::try_from(lr.severity_number.clamp(0, 24)).unwrap_or(0),
        severity_text: empty_to_none(lr.severity_text),
        scope_name: scope.and_then(|s| empty_to_none(s.name.clone())),
        scope_version: scope.and_then(|s| empty_to_none(s.version.clone())),
        // RFC 0018 §3.1 — mirror the production receiver: carry the scope's
        // own attributes so the bench corpus exercises the scope-metadata
        // path. The per-resource / per-scope `schema_url`s live on the
        // `ResourceLogs`/`ScopeLogs` wrappers, which this per-record loader
        // doesn't thread through — left `None` (bench input, not a fidelity
        // gate).
        scope_attributes: scope.map(|s| s.attributes.clone()).unwrap_or_default(),
        resource_schema_url: None,
        scope_schema_url: None,
        attributes: lr.attributes,
        dropped_attributes_count: lr.dropped_attributes_count,
        resource_attributes: resource_attrs.to_vec(),
        // Trace / span ids are wire-typed as `Vec<u8>` but
        // OTLP fixes their length (16 / 8 bytes). Reject any
        // other length to `None` rather than panicking — a
        // malformed id is a wire-level concern, not the
        // bench's. Per RFC 0003 §5 (RFC0003.11), transport
        // errors are surfaced as `None` here, not panics.
        trace_id: <[u8; 16]>::try_from(lr.trace_id.as_slice()).ok(),
        span_id: <[u8; 8]>::try_from(lr.span_id.as_slice()).ok(),
        flags: lr.flags,
        event_name: empty_to_none(lr.event_name),
        // `Body::from_any_value` is the single-sourced
        // String-vs-Structured fork in `ourios-core`; the
        // earlier local `any_value_to_body` duplicated its
        // logic. The deferred-canonicalisation rule (RFC
        // 0003 §6.4: receiver hands the miner the decoded
        // `AnyValue` verbatim) lives with the helper.
        body: lr.body.and_then(Body::from_any_value),
    }
}

fn empty_to_none(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn seed_corpus_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("testdata/corpus")
    }

    #[test]
    fn loads_seed_corpus_into_otlp_records() {
        let load = load(&seed_corpus_dir()).expect("seed corpus loads");
        assert!(load.total_files >= 1, "seed corpus has at least one *.txt");
        assert!(!load.lines.is_empty(), "seed corpus has at least one line");
        assert!(load.raw_bytes > 0, "raw_bytes is the sum of *.txt sizes");

        // First line uses the §3.3 baseline timestamp + the
        // pinned severity / scope envelope so the miner sees
        // every bench record on the same template-key bucket
        // (RFC 0001 §6.1: severity is part of the key).
        let first = &load.lines[0];
        assert_eq!(first.time_unix_nano, TIME_BASELINE_NS);
        assert_eq!(first.tenant_id.as_str(), BENCH_TENANT);
        assert_eq!(first.severity_number, BENCH_SEVERITY_NUMBER);
        assert_eq!(first.severity_text.as_deref(), Some(BENCH_SEVERITY_TEXT));
        assert_eq!(first.scope_name, None);
        assert_eq!(first.scope_version, None);
        assert!(
            matches!(first.body, Some(Body::String(_))),
            "every line wraps as Body::String",
        );
        assert!(
            line_bytes(first).is_some(),
            "line_bytes() recovers the input bytes from Body::String",
        );

        // Subsequent lines advance by exactly TIME_INCREMENT_NS
        // (mod the saturating-add edge case which can't fire on
        // any realistic corpus). Pin on the second line.
        if let Some(second) = load.lines.get(1) {
            assert_eq!(second.time_unix_nano, TIME_BASELINE_NS + TIME_INCREMENT_NS,);
        }
    }

    #[test]
    fn empty_directory_errors_with_corpus_variant() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let err = load(tmp.path()).expect_err("empty dir must error");
        assert!(
            matches!(err, BenchError::Corpus { .. }),
            "expected Corpus variant, got {err:?}",
        );
    }

    /// CRLF line endings get their trailing `\r` stripped (the
    /// `while raw.ends_with('\r') { raw.pop() }` path) and
    /// blank lines — including CR-only lines — are skipped.
    /// Pins the loader's CR handling, which the
    /// single-allocation refactor rewrote from
    /// `trim_end_matches('\r')` to in-place `pop`.
    #[test]
    fn strips_crlf_and_skips_blank_lines() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("crlf.txt");
        let mut file = std::fs::File::create(&path).expect("create");
        // Two real CRLF lines with a CR-only blank line
        // between them — only the two real lines survive.
        // (`lines()` splits on `\n`, so each `\r\n` becomes a
        // line with a trailing `\r`, and the lone `\r\n`
        // becomes a `"\r"` that pops to empty.)
        file.write_all(b"user 42 logged in\r\n\r\nuser 43 logged in\r\n")
            .expect("write");
        drop(file);

        let load = load(tmp.path()).expect("crlf corpus loads");
        assert_eq!(load.lines.len(), 2, "blank + CR-only lines are skipped");
        assert_eq!(
            line_bytes(&load.lines[0]),
            Some("user 42 logged in".as_bytes()),
            "trailing CR stripped, no leftover \\r",
        );
        assert_eq!(
            line_bytes(&load.lines[1]),
            Some("user 43 logged in".as_bytes()),
        );
    }

    /// `line_bytes` recovers the input bytes from a
    /// `Body::String` record and returns `None` for any other
    /// body shape. The bench corpus only ever produces
    /// `Body::String`, so the `None` arm is defensive — pin
    /// it so a future loader change that emits a non-string
    /// body surfaces here rather than silently dropping the
    /// line from C1's denominator.
    #[test]
    fn line_bytes_handles_string_and_non_string_bodies() {
        let string_record = OtlpLogRecord {
            body: Some(Body::String("user 42 logged in".to_string())),
            ..Default::default()
        };
        assert_eq!(
            line_bytes(&string_record),
            Some("user 42 logged in".as_bytes()),
        );

        let absent_body = OtlpLogRecord {
            body: None,
            ..Default::default()
        };
        assert_eq!(line_bytes(&absent_body), None);
    }

    /// A symlinked subdirectory pointing back at an ancestor
    /// must surface as `BenchError::Corpus` (cycle detected)
    /// rather than recursing until the stack overflows.
    /// Unix-only: portable symlink creation needs
    /// `std::os::unix`, and Windows symlinks require elevated
    /// privileges in CI.
    #[cfg(unix)]
    #[test]
    fn symlink_cycle_errors_rather_than_recursing_forever() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        // A real corpus file so the failure can't be the
        // "empty corpus" path.
        std::fs::write(tmp.path().join("seed.txt"), "line one\n").expect("write seed");
        // sub/loop -> tmp forms the cycle.
        let subdir = tmp.path().join("sub");
        std::fs::create_dir(&subdir).expect("mkdir sub");
        symlink(tmp.path(), subdir.join("loop")).expect("symlink loop");
        let err = load(tmp.path()).expect_err("symlink cycle must error");
        assert!(
            matches!(err, BenchError::Corpus { .. }),
            "expected Corpus cycle error, got {err:?}",
        );
    }

    // ---------------------------------------------------------------
    // OTLP/JSON path — RFC 0006 §3.1 (the §6.5 MVP bench source).
    // ---------------------------------------------------------------

    fn otlp_sample_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/otlp")
    }

    /// Loads the committed OTLP/JSON sample (3 `LogsData` lines,
    /// 4 wire `LogRecord`s) and pins every envelope field the
    /// loader maps onto [`OtlpLogRecord`] per the RFC 0003 §6.6
    /// in-memory shape. Catches drift in the OTLP/JSON spec
    /// mapping (camelCase keys, string-encoded `u64`s for
    /// `timeUnixNano`, base64 bytes), the resource-attribute
    /// fan-out copy, the scope inheritance, and the empty-
    /// string → `None` collapse on optional fields.
    #[test]
    fn loads_otlp_corpus_envelope_one_to_one() {
        let load = load(&otlp_sample_dir()).expect("OTLP sample loads");
        assert_eq!(load.total_files, 1, "the sample dir has one .jsonl file");
        assert_eq!(
            load.lines.len(),
            4,
            "3 LogsData lines × (1 + 2 + 1) records = 4 — the kvlistValue \
             record is back now that RFC 0005 §3.3 canonicalisation (PR #62) \
             carries structured bodies through to disk",
        );
        assert!(load.raw_bytes > 0, "raw_bytes counts the .jsonl file size");

        let first = &load.lines[0];
        assert_eq!(first.tenant_id.as_str(), BENCH_TENANT);
        assert_eq!(first.time_unix_nano, 1_775_127_480_000_000_000);
        assert_eq!(
            first.observed_time_unix_nano,
            Some(1_775_127_480_000_000_123)
        );
        assert_eq!(first.severity_number, 9);
        assert_eq!(first.severity_text.as_deref(), Some("INFO"));
        assert_eq!(first.scope_name.as_deref(), Some("bench.scope"));
        assert_eq!(first.scope_version.as_deref(), Some("1.0.0"));
        assert_eq!(first.flags, 1);
        assert_eq!(first.attributes.len(), 1, "one log attribute on record 0");
        assert_eq!(
            first.resource_attributes.len(),
            2,
            "two resource attributes (service.name + host.name)",
        );
        assert_eq!(
            line_bytes(first),
            Some("user 42 logged in".as_bytes()),
            "Body::String unwraps to the wire string",
        );

        // Second LogsData has 2 records — both should land,
        // sharing the resource attribute (only service.name on
        // that line).
        let second = &load.lines[1];
        assert_eq!(second.severity_number, 13, "WARN");
        assert_eq!(line_bytes(second), Some("slow query: 1843ms".as_bytes()));
        assert_eq!(second.resource_attributes.len(), 1);
        let third = &load.lines[2];
        assert_eq!(third.severity_number, 17, "ERROR");
        assert_eq!(line_bytes(third), Some("connection refused".as_bytes()));
    }

    /// `body.kvlistValue` (and anything that isn't `stringValue`)
    /// stays on the record as `Body::Structured(AnyValue)`,
    /// **not** flattened to text or dropped. Per RFC 0003 §6.4
    /// the receiver hands the miner the decoded `AnyValue`
    /// verbatim; the storage layer canonicalises at write
    /// time. `line_bytes` returns `None` on these records
    /// (C1's denominator excludes them — see `c1::record`).
    #[test]
    fn otlp_structured_body_maps_to_body_structured() {
        let load = load(&otlp_sample_dir()).expect("sample loads");
        let structured = &load.lines[3];
        assert!(
            matches!(structured.body, Some(Body::Structured(_))),
            "kvlistValue body must round-trip as Body::Structured, got {:?}",
            structured.body,
        );
        assert_eq!(
            line_bytes(structured),
            None,
            "non-string bodies have no `line` representation",
        );
    }

    /// Blank lines (and a trailing newline at EOF) are skipped
    /// without erroring — the `OTel` File Exporter emits one
    /// `LogsData` per line but operators concatenating multiple
    /// exporter outputs may end up with extra blanks. The loader
    /// is permissive about whitespace-only lines and strict
    /// about everything else.
    #[test]
    fn otlp_skips_blank_lines() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("blanks.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(
            b"\n   \n{\"resourceLogs\":[{\"scopeLogs\":[{\"logRecords\":\
              [{\"body\":{\"stringValue\":\"only real line\"}}]}]}]}\n\n",
        )
        .expect("write");
        drop(file);

        let load = load(tmp.path()).expect("blank-only-padding loads");
        assert_eq!(load.lines.len(), 1);
        assert_eq!(
            line_bytes(&load.lines[0]),
            Some("only real line".as_bytes())
        );
    }

    /// A malformed JSON line surfaces as `BenchError::Corpus`
    /// carrying the 1-based line number — operators routinely
    /// type that into `sed -n '<n>p'` or open the file at the
    /// reported position. Silent truncation past the bad line
    /// would corrupt the corpus count.
    #[test]
    fn otlp_malformed_line_errors_with_line_number() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("bad.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        // Line 1 valid, line 2 malformed.
        file.write_all(b"{\"resourceLogs\":[]}\nnot json\n")
            .expect("write");
        drop(file);

        let err = load(tmp.path()).expect_err("malformed JSON must error");
        match err {
            BenchError::Corpus { detail } => {
                assert!(
                    detail.contains(":2:"),
                    "error must name the 1-based line (`:2:`), got {detail:?}",
                );
            }
            other => panic!("expected BenchError::Corpus, got {other:?}"),
        }
    }

    /// `severityNumber` is wire-typed `i32`; OTLP defines the
    /// valid band as `0..=24`. Out-of-band values clamp to
    /// `24` (FATAL4) — matches the "narrowed from proto's
    /// unbounded `i32` at the receiver boundary" comment on
    /// [`OtlpLogRecord::severity_number`]. Pins the clamp so a
    /// regression doesn't truncate a `99` to `99 as u8 = 99`
    /// (which is out of the documented enum range).
    #[test]
    fn otlp_severity_above_24_clamps_to_24() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("oversev.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(
            b"{\"resourceLogs\":[{\"scopeLogs\":[{\"logRecords\":\
              [{\"severityNumber\":99,\"body\":{\"stringValue\":\"x\"}}]}]}]}\n",
        )
        .expect("write");
        drop(file);

        let load = load(tmp.path()).expect("loads");
        assert_eq!(load.lines[0].severity_number, 24, "clamped to FATAL4");
    }

    /// `.txt` and `.jsonl` may coexist in the same corpus dir
    /// — the walker dispatches on extension and both contribute
    /// records. `total_files` and `raw_bytes` count every input
    /// regardless of format, matching §3.4.1's "the bytes the
    /// bench actually read."
    #[test]
    fn mixed_txt_and_jsonl_both_load() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        // One plain-text line.
        std::fs::write(tmp.path().join("a.txt"), "txt line one\n").expect("write txt");
        // One OTLP record.
        let jsonl_path = tmp.path().join("b.jsonl");
        let mut f = std::fs::File::create(&jsonl_path).expect("create jsonl");
        f.write_all(
            b"{\"resourceLogs\":[{\"scopeLogs\":[{\"logRecords\":\
              [{\"body\":{\"stringValue\":\"otlp line one\"}}]}]}]}\n",
        )
        .expect("write jsonl");
        drop(f);

        let load = load(tmp.path()).expect("mixed corpus loads");
        assert_eq!(load.total_files, 2, "both files counted");
        assert_eq!(load.lines.len(), 2, "one record per file");
        // Sorted order: a.txt before b.jsonl, so the txt line
        // is first.
        assert_eq!(line_bytes(&load.lines[0]), Some("txt line one".as_bytes()));
        assert_eq!(line_bytes(&load.lines[1]), Some("otlp line one".as_bytes()));
    }
}
