//! §3.6 results-file writer.
//!
//! Serialises a [`ResultsFile`] to a per-run JSON file under
//! the results directory. The §9 `docs/benchmarks.md`
//! appender (the `--update-benchmarks-md` path) is a separate
//! follow-up — this module only owns the machine-readable
//! JSON artifact, which lands on every run regardless of the
//! markdown flag.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{BenchError, ResultsFile};

/// Upper bound on collision-suffix candidates tried before
/// giving up. A few thousand results files sharing one
/// `(timestamp-ms, git_sha)` is already pathological; the cap
/// stops a wedged filesystem from spinning forever.
const MAX_COLLISION_CANDIDATES: u32 = 10_000;

/// Write `results` as pretty JSON to `results_dir`, returning
/// the path written. The file name is
/// `<timestamp>-<git_sha>.json` per §3.6, with a numeric
/// suffix (`-1`, `-2`, …) appended on collision so two runs
/// landing in the same millisecond on the same commit don't
/// clobber each other.
///
/// Each candidate is created with `OpenOptions::create_new`
/// (atomic "create iff absent") and retried on
/// `AlreadyExists`, so the file is never clobbered — neither
/// by a TOCTOU race against a concurrent run nor by the
/// suffix budget running out (that returns an error rather
/// than overwriting `<stem>-<MAX>.json`).
///
/// # Errors
///
/// [`BenchError::Report`] when the directory can't be created,
/// the results can't be serialised, the file write fails, or
/// all `MAX_COLLISION_CANDIDATES + 1` name candidates are
/// taken (the unsuffixed `<stem>.json` plus
/// `<stem>-1 ..= <stem>-MAX`).
pub fn write_results_json(
    results: &ResultsFile,
    results_dir: &Path,
) -> Result<PathBuf, BenchError> {
    std::fs::create_dir_all(results_dir).map_err(|e| BenchError::Report {
        detail: format!("create_dir_all({}): {e}", results_dir.display()),
    })?;

    let stem = file_stem(&results.timestamp, &results.git_sha);
    let json = serde_json::to_string_pretty(results).map_err(|e| BenchError::Report {
        detail: format!("serialise results: {e}"),
    })?;

    for counter in 0..=MAX_COLLISION_CANDIDATES {
        let path = if counter == 0 {
            results_dir.join(format!("{stem}.json"))
        } else {
            results_dir.join(format!("{stem}-{counter}.json"))
        };
        // `create_new` is atomic: it fails with `AlreadyExists`
        // rather than truncating an existing file, closing the
        // check-then-write race.
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(json.as_bytes())
                    .map_err(|e| BenchError::Report {
                        detail: format!("write({}): {e}", path.display()),
                    })?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(BenchError::Report {
                    detail: format!("create({}): {e}", path.display()),
                });
            }
        }
    }

    Err(BenchError::Report {
        detail: format!(
            "exhausted {} results-file name candidates for stem {stem} under {} — every \
             <stem>[-N].json is taken",
            MAX_COLLISION_CANDIDATES + 1,
            results_dir.display(),
        ),
    })
}

/// File-name stem `<timestamp>-<git_sha>`, with `:` from the
/// RFC3339 timestamp replaced by `-`. Colons are illegal in
/// filenames on Windows (and awkward on some tooling), so the
/// on-disk name uses a colon-free form even though the
/// `timestamp` field inside the JSON keeps canonical RFC3339.
fn file_stem(timestamp: &str, git_sha: &str) -> String {
    format!("{}-{}", timestamp.replace(':', "-"), git_sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{A1Result, C1Result, CorpusStats, OuriosStats, ZstdStats};

    fn sample_results() -> ResultsFile {
        ResultsFile {
            rfc: "RFC 0006".to_string(),
            rfc_version: "v1".to_string(),
            timestamp: "2026-05-26T14:30:00.123Z".to_string(),
            git_sha: "abc1234".to_string(),
            hardware_kind: "baseline-8vcpu-32gib".to_string(),
            corpus: CorpusStats {
                directory: "testdata/corpus".to_string(),
                total_lines: 100,
                total_files: 2,
                raw_bytes: 4096,
            },
            ourios: OuriosStats {
                data_parquet_bytes: 300,
                audit_parquet_bytes: 0,
                total_parquet_bytes: 300,
            },
            zstd: ZstdStats {
                level: 19,
                compressed_bytes: 1024,
            },
            a1: Some(A1Result {
                ourios_ratio: 13.6,
                zstd_ratio: 4.0,
                delta: 3.4,
                target_delta: 3.0,
                pass: true,
            }),
            c1: Some(C1Result {
                non_lossy_total: 100,
                non_lossy_reconstruct_ok: 100,
                rate: 1.0,
                lossy_flag_ratio: 0.0,
                pass: true,
            }),
            c2: None,
        }
    }

    /// RFC0006.4 (JSON half): a written results file parses
    /// back to an equal `ResultsFile` and carries the §3.6
    /// required keys. Pins the on-disk contract downstream
    /// analysis depends on.
    #[test]
    fn results_json_round_trips_through_disk() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let original = sample_results();
        let path = write_results_json(&original, tmp.path()).expect("write");

        assert!(path.exists(), "results file written");
        let text = std::fs::read_to_string(&path).expect("read back");
        // Every §3.6 required key is present as a top-level
        // object key (parse to `Value` rather than substring-
        // matching, which would false-positive if a key name
        // appeared inside a string value).
        let value: serde_json::Value = serde_json::from_str(&text).expect("parse to value");
        let obj = value.as_object().expect("top level is a JSON object");
        for key in [
            "rfc",
            "rfc_version",
            "timestamp",
            "git_sha",
            "hardware_kind",
            "corpus",
            "ourios",
            "zstd",
            "a1",
            "c1",
            "c2",
        ] {
            assert!(obj.contains_key(key), "missing top-level key {key}");
        }
        let parsed: ResultsFile = serde_json::from_str(&text).expect("parse");
        assert_eq!(parsed, original, "round-trip preserves every field");
    }

    /// The on-disk name is colon-free (RFC3339 colons → `-`) so
    /// it's valid on every filesystem, and it embeds the git
    /// sha.
    #[test]
    fn file_name_is_colon_free_and_embeds_sha() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = write_results_json(&sample_results(), tmp.path()).expect("write");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(!name.contains(':'), "no colons in filename: {name}");
        assert!(name.contains("abc1234"), "filename embeds git sha: {name}");
        assert!(name.ends_with(".json"));
    }

    /// A second write on the same `(timestamp, sha)` gets a
    /// distinct suffixed file rather than clobbering the first
    /// — the §3.6 collision-retry rule.
    #[test]
    fn collision_appends_a_suffix() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let r = sample_results();
        let first = write_results_json(&r, tmp.path()).expect("first");
        let second = write_results_json(&r, tmp.path()).expect("second");
        assert_ne!(first, second, "second run gets a distinct path");
        assert!(first.exists() && second.exists(), "both files survive");
    }
}
