//! Sealed-partition compaction (RFC 0009), through the object-storage
//! [`Store`] seam (RFC 0019).
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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use chrono::NaiveDate;

use crate::manifest::{MANIFEST_FILENAME, Manifest, ManifestError, Published};
use crate::partition::{PartitionKey, percent_encode_tenant};
use crate::reader::{Reader, ReaderError};
use crate::store::{Store, StoreError};
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
    let key = manifest_key(partition);
    let (existing, etag) =
        match Manifest::read_with_etag(store, &key).map_err(CompactionError::Manifest)? {
            Some((manifest, etag)) => (Some(manifest), etag),
            None => (None, None),
        };
    let inputs = live_file_keys(store, partition, existing.as_ref())?;
    if inputs.len() < 2 {
        return Ok(no_op_outcome(inputs.len()));
    }

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

    // Stream the inputs into the consolidated file one at a time, so peak
    // memory is bounded by a single input file's rows rather than the whole
    // partition. `open_partition_bytes` validates each row's tenant + time
    // bucket against this partition (RFC 0005 §3.9 / RFC0009.5), so a
    // mis-partitioned input aborts the compaction instead of being silently
    // merged. Row groups rotate at the RFC 0005 §3.5 threshold within the
    // single output.
    let mut writer = Writer::open_in(store, partition.clone()).map_err(CompactionError::Write)?;
    let mut row_count: u64 = 0;
    let mut bytes_read: u64 = 0;
    for input in &inputs {
        let bytes = store
            .get_blocking(input)
            .map_err(|e| store_io("get", input, e))?;
        bytes_read = bytes_read.saturating_add(bytes.len() as u64);
        let reader = Reader::open_partition_bytes(Bytes::from(bytes), partition.clone(), input)
            .map_err(CompactionError::Read)?;
        let records = reader.read_all().map_err(CompactionError::Read)?;
        // `usize <= u64` on every supported target; saturate rather than panic
        // on a theoretically wider one.
        row_count = row_count.saturating_add(u64::try_from(records.len()).unwrap_or(u64::MAX));
        writer
            .append_records(&records)
            .map_err(CompactionError::Write)?;
    }
    let written = writer.close().map_err(CompactionError::Write)?;
    let bytes_written = written.bytes_written;
    let consolidated = basename(&written.key).to_owned();

    // Commit: swap the manifest to name only the consolidated file. The input
    // names (the merged-away set) for the §3.6 audit event are captured here,
    // sorted so the event is deterministic regardless of listing order.
    let generation = base_generation.saturating_add(1);
    let mut input_files = basenames(&inputs);
    input_files.sort();
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
/// objects, by listing every key under `data/tenant_id=<enc>/` and parsing the
/// partition tuple from each key's path segments. Keys whose trailing
/// `…/year=/month=/day=/hour=/<file>` run isn't the canonical zero-padded form
/// (the shape [`PartitionKey::data_path`] writes, so the key round-trips) are
/// skipped — the key-based equivalent of the old directory walk dropping
/// non-canonical names. Returned sorted chronologically (oldest first) and
/// deduplicated (many files, plus the manifest, map to one partition).
fn hour_partitions(store: &Store, tenant: &str) -> Result<Vec<PartitionKey>, CompactionError> {
    let prefix = format!("data/tenant_id={}", percent_encode_tenant(tenant));
    let keys = store
        .list_blocking(Some(&prefix))
        .map_err(|e| store_io("list", &prefix, e))?;
    let mut partitions: Vec<PartitionKey> = keys
        .iter()
        .filter_map(|key| parse_hour_partition_key(tenant, key))
        .collect();
    // Ascending tuple order is chronological (oldest sealed partition first);
    // dedup after the sort collapses the many keys that share one partition.
    partitions.sort_by_key(|p| (p.year, p.month, p.day, p.hour));
    partitions.dedup();
    Ok(partitions)
}

/// Parse a [`PartitionKey`] from a data-object `key` by matching its trailing
/// `…/year=YYYY/month=MM/day=DD/hour=HH/<file>` segments (the leaf file name
/// dropped). Requires a **contiguous** run of the four canonical zero-padded
/// segments — mirroring the querier's `parse_day_partition` fix (don't pick
/// `year=` from an arbitrary position) — so a non-contiguous, non-canonical, or
/// foreign layout yields `None` and is skipped.
fn parse_hour_partition_key(tenant: &str, key: &str) -> Option<PartitionKey> {
    // Segments deepest-first, with the leaf (file name) dropped.
    let mut segments = key.rsplit('/').skip(1);
    let hour = parse_partition_segment(segments.next()?, "hour", 2)?;
    let day = parse_partition_segment(segments.next()?, "day", 2)?;
    let month = parse_partition_segment(segments.next()?, "month", 2)?;
    let year = parse_partition_segment(segments.next()?, "year", 4)?;
    Some(PartitionKey {
        tenant_id: tenant.to_owned(),
        year: i32::try_from(year).ok()?,
        month,
        day,
        hour,
    })
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
/// Used both for a true no-op (a sub-two-file partition — no I/O happens) and
/// for a lost manifest CAS: in the lost-race case inputs were read and a
/// consolidated object was written, but it lost the swap and is left as an
/// orphan (a later `gc_orphans` reclaims it), so from the sweep's accounting
/// nothing was committed. The read/written bytes of a lost race are not
/// attributed here (the work is discarded).
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
}
