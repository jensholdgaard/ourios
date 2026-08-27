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
    ClusterKeys, DEFAULT_ZSTD_LEVEL, MAX_COMPACTED_RG_BYTES, SUB_BATCH_ROWS, Writer, WriterError,
};

/// One hour in nanoseconds — the span a `…/hour=HH/` partition covers.
const HOUR_NANOS: u64 = 3_600_000_000_000;

mod commit;
mod keys;
mod plan;
mod sort;
#[cfg(test)]
mod tests;

// The sibling modules were split out of this file mechanically (epic
// #745 wave 1); gluing their scopes back together here keeps every
// pre-split path — including the test module's `super::X` — resolving
// unchanged.
// Scope glue: siblings and the test module reach each other's items
// through this parent scope, so every pre-split path resolves
// unchanged. Wildcards are the point (the children ARE this module);
// unused-allow because not every sibling needs every other.
#[allow(unused_imports, clippy::wildcard_imports)]
use commit::*;
pub use commit::{OrphanGc, gc_orphans};
#[allow(unused_imports, clippy::wildcard_imports)]
use keys::*;
#[allow(unused_imports, clippy::wildcard_imports)]
use plan::*;
pub use plan::{hour_partitions, plan_candidates, visit_partition_rows};
#[allow(unused_imports, clippy::wildcard_imports)]
use sort::*;

/// What a [`compact_partition`] call did.
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    /// Number of live files before compaction.
    pub files_before: usize,
    /// Rows in the consolidated file (the total input rows minus any an
    /// RFC 0047 §3.6 erasure dropped). `0` on a no-op.
    pub rows: u64,
    /// Rows an erasure filter removed (RFC 0047 §3.6); `0` without one.
    pub rows_dropped: u64,
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

/// Row-level hooks a caller threads through the rewrite (RFC 0047 §3.3 /
/// §3.6): `observe` sees every input row once, as decoded (the graph
/// emitter's feed); `drop` removes rows from the consolidated output (a
/// conversation-scoped erasure) — with a `drop` filter the partition is
/// rewritten even when it holds a single file, so the erasure lands.
#[derive(Default)]
pub struct RowHooks<'a> {
    /// Called once per input file with its decoded rows, before any drop.
    pub observe: Option<&'a mut RowObserver<'a>>,
    /// Rows for which this returns `true` are not written back.
    pub drop: Option<&'a RowFilter<'a>>,
}

/// A [`RowHooks::observe`] callback.
pub type RowObserver<'a> = dyn FnMut(&[MinedRecord]) + 'a;
/// A [`RowHooks::drop`] predicate.
pub type RowFilter<'a> = dyn Fn(&MinedRecord) -> bool + 'a;

/// [`compact_partition_with_promoted`] with [`RowHooks`].
///
/// # Errors
///
/// See [`compact_partition`].
pub fn compact_partition_hooked(
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
    hooks: &mut RowHooks<'_>,
) -> Result<CompactionOutcome, CompactionError> {
    compact_sorted_hooked(
        store,
        partition,
        promoted,
        ClusterKeys::for_promoted(promoted),
        SortTuning::default(),
        hooks,
    )
}

/// Like [`compact_partition`] but rotating compacted row groups at an
/// explicit `flush_bytes` threshold instead of the RFC 0036 §3.3
/// **adaptive** default (`OURIOS_COMPACTED_RG_BYTES` env, else the value
/// [`adaptive_flush_bytes`](crate::adaptive_flush_bytes) derives from the
/// partition's input size) — the deterministic seam for the RFC 0036 §7
/// threshold sweep (16 / 32 / 64 MiB). An explicit threshold wins over
/// both the env override and the adaptive value. This is a
/// **physical-layout knob**, not a schema or content change: the
/// consolidated rows, their §3.1 order, and the declared `sorting_columns`
/// are identical for any threshold. Only the row-group rotation boundaries
/// move — and with them the group count, each group's
/// statistics/dictionary/page encoding decisions, and the on-disk size
/// (the whole point of the sweep). Production compacts via
/// [`compact_partition`] (the adaptive default); the sweep passes an
/// explicit value here so it never sets a process env var (unsound under
/// `cargo test`'s in-process parallelism — it races libc `getenv`).
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
    /// `None` (production) takes the adaptive default (`OURIOS_COMPACTED_RG_BYTES`
    /// env, else [`adaptive_flush_bytes`](crate::adaptive_flush_bytes) of the
    /// partition's input size); `Some(t)` pins it for the §7 threshold
    /// sweep's deterministic seam ([`compact_partition_with_flush_threshold`]).
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
    compact_sorted_hooked(
        store,
        partition,
        promoted,
        keys,
        tuning,
        &mut RowHooks::default(),
    )
}

fn compact_sorted_hooked(
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
    keys: ClusterKeys,
    tuning: SortTuning,
    hooks: &mut RowHooks<'_>,
) -> Result<CompactionOutcome, CompactionError> {
    let key = manifest_key(partition);
    let (existing, etag) =
        match Manifest::read_with_etag(store, &key).map_err(CompactionError::Manifest)? {
            Some((manifest, etag)) => (Some(manifest), etag),
            None => (None, None),
        };
    let mut inputs = live_file_keys(store, partition, existing.as_ref())?;
    // Consolidation needs two files; an erasure (`drop`) rewrites any
    // non-empty partition.
    let minimum_inputs = if hooks.drop.is_some() { 1 } else { 2 };
    if inputs.len() < minimum_inputs {
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

    // RFC 0036 §3.3 adaptive row-group threshold: estimate the output size
    // as the sum of the live input files' sizes (the sorted output is the
    // same rows re-compressed, so this is a safe upper estimate), and let
    // the writer scale the rotation threshold to ~K groups from it. An
    // explicit `tuning.compacted_flush_bytes` (the sweep seam) or the
    // `OURIOS_COMPACTED_RG_BYTES` env override takes precedence over the
    // adaptive value (resolved in `open_in_compacted`).
    let estimated_output_bytes = sum_input_sizes(store, partition, &inputs)?;

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
        estimated_output_bytes,
    )
    .map_err(CompactionError::Write)?;
    let (row_count, bytes_read, rows_dropped) = sort_inputs_into(
        &mut writer,
        store,
        partition,
        promoted,
        keys,
        tuning,
        &inputs,
        hooks,
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
        rows_dropped,
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
