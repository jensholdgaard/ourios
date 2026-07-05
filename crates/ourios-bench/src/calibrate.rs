//! RFC 0024 §3.1 calibration extraction — the `--calibrate` pass.
//!
//! Streams a corpus through the same loader the gates run on and
//! folds every record into
//! [`ourios_testgen::manifest::CalibrationAccumulator`], producing
//! the committed distribution summary
//! (`testdata/calibration/<corpus-tag>.json`) that shapes
//! [`ourios_testgen::strategies::calibrated`]. The manifest is a
//! measurement: every accumulated statistic is order-insensitive
//! (ordered maps, counters, maxima), so reruns over the same corpus
//! are byte-identical regardless of walk order (RFC0024.1).

use std::path::{Path, PathBuf};

use ourios_testgen::manifest::{CalibrationAccumulator, CalibrationManifest};

use crate::BenchError;
use crate::corpus::{self, TxtSeverity};

/// Where committed manifests live, relative to the repo root —
/// `--calibrate` defaults its output to
/// `<CALIBRATION_DIR>/<corpus-tag>.json`.
pub const CALIBRATION_DIR: &str = "testdata/calibration";

/// Stream the corpus at `corpus_dir` and measure it into a
/// [`CalibrationManifest`] tagged `corpus_tag`.
///
/// # Errors
///
/// - [`BenchError::Corpus`] if the directory is missing, empty
///   (no files, or files with no records — same semantics as the
///   gate loaders), or a record fails to read/parse mid-stream.
pub fn extract_manifest(
    corpus_dir: &Path,
    corpus_tag: &str,
    txt_severity: TxtSeverity,
) -> Result<CalibrationManifest, BenchError> {
    let (stream, meta) = corpus::stream(corpus_dir, txt_severity)?;
    let mut accumulator = CalibrationAccumulator::new();
    for record in stream {
        accumulator.observe(&record?);
    }
    let manifest = accumulator.finish(corpus_tag);
    if manifest.records == 0 {
        // Files full of blank lines stream zero records; a 0-record
        // manifest can't shape a generator, so fail exactly like the
        // eager loader's empty-corpus diagnostic.
        return Err(corpus::no_lines_error(corpus_dir, meta.total_files));
    }
    Ok(manifest)
}

/// Write `manifest` in its committed-file form (deterministic pretty
/// JSON + trailing newline), creating parent directories as needed.
/// Returns the path written.
///
/// # Errors
///
/// [`BenchError::Report`] on serialization or file-system failure.
pub fn write_manifest(manifest: &CalibrationManifest, out: &Path) -> Result<PathBuf, BenchError> {
    let bytes = manifest.to_json_bytes().map_err(|e| BenchError::Report {
        detail: format!("serialize calibration manifest: {e}"),
    })?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| BenchError::Report {
            detail: format!("create {}: {e}", parent.display()),
        })?;
    }
    std::fs::write(out, bytes).map_err(|e| BenchError::Report {
        detail: format!("write {}: {e}", out.display()),
    })?;
    Ok(out.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus that streams zero records (files exist, but only
    /// blank lines) is rejected with the loaders' empty-corpus
    /// diagnostic instead of producing a 0-record manifest.
    #[test]
    fn extract_manifest_rejects_a_recordless_corpus() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("blank.txt"), "\n\n\n").expect("write fixture");
        let err = extract_manifest(dir.path(), "empty", TxtSeverity::Fixed)
            .expect_err("blank-line corpus must not calibrate");
        assert!(matches!(err, BenchError::Corpus { .. }), "got {err}");
    }
}
