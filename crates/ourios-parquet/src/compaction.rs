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

use crate::manifest::{Manifest, ManifestError};
use crate::partition::PartitionKey;
use crate::reader::{Reader, ReaderError};
use crate::writer::{Writer, WriterError};

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
}

/// The committed result of a compaction.
#[derive(Debug, Clone)]
pub struct Committed {
    /// Name of the consolidated file (the sole live file afterwards).
    pub file: String,
    /// Manifest generation the consolidation was committed at.
    pub generation: u64,
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
    /// A filesystem operation (directory scan, orphan removal) failed.
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
/// Panics if a partition file name is not valid UTF-8, or if the
/// partition's row count exceeds `u64` — neither is reachable on a
/// supported target (file names are UUIDs; `usize <= u64`).
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
            files: file_names(&inputs),
        };
        bootstrap
            .write_atomic(&partition_dir)
            .map_err(CompactionError::Manifest)?;
        1
    };

    // Read every input row. `open_partition` validates each row's
    // tenant + time bucket against this partition (RFC 0005 §3.9 /
    // RFC0009.5), so a mis-partitioned input aborts the compaction
    // instead of being silently merged.
    let mut rows = Vec::new();
    for file in &inputs {
        let reader =
            Reader::open_partition(file, partition.clone()).map_err(CompactionError::Read)?;
        rows.extend(reader.read_all().map_err(CompactionError::Read)?);
    }
    let row_count = u64::try_from(rows.len()).expect("row count fits in u64");

    // Write the consolidated file (row groups rotate at the RFC 0005
    // §3.5 threshold within this single file).
    let mut writer =
        Writer::open(bucket_root, partition.clone()).map_err(CompactionError::Write)?;
    writer
        .append_records(&rows)
        .map_err(CompactionError::Write)?;
    let written = writer.close().map_err(CompactionError::Write)?;
    let consolidated = written
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .expect("UUID file name is valid UTF-8")
        .to_string();

    // Commit: swap the manifest to name only the consolidated file.
    generation += 1;
    Manifest {
        generation,
        files: vec![consolidated.clone()],
    }
    .write_atomic(&partition_dir)
    .map_err(CompactionError::Manifest)?;

    // GC the now-superseded inputs. Post-commit: a crash here leaves
    // orphaned files the manifest already excludes — harmless, and a
    // later sweep reclaims them.
    for file in &inputs {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CompactionError::Io {
                    op: "remove superseded input",
                    path: file.clone(),
                    source,
                });
            }
        }
    }

    Ok(CompactionOutcome {
        files_before: inputs.len(),
        rows: row_count,
        committed: Some(Committed {
            file: consolidated,
            generation,
        }),
    })
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

/// Bare file names of `paths` (for a manifest's `files` list).
fn file_names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .expect("partition file name is valid UTF-8")
                .to_string()
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
        let names = file_names(&live_files(&dir).expect("live"));
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
}
