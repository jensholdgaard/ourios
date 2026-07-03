//! §3.6 results-file writer + §9 `docs/benchmarks.md` appender.
//!
//! Two outputs grow out of a bench run:
//!
//! - the machine-readable per-run JSON file
//!   ([`write_results_json`]), written on every run; and
//! - the human-readable `docs/benchmarks.md` §9 Results
//!   summary ([`update_status_section`]), written only when
//!   `--update-benchmarks-md` is passed.
//!
//! The §9 appender keeps one block per `(git_sha,
//! hardware_kind)` pair inside a bench-managed region; re-runs
//! rewrite the matching block in place (no duplicate rows per
//! RFC0006.4) and a partial `--gates` run updates only the
//! gates it measured, leaving the others' prior numbers intact
//! (RFC0006.6).

use std::collections::BTreeMap;
use std::fmt::Write as _;
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

/// Start of the bench-managed region inside §9. Everything
/// between this and [`REGION_END`] is regenerated on each
/// `--update-benchmarks-md` run; prose outside it is never
/// touched.
const REGION_BEGIN: &str = "<!-- BENCH-RESULTS:BEGIN (managed by `ourios-bench --update-benchmarks-md`; do not edit by hand) -->";
const REGION_END: &str = "<!-- BENCH-RESULTS:END -->";

/// One gate's row in a §9 results block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateRow {
    measurement: String,
    target: String,
    verdict: String,
}

/// The recorded gates for one `(git_sha, hardware_kind)` block.
/// Each gate is independently `Some`/`None` so a partial
/// `--gates` run merges without disturbing gates it didn't
/// measure (RFC0006.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Block {
    date: String,
    a1: Option<GateRow>,
    c1: Option<GateRow>,
    c2: Option<GateRow>,
}

/// Update the §9 Results summary in `md` with `results`,
/// returning the new markdown. Pure (`&str -> String`) so the
/// merge / rewrite logic is testable without touching the
/// filesystem; the binary reads `docs/benchmarks.md`, calls
/// this, and writes it back.
///
/// The block for the run's `(git_sha, hardware_kind)` is
/// created or rewritten in place; only the gates present in
/// `results` (`a1` / `c1` / `c2` that are `Some`) are
/// replaced, so a `--gates c1` re-run leaves any previously
/// recorded A1 / C2 numbers for that pair untouched.
///
/// # Errors
///
/// [`BenchError::Report`] when:
/// - the managed region doesn't exist yet **and** `md` has no
///   `## 9. Status` heading to anchor a fresh region to (the
///   anchor is only consulted on this first-insertion path —
///   once the marker region exists, updates are
///   region-relative and don't re-check the heading); or
/// - the `BENCH-RESULTS` marker pair is mismatched (a `BEGIN`
///   without a following `END`, or an orphan `END`).
pub fn update_status_section(md: &str, results: &ResultsFile) -> Result<String, BenchError> {
    // The block header is backtick-delimited
    // (`#### `sha` on `hw` — updated …`); a backtick or
    // control char (notably a newline) in a user-influenced
    // field would corrupt that format and mis-parse on the
    // next run. Reject them up front. `git_sha` is hex-or-
    // "unknown" so it can't trip this, but it's cheap to guard
    // both header-bound fields.
    ensure_header_safe("hardware_kind", &results.hardware_kind)?;
    ensure_header_safe("git_sha", &results.git_sha)?;

    let mut blocks = parse_region(md)?;

    // Merge this run into the block for its (sha, hardware)
    // pair — created if absent, gate-rows replaced only for
    // the gates that ran.
    // The displayed "updated <date>" is the `YYYY-MM-DD`
    // prefix of the RFC3339 timestamp; validate it rather than
    // emitting an empty date from a malformed field.
    let date = results_date(&results.timestamp)?;
    let key = (results.git_sha.clone(), results.hardware_kind.clone());
    let block = blocks.entry(key).or_default();
    block.date = date;
    if let Some(a1) = &results.a1 {
        block.a1 = Some(GateRow {
            measurement: format!(
                "delta {:.3}× (ourios {:.3}× / zstd-19 {:.3}×)",
                a1.delta, a1.ourios_ratio, a1.zstd_ratio,
            ),
            target: format!("≥ {:.1}×", a1.target_delta),
            verdict: pass_verdict(a1.pass),
        });
    }
    if let Some(c1) = &results.c1 {
        block.c1 = Some(GateRow {
            measurement: format!(
                "{:.6} ({}/{} non-lossy; lossy {:.4})",
                c1.rate, c1.non_lossy_reconstruct_ok, c1.non_lossy_total, c1.lossy_flag_ratio,
            ),
            // Target shown in the same fraction form as the
            // measurement (`1.000000`), not `100.000%` — the
            // measurement column is the fraction, so a percent
            // target would mix units.
            target: "1.000000".to_string(),
            verdict: pass_verdict(c1.pass),
        });
    }
    if let Some(c2) = &results.c2 {
        // §3.4.3 pairs `convergence_ratio` and
        // `template_count_at_1m_lines`: both `Some` on a ≥ 1 M
        // corpus, both `None` on abstention. A mixed state is
        // a corrupt `ResultsFile`, surfaced rather than papered
        // over with a `0` count.
        let measurement = match (c2.template_count_at_1m_lines, c2.convergence_ratio) {
            (Some(count_1m), Some(ratio)) => {
                format!(
                    "ratio {ratio:.3} (count@1M {count_1m} / SS {})",
                    c2.template_count_at_end,
                )
            }
            (None, None) => format!("n/a (SS {}, corpus < 1 M lines)", c2.template_count_at_end),
            _ => {
                return Err(BenchError::Report {
                    detail: "C2 result is inconsistent: convergence_ratio and \
                             template_count_at_1m_lines must both be set or both absent \
                             (§3.4.3)"
                        .to_string(),
                });
            }
        };
        let verdict = match c2.pass {
            Some(true) => "PASS".to_string(),
            Some(false) => "FAIL".to_string(),
            None => "ABSTAIN".to_string(),
        };
        block.c2 = Some(GateRow {
            measurement,
            target: "≥ 0.5".to_string(),
            verdict,
        });
    }

    let region = render_region(&blocks);
    splice_region(md, &region)
}

fn pass_verdict(pass: bool) -> String {
    if pass { "PASS" } else { "FAIL" }.to_string()
}

/// Reject a header-bound field that would break the
/// backtick-delimited `#### …` block header — a backtick (the
/// field delimiter) or any control character (newline / tab /
/// etc.). Surfaced as [`BenchError::Report`] rather than
/// silently writing a region that won't round-trip.
fn ensure_header_safe(field: &str, value: &str) -> Result<(), BenchError> {
    if value.contains('`') || value.chars().any(char::is_control) {
        return Err(BenchError::Report {
            detail: format!(
                "{field} {value:?} contains a backtick or control character, which would \
                 corrupt the §9 results-block header"
            ),
        });
    }
    Ok(())
}

/// True when `s` is exactly a `YYYY-MM-DD` date: 10 chars,
/// dashes at indices 4 and 7, ASCII digits everywhere else.
fn is_date_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

/// Extract the `YYYY-MM-DD` date from an RFC3339 `timestamp`,
/// erroring if its prefix isn't shaped like one. Guards
/// against a malformed `timestamp` (e.g. from corrupt JSON)
/// silently rendering an empty "updated " in the §9 header.
fn results_date(timestamp: &str) -> Result<String, BenchError> {
    match timestamp.get(..10) {
        Some(date) if is_date_shaped(date) => Ok(date.to_string()),
        _ => Err(BenchError::Report {
            detail: format!(
                "results timestamp {timestamp:?} is not RFC3339-shaped (need a YYYY-MM-DD prefix)"
            ),
        }),
    }
}

/// Parse the existing per-`(sha, hw)` blocks out of the
/// managed region. Returns empty when the region is absent
/// (first run). Only recognises the shape `render_region`
/// emits, so it round-trips its own output.
fn parse_region(md: &str) -> Result<BTreeMap<(String, String), Block>, BenchError> {
    let mut blocks = BTreeMap::new();
    let Some(region) = md
        .split_once(REGION_BEGIN)
        .and_then(|(_, rest)| rest.split_once(REGION_END))
        .map(|(inner, _)| inner)
    else {
        return Ok(blocks);
    };

    let mut current: Option<((String, String), Block)> = None;
    for line in region.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix("#### ") {
            if let Some((key, block)) = current.take() {
                blocks.insert(key, block);
            }
            // Every `####` in the managed region is a block
            // header we wrote. One that won't parse means the
            // region was hand-edited into a corrupt state;
            // surface it rather than silently dropping the
            // block (and its data) on the next rewrite.
            let (sha, hw, date) = parse_header(header).ok_or_else(|| BenchError::Report {
                detail: format!(
                    "unparseable results block header in the managed region: `#### {header}` \
                     — fix the BENCH-RESULTS region by hand"
                ),
            })?;
            current = Some((
                (sha, hw),
                Block {
                    date,
                    ..Block::default()
                },
            ));
        } else if let Some((_, block)) = current.as_mut()
            && let Some((gate, row)) = parse_row(line)
        {
            match gate {
                "A1" => block.a1 = Some(row),
                "C1" => block.c1 = Some(row),
                "C2" => block.c2 = Some(row),
                _ => {}
            }
        }
    }
    if let Some((key, block)) = current.take() {
        blocks.insert(key, block);
    }
    Ok(blocks)
}

/// Parse a block header of the form
/// "BACKTICK sha BACKTICK on BACKTICK hw BACKTICK — updated date"
/// into `(sha, hw, date)`. The sha and hw are the
/// backtick-delimited fields; the date is whatever follows
/// "updated ".
fn parse_header(header: &str) -> Option<(String, String, String)> {
    // The header `render_region` emits, after the `#### `
    // prefix, splits on backtick into exactly five segments:
    //   ["", sha, " on ", hw, " — updated <date>"].
    // Validate every fixed delimiter and reject any extra
    // backtick segment, so a hand-edited / corrupt header is
    // surfaced as corruption by `parse_region` rather than
    // silently normalised onto an unintended `(sha, hw)` key.
    let mut parts = header.split('`');
    let before = parts.next()?;
    let sha = parts.next()?;
    let mid = parts.next()?;
    let hw = parts.next()?;
    let tail = parts.next()?;
    if parts.next().is_some() {
        // Extra backtick → not our shape.
        return None;
    }
    if !before.is_empty() || mid != " on " {
        return None;
    }
    let date = tail.strip_prefix(" — updated ")?;
    if !is_date_shaped(date) {
        return None;
    }
    Some((sha.to_string(), hw.to_string(), date.to_string()))
}

/// Parse a table data row `| <gate> | <m> | <t> | <v> |` into
/// `(gate, GateRow)`. Returns `None` for non-data rows (header
/// / separator / anything not starting a recognised gate).
fn parse_row(line: &str) -> Option<(&'static str, GateRow)> {
    if !line.starts_with('|') {
        return None;
    }
    let cols: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
    if cols.len() != 4 {
        return None;
    }
    let gate = match cols[0] {
        "A1" => "A1",
        "C1" => "C1",
        "C2" => "C2",
        _ => return None,
    };
    Some((
        gate,
        GateRow {
            measurement: cols[1].to_string(),
            target: cols[2].to_string(),
            verdict: cols[3].to_string(),
        },
    ))
}

/// Render the full managed region (markers included) from the
/// merged blocks, in deterministic `(sha, hw)` order.
fn render_region(blocks: &BTreeMap<(String, String), Block>) -> String {
    let mut out = String::new();
    out.push_str(REGION_BEGIN);
    out.push_str("\n\n");
    for ((sha, hw), block) in blocks {
        let _ = writeln!(out, "#### `{sha}` on `{hw}` — updated {}", block.date);
        out.push('\n');
        out.push_str("| Gate | Measurement | Target | Verdict |\n");
        out.push_str("| --- | --- | --- | --- |\n");
        for (name, row) in [("A1", &block.a1), ("C1", &block.c1), ("C2", &block.c2)] {
            if let Some(row) = row {
                let _ = writeln!(
                    out,
                    "| {name} | {} | {} | {} |",
                    row.measurement, row.target, row.verdict,
                );
            }
        }
        out.push('\n');
    }
    out.push_str(REGION_END);
    out
}

/// Splice the rendered region back into `md`: replace an
/// existing region between the markers, or append a fresh
/// `### Results` sub-section at the end of the `## 9.` section
/// (which is the last section, so end-of-file).
///
/// The `END` marker is searched for **after** `BEGIN`, and a
/// mismatched pair (one marker without the other, or `END`
/// before `BEGIN`) is a [`BenchError::Report`] rather than a
/// silent second region — a half-present region means the doc
/// was hand-edited into a corrupt state, which we surface
/// instead of compounding. More than one of either marker is
/// likewise rejected: updating only the first region would
/// leave the others stale.
fn splice_region(md: &str, region: &str) -> Result<String, BenchError> {
    if md.matches(REGION_BEGIN).count() > 1 || md.matches(REGION_END).count() > 1 {
        return Err(BenchError::Report {
            detail: "docs/benchmarks.md has more than one BENCH-RESULTS region — collapse them \
                     to a single managed region by hand before re-running"
                .to_string(),
        });
    }
    let begin = md.find(REGION_BEGIN);
    // Only accept an END that comes after the BEGIN marker.
    let end = begin.and_then(|b| {
        let after = b + REGION_BEGIN.len();
        md[after..].find(REGION_END).map(|rel| after + rel)
    });

    match (begin, end) {
        // Well-formed region: replace it (markers included).
        (Some(b), Some(e)) => {
            let end_abs = e + REGION_END.len();
            let mut out = String::with_capacity(md.len());
            out.push_str(&md[..b]);
            out.push_str(region);
            out.push_str(&md[end_abs..]);
            Ok(out)
        }
        // No markers at all: first run — anchor a fresh region
        // to the §9 Status section.
        (None, None) if !md.contains(REGION_END) => {
            if !md.contains("## 9. Status") {
                return Err(BenchError::Report {
                    detail: "docs/benchmarks.md has no `## 9. Status` section to anchor the \
                             results region"
                        .to_string(),
                });
            }
            // Preserve `md` byte-for-byte (prose outside the
            // region is never touched, including its trailing
            // whitespace); only *add* the newlines needed to
            // separate the appended section.
            let mut out = md.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str("### Results\n\n");
            out.push_str(region);
            out.push('\n');
            Ok(out)
        }
        // Corrupt: BEGIN without a following END, or an END
        // marker with no (preceding) BEGIN.
        _ => Err(BenchError::Report {
            detail: "docs/benchmarks.md has a mismatched BENCH-RESULTS marker pair (a BEGIN \
                     without a following END, or an orphan END) — fix the managed region by \
                     hand before re-running --update-benchmarks-md"
                .to_string(),
        }),
    }
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
                mismatches: Vec::new(),
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

    /// Minimal §9-bearing markdown to feed the appender (the
    /// `## 9. Status` anchor is all `update_status_section`
    /// requires).
    fn md_with_status() -> String {
        "# Benchmarks\n\n## 9. Status\n\nNo benchmark has been run yet.\n".to_string()
    }

    /// First `--update-benchmarks-md` run with no prior region
    /// creates a `### Results` sub-section with one block for
    /// the run's `(git_sha, hardware_kind)`, and the
    /// prior prose survives.
    #[test]
    fn first_update_creates_results_block() {
        let md = update_status_section(&md_with_status(), &sample_results()).expect("update");
        assert!(md.contains("## 9. Status"), "prose section preserved");
        assert!(md.contains("### Results"));
        assert!(md.contains("#### `abc1234` on `baseline-8vcpu-32gib`"));
        assert!(md.contains("| A1 |") && md.contains("| C1 |"));
        // C2 was None in the sample → no C2 row.
        assert!(!md.contains("| C2 |"), "absent gate has no row");
    }

    /// RFC0006.4 — re-running on the same `(git_sha,
    /// hardware_kind)` rewrites the block in place; no
    /// duplicate sub-heading.
    #[test]
    fn rerun_same_sha_hw_rewrites_in_place() {
        let once = update_status_section(&md_with_status(), &sample_results()).expect("once");
        let mut updated = sample_results();
        // A different measurement on the same (sha, hw).
        updated.c1.as_mut().unwrap().rate = 0.999_999;
        updated.c1.as_mut().unwrap().non_lossy_reconstruct_ok = 99;
        let twice = update_status_section(&once, &updated).expect("twice");

        assert_eq!(
            twice
                .matches("#### `abc1234` on `baseline-8vcpu-32gib`")
                .count(),
            1,
            "same (sha, hw) must not duplicate the sub-heading",
        );
        assert!(
            twice.contains("0.999999"),
            "block reflects the rerun's number"
        );
    }

    /// RFC0006.6 — a partial `--gates c1` run updates only the
    /// C1 row, leaving the A1 number previously recorded for
    /// the same `(sha, hw)` intact.
    #[test]
    fn partial_gate_rerun_preserves_other_gates() {
        // First a full A1+C1 run.
        let full = update_status_section(&md_with_status(), &sample_results()).expect("full");
        let a1_line = full
            .lines()
            .find(|l| l.starts_with("| A1 |"))
            .expect("A1 row present after full run")
            .to_string();

        // Then a C1-only rerun (A1 / C2 absent from results).
        let mut c1_only = sample_results();
        c1_only.a1 = None;
        c1_only.c2 = None;
        c1_only.c1.as_mut().unwrap().rate = 0.5;
        let after = update_status_section(&full, &c1_only).expect("c1-only");

        // A1 row is byte-for-byte preserved; C1 row changed.
        assert!(
            after.lines().any(|l| l == a1_line),
            "the prior A1 row must survive a C1-only rerun untouched",
        );
        assert!(after.contains("0.500000"), "C1 row reflects the rerun");
    }

    /// Missing the `## 9.` anchor is a `Report` error rather
    /// than silently producing a malformed doc.
    #[test]
    fn missing_status_section_errors() {
        let err = update_status_section("# Benchmarks\n\nno section nine\n", &sample_results())
            .expect_err("must error without §9");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// A half-present managed region (a BEGIN marker with no
    /// following END) is a `Report` error — we don't append a
    /// second region on top of a corrupt one.
    #[test]
    fn mismatched_marker_pair_errors() {
        let corrupt = format!("## 9. Status\n\n{REGION_BEGIN}\n\n(truncated, no end marker)\n");
        let err = update_status_section(&corrupt, &sample_results())
            .expect_err("a BEGIN without END must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// An unparseable `####` header inside the managed region
    /// is corruption — surfaced as `Report`, not silently
    /// dropped (which would lose the block on rewrite).
    #[test]
    fn corrupt_block_header_errors() {
        let corrupt = format!(
            "## 9. Status\n\n{REGION_BEGIN}\n\n#### not a valid block header\n\n{REGION_END}\n"
        );
        let err = update_status_section(&corrupt, &sample_results())
            .expect_err("a malformed block header must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// A `Some(ratio)` + `None` count C2 result is an
    /// impossible §3.4.3 state → `Report`, not a silent
    /// `count@1M 0`.
    #[test]
    fn inconsistent_c2_result_errors() {
        let mut r = sample_results();
        r.c2 = Some(crate::C2Result {
            sample_cadence: 1000,
            total_lines: 1_000_000,
            template_count_at_1m_lines: None, // missing…
            template_count_at_end: 42,
            convergence_ratio: Some(0.9), // …but ratio present
            convergence_curve: Vec::new(),
            pass: Some(true),
            corpus_at_least_1m: true,
        });
        let err =
            update_status_section(&md_with_status(), &r).expect_err("inconsistent C2 must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// A malformed `timestamp` (not RFC3339-shaped) errors
    /// rather than rendering an empty "updated " date.
    #[test]
    fn malformed_timestamp_errors() {
        let mut r = sample_results();
        r.timestamp = "not-a-date".to_string();
        let err = update_status_section(&md_with_status(), &r)
            .expect_err("malformed timestamp must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// A block header missing the "updated <date>" suffix is
    /// corruption (an empty date would otherwise round-trip)
    /// → `parse_region` raises `Report`.
    #[test]
    fn block_header_without_date_errors() {
        let corrupt = format!(
            "## 9. Status\n\n{REGION_BEGIN}\n\n#### `deadbee` on `somebox`\n\n{REGION_END}\n"
        );
        let err = update_status_section(&corrupt, &sample_results())
            .expect_err("a dateless block header must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// A block header with the wrong fixed delimiter (here
    /// `" ON "` instead of `" on "`) is corruption — it must
    /// not be silently normalised onto a `(sha, hw)` key.
    #[test]
    fn block_header_wrong_delimiter_errors() {
        let corrupt = format!(
            "## 9. Status\n\n{REGION_BEGIN}\n\n#### `deadbee` ON `somebox` — updated 2026-05-26\n\n{REGION_END}\n"
        );
        let err = update_status_section(&corrupt, &sample_results())
            .expect_err("a wrong-delimiter header must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// A backtick in `hardware_kind` would break the
    /// backtick-delimited header → rejected as `Report` before
    /// any write.
    #[test]
    fn backtick_in_hardware_kind_errors() {
        let mut r = sample_results();
        r.hardware_kind = "evil`box".to_string();
        let err = update_status_section(&md_with_status(), &r)
            .expect_err("backtick in hardware_kind must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// More than one managed region is corruption — updating
    /// only the first would leave the others stale, so it's a
    /// `Report` error.
    #[test]
    fn multiple_regions_error() {
        let doc = format!(
            "## 9. Status\n\n{REGION_BEGIN}\n\n{REGION_END}\n\nstray\n\n{REGION_BEGIN}\n\n{REGION_END}\n"
        );
        let err =
            update_status_section(&doc, &sample_results()).expect_err("two regions must error");
        assert!(matches!(err, BenchError::Report { .. }), "got {err:?}");
    }

    /// First insertion preserves the prose byte-for-byte (no
    /// trailing-whitespace trimming outside the markers); the
    /// original content survives unchanged as a prefix.
    #[test]
    fn first_insertion_preserves_prose_verbatim() {
        let original = "# Benchmarks\n\n## 9. Status\n\nNo run yet.\n";
        let updated = update_status_section(original, &sample_results()).expect("update");
        assert!(
            updated.starts_with(original),
            "original prose must be preserved verbatim as a prefix:\n{updated}",
        );
    }
}
