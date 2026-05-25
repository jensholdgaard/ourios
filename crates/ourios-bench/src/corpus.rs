//! Corpus loader for the RFC 0006 bench harness.
//!
//! Per RFC 0006 §3.3, the bench reads plain-text `*.txt` files
//! under a corpus directory, one line per row, UTF-8. Each non-
//! empty line becomes one `OtlpLogRecord` with
//! `Body::String(line)`, a default tenant (`bench-tenant`),
//! severity `9` / `INFO`, and `scope = (None, None)`. Time
//! stamps advance deterministically — `time_unix_nano` starts
//! at the §3.3 baseline (`1_775_127_480_000_000_000`, i.e.
//! 2026-04-02T10:58:00 UTC) and ticks `1_000_000` ns (1 ms) per
//! line.
//!
//! The walk is **recursive** — same shape as
//! `tests/a1.rs::sum_txt_bytes` after the second round of
//! review. The committed seed corpus is flat today but a
//! future nested `testdata/corpus/<archetype>/...` layout
//! would silently undercount with a single-level loader, and
//! the §3.4.1 definition of `bytes(raw_corpus)` is naturally
//! recursive.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

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

/// One loaded corpus line — the original input bytes (for the
/// `C1` reconstruction comparison) plus the `OtlpLogRecord`
/// handed to the miner.
#[derive(Debug)]
pub(crate) struct CorpusLine {
    /// The trimmed `*.txt` line as read from disk.
    pub line: String,
    /// The OTLP record fed to `MinerCluster::ingest`.
    pub record: OtlpLogRecord,
}

/// Aggregate output of [`load`]: lines + the `corpus` metadata
/// fields the §3.6 results JSON requires.
#[derive(Debug)]
pub(crate) struct CorpusLoad {
    pub lines: Vec<CorpusLine>,
    /// Number of `*.txt` files found under the corpus
    /// directory (recursive). Empty files count — the §3.4.1
    /// A1 formula sums `metadata.len()` over every `*.txt`,
    /// not just non-empty ones, so the diagnostic surfaces
    /// the input the formula actually consumed.
    pub total_files: u32,
    /// Sum of `fs::metadata(*.txt).len()` over every `*.txt`
    /// file found (empty files included). Matches the §3.4.1
    /// `bytes(raw_corpus)` formula 1:1.
    pub raw_bytes: u64,
    /// User-facing directory path string for the results JSON.
    pub directory: String,
}

/// Load every `*.txt` file under `dir` (recursive) into a
/// [`CorpusLoad`]. Errors with [`BenchError::Corpus`] when the
/// directory is unreadable, contains no `*.txt` files, or
/// every contributing file is empty.
pub(crate) fn load(dir: &Path) -> Result<CorpusLoad, BenchError> {
    let mut total_files = 0u32;
    let mut raw_bytes = 0u64;
    let mut lines = Vec::new();
    let tenant = TenantId::new(BENCH_TENANT);
    let mut next_ns = TIME_BASELINE_NS;

    walk(
        dir,
        &mut total_files,
        &mut raw_bytes,
        &mut lines,
        &tenant,
        &mut next_ns,
    )?;

    if lines.is_empty() {
        return Err(BenchError::Corpus {
            detail: format!(
                "no non-empty `*.txt` lines under {} (read {} file(s))",
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

fn walk(
    dir: &Path,
    total_files: &mut u32,
    raw_bytes: &mut u64,
    lines: &mut Vec<CorpusLine>,
    tenant: &TenantId,
    next_ns: &mut u64,
) -> Result<(), BenchError> {
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
            walk(&path, total_files, raw_bytes, lines, tenant, next_ns)?;
            continue;
        }
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
        {
            continue;
        }
        *total_files += 1;
        *raw_bytes += meta.len();
        // Stream lines via `BufReader` rather than slurping
        // the whole file with `read_to_string` — RFC 0006
        // sizes corpora at low millions of lines per file,
        // and a 100 MiB-class single-file corpus would spike
        // memory unnecessarily if we read the full contents
        // up front.
        let file = File::open(&path).map_err(|e| BenchError::Corpus {
            detail: format!("open({}): {e}", path.display()),
        })?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let raw = line.map_err(|e| BenchError::Corpus {
                detail: format!("read line in {}: {e}", path.display()),
            })?;
            let trimmed = raw.trim_end_matches('\r');
            if trimmed.is_empty() {
                continue;
            }
            let record = OtlpLogRecord {
                tenant_id: tenant.clone(),
                body: Some(Body::String(trimmed.to_string())),
                time_unix_nano: *next_ns,
                severity_number: BENCH_SEVERITY_NUMBER,
                severity_text: Some(BENCH_SEVERITY_TEXT.to_string()),
                ..Default::default()
            };
            lines.push(CorpusLine {
                line: trimmed.to_string(),
                record,
            });
            *next_ns = next_ns.saturating_add(TIME_INCREMENT_NS);
        }
    }
    Ok(())
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
        assert_eq!(first.record.time_unix_nano, TIME_BASELINE_NS);
        assert_eq!(first.record.tenant_id.as_str(), BENCH_TENANT);
        assert_eq!(first.record.severity_number, BENCH_SEVERITY_NUMBER);
        assert_eq!(
            first.record.severity_text.as_deref(),
            Some(BENCH_SEVERITY_TEXT),
        );
        assert_eq!(first.record.scope_name, None);
        assert_eq!(first.record.scope_version, None);
        assert!(
            matches!(first.record.body, Some(Body::String(_))),
            "every line wraps as Body::String",
        );

        // Subsequent lines advance by exactly TIME_INCREMENT_NS
        // (mod the saturating-add edge case which can't fire on
        // any realistic corpus). Pin on the second line.
        if let Some(second) = load.lines.get(1) {
            assert_eq!(
                second.record.time_unix_nano,
                TIME_BASELINE_NS + TIME_INCREMENT_NS,
            );
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
}
