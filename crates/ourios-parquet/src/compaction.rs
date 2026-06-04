//! Sealed-partition compaction (RFC 0009).
//!
//! [`compact_partition`] consolidates a partition's many small
//! `*.parquet` files into one, **preserving every stored row** (it
//! copies rows via [`Reader`]/[`Writer`], never re-mines them), and
//! commits the result by atomically swapping the partition manifest
//! ([`Manifest::write_atomic`]) so a concurrent query never sees a
//! row twice or misses one (RFC0009.2 / RFC0009.3). It operates on a
//! single partition and validates that every row belongs to it
//! (`Reader::open_partition`, RFC0009.5); the *scheduler* that
//! decides which sealed partitions are candidates and the orphan GC
//! cadence are separate concerns (epic #94).

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::manifest::{Manifest, ManifestError};
use crate::partition::{PartitionKey, percent_encode_tenant};
use crate::reader::{Reader, ReaderError};
use crate::writer::{Writer, WriterError};

/// One hour in nanoseconds — the span a `…/hour=HH/` partition covers.
const HOUR_NANOS: u64 = 3_600_000_000_000;

/// What a [`compact_partition`] call did.
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    /// Number of live files before compaction.
    pub files_before: usize,
    /// Rows in the consolidated file (equal to the total input rows).
    /// `0` on a no-op.
    pub rows: u64,
    /// The commit, or `None` when compaction was a no-op (fewer than
    /// two live files — nothing to consolidate).
    pub committed: Option<Committed>,
    /// Superseded input files that could not be removed after the
    /// commit. These are non-live (the committed manifest excludes
    /// them) — harmless orphans a later GC sweep reclaims — so they
    /// are *counted*, not fatal: a post-commit cleanup failure must
    /// not report a successful compaction as failed.
    pub gc_failures: usize,
    /// Total bytes of the live input files read (`0` on a no-op) — the
    /// read volume for `ourios.compaction.io` (RFC 0009 §3.6).
    pub bytes_read: u64,
    /// Size in bytes of the consolidated output file (`0` on a no-op) —
    /// the write volume for `ourios.compaction.io` and the sample for
    /// the `ourios.storage.parquet.file.size` H4 detector (RFC 0009
    /// §3.6). Best-effort: a `stat` failure on a file we just wrote or
    /// read records `0` rather than failing a committed compaction.
    pub bytes_written: u64,
}

/// The committed result of a compaction.
#[derive(Debug, Clone)]
pub struct Committed {
    /// Name of the consolidated file (the sole live file afterwards).
    pub file: String,
    /// Manifest generation the consolidation was committed at.
    pub generation: u64,
    /// Names of the input files merged away (the pre-compaction live
    /// set). Surfaced for the RFC 0009 §3.6 compaction audit event.
    pub input_files: Vec<String>,
}

/// Policy controlling which sealed partitions [`plan_candidates`]
/// selects for compaction (RFC 0009 §3.3). A tunable — the RFC 0004
/// config surface; defaults match the RFC.
#[derive(Debug, Clone, Copy)]
pub struct CompactionPolicy {
    /// A sealed partition is a candidate when it holds more than this
    /// many live files (RFC 0009 §3.3, default 4).
    pub min_files: usize,
    /// …or holds a live file smaller than this many bytes (the H4
    /// small-file threshold, default 128 MiB).
    pub small_file_bytes: u64,
    /// Grace after an hour ends before its partition is considered
    /// sealed, absorbing late-arriving records (RFC 0009 §3.3, default
    /// 15 min), in nanoseconds.
    pub grace_nanos: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            min_files: 4,
            small_file_bytes: 128 * 1024 * 1024,
            grace_nanos: 15 * 60 * 1_000_000_000,
        }
    }
}

/// Failure during [`compact_partition`].
#[derive(Debug)]
#[non_exhaustive]
pub enum CompactionError {
    /// Reading an input file failed (includes RFC 0005 §3.9
    /// row-vs-path validation failures).
    Read(ReaderError),
    /// Writing the consolidated file failed.
    Write(WriterError),
    /// Reading or committing the manifest failed.
    Manifest(ManifestError),
    /// A live data file has a non-UTF-8 name, so it can't be recorded
    /// in the UTF-8 JSON manifest. Reachable only via the glob
    /// fallback (a foreign file dropped into a partition); the writer
    /// only ever emits UUID names.
    NonUtf8FileName(PathBuf),
    /// A filesystem operation (directory scan) failed.
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "compaction read: {e}"),
            Self::Write(e) => write!(f, "compaction write: {e}"),
            Self::Manifest(e) => write!(f, "compaction manifest: {e}"),
            Self::NonUtf8FileName(path) => {
                write!(
                    f,
                    "compaction: non-UTF-8 data file name: {}",
                    path.display()
                )
            }
            Self::Io { op, path, source } => {
                write!(f, "compaction {op} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Write(e) => Some(e),
            Self::Manifest(e) => Some(e),
            Self::NonUtf8FileName(_) => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Compact the partition `partition` under `bucket_root`: read its
/// live files' rows and rewrite them as one file, then atomically
/// commit the manifest to name only that file and remove the
/// superseded inputs.
///
/// A no-op (returns `committed: None`) when the partition has fewer
/// than two live files — there is nothing to consolidate.
///
/// # Errors
///
/// [`CompactionError`] if an input can't be read (including a
/// row-vs-path mismatch), the consolidated file can't be written,
/// the manifest can't be read/committed, or a filesystem operation
/// fails. On any error before the commit, the inputs are untouched
/// and the partition reads exactly as before.
///
/// # Panics
///
/// Panics if the consolidated file's UUID name is not valid UTF-8, or
/// if a single input file's row count exceeds `u64` — neither is
/// reachable (the name is a UUID we just wrote; `usize <= u64`). A
/// *foreign* non-UTF-8 file name surfaces as
/// [`CompactionError::NonUtf8FileName`], not a panic.
pub fn compact_partition(
    bucket_root: &Path,
    partition: &PartitionKey,
) -> Result<CompactionOutcome, CompactionError> {
    let partition_dir = partition.data_path(bucket_root);
    let inputs = live_files(&partition_dir)?;
    if inputs.len() < 2 {
        return Ok(CompactionOutcome {
            files_before: inputs.len(),
            rows: 0,
            committed: None,
            gc_failures: 0,
            bytes_read: 0,
            bytes_written: 0,
        });
    }

    // Make the reader manifest-authoritative *before* the consolidated
    // file appears. With no prior manifest, a concurrent glob reader
    // would otherwise see the inputs *and* the new file in the window
    // before the commit (a double count). Bootstrapping a manifest
    // that names the current inputs is the same set the glob already
    // returns, so it changes nothing visible (RFC0009.3 — no torn
    // read), and from then on the new file stays invisible until the
    // commit names it.
    let mut generation = if let Some(manifest) =
        Manifest::read(&partition_dir).map_err(CompactionError::Manifest)?
    {
        manifest.generation
    } else {
        let bootstrap = Manifest {
            generation: 1,
            files: file_names(&inputs)?,
        };
        bootstrap
            .write_atomic(&partition_dir)
            .map_err(CompactionError::Manifest)?;
        1
    };

    // Stream the inputs into the consolidated file one at a time, so
    // peak memory is bounded by a single input file's rows rather than
    // the whole partition (which can be large). `open_partition`
    // validates each row's tenant + time bucket against this partition
    // (RFC 0005 §3.9 / RFC0009.5), so a mis-partitioned input aborts
    // the compaction instead of being silently merged. Row groups
    // rotate at the RFC 0005 §3.5 threshold within the single output.
    let mut writer =
        Writer::open(bucket_root, partition.clone()).map_err(CompactionError::Write)?;
    let mut row_count: u64 = 0;
    let mut bytes_read: u64 = 0;
    for file in &inputs {
        bytes_read = bytes_read.saturating_add(file_len(file));
        let reader =
            Reader::open_partition(file, partition.clone()).map_err(CompactionError::Read)?;
        let records = reader.read_all().map_err(CompactionError::Read)?;
        row_count += u64::try_from(records.len()).expect("file row count fits in u64");
        writer
            .append_records(&records)
            .map_err(CompactionError::Write)?;
    }
    let written = writer.close().map_err(CompactionError::Write)?;
    let bytes_written = file_len(&written.path);
    let consolidated = written
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .expect("UUID file name is valid UTF-8")
        .to_string();

    // Commit: swap the manifest to name only the consolidated file.
    generation += 1;
    // The input file names (the merged-away set) for the §3.6 audit
    // event — captured before the GC loop removes the files (names are
    // stable regardless). Sorted so the audit event is deterministic
    // regardless of the `live_files` read-dir order (the consolidation
    // itself reads `inputs` in their original order).
    let mut input_files = file_names(&inputs)?;
    input_files.sort();
    Manifest {
        generation,
        files: vec![consolidated.clone()],
    }
    .write_atomic(&partition_dir)
    .map_err(CompactionError::Manifest)?;

    // GC the now-superseded inputs. The commit already succeeded, so a
    // delete failure only leaves a non-live orphan (the manifest
    // excludes it) for a later sweep — it must NOT turn a committed
    // compaction into a reported failure. Count such failures so the
    // caller can surface them, and continue.
    let mut gc_failures = 0;
    for file in &inputs {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => gc_failures += 1,
        }
    }

    Ok(CompactionOutcome {
        files_before: inputs.len(),
        rows: row_count,
        committed: Some(Committed {
            file: consolidated,
            generation,
            input_files,
        }),
        gc_failures,
        bytes_read,
        bytes_written,
    })
}

/// On-disk size of `path` in bytes, best-effort: a `stat` failure
/// yields `0`. Used only for `ourios.compaction.io` /
/// `ourios.storage.parquet.file.size` volume — a metric inaccuracy on
/// a file we just wrote or read must never fail a committed compaction.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Select the `tenant`'s sealed partitions that are worth compacting
/// (RFC 0009 §3.3), as of wall-clock `now_unix_nanos`. The result is
/// the work list a background compactor feeds to [`compact_partition`];
/// this function makes only the *decision* — read-only I/O (directory
/// scans, file metadata, and each candidate partition's
/// `manifest.json`), no mutation — so it is deterministic and
/// testable. The driving loop (timer + bounded concurrency) belongs in
/// the ingester role, which doesn't exist yet.
///
/// A partition is selected when it is **sealed** — its hour ended at
/// least `policy.grace_nanos` ago, so no writer is still appending —
/// and a **candidate**: it has at least two live files (fewer can't be
/// consolidated) and either more than `policy.min_files` of them or one
/// below `policy.small_file_bytes`. The list is ordered chronologically
/// (oldest partition first), deterministic across runs.
///
/// # Errors
///
/// [`CompactionError::Io`] if a directory scan or file `stat` fails,
/// or [`CompactionError::Manifest`] if a partition's manifest can't be
/// read while counting its live files.
pub fn plan_candidates(
    bucket_root: &Path,
    tenant: &str,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> Result<Vec<PartitionKey>, CompactionError> {
    let tenant_dir = bucket_root
        .join("data")
        .join(format!("tenant_id={}", percent_encode_tenant(tenant)));
    let mut selected = Vec::new();
    for (partition, dir) in hour_partitions(&tenant_dir, tenant)? {
        if is_sealed(&partition, now_unix_nanos, policy) && is_candidate(&dir, policy)? {
            selected.push(partition);
        }
    }
    Ok(selected)
}

/// Whether the partition's hour ended at least `grace_nanos` before
/// `now` (the comparison is inclusive: sealed at exactly
/// `hour_end + grace`). A partition whose `(year, month, day, hour)` is not a real
/// UTC instant (a corrupt directory name) is treated as not sealed.
fn is_sealed(partition: &PartitionKey, now_unix_nanos: u64, policy: &CompactionPolicy) -> bool {
    let Some(hour_start) = NaiveDate::from_ymd_opt(partition.year, partition.month, partition.day)
        .and_then(|d| d.and_hms_opt(partition.hour, 0, 0))
        .map(|ndt| ndt.and_utc())
    else {
        return false;
    };
    let Some(start_nanos) = hour_start.timestamp_nanos_opt() else {
        return false;
    };
    let Ok(start) = u64::try_from(start_nanos) else {
        return false; // pre-1970; not a partition Ourios writes
    };
    now_unix_nanos
        >= start
            .saturating_add(HOUR_NANOS)
            .saturating_add(policy.grace_nanos)
}

/// Whether a partition is worth compacting per `policy`: at least two
/// live files (fewer can't be consolidated — [`compact_partition`]
/// no-ops), and either more than `min_files` of them or one smaller
/// than `small_file_bytes`.
fn is_candidate(partition_dir: &Path, policy: &CompactionPolicy) -> Result<bool, CompactionError> {
    let live = live_files(partition_dir)?;
    if live.len() < 2 {
        return Ok(false);
    }
    if live.len() > policy.min_files {
        return Ok(true);
    }
    for file in &live {
        let len = std::fs::metadata(file)
            .map_err(|source| CompactionError::Io {
                op: "stat",
                path: file.clone(),
                source,
            })?
            .len();
        if len < policy.small_file_bytes {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Enumerate `tenant_dir`'s `year=/month=/day=/hour=` leaf partitions
/// as `(PartitionKey, hour_dir)` — the **cartesian product** of the
/// four Hive levels. Each level is one [`expand_level`] step folded
/// over the running frontier, so the descent is a single generic
/// expansion rather than four hand-nested loops. Directory names that
/// aren't the canonical `key=value` segments drop out (not Ourios
/// output); a missing `tenant_dir` yields an empty list.
fn hour_partitions(
    tenant_dir: &Path,
    tenant: &str,
) -> Result<Vec<(PartitionKey, PathBuf)>, CompactionError> {
    // The Hive levels, outermost first, with the zero-pad width
    // `PartitionKey::data_path` writes each segment at.
    const LEVELS: [(&str, usize); 4] = [("year", 4), ("month", 2), ("day", 2), ("hour", 2)];

    // Fold the levels into the product: start at the tenant root with
    // no numbers, expand by one level each step (short-circuiting on
    // I/O error), and end with every hour leaf + its [y, m, d, h].
    let root = vec![(tenant_dir.to_path_buf(), Vec::<u32>::new())];
    let leaves = LEVELS.iter().try_fold(root, |frontier, &(prefix, width)| {
        expand_level(frontier, prefix, width)
    })?;

    Ok(leaves
        .into_iter()
        .filter_map(|(hour_dir, nums)| partition_from(tenant, &nums, hour_dir))
        .collect())
}

/// Expand every `(dir, nums)` in `frontier` by one Hive level: replace
/// it with one entry per `<prefix>=<n>` child directory, appending the
/// parsed `n` to `nums`. Folding this over the levels is the cartesian
/// product `year × month × day × hour`.
fn expand_level(
    frontier: Vec<(PathBuf, Vec<u32>)>,
    prefix: &str,
    width: usize,
) -> Result<Vec<(PathBuf, Vec<u32>)>, CompactionError> {
    frontier
        .into_iter()
        .map(|(dir, nums)| {
            numbered_children(&dir, prefix, width).map(|children| {
                children.into_iter().map(move |(child, value)| {
                    let mut nums = nums.clone();
                    nums.push(value);
                    (child, nums)
                })
            })
        })
        .collect::<Result<Vec<_>, CompactionError>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

/// Build a `(PartitionKey, hour_dir)` from the four parsed
/// `[year, month, day, hour]` numbers, or `None` if there aren't
/// exactly four or `year` doesn't fit `i32` (not a partition Ourios
/// writes).
fn partition_from(
    tenant: &str,
    nums: &[u32],
    hour_dir: PathBuf,
) -> Option<(PartitionKey, PathBuf)> {
    let [year, month, day, hour] = *nums else {
        return None;
    };
    Some((
        PartitionKey {
            tenant_id: tenant.to_owned(),
            year: i32::try_from(year).ok()?,
            month,
            day,
            hour,
        },
        hour_dir,
    ))
}

/// Subdirectories of `dir` named `<prefix>=<n>` in the canonical
/// zero-padded form `PartitionKey::data_path` writes (`width` digits,
/// e.g. `month=04`), returned as `(path, n)`. A non-canonical name
/// (`month=4`, `month=004`) is skipped: it would parse to a value
/// whose `data_path` form (`month=04`) names a *different* directory,
/// so the resulting `PartitionKey` wouldn't round-trip to the scanned
/// dir (RFC 0005 §3.4). Non-matching entries and non-directories are
/// skipped; a missing `dir` yields an empty list.
fn numbered_children(
    dir: &Path,
    prefix: &str,
    width: usize,
) -> Result<Vec<(PathBuf, u32)>, CompactionError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(CompactionError::Io {
                op: "read_dir",
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CompactionError::Io {
            op: "read_dir entry",
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .map_err(|source| CompactionError::Io {
                op: "file_type",
                path: path.clone(),
                source,
            })?
            .is_dir();
        if !is_dir {
            continue;
        }
        if let Some(value) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(prefix)?.strip_prefix('='))
            .and_then(|digits| {
                let value: u32 = digits.parse().ok()?;
                // Accept only the exact zero-padded form `data_path`
                // emits, so the PartitionKey round-trips to this dir.
                (digits == format!("{value:0width$}")).then_some(value)
            })
        {
            out.push((path, value));
        }
    }
    // `read_dir` order is unspecified; sort by value so the descent —
    // and thus `plan_candidates`' work list — is deterministic
    // (ascending = chronological: oldest sealed partitions first).
    out.sort_by_key(|(_, value)| *value);
    Ok(out)
}

/// The partition's live data files: the manifest's named files when a
/// manifest is present (authoritative), else every committed
/// `*.parquet` in the directory (`*.parquet.tmp` and `manifest.json`
/// are excluded by extension). Mirrors the querier's resolution.
fn live_files(partition_dir: &Path) -> Result<Vec<PathBuf>, CompactionError> {
    if let Some(manifest) = Manifest::read(partition_dir).map_err(CompactionError::Manifest)? {
        return Ok(manifest
            .files
            .iter()
            .map(|name| partition_dir.join(name))
            .collect());
    }
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(partition_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(source) => {
            return Err(CompactionError::Io {
                op: "read_dir",
                path: partition_dir.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| CompactionError::Io {
            op: "read_dir entry",
            path: partition_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let is_file = entry
            .file_type()
            .map_err(|source| CompactionError::Io {
                op: "file_type",
                path: path.clone(),
                source,
            })?
            .is_file();
        if is_file && path.extension().is_some_and(|x| x == "parquet") {
            files.push(path);
        }
    }
    Ok(files)
}

/// Bare file names of `paths` (for a manifest's `files` list). A
/// non-UTF-8 name can't be written to the JSON manifest, so it is a
/// [`CompactionError::NonUtf8FileName`] rather than a panic — the
/// glob fallback may pick up a foreign file.
fn file_names(paths: &[PathBuf]) -> Result<Vec<String>, CompactionError> {
    paths
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
                .ok_or_else(|| CompactionError::NonUtf8FileName(p.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;

    use super::*;
    use crate::manifest::MANIFEST_FILENAME;

    /// 2026-04-02T10:58:00 UTC — offsets stay within hour 10, so
    /// every record shares one partition.
    const TS0: u64 = 1_775_127_480_000_000_000;

    fn rec(template_id: u64, ts_ns: u64) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("a"),
            template_id,
            template_version: 1,
            severity_number: 9,
            severity_text: Some("INFO".to_string()),
            scope_name: Some("lib.cart".to_string()),
            scope_version: Some("1.0.0".to_string()),
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0x01,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            }],
            separators: vec![String::new(), " ".to_string()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    fn partition() -> PartitionKey {
        PartitionKey::derive(&rec(1, TS0)).expect("derive partition")
    }

    /// Write `recs` (sharing one partition) as one committed file.
    fn write_file(bucket: &Path, recs: &[MinedRecord]) {
        let mut w = Writer::open(bucket, partition()).expect("open writer");
        w.append_records(recs).expect("append");
        w.close().expect("close");
    }

    #[test]
    fn compacts_two_files_into_one_preserving_rows() {
        // Arrange — two committed files in one partition (5 rows total).
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
        write_file(
            bucket.path(),
            &[
                rec(2, TS0 + 2_000_000),
                rec(2, TS0 + 3_000_000),
                rec(2, TS0 + 4_000_000),
            ],
        );
        let dir = partition().data_path(bucket.path());

        // Act
        let outcome = compact_partition(bucket.path(), &partition()).expect("compact");

        // Assert — consolidated to one file with all 5 rows, manifest
        // names it, inputs GC'd, rows preserved.
        assert_eq!(outcome.files_before, 2);
        assert_eq!(outcome.rows, 5);
        assert_eq!(outcome.gc_failures, 0, "both inputs removed");
        let committed = outcome.committed.expect("committed");
        let live = live_files(&dir).expect("live");
        assert_eq!(live.len(), 1, "one file remains live");
        assert!(live[0].ends_with(&committed.file));
        let rows = Reader::open_partition(&live[0], partition())
            .expect("open")
            .read_all()
            .expect("read");
        assert_eq!(rows.len(), 5, "every row preserved");
    }

    #[test]
    fn reports_byte_volumes_for_io_and_file_size_metrics() {
        // Arrange — two committed files in one partition.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
        write_file(bucket.path(), &[rec(2, TS0 + 2_000_000)]);
        let dir = partition().data_path(bucket.path());

        // Act
        let outcome = compact_partition(bucket.path(), &partition()).expect("compact");

        // Assert — read volume covers both inputs, write volume is the
        // (sole, live) consolidated file's actual on-disk size.
        let committed = outcome.committed.expect("committed");
        let live = live_files(&dir).expect("live");
        assert_eq!(live.len(), 1, "one consolidated file remains live");
        let on_disk = std::fs::metadata(&live[0]).expect("stat").len();
        assert!(outcome.bytes_read > 0, "read volume is recorded");
        assert_eq!(
            outcome.bytes_written, on_disk,
            "write volume is the consolidated file's byte size"
        );
        assert!(live[0].ends_with(&committed.file));
    }

    #[test]
    fn no_op_reports_zero_byte_volumes() {
        // Arrange — one file: a no-op, nothing read or written.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);

        // Act
        let outcome = compact_partition(bucket.path(), &partition()).expect("compact");

        // Assert
        assert!(outcome.committed.is_none());
        assert_eq!(outcome.bytes_read, 0);
        assert_eq!(outcome.bytes_written, 0);
    }

    #[test]
    fn single_file_partition_is_a_no_op() {
        // Arrange — one file, nothing to consolidate.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);
        let dir = partition().data_path(bucket.path());

        // Act
        let outcome = compact_partition(bucket.path(), &partition()).expect("compact");

        // Assert — no-op: no commit, no manifest written.
        assert_eq!(outcome.files_before, 1);
        assert!(outcome.committed.is_none());
        assert!(!dir.join(MANIFEST_FILENAME).exists());
    }

    #[test]
    fn bumps_generation_from_an_existing_manifest() {
        // Arrange — two files plus a manifest already at generation 5.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);
        write_file(bucket.path(), &[rec(2, TS0 + 1_000_000)]);
        let dir = partition().data_path(bucket.path());
        let names = file_names(&live_files(&dir).expect("live")).expect("names");
        Manifest {
            generation: 5,
            files: names,
        }
        .write_atomic(&dir)
        .expect("seed manifest");

        // Act
        let outcome = compact_partition(bucket.path(), &partition()).expect("compact");

        // Assert — committed at generation 6.
        assert_eq!(outcome.committed.expect("committed").generation, 6);
    }

    // --- plan_candidates (RFC 0009 §3.3 sealed + candidate selection) ---

    /// `now` inside the partition's hour → not sealed; well past the
    /// hour-end + grace → sealed.
    const NOW_UNSEALED: u64 = TS0;
    const NOW_SEALED: u64 = TS0 + 2 * HOUR_NANOS;

    #[test]
    fn plan_skips_an_unsealed_partition() {
        // Arrange — two small files, but `now` is still inside the hour.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);
        write_file(bucket.path(), &[rec(2, TS0 + 1_000_000)]);

        // Act
        let selected = plan_candidates(
            bucket.path(),
            "a",
            NOW_UNSEALED,
            &CompactionPolicy::default(),
        )
        .expect("plan");

        // Assert
        assert!(
            selected.is_empty(),
            "an unsealed partition is never selected"
        );
    }

    #[test]
    fn plan_selects_a_sealed_small_file_partition() {
        // Arrange — two committed files (each well under 128 MiB), and
        // `now` past the hour-end + grace.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);
        write_file(bucket.path(), &[rec(2, TS0 + 1_000_000)]);

        // Act
        let selected =
            plan_candidates(bucket.path(), "a", NOW_SEALED, &CompactionPolicy::default())
                .expect("plan");

        // Assert
        assert_eq!(
            selected,
            vec![partition()],
            "the sealed small-file partition is selected"
        );
    }

    #[test]
    fn plan_returns_partitions_in_chronological_order() {
        // Arrange — two sealed small-file partitions, hour 10 and 11.
        let bucket = tempfile::tempdir().expect("temp");
        for ts in [TS0, TS0 + HOUR_NANOS] {
            for template_id in [1_u64, 2] {
                let record = rec(template_id, ts);
                let mut w = Writer::open(bucket.path(), PartitionKey::derive(&record).unwrap())
                    .expect("open");
                w.append_records(&[record]).expect("append");
                w.close().expect("close");
            }
        }
        let now = TS0 + 3 * HOUR_NANOS; // past hour 11's end + grace

        // Act
        let selected =
            plan_candidates(bucket.path(), "a", now, &CompactionPolicy::default()).expect("plan");

        // Assert — both selected, oldest first, regardless of read_dir order.
        let hours: Vec<u32> = selected.iter().map(|p| p.hour).collect();
        assert_eq!(hours, vec![10, 11], "deterministic, chronological");
    }

    #[test]
    fn plan_skips_a_single_file_partition() {
        // Arrange — one file: sealed, but nothing to consolidate.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);

        // Act
        let selected =
            plan_candidates(bucket.path(), "a", NOW_SEALED, &CompactionPolicy::default())
                .expect("plan");

        // Assert
        assert!(
            selected.is_empty(),
            "a one-file partition can't be consolidated"
        );
    }

    #[test]
    fn plan_selects_a_sealed_many_file_partition_via_count() {
        // Arrange — five files (> default min_files of 4), sealed, with
        // the size arm disabled (1-byte threshold) so *only* the count
        // arm can select.
        let bucket = tempfile::tempdir().expect("temp");
        for i in 0..5 {
            write_file(bucket.path(), &[rec(1, TS0 + i * 1_000)]);
        }
        let policy = CompactionPolicy {
            min_files: 4,
            small_file_bytes: 1,
            grace_nanos: CompactionPolicy::default().grace_nanos,
        };

        // Act
        let selected = plan_candidates(bucket.path(), "a", NOW_SEALED, &policy).expect("plan");

        // Assert
        assert_eq!(
            selected,
            vec![partition()],
            "the count arm selects a partition with more than min_files"
        );
    }

    #[test]
    fn plan_skips_when_files_are_large_and_few() {
        // Arrange — two files, sealed, but a policy where neither the
        // count (2 ≤ min_files) nor the size (1-byte threshold) arm
        // fires.
        let bucket = tempfile::tempdir().expect("temp");
        write_file(bucket.path(), &[rec(1, TS0)]);
        write_file(bucket.path(), &[rec(2, TS0 + 1_000_000)]);
        let policy = CompactionPolicy {
            min_files: 4,
            small_file_bytes: 1,
            grace_nanos: CompactionPolicy::default().grace_nanos,
        };

        // Act
        let selected = plan_candidates(bucket.path(), "a", NOW_SEALED, &policy).expect("plan");

        // Assert
        assert!(selected.is_empty(), "few large files are not a candidate");
    }

    #[test]
    fn plan_skips_non_canonical_partition_dir_names() {
        // Arrange — a sealed partition whose `month` segment isn't
        // zero-padded (`month=4`, not `month=04`). A PartitionKey from
        // it would render `month=04` via data_path and miss this dir,
        // so it must not be selected.
        let bucket = tempfile::tempdir().expect("temp");
        let bad = bucket
            .path()
            .join("data/tenant_id=a/year=2026/month=4/day=02/hour=10");
        std::fs::create_dir_all(&bad).expect("mkdir");
        std::fs::write(bad.join("a.parquet"), b"x").expect("a");
        std::fs::write(bad.join("b.parquet"), b"y").expect("b");

        // Act
        let selected =
            plan_candidates(bucket.path(), "a", NOW_SEALED, &CompactionPolicy::default())
                .expect("plan");

        // Assert
        assert!(selected.is_empty(), "non-canonical dir names are skipped");
    }

    #[test]
    fn plan_for_a_tenant_with_no_data_is_empty() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");

        // Act
        let selected = plan_candidates(
            bucket.path(),
            "ghost",
            NOW_SEALED,
            &CompactionPolicy::default(),
        )
        .expect("plan");

        // Assert
        assert!(selected.is_empty());
    }
}
