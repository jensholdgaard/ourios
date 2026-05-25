//! A1 — Compression ratio.
//!
//! Per RFC 0006 §3.4.1:
//!
//! ```text
//! ourios_ratio = bytes(raw_corpus) / bytes(ourios_output)
//! zstd_ratio   = bytes(raw_corpus) / bytes(zstd_corpus)
//! A1_delta     = ourios_ratio / zstd_ratio
//! ```
//!
//! - `bytes(raw_corpus)` — sum of `*.txt` file sizes (computed
//!   by [`crate::corpus`] during load, passed in here).
//! - `bytes(ourios_output)` — sum of every `*.parquet` file
//!   under the bench's output bucket, **including the
//!   `audit/...` series**; the pre-rename `*.parquet.tmp`
//!   files are skipped (RFC 0005 §7 atomic-publish).
//! - `bytes(zstd_corpus)` — sum of `zstd -19` output over each
//!   `*.txt` **individually** (not concatenated — per-file is
//!   the honest, stricter comparison the Drain paper uses).
//! - `A1_delta` — the ratio of ratios, rounded *down* to three
//!   significant figures so reported numbers err pessimistic.
//!
//! The accumulator streams emitted records into per-partition
//! `ourios_parquet::Writer`s during the harness loop (memory
//! stays at ~one row group per open partition), captures the
//! miner's audit-event stream, then measures the on-disk
//! footprint at finalize time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ourios_core::audit::AuditEvent;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_parquet::{AuditWriter, PartitionKey, Writer};

use crate::A1Result;

/// RFC 0006 §3.4.1 ZSTD reference level. Level 19 (not 3)
/// matches the Drain paper's published comparison and is the
/// strictest competent byte codec; ZSTD-3 would make A1
/// trivially pass.
const ZSTD_LEVEL: i32 = 19;

/// RFC 0006 §3.4.1 pass target — Ourios must beat ZSTD-alone
/// by at least 3×.
const TARGET_DELTA: f64 = 3.0;

/// Everything [`A1Accumulator::finalize`] hands back: the
/// §3.4.1 ratio result plus the raw byte counts `run` needs to
/// populate the §3.6 `ourios` / `zstd` blocks. Bundled so the
/// measurement is computed once and the caller doesn't
/// re-derive the splits.
pub(crate) struct A1Outcome {
    pub result: A1Result,
    pub data_parquet_bytes: u64,
    pub audit_parquet_bytes: u64,
    pub total_parquet_bytes: u64,
    pub zstd_bytes: u64,
}

/// Streaming accumulator for the §3.4.1 A1 measurement.
///
/// `record` is called once per emitted record by the harness
/// loop and writes it into the partition's data Parquet file.
/// Writes can fail (I/O); since the harness callback can't
/// return a `Result`, the first error is stashed in `error`
/// and surfaced by [`Self::finalize`] — subsequent `record`
/// calls become no-ops so we don't pile errors on a poisoned
/// writer.
pub(crate) struct A1Accumulator {
    bucket_root: PathBuf,
    data_writers: HashMap<PartitionKey, Writer>,
    error: Option<crate::BenchError>,
}

impl A1Accumulator {
    /// Create an accumulator writing under `bucket_root`. The
    /// directory is created lazily by the per-partition
    /// writers on first record.
    pub(crate) fn new(bucket_root: &Path) -> Self {
        Self {
            bucket_root: bucket_root.to_path_buf(),
            data_writers: HashMap::new(),
            error: None,
        }
    }

    /// Stream one emitted record into its partition's data
    /// writer. No-op once an earlier write has errored.
    pub(crate) fn record(&mut self, emitted: &MinedRecord) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = self.record_inner(emitted) {
            self.error = Some(e);
        }
    }

    fn record_inner(&mut self, emitted: &MinedRecord) -> Result<(), crate::BenchError> {
        let partition = PartitionKey::derive(emitted).map_err(|e| crate::BenchError::Pipeline {
            detail: format!("partition derive failed: {e}"),
        })?;
        let writer = match self.data_writers.entry(partition.clone()) {
            std::collections::hash_map::Entry::Occupied(w) => w.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let w = Writer::open(&self.bucket_root, partition).map_err(|e| {
                    crate::BenchError::Pipeline {
                        detail: format!("parquet writer open: {e}"),
                    }
                })?;
                slot.insert(w)
            }
        };
        writer
            .append_records(std::slice::from_ref(emitted))
            .map_err(|e| crate::BenchError::Pipeline {
                detail: format!("parquet append_records: {e}"),
            })
    }

    /// Write the miner's audit-event stream into the
    /// `audit/...` partition series. Grouped by the same
    /// partition key the data records use (derived from a
    /// proxy record carrying the event's tenant + timestamp,
    /// the established `ourios-parquet` pattern). Called once
    /// after the harness loop, before [`Self::finalize`].
    pub(crate) fn write_audit(&mut self, events: &[AuditEvent]) -> Result<(), crate::BenchError> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        let mut by_partition: HashMap<PartitionKey, Vec<AuditEvent>> = HashMap::new();
        for event in events {
            let partition = audit_partition(event)?;
            by_partition
                .entry(partition)
                .or_default()
                .push(event.clone());
        }
        for (partition, events) in by_partition {
            let mut writer = AuditWriter::open(&self.bucket_root, partition).map_err(|e| {
                crate::BenchError::Pipeline {
                    detail: format!("audit writer open: {e}"),
                }
            })?;
            writer
                .append_events(&events)
                .map_err(|e| crate::BenchError::Pipeline {
                    detail: format!("audit append_events: {e}"),
                })?;
            writer.close().map_err(|e| crate::BenchError::Pipeline {
                detail: format!("audit writer close: {e}"),
            })?;
        }
        Ok(())
    }

    /// Close every data writer, measure the on-disk footprint,
    /// run the ZSTD-19 reference codec over `corpus_dir`, and
    /// compute the §3.4.1 ratios.
    ///
    /// `raw_bytes` is `bytes(raw_corpus)` from the corpus load
    /// (sum of `*.txt` sizes); `corpus_dir` is re-walked here
    /// to feed the reference codec per file.
    pub(crate) fn finalize(
        mut self,
        raw_bytes: u64,
        corpus_dir: &Path,
    ) -> Result<A1Outcome, crate::BenchError> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        // Close every data writer so the `*.parquet.tmp` files
        // are atomically renamed to their final `*.parquet`
        // names before we measure.
        for (_partition, writer) in self.data_writers.drain() {
            writer.close().map_err(|e| crate::BenchError::Pipeline {
                detail: format!("parquet writer close: {e}"),
            })?;
        }

        let data_parquet_bytes = sum_parquet_bytes(&self.bucket_root.join("data"))?;
        let audit_parquet_bytes = sum_parquet_bytes(&self.bucket_root.join("audit"))?;
        let total_parquet_bytes = data_parquet_bytes + audit_parquet_bytes;
        let zstd_bytes = zstd_level_19_bytes(corpus_dir)?;

        Ok(A1Outcome {
            result: compute_a1(raw_bytes, total_parquet_bytes, zstd_bytes),
            data_parquet_bytes,
            audit_parquet_bytes,
            total_parquet_bytes,
            zstd_bytes,
        })
    }
}

/// Derive the partition key for an audit event from a proxy
/// `MinedRecord` carrying the event's tenant + timestamp — the
/// same approach `ourios-parquet`'s audit round-trip test uses
/// (`PartitionKey::derive` only reads tenant + timestamp).
fn audit_partition(event: &AuditEvent) -> Result<PartitionKey, crate::BenchError> {
    let nanos = event
        .timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| crate::BenchError::Pipeline {
            detail: format!("audit event timestamp before Unix epoch: {e}"),
        })?
        .as_nanos();
    // The on-disk `time_unix_nano` is `u64`; an audit event
    // stamped past ~year 2554 overflows it. Surface that
    // explicitly rather than silently clamping to `u64::MAX`
    // (which would land every overflowing event in one bogus
    // partition).
    let time_unix_nano = u64::try_from(nanos).map_err(|_| crate::BenchError::Pipeline {
        detail: format!(
            "audit event timestamp {nanos} ns exceeds the u64 time_unix_nano representation",
        ),
    })?;
    let proxy = MinedRecord {
        tenant_id: event.tenant_id.clone(),
        time_unix_nano,
        ..proxy_record()
    };
    PartitionKey::derive(&proxy).map_err(|e| crate::BenchError::Pipeline {
        detail: format!("audit partition derive failed: {e}"),
    })
}

/// A `MinedRecord` with every field at its zero value, used
/// only as a carrier for `(tenant_id, time_unix_nano)` into
/// `PartitionKey::derive`. `MinedRecord` has no `Default`, so
/// the fields are spelled out once here.
fn proxy_record() -> MinedRecord {
    use ourios_core::record::BodyKind;
    MinedRecord {
        tenant_id: TenantId::new(""),
        template_id: 0,
        template_version: 0,
        severity_number: 0,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        time_unix_nano: 0,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new()],
        body: None,
        confidence: 0.0,
        lossy_flag: false,
    }
}

/// Sum `*.parquet` file sizes under `dir` (recursive), skipping
/// pre-rename `*.parquet.tmp` files per RFC 0005 §7. A missing
/// directory yields `Ok(0)` (a gate that produced no output
/// for that subtree — e.g. zero audit events); any *other* I/O
/// error surfaces as [`crate::BenchError::Pipeline`] rather
/// than silently undercounting. Errors here are categorised
/// `Pipeline` (reading writer artifacts is part of the
/// measurement, not JSON/markdown reporting).
fn sum_parquet_bytes(dir: &Path) -> Result<u64, crate::BenchError> {
    // `fs::read_dir` (below) returns `NotFound` for a missing
    // directory; we map only that to `Ok(0)` and propagate
    // every other error (e.g. permission denied), unlike
    // `Path::exists()` which collapses all I/O errors to
    // "false" and would undercount.
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(crate::BenchError::Pipeline {
                    detail: format!("read_dir({}): {e}", d.display()),
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|e| crate::BenchError::Pipeline {
                detail: format!("read_dir entry under {}: {e}", d.display()),
            })?;
            let path = entry.path();
            let meta = std::fs::metadata(&path).map_err(|e| crate::BenchError::Pipeline {
                detail: format!("metadata({}): {e}", path.display()),
            })?;
            if meta.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
            {
                // `.parquet.tmp` has extension `tmp`, not
                // `parquet`, so the suffix check naturally
                // skips in-flight writes.
                total += meta.len();
            }
        }
    }
    Ok(total)
}

/// A sink that discards what it's written and only tallies the
/// byte count — lets the ZSTD encoder stream its output past
/// us without us holding the compressed buffer.
struct CountingSink(u64);

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // `usize → u64` is lossless on every supported target;
        // saturating keeps the counter monotone even on the
        // hypothetical 128-bit one.
        self.0 = self.0.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Compute `bytes(zstd_corpus)` — run ZSTD-19 over each `*.txt`
/// file under `dir` individually and sum the compressed
/// lengths. Per-file (not concatenated) per §3.4.1. The input
/// is streamed from the file and the compressed output is
/// streamed into a [`CountingSink`], so neither the raw file
/// nor its compressed image is held in memory in full —
/// bounded regardless of corpus file size.
#[allow(clippy::cast_possible_truncation)]
fn zstd_level_19_bytes(dir: &Path) -> Result<u64, crate::BenchError> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|e| crate::BenchError::Corpus {
            detail: format!("read_dir({}): {e}", d.display()),
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| crate::BenchError::Corpus {
                detail: format!("read_dir entry under {}: {e}", d.display()),
            })?;
            let path = entry.path();
            let meta = std::fs::metadata(&path).map_err(|e| crate::BenchError::Corpus {
                detail: format!("metadata({}): {e}", path.display()),
            })?;
            if meta.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
            {
                let file = std::fs::File::open(&path).map_err(|e| crate::BenchError::Corpus {
                    detail: format!("open({}): {e}", path.display()),
                })?;
                let mut sink = CountingSink(0);
                zstd::stream::copy_encode(file, &mut sink, ZSTD_LEVEL).map_err(|e| {
                    crate::BenchError::Pipeline {
                        detail: format!("zstd compress({}): {e}", path.display()),
                    }
                })?;
                total += sink.0;
            }
        }
    }
    Ok(total)
}

/// Pure §3.4.1 formula given the measured byte counts. Split
/// out so the ratio math + 3-sigfig rounding is unit-testable
/// without touching the filesystem. The data/audit split
/// doesn't enter the formula (A1 operates on the total) — it's
/// carried separately in [`A1Outcome`] for the §3.6 `ourios`
/// block.
#[allow(clippy::cast_precision_loss)]
fn compute_a1(raw_bytes: u64, total_parquet_bytes: u64, zstd_bytes: u64) -> A1Result {
    let ourios_ratio = ratio(raw_bytes, total_parquet_bytes);
    let zstd_ratio = ratio(raw_bytes, zstd_bytes);
    let delta_raw = if zstd_ratio > 0.0 {
        ourios_ratio / zstd_ratio
    } else {
        0.0
    };
    let delta = round_down_3_sigfigs(delta_raw);
    A1Result {
        ourios_ratio: round_down_3_sigfigs(ourios_ratio),
        zstd_ratio: round_down_3_sigfigs(zstd_ratio),
        delta,
        target_delta: TARGET_DELTA,
        pass: delta >= TARGET_DELTA,
    }
}

/// `numerator / denominator` as `f64`, `0.0` when the
/// denominator is zero (an empty output can't have a ratio).
#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    (numerator as f64) / (denominator as f64)
}

/// Round `value` *down* to three significant figures, so a
/// reported A1 number never overstates compression (§3.4.1).
fn round_down_3_sigfigs(value: f64) -> f64 {
    if value <= 0.0 || !value.is_finite() {
        return 0.0;
    }
    // Number of digits left of the decimal point.
    let magnitude = value.abs().log10().floor();
    // Scale so three significant figures sit left of the
    // decimal, floor, then scale back.
    let scale = 10f64.powf(2.0 - magnitude);
    (value * scale).floor() / scale
}

#[cfg(test)]
mod tests {
    // The formula tests compare against exact `f64` literals
    // (`0.0`, `3.0`, `4.0`, `20.0`) that the arithmetic
    // reproduces bit-exactly — `assert_eq!` is correct here,
    // not a tolerance.
    #![allow(clippy::float_cmp)]
    use super::*;

    /// 3-sigfig floor: `12.49` → `12.4`, `3.456` → `3.45`,
    /// `0.98765` → `0.987`. Always rounds toward zero so a
    /// reported ratio never overstates compression.
    #[test]
    fn round_down_3_sigfigs_truncates_to_three_figures() {
        assert!((round_down_3_sigfigs(12.49) - 12.4).abs() < 1e-9);
        assert!((round_down_3_sigfigs(3.456) - 3.45).abs() < 1e-9);
        assert!((round_down_3_sigfigs(0.987_65) - 0.987).abs() < 1e-9);
        assert!((round_down_3_sigfigs(100.9) - 100.0).abs() < 1e-9);
        assert_eq!(round_down_3_sigfigs(0.0), 0.0);
    }

    /// The A1 formula: ratio of ratios, target 3×.
    #[test]
    fn compute_a1_formula_and_pass_threshold() {
        // raw 1000, ourios total 50 → ratio 20; zstd 250 →
        // ratio 4. delta = 20 / 4 = 5.0 ≥ 3 → pass.
        let a1 = compute_a1(1000, 50, 250);
        assert!((a1.ourios_ratio - 20.0).abs() < 1e-9);
        assert!((a1.zstd_ratio - 4.0).abs() < 1e-9);
        assert!((a1.delta - 5.0).abs() < 1e-9);
        assert_eq!(a1.target_delta, 3.0);
        assert!(a1.pass);
    }

    /// delta just under 3× fails the gate.
    #[test]
    fn compute_a1_fails_below_target() {
        // raw 1000, ourios total 100 → 10; zstd 250 → 4.
        // delta = 10 / 4 = 2.5 < 3 → fail.
        let a1 = compute_a1(1000, 100, 250);
        assert!((a1.delta - 2.5).abs() < 1e-9);
        assert!(!a1.pass);
    }

    /// Zero-output guard: a denominator of zero yields a `0.0`
    /// ratio rather than `inf` / `NaN`, and the gate fails.
    #[test]
    fn compute_a1_handles_zero_output() {
        let a1 = compute_a1(1000, 0, 0);
        assert_eq!(a1.ourios_ratio, 0.0);
        assert_eq!(a1.zstd_ratio, 0.0);
        assert_eq!(a1.delta, 0.0);
        assert!(!a1.pass);
    }
}
