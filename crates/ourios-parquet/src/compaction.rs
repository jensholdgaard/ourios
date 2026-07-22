//! Sealed-partition compaction (RFC 0009), through the object-storage
//! [`Store`] seam (RFC 0019), with the RFC 0036 write-side layout:
//! the consolidated file is clustered by the §3.1 key (promoted
//! `service.name`, then `time_unix_nano`) via a bounded-memory
//! external merge sort, rotates row groups at the §3.3 compacted
//! threshold, and declares the §3.4 `sorting_columns`.
//!
//! [`compact_partition`] consolidates a partition's many small
//! `*.parquet` objects into one, **preserving every stored row** (it
//! copies rows via [`Reader`]/[`Writer`], never re-mines them), and
//! commits the result by atomically swapping the partition manifest so a
//! concurrent query never sees a row twice or misses one (RFC0009.2 /
//! RFC0009.3). The swap is backend-appropriate: a conditional PUT
//! ([`Manifest::publish_cas`], RFC0019.4) on an S3 backend, or an atomic
//! overwrite on the local backend (which has no `If-Match` CAS — RFC0019.7
//! keeps the local commit byte-for-byte unchanged). It operates on a single
//! partition and validates that every row belongs to it
//! ([`Reader::open_partition_bytes`], RFC0009.5); the *scheduler* that
//! decides which sealed partitions are candidates and the orphan GC cadence
//! are separate concerns (epic #94).
//!
//! Every filesystem walk is a [`Store`] listing (RFC 0019 §3.3), so the same
//! code path targets `LocalFileSystem` or S3 — there is no local/remote
//! hybrid here (unlike the querier): the compactor's tenant isolation is
//! structural (per-partition prefix) plus the row-vs-path validation, neither
//! of which needs local-path canonicalisation.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use chrono::NaiveDate;
use ourios_core::record::MinedRecord;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties};

use crate::data_schema_with_promoted;
use crate::manifest::{MANIFEST_FILENAME, Manifest, ManifestError, Published};
use crate::partition::{PartitionKey, percent_encode_tenant};
use crate::promoted::{PromotedAttributes, SERVICE_NAME_KEY, project_string_value};
use crate::reader::{Reader, ReaderError};
use crate::record_batch::mined_records_to_batch_with_promoted;
use crate::store::{Store, StoreError};
use crate::writer::{
    COMPACTED_ROW_GROUP_FLUSH_BYTES, ClusterKeys, DEFAULT_ZSTD_LEVEL, SUB_BATCH_ROWS, Writer,
    WriterError,
};

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
    /// two live files — nothing to consolidate — or a lost CAS race that
    /// left the work for a later sweep).
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
    /// §3.6). This is the encoded byte length the [`Writer`] reports
    /// ([`crate::WrittenFile::bytes_written`]), not a `stat` of a path —
    /// a store-backed (S3) output can't be `stat`-ed.
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
    /// A [`Store`] operation (a key listing, object read, or delete) failed.
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

/// Compact the partition `partition` in `store`: read its live files' rows and
/// rewrite them as one file, then atomically commit the manifest to name only
/// that file and remove the superseded inputs.
///
/// A no-op (returns `committed: None`) when the partition has fewer than two
/// live files (nothing to consolidate), or when a compare-and-swap on the
/// manifest is lost to a concurrent sweep (S3 only) — in the latter case the
/// freshly written consolidated file is a non-live orphan a later
/// [`gc_orphans`] reclaims.
///
/// # Errors
///
/// [`CompactionError`] if an input can't be read (including a row-vs-path
/// mismatch), the consolidated file can't be written, the manifest can't be
/// read/committed, or a [`Store`] listing fails. On any error before the
/// commit, the inputs are untouched and the partition reads exactly as before.
pub fn compact_partition(
    store: &Store,
    partition: &PartitionKey,
) -> Result<CompactionOutcome, CompactionError> {
    compact_partition_with_promoted(store, partition, &PromotedAttributes::default())
}

/// Like [`compact_partition`] but re-projecting the rewritten rows under an
/// explicit RFC 0022 promoted attribute set (§3.4: compaction rewrites with
/// the *current* set, so history converges toward pruneability as a side
/// effect). The bare [`compact_partition`] delegates with the default
/// (`service.name`-only) set.
///
/// # Errors
///
/// See [`compact_partition`].
pub fn compact_partition_with_promoted(
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
) -> Result<CompactionOutcome, CompactionError> {
    compact_sorted(
        store,
        partition,
        promoted,
        ClusterKeys::for_promoted(promoted),
        SortTuning::default(),
    )
}

/// Like [`compact_partition`] but rotating compacted row groups at an
/// explicit `flush_bytes` threshold instead of the shipped
/// `COMPACTED_ROW_GROUP_FLUSH_BYTES` (32 MiB) / `OURIOS_COMPACTED_RG_BYTES`
/// default — the deterministic seam for the RFC 0036 §7 threshold sweep
/// (16 / 32 / 64 MiB). This is a **physical-layout knob**, not a schema
/// or content change: the consolidated rows, their §3.1 order, the
/// declared `sorting_columns`, and every column's encoding are identical
/// for any threshold; only the row-group rotation boundaries (and hence
/// group count, per-group statistics tightness, and on-disk size) move.
/// Production compacts via [`compact_partition`] (the env/const default);
/// the sweep passes an explicit value here so it never sets a process env
/// var (unsound under `cargo test`'s in-process parallelism — it races
/// libc `getenv`).
///
/// # Errors
///
/// See [`compact_partition`].
pub fn compact_partition_with_flush_threshold(
    store: &Store,
    partition: &PartitionKey,
    flush_bytes: usize,
) -> Result<CompactionOutcome, CompactionError> {
    let promoted = PromotedAttributes::default();
    compact_sorted(
        store,
        partition,
        &promoted,
        ClusterKeys::for_promoted(&promoted),
        SortTuning {
            compacted_flush_bytes: Some(flush_bytes),
            ..SortTuning::default()
        },
    )
}

/// RFC 0036 §3.2 sort tuning: the in-memory short-circuit bound and
/// the merge fan-in cap F. Internal so unit tests can force the spill
/// and hierarchical-merge paths at unit scale; production always runs
/// the defaults.
#[derive(Debug, Clone, Copy)]
struct SortTuning {
    /// Sort wholly in memory (no spill) while the partition's total
    /// encoded input bytes stay within this bound. 256 MiB is the
    /// ingest seal target (`SINK_TARGET_BYTES`, RFC 0014 §3): a
    /// partition no larger than one worst-case input file costs no
    /// more to hold decoded than phase 1's existing one-input bound.
    in_memory_max_bytes: u64,
    /// Fan-in cap F: more sorted runs than this merge hierarchically,
    /// so phase-2 memory is ≤ F × one decoded batch regardless of
    /// backlog. 64 single-passes every realistic partition (§9.7's
    /// band-scale case held 32 input files) while capping worst-case
    /// residency far below one decoded input file.
    fan_in: usize,
    /// RFC 0036 §3.3 compacted row-group rotation threshold override.
    /// `None` (production) takes the shipped `COMPACTED_ROW_GROUP_FLUSH_BYTES`
    /// / `OURIOS_COMPACTED_RG_BYTES` default; `Some(t)` pins it for the
    /// §7 threshold sweep's deterministic seam
    /// ([`compact_partition_with_flush_threshold`]).
    compacted_flush_bytes: Option<usize>,
}

impl Default for SortTuning {
    fn default() -> Self {
        Self {
            in_memory_max_bytes: 256 * 1024 * 1024,
            fan_in: 64,
            compacted_flush_bytes: None,
        }
    }
}

/// [`compact_partition_with_promoted`] with the RFC 0036 §3.1 key
/// shape and §3.2 tuning explicit — the seam unit tests use to drive
/// the time-only key and the forced-spill / hierarchical-merge paths.
fn compact_sorted(
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
    keys: ClusterKeys,
    tuning: SortTuning,
) -> Result<CompactionOutcome, CompactionError> {
    let key = manifest_key(partition);
    let (existing, etag) =
        match Manifest::read_with_etag(store, &key).map_err(CompactionError::Manifest)? {
            Some((manifest, etag)) => (Some(manifest), etag),
            None => (None, None),
        };
    let mut inputs = live_file_keys(store, partition, existing.as_ref())?;
    if inputs.len() < 2 {
        return Ok(no_op_outcome(inputs.len()));
    }
    // §3.1 tie-break: the input-file ordinal is sorted-basename order.
    // The keys share one partition prefix, so sorting the full keys is
    // basename order — and pins every later step to it, so the output
    // is independent of the store's listing order (RFC0036.4).
    inputs.sort();

    // Make the reader manifest-authoritative *before* the consolidated file
    // appears. With no prior manifest, a concurrent glob reader would otherwise
    // see the inputs *and* the new file in the window before the commit (a
    // double count). Bootstrapping a manifest naming the current inputs is the
    // same set the glob already returns, so it changes nothing visible
    // (RFC0009.3 — no torn read), and from then on the new file stays invisible
    // until the commit names it. The bootstrap is a create-if-absent
    // conditional PUT (supported on both backends); a lost race means another
    // compactor owns this partition, so back off as a no-op rather than fight it.
    let (base_generation, commit_etag) = if let Some(manifest) = &existing {
        (manifest.generation, etag)
    } else {
        let bootstrap = Manifest {
            generation: 1,
            files: basenames(&inputs),
        };
        match bootstrap
            .publish_cas(store, &key, None)
            .map_err(CompactionError::Manifest)?
        {
            Published::Won => {}
            Published::Lost => return Ok(no_op_outcome(inputs.len())),
        }
        // Re-read to learn the bootstrap generation's ETag for the CAS commit
        // (the S3 path; the local overwrite commit ignores it).
        (1, read_manifest_etag(store, &key)?)
    };

    // RFC 0036 §3.2 external merge sort into the consolidated file.
    // Phase 1 decodes the inputs strictly one at a time, so its peak is
    // one fully-decoded input — the same bound the pre-sort streaming
    // loop had. `open_partition_bytes` validates each row's tenant +
    // time bucket against this partition (RFC 0005 §3.9 / RFC0009.5),
    // so a mis-partitioned input aborts the compaction instead of being
    // silently merged. Row groups rotate at the §3.3 compacted
    // threshold and the file declares the §3.4 `sorting_columns`.
    let mut writer = Writer::open_in_compacted(
        store,
        partition.clone(),
        DEFAULT_ZSTD_LEVEL,
        promoted.clone(),
        keys,
        tuning.compacted_flush_bytes,
    )
    .map_err(CompactionError::Write)?;
    let (row_count, bytes_read) = sort_inputs_into(
        &mut writer,
        store,
        partition,
        promoted,
        keys,
        tuning,
        &inputs,
    )?;
    let written = writer.close().map_err(CompactionError::Write)?;
    let bytes_written = written.bytes_written;
    let consolidated = basename(&written.key).to_owned();

    // Commit: swap the manifest to name only the consolidated file. The input
    // names (the merged-away set) for the §3.6 audit event are already sorted
    // (the §3.1 tie-break order), so the event is deterministic regardless of
    // listing order.
    let generation = base_generation.saturating_add(1);
    let input_files = basenames(&inputs);
    let commit = Manifest {
        generation,
        files: vec![consolidated.clone()],
    };
    match commit_manifest(store, &key, &commit, commit_etag.as_deref())? {
        Published::Won => {}
        // Lost the CAS race (S3 only — the local overwrite always wins): the
        // consolidated file is now a non-live orphan a later `gc_orphans`
        // reclaims. Not an error — the work is left for the next sweep.
        Published::Lost => return Ok(no_op_outcome(inputs.len())),
    }

    // GC the now-superseded inputs. The commit already succeeded, so a delete
    // failure only leaves a non-live orphan (the manifest excludes it) for a
    // later sweep — it must NOT turn a committed compaction into a reported
    // failure. Count such failures and continue; a not-found is
    // already-reclaimed (S3 DELETE is idempotent; the local backend reports
    // not-found — the GC treats both alike).
    let mut gc_failures = 0;
    for input in &inputs {
        match store.delete_blocking(input) {
            Ok(()) => {}
            Err(e) if e.is_not_found() => {}
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

/// Test-only decoded-row residency gauge (RFC 0036 §3.2 / RFC0036.3).
/// Counts the `MinedRecord`s the sort holds decoded in RAM on the
/// current thread, exposing the peak so the forced-spill memory test
/// can assert the one-input-plus-`F × batch` bound rather than
/// whole-partition residency (RFC 0036 §6, "an instrumentation counter
/// inside `sort_inputs_into`/`merge_runs`"). Thread-local because a
/// `compact_*` call runs entirely on its caller's thread (blocking I/O
/// throughout), so parallel tests never pollute each other's peak — the
/// property a process-global gauge (or a tracking allocator) cannot
/// offer under `cargo test`'s in-process parallelism.
#[cfg(test)]
mod residency {
    use std::cell::Cell;

    thread_local! {
        static CURRENT: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    /// Zero both the running count and the high-water mark before a
    /// measured compaction.
    pub(super) fn reset() {
        CURRENT.with(|c| c.set(0));
        PEAK.with(|p| p.set(0));
    }

    /// The peak concurrently-live decoded-row count since the last
    /// [`reset`].
    pub(super) fn peak() -> usize {
        PEAK.with(Cell::get)
    }

    /// `n` decoded rows entered residency.
    pub(super) fn add(n: usize) {
        let now = CURRENT.with(|c| {
            let now = c.get() + n;
            c.set(now);
            now
        });
        PEAK.with(|p| {
            if now > p.get() {
                p.set(now);
            }
        });
    }

    /// `n` decoded rows left residency (spilled and dropped, or emitted).
    /// Underflow means the instrumentation's add/sub calls are unbalanced
    /// — a bug in the gauge the RFC0036.3 bound relies on — so panic
    /// rather than saturate and silently under-report the peak.
    pub(super) fn sub(n: usize) {
        CURRENT.with(|c| {
            let now = c
                .get()
                .checked_sub(n)
                .expect("residency gauge underflow: unbalanced add/sub");
            c.set(now);
        });
    }
}

/// Where the RFC 0036 §3.2 sort currently holds the partition's rows:
/// decoded in memory while the running encoded input total fits
/// [`SortTuning::in_memory_max_bytes`], or spilled to local scratch as
/// sorted runs once it doesn't (scratch is cache, not truth —
/// `CLAUDE.md` §3.6; the `TempDir` tears the runs down when the
/// compaction call ends, success or error).
enum SortState {
    /// One decoded-row vec per input, in input-ordinal order.
    Buffering(Vec<Vec<MinedRecord>>),
    /// Sorted runs on scratch, one per input so far, in input-ordinal
    /// order.
    Spilling {
        scratch: tempfile::TempDir,
        runs: Vec<PathBuf>,
    },
}

/// Phases 1–2 of the RFC 0036 §3.2 external merge sort: decode
/// `inputs` (already in sorted-basename order) one at a time, sort by
/// the §3.1 key, and emit every row into `writer` in that key order.
/// Returns `(rows, input bytes read)`.
///
/// Peak decoded-row residency depends on the path (see [`SortTuning`]):
/// - **Spill path** (encoded input total > `in_memory_max_bytes`): one
///   decoded input file during run formation (inputs are decoded
///   strictly one at a time, then spilled), then — via [`reduce_runs`]'s
///   fan-in cap F — F × one decoded batch during the merge. This
///   preserves the pre-sort one-input-file bound.
/// - **In-memory path** (encoded input total ≤ `in_memory_max_bytes`,
///   default 256 MiB = one ingest seal target): all inputs' decoded
///   rows are held at once to sort in place and skip spilling — bounded
///   by one seal-target's worth of input, so no larger than decoding a
///   single worst-case input file (the [`SortTuning`] tradeoff).
fn sort_inputs_into(
    writer: &mut Writer,
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
    keys: ClusterKeys,
    tuning: SortTuning,
    inputs: &[String],
) -> Result<(u64, u64), CompactionError> {
    let mut row_count: u64 = 0;
    let mut bytes_read: u64 = 0;
    let mut state = SortState::Buffering(Vec::new());
    for input in inputs {
        let bytes = store
            .get_blocking(input)
            .map_err(|e| store_io("get", input, e))?;
        bytes_read = bytes_read.saturating_add(bytes.len() as u64);
        let reader = Reader::open_partition_bytes(Bytes::from(bytes), partition.clone(), input)
            .map_err(CompactionError::Read)?;
        let mut records = reader.read_all().map_err(CompactionError::Read)?;
        #[cfg(test)]
        residency::add(records.len());
        // `usize <= u64` on every supported target; saturate rather than panic
        // on a theoretically wider one.
        row_count = row_count.saturating_add(u64::try_from(records.len()).unwrap_or(u64::MAX));
        state = match state {
            SortState::Buffering(mut buffered) if bytes_read <= tuning.in_memory_max_bytes => {
                buffered.push(records);
                SortState::Buffering(buffered)
            }
            // Crossed the in-memory bound: spill mode from here on.
            // Flush the inputs buffered so far as sorted runs first,
            // preserving input-ordinal order (the §3.1 tie-break).
            SortState::Buffering(buffered) => {
                let scratch = tempfile::tempdir().map_err(|source| CompactionError::Io {
                    op: "create scratch",
                    path: PathBuf::from("<scratch>"),
                    source,
                })?;
                let mut runs = Vec::with_capacity(inputs.len());
                for mut prior in buffered {
                    sort_records(keys, &mut prior);
                    runs.push(spill_run(scratch.path(), runs.len(), &prior, promoted)?);
                    #[cfg(test)]
                    residency::sub(prior.len());
                }
                sort_records(keys, &mut records);
                runs.push(spill_run(scratch.path(), runs.len(), &records, promoted)?);
                #[cfg(test)]
                residency::sub(records.len());
                SortState::Spilling { scratch, runs }
            }
            SortState::Spilling { scratch, mut runs } => {
                sort_records(keys, &mut records);
                runs.push(spill_run(scratch.path(), runs.len(), &records, promoted)?);
                #[cfg(test)]
                residency::sub(records.len());
                SortState::Spilling { scratch, runs }
            }
        };
    }
    match state {
        // Whole partition within the one-input-file bound: sort in
        // memory and skip spilling (§3.2 / §7). Concatenation order is
        // (input ordinal, row ordinal), so the stable sort realises the
        // §3.1 tie-break; the single `append_records` call sub-batches
        // exactly as the merge path's chunked emit does (§3.5).
        SortState::Buffering(buffered) => {
            let mut rows: Vec<MinedRecord> = buffered.into_iter().flatten().collect();
            sort_records(keys, &mut rows);
            writer
                .append_records(&rows)
                .map_err(CompactionError::Write)?;
            #[cfg(test)]
            residency::sub(rows.len());
        }
        SortState::Spilling { scratch, runs } => {
            let runs = reduce_runs(scratch.path(), runs, tuning.fan_in, keys, promoted)?;
            merge_runs(&runs, keys, |chunk| {
                writer.append_records(chunk).map_err(CompactionError::Write)
            })?;
            drop(scratch);
        }
    }
    Ok((row_count, bytes_read))
}

/// Stable §3.1 sort of one input's decoded rows: promoted
/// `service.name` value (lexicographic UTF-8 bytes, absent/null first)
/// then `time_unix_nano` — stability preserves pre-sort row ordinals,
/// the second half of the §3.1 tie-break.
fn sort_records(keys: ClusterKeys, records: &mut [MinedRecord]) {
    match keys {
        ClusterKeys::ServiceThenTime => records.sort_by(|a, b| {
            let ka = (
                project_string_value(&a.resource_attributes, SERVICE_NAME_KEY),
                a.time_unix_nano,
            );
            let kb = (
                project_string_value(&b.resource_attributes, SERVICE_NAME_KEY),
                b.time_unix_nano,
            );
            ka.cmp(&kb)
        }),
        ClusterKeys::TimeOnly => records.sort_by_key(|r| r.time_unix_nano),
    }
}

/// A sorted run being written to local scratch (RFC 0036 §3.2): a
/// Parquet file in the data schema with spill-oriented properties —
/// no dictionaries, no statistics, light compression — since a run is
/// write-once read-once cache whose bytes influence the output only
/// through the decoded rows.
struct RunWriter<'a> {
    inner: ArrowWriter<File>,
    promoted: &'a PromotedAttributes,
}

impl<'a> RunWriter<'a> {
    fn create(path: &Path, promoted: &'a PromotedAttributes) -> Result<Self, CompactionError> {
        let file = File::create(path).map_err(|source| CompactionError::Io {
            op: "create run",
            path: path.to_path_buf(),
            source,
        })?;
        let zstd = ZstdLevel::try_new(1).map_err(parquet_write)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(zstd))
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .build();
        let inner = ArrowWriter::try_new(file, data_schema_with_promoted(promoted), Some(props))
            .map_err(parquet_write)?;
        Ok(Self { inner, promoted })
    }

    fn append(&mut self, records: &[MinedRecord]) -> Result<(), CompactionError> {
        for chunk in records.chunks(SUB_BATCH_ROWS) {
            // Cap the buffered row group so writing an intermediate
            // merge run never holds the whole merged output encoded in
            // memory (the §3.2 phase-2 bound).
            if self.inner.in_progress_size() >= COMPACTED_ROW_GROUP_FLUSH_BYTES {
                self.inner.flush().map_err(parquet_write)?;
            }
            let batch = mined_records_to_batch_with_promoted(chunk, self.promoted)
                .map_err(|e| CompactionError::Write(WriterError::Batch(e)))?;
            self.inner.write(&batch).map_err(parquet_write)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<(), CompactionError> {
        self.inner.close().map_err(parquet_write)?;
        Ok(())
    }
}

/// Write one input's sorted rows as run file `index` under `dir`.
fn spill_run(
    dir: &Path,
    index: usize,
    records: &[MinedRecord],
    promoted: &PromotedAttributes,
) -> Result<PathBuf, CompactionError> {
    let path = dir.join(format!("run-{index:06}.parquet"));
    let mut run = RunWriter::create(&path, promoted)?;
    run.append(records)?;
    run.finish()?;
    Ok(path)
}

/// Collapse `runs` hierarchically until at most `fan_in` remain
/// (RFC 0036 §3.2's cap F): each pass merges consecutive groups of
/// `fan_in` runs into one intermediate run, preserving run order so
/// the §3.1 tie-break (input ordinal) survives every level.
fn reduce_runs(
    scratch: &Path,
    mut runs: Vec<PathBuf>,
    fan_in: usize,
    keys: ClusterKeys,
    promoted: &PromotedAttributes,
) -> Result<Vec<PathBuf>, CompactionError> {
    let fan_in = fan_in.max(2);
    let mut next_index = runs.len();
    while runs.len() > fan_in {
        let mut merged = Vec::with_capacity(runs.len().div_ceil(fan_in));
        for group in runs.chunks(fan_in) {
            if let [single] = group {
                merged.push(single.clone());
                continue;
            }
            let path = scratch.join(format!("run-{next_index:06}.parquet"));
            next_index += 1;
            let mut out = RunWriter::create(&path, promoted)?;
            merge_runs(group, keys, |chunk| out.append(chunk))?;
            out.finish()?;
            merged.push(path);
            for consumed in group {
                // Best-effort: the TempDir reclaims scratch either way;
                // early removal just bounds peak scratch-disk use.
                let _ = std::fs::remove_file(consumed);
            }
        }
        runs = merged;
    }
    Ok(runs)
}

/// K-way merge of sorted `runs` in §3.1 key order, emitting
/// [`SUB_BATCH_ROWS`]-sized chunks to `emit` — exactly the
/// sub-batching `Writer::append_records` applies itself, so the spill
/// path and the in-memory path drive the Parquet writer with an
/// identical call sequence (§3.5).
///
/// Peak memory is one decoded batch per run: each [`RunCursor`]
/// streams its file batch-by-batch, and [`reduce_runs`] caps the run
/// count at F, so this holds ≤ F × batch bytes no matter how many
/// inputs the partition accrued — far below phase 1's
/// one-decoded-input bound.
fn merge_runs<F>(runs: &[PathBuf], keys: ClusterKeys, mut emit: F) -> Result<(), CompactionError>
where
    F: FnMut(&[MinedRecord]) -> Result<(), CompactionError>,
{
    let mut cursors = Vec::with_capacity(runs.len());
    for path in runs {
        cursors.push(RunCursor::open(path)?);
    }
    let mut heap = BinaryHeap::with_capacity(cursors.len());
    for (run, cursor) in cursors.iter_mut().enumerate() {
        if let Some(record) = cursor.next_record()? {
            heap.push(Reverse(MergeEntry::new(keys, run, record)));
        }
    }
    let mut out: Vec<MinedRecord> = Vec::with_capacity(SUB_BATCH_ROWS);
    while let Some(Reverse(entry)) = heap.pop() {
        let run = entry.run;
        out.push(entry.record);
        if out.len() == SUB_BATCH_ROWS {
            emit(&out)?;
            out.clear();
        }
        if let Some(record) = cursors[run].next_record()? {
            heap.push(Reverse(MergeEntry::new(keys, run, record)));
        }
    }
    if !out.is_empty() {
        emit(&out)?;
    }
    Ok(())
}

/// One run's head row in the merge heap, ordered by (§3.1 key, run
/// ordinal): equal-key rows pop in run order — which is input-ordinal
/// order — and a run holds one input's equal-key rows in pre-sort row
/// order (the stable phase-1 sort), so the pop sequence realises the
/// full §3.1 tie-break.
struct MergeEntry {
    /// Promoted `service.name` value, precomputed once so heap
    /// comparisons don't rescan `resource_attributes`. `None` under
    /// [`ClusterKeys::TimeOnly`] regardless of the row.
    service: Option<String>,
    time: u64,
    run: usize,
    record: MinedRecord,
}

impl MergeEntry {
    fn new(keys: ClusterKeys, run: usize, record: MinedRecord) -> Self {
        let service = match keys {
            ClusterKeys::ServiceThenTime => {
                project_string_value(&record.resource_attributes, SERVICE_NAME_KEY)
                    .map(str::to_owned)
            }
            ClusterKeys::TimeOnly => None,
        };
        Self {
            service,
            time: record.time_unix_nano,
            run,
            record,
        }
    }

    fn key(&self) -> (Option<&str>, u64, usize) {
        (self.service.as_deref(), self.time, self.run)
    }
}

impl Ord for MergeEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for MergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for MergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for MergeEntry {}

/// A sorted run streamed batch-by-batch off scratch — the phase-2
/// merge holds exactly one decoded batch per open run.
struct RunCursor {
    reader: Reader,
    batch: std::vec::IntoIter<MinedRecord>,
    /// Rows in the batch this cursor currently holds decoded, for the
    /// RFC0036.3 residency gauge: the merge keeps ≤ one batch resident
    /// per open run, so the gauge peaks at `F × batch`, not the whole
    /// partition. The count is charged for the whole batch's lifetime
    /// (a small over-count as its rows drain into the merge heap), and
    /// released when the next batch loads or the run is exhausted.
    #[cfg(test)]
    batch_len: usize,
}

impl RunCursor {
    fn open(path: &Path) -> Result<Self, CompactionError> {
        Ok(Self {
            reader: Reader::open_streaming_file(path).map_err(CompactionError::Read)?,
            batch: Vec::new().into_iter(),
            #[cfg(test)]
            batch_len: 0,
        })
    }

    fn next_record(&mut self) -> Result<Option<MinedRecord>, CompactionError> {
        loop {
            if let Some(record) = self.batch.next() {
                return Ok(Some(record));
            }
            if let Some(batch) = self.reader.next_batch().map_err(CompactionError::Read)? {
                #[cfg(test)]
                {
                    residency::sub(self.batch_len);
                    residency::add(batch.len());
                    self.batch_len = batch.len();
                }
                self.batch = batch.into_iter();
            } else {
                #[cfg(test)]
                {
                    residency::sub(self.batch_len);
                    self.batch_len = 0;
                }
                return Ok(None);
            }
        }
    }
}

/// Map an `ArrowWriter` failure on a run file onto the same
/// [`CompactionError::Write`] channel the consolidated writer uses.
fn parquet_write(e: parquet::errors::ParquetError) -> CompactionError {
    CompactionError::Write(WriterError::Parquet(e))
}

/// Commit `manifest` to `key` in `store` with the backend-appropriate atomic
/// swap (CLAUDE.md §3.5 / RFC0009.3 — no torn read either way):
///
/// - **CAS-capable backend (S3, RFC0019.4):** an `If-Match` conditional PUT
///   ([`Manifest::publish_cas`]) against `expected`; a lost race returns
///   [`Published::Lost`].
/// - **Local backend (RFC0019.7 — byte-for-byte unchanged):** `LocalFileSystem`
///   rejects `PutMode::Update`, so commit with an atomic overwrite (it stages to
///   a temp object and renames it into place — the same swap
///   [`Manifest::write_atomic`] performed pre-RFC-0019). Last-writer-wins; there
///   is no S3-style CAS race to lose on a single local host, so it always wins.
fn commit_manifest(
    store: &Store,
    key: &str,
    manifest: &Manifest,
    expected: Option<&str>,
) -> Result<Published, CompactionError> {
    if store.supports_conditional_update() {
        return manifest
            .publish_cas(store, key, expected)
            .map_err(CompactionError::Manifest);
    }
    manifest.validate().map_err(CompactionError::Manifest)?;
    let bytes = manifest
        .to_json()
        .map_err(|e| CompactionError::Manifest(ManifestError::Parse(e)))?;
    store
        .put_blocking(key, bytes)
        .map_err(|e| CompactionError::Manifest(ManifestError::Io(std::io::Error::other(e))))?;
    Ok(Published::Won)
}

/// Outcome of a [`gc_orphans`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OrphanGc {
    /// Orphan files unlinked this pass.
    pub reclaimed: u64,
    /// Orphans whose unlink failed (left for a later pass, not an error).
    pub failures: u64,
}

/// Reclaim a partition's **orphan** files — those a compaction left when it
/// crashed before its in-process GC finished (RFC0009.4). The commit point is
/// the atomic manifest swap (§3.4), so a crash always freezes a partition at a
/// clean generation; what it can leave behind is dead files the manifest does
/// not name. When a `manifest.json` is present it is authoritative (RFC0009.3):
/// every `*.parquet` object **not** named by it is provably dead — a pre-commit
/// consolidated file, or a superseded input the post-commit GC never reached —
/// and any `*.parquet.tmp` is an interrupted publish (absent on S3). Both are
/// safe to unlink. With **no** manifest the glob is the live set, so no
/// `*.parquet` is an orphan and only stray `*.parquet.tmp` are reclaimed.
///
/// Idempotent, never touches a live file, and safe to run on any sealed
/// partition at any time — so orphans left by a crash are *reclaimable*
/// (RFC0009.4) on the next sweep.
///
/// # Errors
///
/// [`CompactionError::Manifest`] if the partition's `manifest.json` can't be
/// read, or [`CompactionError::Io`] if the [`Store`] listing fails. A failed
/// unlink of an individual orphan is counted in [`OrphanGc::failures`], not
/// surfaced — an orphan that outlives one pass is reclaimed by the next.
pub fn gc_orphans(store: &Store, partition: &PartitionKey) -> Result<OrphanGc, CompactionError> {
    let prefix = partition_data_prefix(partition);
    let live: Option<HashSet<String>> =
        read_manifest(store, partition)?.map(|m| m.files.into_iter().collect());
    let keys = store
        .list_blocking(Some(&prefix))
        .map_err(|e| store_io("list", &prefix, e))?;
    let mut gc = OrphanGc::default();
    for object in keys {
        let name = basename(&object);
        // `.parquet.tmp` is always a dead interrupted publish. A `.parquet` is
        // an orphan only when a manifest names a set that excludes it (no
        // manifest ⇒ glob ⇒ every `.parquet` is live). Anything else
        // (`manifest.json`, a future sidecar) is not ours.
        let orphan = if name.ends_with(".parquet.tmp") {
            true
        } else if name.ends_with(".parquet") {
            live.as_ref().is_some_and(|l| !l.contains(name))
        } else {
            false
        };
        if orphan {
            match store.delete_blocking(&object) {
                Ok(()) => gc.reclaimed += 1,
                Err(e) if e.is_not_found() => {}
                Err(_) => gc.failures += 1,
            }
        }
    }
    Ok(gc)
}

/// Select the `tenant`'s sealed partitions that are worth compacting
/// (RFC 0009 §3.3), as of wall-clock `now_unix_nanos`. The result is the work
/// list a background compactor feeds to [`compact_partition`]; this function
/// makes only the *decision* — read-only [`Store`] listings and each candidate
/// partition's `manifest.json`, no mutation — so it is deterministic and
/// testable. The driving loop (timer + bounded concurrency) belongs in the
/// ingester role.
///
/// A partition is selected when it is **sealed** — its hour ended at least
/// `policy.grace_nanos` ago, so no writer is still appending — and a
/// **candidate**: it has at least two live files (fewer can't be consolidated)
/// and either more than `policy.min_files` of them or one below
/// `policy.small_file_bytes`. The list is ordered chronologically (oldest
/// partition first), deterministic across runs.
///
/// # Errors
///
/// [`CompactionError::Io`] if a [`Store`] listing fails, or
/// [`CompactionError::Manifest`] if a partition's manifest can't be read while
/// counting its live files.
pub fn plan_candidates(
    store: &Store,
    tenant: &str,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> Result<Vec<PartitionKey>, CompactionError> {
    let mut selected = Vec::new();
    for partition in hour_partitions(store, tenant)? {
        if is_sealed(&partition, now_unix_nanos, policy) && is_candidate(store, &partition, policy)?
        {
            selected.push(partition);
        }
    }
    Ok(selected)
}

/// Whether the partition's hour ended at least `grace_nanos` before `now` (the
/// comparison is inclusive: sealed at exactly `hour_end + grace`). A partition
/// whose `(year, month, day, hour)` is not a real UTC instant (a corrupt key)
/// is treated as not sealed.
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

/// Whether a partition is worth compacting per `policy`: at least two live
/// files, and either more than `min_files` of them or one smaller than
/// `small_file_bytes`. Resolves the live set + sizes from one
/// [`Store::list_with_sizes_blocking`] — when a manifest is present it restricts
/// the live set to the named files; otherwise every committed `*.parquet` under
/// the prefix is live (the glob fallback).
fn is_candidate(
    store: &Store,
    partition: &PartitionKey,
    policy: &CompactionPolicy,
) -> Result<bool, CompactionError> {
    let prefix = partition_data_prefix(partition);
    let manifest = read_manifest(store, partition)?;
    let live_names: Option<HashSet<&str>> = manifest
        .as_ref()
        .map(|m| m.files.iter().map(String::as_str).collect());
    let entries = store
        .list_with_sizes_blocking(Some(&prefix))
        .map_err(|e| store_io("list", &prefix, e))?;
    let sizes: Vec<u64> = entries
        .iter()
        .filter(|(key, _)| is_committed_parquet(key) && is_immediate_child(key, &prefix))
        .filter(|(key, _)| {
            live_names
                .as_ref()
                .is_none_or(|n| n.contains(basename(key)))
        })
        .map(|(_, size)| *size)
        .collect();
    if sizes.len() < 2 {
        return Ok(false);
    }
    if sizes.len() > policy.min_files {
        return Ok(true);
    }
    Ok(sizes.iter().any(|&len| len < policy.small_file_bytes))
}

/// Enumerate the tenant's `year=/month=/day=/hour=` leaf partitions that hold
/// objects, walking the Hive levels with a **delimiter rollup** at each step:
/// from `data/tenant_id=<enc>` roll up the `year=` child prefixes, then for each
/// the `month=`, then `day=`, then `hour=` children — every listing returns only
/// the immediate common-prefixes (cheap), never the full object set. This is the
/// object-store equivalent of the pre-RFC-0019 level-by-level `read_dir` walk,
/// not a recursive `O(N_objects)` scan. Each level's segment is parsed in the
/// canonical zero-padded form ([`parse_partition_segment`]); a non-canonical
/// child prefix is dropped exactly as the old walk dropped non-canonical dirs.
/// Returned sorted chronologically (oldest first) and deduplicated.
fn hour_partitions(store: &Store, tenant: &str) -> Result<Vec<PartitionKey>, CompactionError> {
    let root = format!("data/tenant_id={}", percent_encode_tenant(tenant));
    let mut partitions = Vec::new();
    for (year_prefix, year) in numbered_child_prefixes(store, &root, "year", 4)? {
        // `year` is a calendar year; skip the (unreachable for Ourios output)
        // value that wouldn't fit the `PartitionKey` `i32`.
        let Ok(year) = i32::try_from(year) else {
            continue;
        };
        for (month_prefix, month) in numbered_child_prefixes(store, &year_prefix, "month", 2)? {
            for (day_prefix, day) in numbered_child_prefixes(store, &month_prefix, "day", 2)? {
                for (_hour_prefix, hour) in numbered_child_prefixes(store, &day_prefix, "hour", 2)?
                {
                    partitions.push(PartitionKey {
                        tenant_id: tenant.to_owned(),
                        year,
                        month,
                        day,
                        hour,
                    });
                }
            }
        }
    }
    // Ascending tuple order is chronological (oldest sealed partition first);
    // dedup after the sort is a belt-and-braces guard (the rollup yields each
    // partition once).
    partitions.sort_by_key(|p| (p.year, p.month, p.day, p.hour));
    partitions.dedup();
    Ok(partitions)
}

/// Roll up the immediate child common-prefixes of `parent` (one delimiter level,
/// via [`Store::list_common_prefixes_blocking`]) and parse each one's trailing
/// `<name>=NN` segment in the canonical zero-padded form, returning
/// `(child_prefix, value)` for the matches. A non-canonical child (`month=4`,
/// `month=004`) parses to `None` and is dropped — the same way the pre-RFC-0019
/// `read_dir` walk skipped non-canonical directory names (RFC 0005 §3.4).
fn numbered_child_prefixes(
    store: &Store,
    parent: &str,
    name: &str,
    width: usize,
) -> Result<Vec<(String, u32)>, CompactionError> {
    let children = store
        .list_common_prefixes_blocking(Some(parent))
        .map_err(|e| store_io("list", parent, e))?;
    Ok(children
        .into_iter()
        .filter_map(|child| {
            let value = parse_partition_segment(basename(&child), name, width)?;
            Some((child, value))
        })
        .collect())
}

/// Parse one canonical Hive segment `<prefix>=<zero-padded number>` to its
/// value. Accepts only the exact zero-padded width [`PartitionKey::data_path`]
/// writes (`month=04`, not `month=4` or `month=004`), so the parsed key
/// round-trips to the scanned object (RFC 0005 §3.4); any other form is `None`.
fn parse_partition_segment(segment: &str, prefix: &str, width: usize) -> Option<u32> {
    let digits = segment.strip_prefix(prefix)?.strip_prefix('=')?;
    let value: u32 = digits.parse().ok()?;
    (digits == format!("{value:0width$}")).then_some(value)
}

/// The partition's live data-file *keys*: the manifest's named files joined to
/// the partition prefix when a manifest is present (authoritative), else every
/// committed `*.parquet` object under the prefix (`*.parquet.tmp` and
/// `manifest.json` are excluded by suffix). Mirrors the querier's resolution.
fn live_file_keys(
    store: &Store,
    partition: &PartitionKey,
    manifest: Option<&Manifest>,
) -> Result<Vec<String>, CompactionError> {
    let prefix = partition_data_prefix(partition);
    if let Some(manifest) = manifest {
        return Ok(manifest
            .files
            .iter()
            .map(|name| format!("{prefix}/{name}"))
            .collect());
    }
    let keys = store
        .list_blocking(Some(&prefix))
        .map_err(|e| store_io("list", &prefix, e))?;
    Ok(keys
        .into_iter()
        .filter(|k| is_committed_parquet(k) && is_immediate_child(k, &prefix))
        .collect())
}

/// Read the partition's `manifest.json` through the [`Store`], discarding the
/// `ETag`. `Ok(None)` when absent (the pre-compaction / glob-fallback case).
fn read_manifest(
    store: &Store,
    partition: &PartitionKey,
) -> Result<Option<Manifest>, CompactionError> {
    Ok(Manifest::read_with_etag(store, &manifest_key(partition))
        .map_err(CompactionError::Manifest)?
        .map(|(manifest, _etag)| manifest))
}

/// Re-read `key`'s manifest `ETag` after a successful bootstrap publish — the
/// compare-and-swap token the S3 commit needs. `None` if the backend exposes no
/// `ETag` or the manifest vanished under us (the latter only under concurrency;
/// the commit then falls back to a create-if-absent that a winning peer loses).
fn read_manifest_etag(store: &Store, key: &str) -> Result<Option<String>, CompactionError> {
    Ok(Manifest::read_with_etag(store, key)
        .map_err(CompactionError::Manifest)?
        .and_then(|(_manifest, etag)| etag))
}

/// An outcome that **committed nothing** (`committed: None`, zero rows/bytes).
/// Used both for a sub-two-file partition (the listing + manifest read still
/// happened, but no consolidation is performed) and for a lost manifest CAS: in
/// the lost-race case inputs were read and a consolidated object was written,
/// but it lost the swap and is left as an orphan (a later `gc_orphans` reclaims
/// it), so from the sweep's accounting nothing was committed. The read/written
/// bytes of a lost race are not attributed here (the work is discarded).
fn no_op_outcome(files_before: usize) -> CompactionOutcome {
    CompactionOutcome {
        files_before,
        rows: 0,
        committed: None,
        gc_failures: 0,
        bytes_read: 0,
        bytes_written: 0,
    }
}

/// A committed data object: a `*.parquet` key (so `*.parquet.tmp` and
/// `manifest.json` are excluded). The writer only ever emits `<uuid>.parquet`.
fn is_committed_parquet(key: &str) -> bool {
    key.ends_with(".parquet")
}

/// True when `key` is an **immediate** child object of `prefix`
/// (`<prefix>/<name>` with no further `/`). `Store::list*` enumerates the whole
/// subtree, but a partition's files live directly under its prefix, so — like
/// the pre-RFC-0019 `read_dir` immediate-children scan — a nested object under
/// the prefix (a foreign file or a future sidecar layout) is **not** a
/// partition input and must not be folded in.
fn is_immediate_child(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

/// The trailing path segment of a `/`-delimited object key.
fn basename(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// Bare file names of `keys` (for a manifest's `files` list). Keys are valid
/// UTF-8 strings, so this is infallible.
fn basenames(keys: &[String]) -> Vec<String> {
    keys.iter().map(|k| basename(k).to_owned()).collect()
}

/// The partition's data path relative to the store root, as a `/`-delimited
/// object-key prefix (`data/tenant_id=…/year=…/month=…/day=…/hour=…`) — the same
/// address [`Writer::open_in`] publishes files under.
fn partition_data_prefix(partition: &PartitionKey) -> String {
    partition
        .data_path(Path::new(""))
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// The partition's `manifest.json` object key.
fn manifest_key(partition: &PartitionKey) -> String {
    format!("{}/{MANIFEST_FILENAME}", partition_data_prefix(partition))
}

/// Map a [`StoreError`] from a listing / object operation onto
/// [`CompactionError::Io`], keeping the backend cause in the error chain.
fn store_io(op: &'static str, key: &str, source: StoreError) -> CompactionError {
    CompactionError::Io {
        op,
        path: PathBuf::from(key),
        source: std::io::Error::other(source),
    }
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;

    use super::*;

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
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
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

    /// A local [`Store`] rooted at `bucket` — the test seam every migrated
    /// compaction call goes through (RFC 0019 §3.3).
    fn store_at(bucket: &Path) -> Store {
        Store::local(bucket).expect("local store")
    }

    /// Write `recs` (sharing one partition) as one committed file through the
    /// store-backed [`Writer`].
    fn write_file(store: &Store, recs: &[MinedRecord]) {
        let mut w = Writer::open_in(store, partition()).expect("open writer");
        w.append_records(recs).expect("append");
        w.close().expect("close");
    }

    /// RFC 0022 §3.4 — compaction re-projects the rows it rewrites under the
    /// promoted set it is *given*: inputs written under the default
    /// (`service.name`-only) set consolidate into a file that carries the
    /// configured key's column (history converges toward pruneability as a
    /// side effect of ordinary compaction). The bare [`compact_partition`]
    /// stays on the default set.
    #[test]
    fn compaction_reprojects_under_the_given_promoted_set() {
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = store_at(bucket.path());
        let with_ns = |template_id: u64, ts_ns: u64| {
            let kv = |key: &str, value: &str| ourios_core::otlp::KeyValue {
                key: key.to_string(),
                value: Some(ourios_core::otlp::AnyValue {
                    value: Some(ourios_core::otlp::any_value::Value::StringValue(
                        value.to_string(),
                    )),
                }),
                ..Default::default()
            };
            MinedRecord {
                resource_attributes: vec![
                    kv("service.name", "api"),
                    kv("k8s.namespace.name", "prod"),
                ],
                ..rec(template_id, ts_ns)
            }
        };
        write_file(&store, &[with_ns(1, TS0)]);
        write_file(&store, &[with_ns(2, TS0 + 1_000)]);

        let promoted = PromotedAttributes::new(["k8s.namespace.name".to_string()], []);
        let outcome =
            compact_partition_with_promoted(&store, &partition(), &promoted).expect("compact");
        let committed = outcome.committed.expect("committed");

        let key = format!("{}/{}", partition_data_prefix(&partition()), committed.file);
        let bytes = store.get_blocking(&key).expect("get consolidated file");
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            Bytes::from(bytes),
        )
        .expect("open consolidated file");
        let names: Vec<&str> = reader
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        assert!(
            names.contains(&"resource.k8s.namespace.name"),
            "the consolidated file re-projects the configured key: {names:?}"
        );
        assert!(
            names.contains(&"resource.service.name"),
            "the implicit promotion rides along: {names:?}"
        );
    }

    /// Seed a manifest at the partition's manifest key (the test equivalent of
    /// the pre-RFC-0019 `write_atomic`, but through the store seam).
    fn seed_manifest(store: &Store, part: &PartitionKey, manifest: &Manifest) {
        store
            .put_blocking(&manifest_key(part), manifest.to_json().expect("json"))
            .expect("seed manifest");
    }

    /// Resolve [`partition`]'s live file keys the way a reader does
    /// (manifest-authoritative, glob fallback).
    fn live_keys(store: &Store, part: &PartitionKey) -> Vec<String> {
        let manifest = read_manifest(store, part).expect("manifest");
        live_file_keys(store, part, manifest.as_ref()).expect("live")
    }

    /// Count committed `*.parquet` objects physically present under the
    /// partition prefix (what the H4 small-file detector counts).
    fn on_disk_parquet_count(store: &Store, part: &PartitionKey) -> usize {
        store
            .list_blocking(Some(&partition_data_prefix(part)))
            .expect("list")
            .into_iter()
            .filter(|k| is_committed_parquet(k))
            .count()
    }

    /// Read every row in one live file key, through the store seam.
    fn read_key(store: &Store, part: &PartitionKey, key: &str) -> Vec<MinedRecord> {
        let bytes = store.get_blocking(key).expect("get");
        Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), key)
            .expect("open")
            .read_all()
            .expect("read")
    }

    /// Hour-10 start (2026-04-02T10:00:00Z): a record at `+off` for any
    /// `off` in `[0, HOUR_NANOS)` lands in the same partition as
    /// [`partition`].
    const HOUR10_START: u64 = 1_775_124_000_000_000_000;

    /// A record varying only the fields the row-conservation property
    /// exercises (template, in-hour timestamp, severity, one param's
    /// value); everything else is held to the clean-round-trip shape so
    /// equality reflects compaction, not codec edge cases.
    fn prop_rec(
        template_id: u64,
        ts_ns: u64,
        severity_number: u8,
        param_value: &str,
    ) -> MinedRecord {
        MinedRecord {
            template_id,
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            severity_number,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: param_value.to_string(),
            }],
            ..rec(template_id, ts_ns)
        }
    }

    /// Total order over the fields `prop_rec` varies — borrows the param
    /// value (a free fn so the borrow's lifetime ties to the record, which
    /// a closure can't express here).
    fn row_key(r: &MinedRecord) -> (u64, u64, u8, &str) {
        (
            r.template_id,
            r.time_unix_nano,
            r.severity_number,
            r.params[0].value.as_str(),
        )
    }

    /// Resolve [`partition`]'s live files under `store` and read every row, the
    /// way a reader does (manifest-authoritative, glob fallback). Both the file
    /// set and the row-vs-path validation derive from the same [`partition`], so
    /// they can't disagree.
    fn read_partition_rows(store: &Store) -> Vec<MinedRecord> {
        let part = partition();
        let mut rows = Vec::new();
        for key in live_keys(store, &part) {
            rows.extend(read_key(store, &part, &key));
        }
        rows.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
        rows
    }

    proptest::proptest! {
        // Each case builds + compacts + re-reads a multi-file store, so
        // cap the case count to keep the suite fast while still covering
        // a broad spread of splits/contents.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// RFC0009.2 — compaction preserves **every stored row**. For any
        /// split of records across ≥2 files in one partition, the
        /// consolidated file holds exactly the same multiset of rows
        /// (count + content), reordering aside.
        #[test]
        fn compaction_conserves_every_row(
            files in proptest::collection::vec(
                proptest::collection::vec(
                    (
                        proptest::prelude::any::<u64>(),
                        0u64..HOUR_NANOS,
                        proptest::prelude::any::<u8>(),
                        // Numeric, to match the `ParamType::Num` tag
                        // `prop_rec` sets (a clean-round-trip fixture).
                        "[0-9]{1,12}",
                    ),
                    // 1..=15 records, 2..=5 files — 5 files also exceeds
                    // the default `min_files` (4), exercising the count arm.
                    1..=15usize,
                ),
                2..=5usize,
            )
        ) {
            let bucket = tempfile::tempdir().expect("temp");
            let store = store_at(bucket.path());
            let part = partition();
            let mut expected: Vec<MinedRecord> = Vec::new();
            for file in &files {
                let recs: Vec<MinedRecord> = file
                    .iter()
                    .map(|(tid, off, sev, val)| prop_rec(*tid, HOUR10_START + off, *sev, val))
                    .collect();
                expected.extend(recs.iter().cloned());
                let mut w = Writer::open_in(&store, part.clone()).expect("open writer");
                w.append_records(&recs).expect("append");
                w.close().expect("close");
            }

            let outcome = compact_partition(&store, &part).expect("compact");
            proptest::prop_assert!(outcome.committed.is_some(), "≥2 files ⇒ a commit");
            proptest::prop_assert_eq!(outcome.rows, expected.len() as u64, "row count conserved");

            let live = live_keys(&store, &part);
            proptest::prop_assert_eq!(live.len(), 1, "one consolidated file");
            let mut got = read_key(&store, &part, &live[0]);

            // Multiset equality: only `(template, ts, severity, param)`
            // vary, so that tuple is a total key over distinguishable
            // rows; sorting both by it lets the element-wise `==` (full
            // record) confirm content is preserved, not just the count.
            got.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
            expected.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
            proptest::prop_assert_eq!(got, expected, "every row preserved (value-equal)");
        }
    }

    /// RFC0009.3 — atomic publish / no torn read. A compaction first
    /// bootstraps a manifest naming the *inputs*, then writes the
    /// consolidated file, then atomically swaps the manifest to name only
    /// that file. This models the two states a crash could freeze and
    /// asserts a reader is never torn: pre-commit it sees exactly the
    /// inputs (the stored consolidated file is invisible — no double
    /// count), post-commit exactly the consolidated rows (no loss).
    #[test]
    fn atomic_publish_is_never_torn_across_the_swap() {
        // Arrange — two committed input files (3 rows) in one partition.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
        write_file(&store, &[rec(2, TS0 + 2_000_000)]);
        let inputs = live_keys(&store, &part);
        let input_names = basenames(&inputs);
        let originals = read_partition_rows(&store);
        assert_eq!(originals.len(), 3, "three input rows");

        // Mid-compaction, in compact_partition's order: bootstrap the
        // manifest naming the inputs *first* (so the reader is
        // manifest-authoritative before any new file appears)...
        seed_manifest(
            &store,
            &part,
            &Manifest {
                generation: 1,
                files: input_names,
            },
        );
        // ...then write the consolidated file. It now exists in the store but
        // the manifest still names only the inputs.
        let mut w = Writer::open_in(&store, part.clone()).expect("writer");
        w.append_records(&originals).expect("append");
        let consolidated = w.close().expect("close");
        let consolidated_name = basename(&consolidated.key).to_owned();

        // All three files are physically present...
        assert_eq!(
            on_disk_parquet_count(&store, &part),
            3,
            "inputs + consolidated all present pre-commit"
        );
        // ...but the manifest hides the consolidated file: a reader sees
        // exactly the 3 input rows, never 6 (no torn read / double count).
        let pre = read_partition_rows(&store);
        assert_eq!(pre, originals, "pre-commit reader sees only the inputs");

        // Commit: atomic swap to name only the consolidated file.
        seed_manifest(
            &store,
            &part,
            &Manifest {
                generation: 2,
                files: vec![consolidated_name],
            },
        );

        // Post-commit: exactly the consolidated rows — no loss, no dup.
        let post = read_partition_rows(&store);
        assert_eq!(
            post, originals,
            "post-commit reader sees the consolidated rows"
        );
    }

    /// RFC0009.4 — crash safety (shared note). The only commit point is
    /// the atomic manifest swap, so a crash always freezes the partition
    /// at a clean generation (the no-torn-read half is `atomic_publish_…`
    /// above). These three tests assert the other half: the dead files a
    /// crash leaves are *reclaimable* by `gc_orphans`, which never removes
    /// a live file. Each builds the exact on-disk state a `SIGKILL` at
    /// that point would leave — faithful because the commit is a single
    /// atomic swap.
    ///
    /// Crash AFTER the commit swap, before input GC: the manifest names
    /// the consolidated file; the superseded inputs are still present (the
    /// post-commit generation with orphans).
    #[test]
    fn rfc0009_4_post_commit_orphan_inputs_are_reclaimable() {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
        write_file(&store, &[rec(2, TS0 + 2_000_000)]);
        let originals = read_partition_rows(&store);
        let mut w = Writer::open_in(&store, part.clone()).expect("writer");
        w.append_records(&originals).expect("append");
        let consolidated = w.close().expect("close");
        let consolidated_name = basename(&consolidated.key).to_owned();
        seed_manifest(
            &store,
            &part,
            &Manifest {
                generation: 2,
                files: vec![consolidated_name],
            },
        );
        // Reader is already at the clean post generation despite orphans.
        assert_eq!(
            read_partition_rows(&store),
            originals,
            "post-commit reader sees the consolidated rows, ignoring orphans",
        );
        let gc = gc_orphans(&store, &part).expect("gc");
        assert_eq!(
            gc,
            OrphanGc {
                reclaimed: 2,
                failures: 0
            },
            "two orphan inputs reclaimed"
        );
        assert_eq!(live_keys(&store, &part).len(), 1, "consolidated stays live");
        assert_eq!(
            read_partition_rows(&store),
            originals,
            "GC left the live data exactly intact",
        );
        assert_eq!(
            gc_orphans(&store, &part).expect("gc again"),
            OrphanGc::default(),
            "idempotent"
        );
    }

    /// RFC0009.4 — crash BEFORE the commit swap: the manifest still names
    /// the inputs; the freshly written consolidated file is a dead orphan
    /// (the pre-commit generation). See the post-commit test for the
    /// shared crash-safety note.
    #[test]
    fn rfc0009_4_pre_commit_orphan_consolidated_is_reclaimable() {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(7, TS0), rec(7, TS0 + 1_000_000)]);
        write_file(&store, &[rec(8, TS0 + 2_000_000)]);
        let inputs = live_keys(&store, &part);
        let originals = read_partition_rows(&store);
        seed_manifest(
            &store,
            &part,
            &Manifest {
                generation: 1,
                files: basenames(&inputs),
            },
        );
        let mut w = Writer::open_in(&store, part.clone()).expect("writer");
        w.append_records(&originals).expect("append");
        w.close().expect("close"); // consolidated present, NOT in manifest
        assert_eq!(
            read_partition_rows(&store),
            originals,
            "pre-commit reader sees only the inputs (consolidated invisible)",
        );
        let gc = gc_orphans(&store, &part).expect("gc");
        assert_eq!(
            gc,
            OrphanGc {
                reclaimed: 1,
                failures: 0
            },
            "orphan consolidated reclaimed"
        );
        assert_eq!(
            live_keys(&store, &part).len(),
            inputs.len(),
            "inputs stay live"
        );
        assert_eq!(read_partition_rows(&store), originals, "inputs intact");
    }

    /// RFC0009.4 — a stray `*.parquet.tmp` with NO manifest (glob live
    /// set): every `.parquet` is live, so only the interrupted `.tmp`
    /// publish is reclaimed. See the post-commit test for the shared note.
    #[test]
    fn rfc0009_4_stray_tmp_reclaimed_under_glob_fallback() {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(9, TS0)]);
        let tmp_key = format!(
            "{}/0190abcd-dead-7eef-8aaa-000000000000.parquet.tmp",
            partition_data_prefix(&part),
        );
        store
            .put_blocking(&tmp_key, b"torn".to_vec())
            .expect("stray tmp");
        let before = read_partition_rows(&store);
        let gc = gc_orphans(&store, &part).expect("gc");
        assert_eq!(
            gc,
            OrphanGc {
                reclaimed: 1,
                failures: 0
            },
            "only the .tmp reclaimed"
        );
        assert_eq!(
            live_keys(&store, &part).len(),
            1,
            "the live .parquet is untouched"
        );
        assert_eq!(read_partition_rows(&store), before, "glob data intact");
    }

    /// RFC0009.1 — compaction drives the H4 small-file **count** down. A
    /// partition fragmented into more than `CompactionPolicy::min_files`
    /// files (the over-fragmentation trigger) collapses to a single file,
    /// dropping the per-tenant small-file count that H4's "fewer than 5 %
    /// of files below 128 MiB" signal tracks. At unit scale the
    /// consolidated file is itself small — the file-*size* distribution is
    /// the §6 corpus test's job; this asserts the file-count lever and row
    /// conservation across the collapse. The input count derives from the
    /// policy so it can't drift out of sync with the default.
    #[test]
    fn rfc0009_1_many_small_files_collapse_to_one() {
        let policy = CompactionPolicy::default();
        // One past the over-fragmentation trigger. Every record uses the
        // same in-hour timestamp, so all inputs belong to one partition
        // regardless of how large `min_files` is — a per-record time step
        // could otherwise spill past the hour and trip the RFC0009.5
        // row-vs-path check for a reason unrelated to this test.
        let n = policy.min_files + 1;
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        for i in 0..n {
            let template_id = u64::try_from(i + 1).expect("small count");
            write_file(&store, &[rec(template_id, TS0)]);
        }
        let before = live_keys(&store, &part);
        assert_eq!(before.len(), n, "one small file per write");
        assert!(before.len() > policy.min_files, "starts over-fragmented");

        let outcome = compact_partition(&store, &part).expect("compact");
        assert_eq!(outcome.files_before, n);
        assert_eq!(
            outcome.rows,
            u64::try_from(n).expect("small count"),
            "all rows carried",
        );

        let after = live_keys(&store, &part);
        assert_eq!(after.len(), 1, "collapsed to a single live file");
        assert!(
            after.len() <= policy.min_files,
            "no longer over-fragmented (H4 small-file count down)",
        );
        // H4 counts *physical* files (footer reads), so the inputs must
        // actually be gone — not merely manifest-excluded orphans that
        // `live_keys` would hide. Assert both: the GC removed them and
        // exactly one `.parquet` remains present.
        assert_eq!(outcome.gc_failures, 0, "every superseded input removed");
        assert_eq!(
            on_disk_parquet_count(&store, &part),
            1,
            "exactly one physical .parquet file remains"
        );
        let rows = read_key(&store, &part, &after[0]);
        assert_eq!(rows.len(), n, "row conservation across the collapse");
    }

    /// RFC0009.6 — forward-compatible (union-schema) merge. Inputs that
    /// span a schema amendment — one written with the current full schema,
    /// one a pre-amendment file missing an OPTIONAL column — compact into a
    /// single file carrying the union schema, read back without error
    /// (RFC 0005 §3.9), with every row preserved. Compaction reads each
    /// input through `Reader` (which fills a missing OPTIONAL as the §3.9
    /// default) and rewrites via `Writer` (the full schema), so the output
    /// is the superset.
    #[test]
    fn rfc0009_6_merges_inputs_spanning_a_schema_amendment() {
        use parquet::arrow::ArrowWriter;
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        // File A — current full schema.
        write_file(&store, &[rec(1, TS0)]);
        let dir = part.data_path(bucket.path());

        // File B — a pre-amendment file missing the OPTIONAL
        // `effective_time_unix_nano` column (added 2026-06-11). Built by
        // projecting a full batch down by that one column, so no arrays
        // are hand-rolled. Same tenant + hour as A, so the row-vs-path
        // check (RFC0009.5) passes via the surviving `time_unix_nano`.
        // Written directly to the local store path (File A's write already
        // created the partition dir); compaction reads it back via the store.
        let full = crate::mined_records_to_batch(&[rec(2, TS0)]).expect("full batch");
        let drop = full
            .schema()
            .index_of(crate::columns::EFFECTIVE_TIME_UNIX_NANO)
            .expect("amended column present in the full schema");
        let keep: Vec<usize> = (0..full.num_columns()).filter(|&i| i != drop).collect();
        let reduced = full
            .project(&keep)
            .expect("project off the OPTIONAL column");
        assert!(
            reduced
                .schema()
                .index_of(crate::columns::EFFECTIVE_TIME_UNIX_NANO)
                .is_err(),
            "file B is missing the OPTIONAL column",
        );
        let path_b = dir.join("0190abcd-0000-7000-8000-000000000002.parquet");
        let file_b = std::fs::File::create(&path_b).expect("create B");
        let mut w = ArrowWriter::try_new(file_b, reduced.schema(), None).expect("arrow writer");
        w.write(&reduced).expect("write B");
        w.close().expect("close B");

        // Two inputs with differing schemas → union merge.
        let outcome = compact_partition(&store, &part).expect("union merge");
        assert_eq!(outcome.files_before, 2);
        assert_eq!(outcome.rows, 2, "both rows carried across the union merge");

        // Output carries the full (union) schema and reads without error.
        let live = live_keys(&store, &part);
        assert_eq!(live.len(), 1, "consolidated to one file");
        // Assert the union directly: the consolidated Parquet schema
        // carries the amended column file B lacked (not B's reduced one).
        let out_bytes = store.get_blocking(&live[0]).expect("get output");
        let out_schema = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(out_bytes))
            .expect("output reader builder")
            .schema()
            .clone();
        assert!(
            out_schema
                .index_of(crate::columns::EFFECTIVE_TIME_UNIX_NANO)
                .is_ok(),
            "consolidated output carries the union (amended) schema",
        );
        let rows = read_key(&store, &part, &live[0]);
        assert_eq!(rows.len(), 2, "every row preserved across the amendment");
    }

    /// RFC0009.5 — tenant + partition isolation. Compaction reads every
    /// input through `Reader::open_partition_bytes`, which enforces the
    /// RFC 0005 §3.9 row-vs-path contract, so an input file holding a row
    /// that belongs to a *different* time bucket (or tenant) aborts the
    /// compaction instead of being silently merged across the boundary.
    #[test]
    fn rfc0009_5_mis_partitioned_input_aborts_rather_than_merging() {
        use parquet::arrow::ArrowWriter;

        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        // A legitimate input for partition P.
        write_file(&store, &[rec(1, TS0)]);
        let dir = part.data_path(bucket.path());

        // A second file dropped into P's directory whose row belongs to a
        // *different* hour (TS0 + 2 h) — a mis-partitioned input.
        let foreign = rec(2, TS0 + 2 * HOUR_NANOS);
        assert_ne!(
            PartitionKey::derive(&foreign).expect("derive foreign"),
            part,
            "the foreign row really maps to another partition",
        );
        let batch = crate::mined_records_to_batch(&[foreign]).expect("batch");
        let path = dir.join("0190abcd-0000-7000-8000-0000000000f0.parquet");
        let file = std::fs::File::create(&path).expect("create foreign");
        let mut w = ArrowWriter::try_new(file, batch.schema(), None).expect("writer");
        w.write(&batch).expect("write foreign");
        w.close().expect("close foreign");

        // Two inputs, one mis-partitioned → compaction aborts on the
        // row-vs-path check; it never merges rows across partition keys.
        let err = compact_partition(&store, &part).expect_err("must reject");
        assert!(
            matches!(
                err,
                CompactionError::Read(ReaderError::PartitionMismatch { .. })
            ),
            "aborts specifically on the §3.9 row-vs-path check, not some other \
             read failure; got {err:?}",
        );
    }

    #[test]
    fn compacts_two_files_into_one_preserving_rows() {
        // Arrange — two committed files in one partition (5 rows total).
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
        write_file(
            &store,
            &[
                rec(2, TS0 + 2_000_000),
                rec(2, TS0 + 3_000_000),
                rec(2, TS0 + 4_000_000),
            ],
        );

        // Act
        let outcome = compact_partition(&store, &part).expect("compact");

        // Assert — consolidated to one file with all 5 rows, manifest
        // names it, inputs GC'd, rows preserved.
        assert_eq!(outcome.files_before, 2);
        assert_eq!(outcome.rows, 5);
        assert_eq!(outcome.gc_failures, 0, "both inputs removed");
        let committed = outcome.committed.expect("committed");
        let live = live_keys(&store, &part);
        assert_eq!(live.len(), 1, "one file remains live");
        assert!(live[0].ends_with(&committed.file));
        let rows = read_key(&store, &part, &live[0]);
        assert_eq!(rows.len(), 5, "every row preserved");
    }

    #[test]
    fn reports_byte_volumes_for_io_and_file_size_metrics() {
        // Arrange — two committed files in one partition.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
        write_file(&store, &[rec(2, TS0 + 2_000_000)]);

        // Act
        let outcome = compact_partition(&store, &part).expect("compact");

        // Assert — read volume covers both inputs, write volume is the
        // (sole, live) consolidated file's actual stored byte size.
        let committed = outcome.committed.expect("committed");
        let live = live_keys(&store, &part);
        assert_eq!(live.len(), 1, "one consolidated file remains live");
        let stored = store.get_blocking(&live[0]).expect("get").len() as u64;
        assert!(outcome.bytes_read > 0, "read volume is recorded");
        assert_eq!(
            outcome.bytes_written, stored,
            "write volume is the consolidated file's byte size"
        );
        assert!(live[0].ends_with(&committed.file));
    }

    #[test]
    fn no_op_reports_zero_byte_volumes() {
        // Arrange — one file: a no-op, nothing read or written.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0)]);

        // Act
        let outcome = compact_partition(&store, &part).expect("compact");

        // Assert
        assert!(outcome.committed.is_none());
        assert_eq!(outcome.bytes_read, 0);
        assert_eq!(outcome.bytes_written, 0);
    }

    #[test]
    fn single_file_partition_is_a_no_op() {
        // Arrange — one file, nothing to consolidate.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0)]);

        // Act
        let outcome = compact_partition(&store, &part).expect("compact");

        // Assert — no-op: no commit, no manifest written.
        assert_eq!(outcome.files_before, 1);
        assert!(outcome.committed.is_none());
        assert!(
            read_manifest(&store, &part).expect("manifest").is_none(),
            "a no-op writes no manifest",
        );
    }

    #[test]
    fn bumps_generation_from_an_existing_manifest() {
        // Arrange — two files plus a manifest already at generation 5.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0)]);
        write_file(&store, &[rec(2, TS0 + 1_000_000)]);
        let names = basenames(&live_keys(&store, &part));
        seed_manifest(
            &store,
            &part,
            &Manifest {
                generation: 5,
                files: names,
            },
        );

        // Act
        let outcome = compact_partition(&store, &part).expect("compact");

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
        let store = store_at(bucket.path());
        write_file(&store, &[rec(1, TS0)]);
        write_file(&store, &[rec(2, TS0 + 1_000_000)]);

        // Act
        let selected =
            plan_candidates(&store, "a", NOW_UNSEALED, &CompactionPolicy::default()).expect("plan");

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
        let store = store_at(bucket.path());
        write_file(&store, &[rec(1, TS0)]);
        write_file(&store, &[rec(2, TS0 + 1_000_000)]);

        // Act
        let selected =
            plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

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
        let store = store_at(bucket.path());
        for ts in [TS0, TS0 + HOUR_NANOS] {
            for template_id in [1_u64, 2] {
                let record = rec(template_id, ts);
                let mut w =
                    Writer::open_in(&store, PartitionKey::derive(&record).unwrap()).expect("open");
                w.append_records(&[record]).expect("append");
                w.close().expect("close");
            }
        }
        let now = TS0 + 3 * HOUR_NANOS; // past hour 11's end + grace

        // Act
        let selected =
            plan_candidates(&store, "a", now, &CompactionPolicy::default()).expect("plan");

        // Assert — both selected, oldest first, regardless of listing order.
        let hours: Vec<u32> = selected.iter().map(|p| p.hour).collect();
        assert_eq!(hours, vec![10, 11], "deterministic, chronological");
    }

    #[test]
    fn plan_skips_a_single_file_partition() {
        // Arrange — one file: sealed, but nothing to consolidate.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_file(&store, &[rec(1, TS0)]);

        // Act
        let selected =
            plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

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
        let store = store_at(bucket.path());
        for i in 0..5 {
            write_file(&store, &[rec(1, TS0 + i * 1_000)]);
        }
        let policy = CompactionPolicy {
            min_files: 4,
            small_file_bytes: 1,
            grace_nanos: CompactionPolicy::default().grace_nanos,
        };

        // Act
        let selected = plan_candidates(&store, "a", NOW_SEALED, &policy).expect("plan");

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
        let store = store_at(bucket.path());
        write_file(&store, &[rec(1, TS0)]);
        write_file(&store, &[rec(2, TS0 + 1_000_000)]);
        let policy = CompactionPolicy {
            min_files: 4,
            small_file_bytes: 1,
            grace_nanos: CompactionPolicy::default().grace_nanos,
        };

        // Act
        let selected = plan_candidates(&store, "a", NOW_SEALED, &policy).expect("plan");

        // Assert
        assert!(selected.is_empty(), "few large files are not a candidate");
    }

    #[test]
    fn plan_skips_non_canonical_partition_dir_names() {
        // Arrange — a sealed partition whose `month` segment isn't
        // zero-padded (`month=4`, not `month=04`). A PartitionKey from
        // it would render `month=04` via data_path and miss this key,
        // so it must not be selected.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        for name in ["a.parquet", "b.parquet"] {
            store
                .put_blocking(
                    &format!("data/tenant_id=a/year=2026/month=4/day=02/hour=10/{name}"),
                    b"x".to_vec(),
                )
                .expect("put");
        }

        // Act
        let selected =
            plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

        // Assert
        assert!(selected.is_empty(), "non-canonical dir names are skipped");
    }

    #[test]
    fn plan_for_a_tenant_with_no_data_is_empty() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());

        // Act
        let selected = plan_candidates(&store, "ghost", NOW_SEALED, &CompactionPolicy::default())
            .expect("plan");

        // Assert
        assert!(selected.is_empty());
    }

    // --- RFC 0036 §3.2 sorted-compaction internals ---

    /// A record for the sort tests: `service` becomes the promoted
    /// `service.name` resource attribute (`None` = absent, the §3.1
    /// nulls-first case) and `id` a unique param payload so equal-key
    /// rows stay distinguishable for tie-break assertions.
    fn sort_rec(service: Option<&str>, ts_ns: u64, id: u64) -> MinedRecord {
        let resource_attributes = match service {
            Some(name) => vec![ourios_core::otlp::KeyValue {
                key: SERVICE_NAME_KEY.to_string(),
                value: Some(ourios_core::otlp::AnyValue {
                    value: Some(ourios_core::otlp::any_value::Value::StringValue(
                        name.to_string(),
                    )),
                }),
                ..Default::default()
            }],
            None => Vec::new(),
        };
        MinedRecord {
            resource_attributes,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: id.to_string(),
            }],
            ..rec(id, ts_ns)
        }
    }

    /// Mirror partition `part`'s data files from `from` into `to`
    /// byte-for-byte under the same names, so two stores hold the
    /// RFC0036.4 "same bytes, same names" input set.
    fn mirror_partition(from: &Store, to: &Store, part: &PartitionKey) {
        for key in from
            .list_blocking(Some(&partition_data_prefix(part)))
            .expect("list source")
        {
            let bytes = from.get_blocking(&key).expect("get source");
            to.put_blocking(&key, bytes).expect("put mirror");
        }
    }

    /// Read the consolidated file's raw bytes after a committed
    /// compaction.
    fn consolidated_bytes(store: &Store, part: &PartitionKey, committed: &Committed) -> Vec<u8> {
        let key = format!("{}/{}", partition_data_prefix(part), committed.file);
        store.get_blocking(&key).expect("get consolidated")
    }

    proptest::proptest! {
        // Each case compacts the same inputs through both §3.2 paths
        // (in-memory and forced spill + fan-in-2 hierarchical merge),
        // so keep the case count moderate.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        /// RFC0036.1 (§6 merge property, internal half) — for arbitrary
        /// service/time/duplicate-key mixes, both §3.2 paths produce the
        /// §3.1 total order: the multiset equals the inputs' union,
        /// rows are (service, time)-sorted with absent-service first,
        /// equal-key rows land in (sorted-basename input ordinal,
        /// pre-sort row ordinal) tie-break order — and the spill path's
        /// bytes are identical to the in-memory path's, which is the
        /// §3.5 determinism argument across the §7 skip-spill fork.
        /// See `docs/rfcs/0036-write-side-layout.md` §5 / §6.
        #[test]
        fn sorted_merge_realises_the_total_order_on_both_paths(
            files in proptest::collection::vec(
                proptest::collection::vec(
                    // (service index; 0 = absent, times from a small
                    // pool to force duplicate keys)
                    (0usize..4, 0u64..6),
                    1..=12usize,
                ),
                2..=5usize,
            )
        ) {
            let services = [None, Some("svc-a"), Some("svc-b"), Some("svc-c")];
            let bucket_a = tempfile::tempdir().expect("temp a");
            let bucket_b = tempfile::tempdir().expect("temp b");
            let store_a = store_at(bucket_a.path());
            let store_b = store_at(bucket_b.path());
            let part = partition();

            let mut id: u64 = 0;
            let mut inputs: Vec<(String, Vec<MinedRecord>)> = Vec::new();
            for file in &files {
                let recs: Vec<MinedRecord> = file
                    .iter()
                    .map(|(svc, toff)| {
                        id += 1;
                        sort_rec(services[*svc], HOUR10_START + toff * 1_000, id)
                    })
                    .collect();
                let mut w = Writer::open_in(&store_a, part.clone()).expect("open writer");
                w.append_records(&recs).expect("append");
                let written = w.close().expect("close");
                inputs.push((basename(&written.key).to_owned(), recs));
            }
            mirror_partition(&store_a, &store_b, &part);

            // The §3.1 model order: concatenate in sorted-basename input
            // order, then stable-sort by (service, time) — leaving
            // equal-key rows in (input ordinal, row ordinal) order.
            inputs.sort_by(|(a, _), (b, _)| a.cmp(b));
            let mut expected: Vec<MinedRecord> =
                inputs.into_iter().flat_map(|(_, recs)| recs).collect();
            sort_records(ClusterKeys::ServiceThenTime, &mut expected);

            let in_memory = compact_partition(&store_a, &part).expect("compact in-memory");
            let spilled = compact_sorted(
                &store_b,
                &part,
                &PromotedAttributes::default(),
                ClusterKeys::ServiceThenTime,
                SortTuning {
                    in_memory_max_bytes: 0,
                    fan_in: 2,
                    ..SortTuning::default()
                },
            )
            .expect("compact spilled");
            let in_memory = in_memory.committed.expect("in-memory commit");
            let spilled = spilled.committed.expect("spill commit");

            let bytes_a = consolidated_bytes(&store_a, &part, &in_memory);
            let bytes_b = consolidated_bytes(&store_b, &part, &spilled);
            proptest::prop_assert!(
                bytes_a == bytes_b,
                "in-memory and spill paths must emit byte-identical output \
                 ({} vs {} bytes)",
                bytes_a.len(),
                bytes_b.len(),
            );

            let got = Reader::open_partition_bytes(
                Bytes::from(bytes_a),
                part.clone(),
                &in_memory.file,
            )
            .expect("open consolidated")
            .read_all()
            .expect("read consolidated");
            proptest::prop_assert_eq!(got, expected, "§3.1 total order realised");
        }
    }

    /// RFC 0036 §3.1 / §7 — the time-only fallback. A promoted set
    /// without `service.name` is unrepresentable today (RFC 0022 makes
    /// the key implicit and non-removable), so the degradation is
    /// driven through the internal seam: under `ClusterKeys::TimeOnly`
    /// the consolidated rows sort by `time_unix_nano` alone (service
    /// values deliberately anti-lexicographic to prove they are
    /// ignored) and every row group declares the single time sorting
    /// column.
    #[test]
    fn time_only_keys_sort_and_declare_time_alone() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[sort_rec(Some("zzz"), TS0, 1)]);
        write_file(&store, &[sort_rec(Some("aaa"), TS0 + 1_000, 2)]);
        write_file(&store, &[sort_rec(None, TS0 + 500, 3)]);

        let outcome = compact_sorted(
            &store,
            &part,
            &PromotedAttributes::default(),
            ClusterKeys::TimeOnly,
            SortTuning::default(),
        )
        .expect("compact");
        let committed = outcome.committed.expect("committed");
        let bytes = consolidated_bytes(&store, &part, &committed);

        let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone()))
            .expect("open consolidated");
        let meta = builder.metadata();
        for rg in meta.row_groups() {
            let declared = rg.sorting_columns().expect("sorting_columns declared");
            assert_eq!(declared.len(), 1, "time-only declares a single key");
            let leaf = usize::try_from(declared[0].column_idx).expect("leaf index");
            assert_eq!(
                rg.column(leaf).column_path().string(),
                crate::columns::TIME_UNIX_NANO,
                "the single key is time_unix_nano",
            );
            assert!(!declared[0].descending, "ascending");
        }

        let rows = Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), &committed.file)
            .expect("open")
            .read_all()
            .expect("read");
        let times: Vec<u64> = rows.iter().map(|r| r.time_unix_nano).collect();
        assert_eq!(
            times,
            vec![TS0, TS0 + 500, TS0 + 1_000],
            "time-only order ignores service values",
        );
    }

    /// Build a `k`-file partition of `s` rows each, in [`partition`], with
    /// rotating promoted `service.name` values and per-row-unique times so
    /// the §3.1 sort has real work.
    fn build_k_file_partition(store: &Store, k: u64, s: u64) {
        let part = partition();
        let mut id: u64 = 0;
        for _ in 0..k {
            let recs: Vec<MinedRecord> = (0..s)
                .map(|_| {
                    id += 1;
                    let svc = ["svc-a", "svc-b", "svc-c"][usize::try_from(id % 3).expect("mod 3")];
                    sort_rec(Some(svc), HOUR10_START + id, id)
                })
                .collect();
            let mut w = Writer::open_in(store, part.clone()).expect("open writer");
            w.append_records(&recs).expect("append");
            w.close().expect("close");
        }
    }

    /// RFC0036.3 (memory bound) — the load-bearing §3.2 claim. On a
    /// partition of `K` inputs of `S` rows each, the forced-spill sort's
    /// peak decoded-row residency is bounded by one input (phase 1,
    /// inputs decoded strictly one at a time) plus `F × batch` (phase 2,
    /// one streamed batch per open run) — it must NOT regress to holding
    /// the whole `K × S` partition decoded, which is the whole reason the
    /// external merge sort exists. The in-memory (skip-spill) path, by
    /// contrast, deliberately holds the whole partition (§7 tradeoff,
    /// bounded by `in_memory_max_bytes`); measuring both peaks on the
    /// *same* fixture pins both halves of §3.2's accurate bound. The gauge
    /// is thread-local (a `compact_*` call runs entirely on this thread),
    /// so the assertion is immune to `cargo test`'s in-process parallelism.
    /// See `docs/rfcs/0036-write-side-layout.md` §5 / §6.
    #[test]
    fn rfc0036_3_forced_spill_peak_far_below_whole_partition() {
        // K inputs of S rows. The spill path's peak is dominated by
        // phase-1's one fully decoded input (S rows, decoded strictly one
        // at a time); phase-2 opens only K < F cursors — no hierarchical
        // pass — each holding one small reader batch, well under S. So the
        // peak sits at ~one input, an order of magnitude below the whole
        // partition (K × S), making a whole-partition regression
        // unambiguous. (The F × batch term in the RFC 0036 §3.2 bound is
        // the worst case for F saturated runs; it does not bite here.)
        const K: u64 = 6;
        const S: u64 = 12_000;
        let total = usize::try_from(K * S).expect("fits usize");
        let fan_in = SortTuning::default().fan_in;

        // --- Spill path: force it with in_memory_max_bytes = 0 so every
        // input spills one at a time. ---
        let bucket_spill = tempfile::tempdir().expect("temp spill");
        let store_spill = store_at(bucket_spill.path());
        build_k_file_partition(&store_spill, K, S);
        residency::reset();
        let spilled = compact_sorted(
            &store_spill,
            &partition(),
            &PromotedAttributes::default(),
            ClusterKeys::ServiceThenTime,
            SortTuning {
                in_memory_max_bytes: 0,
                fan_in,
                ..SortTuning::default()
            },
        )
        .expect("compact spilled");
        let spill_peak = residency::peak();
        assert!(spilled.committed.is_some(), "≥2 files ⇒ a commit");
        assert_eq!(spilled.rows, K * S, "every row carried");

        // RFC0036.3's property is an *upper* bound — "not whole-partition".
        // The teeth: peak must stay far below the whole partition; this fails
        // if the merge ever buffers everything decoded. We deliberately do
        // NOT assert a lower bound near one input: a future formation that
        // streams within an input could peak below S and still satisfy the
        // RFC. `> 0` is only a gauge-liveness sanity (spilling decodes rows).
        assert!(spill_peak > 0, "the residency gauge recorded nothing");
        assert!(
            spill_peak < total / 2,
            "forced-spill peak {spill_peak} regressed toward whole-partition \
             residency (total {total}) — the merge must not hold the partition decoded",
        );

        // --- In-memory path: same fixture, unbounded skip-spill window. ---
        let bucket_mem = tempfile::tempdir().expect("temp mem");
        let store_mem = store_at(bucket_mem.path());
        build_k_file_partition(&store_mem, K, S);
        residency::reset();
        let in_memory = compact_sorted(
            &store_mem,
            &partition(),
            &PromotedAttributes::default(),
            ClusterKeys::ServiceThenTime,
            SortTuning {
                in_memory_max_bytes: u64::MAX,
                fan_in,
                ..SortTuning::default()
            },
        )
        .expect("compact in-memory");
        let mem_peak = residency::peak();
        assert_eq!(in_memory.rows, K * S, "every row carried");
        assert_eq!(
            mem_peak, total,
            "the in-memory path holds the whole partition decoded (bounded by \
             in_memory_max_bytes — the §7 skip-spill tradeoff)",
        );

        // The contrast is the point: the spill path holds a fraction of what
        // the in-memory path holds on the identical partition.
        assert!(
            spill_peak * 4 < mem_peak,
            "the spill path ({spill_peak}) must hold far less than the \
             in-memory path ({mem_peak})",
        );
    }
}
