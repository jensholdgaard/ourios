//! Key/prefix/manifest helpers shared across the compaction modules.
//! Split from the flat `compaction.rs` (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path — types, siblings, external crates — resolving
// unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn live_file_keys(
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

/// Sum the on-disk byte sizes of the partition's live input files — the
/// RFC 0036 §3.3 `estimated_output_bytes` the adaptive row-group threshold
/// scales from (the sorted output re-compresses the same rows, so the
/// input total is a safe upper estimate). One `list_with_sizes` over the
/// partition prefix (the same call `is_candidate` uses to size small-file
/// candidates), summing only the keys named in `inputs`. An input absent
/// from the listing (a listing that raced a delete omits the key entirely)
/// simply isn't summed — the estimate only steers the threshold within its
/// clamp, so an under-count at worst floors it, never mis-sizes upward.
pub(super) fn sum_input_sizes(
    store: &Store,
    partition: &PartitionKey,
    inputs: &[String],
) -> Result<u64, CompactionError> {
    let prefix = partition_data_prefix(partition);
    let wanted: HashSet<&str> = inputs.iter().map(String::as_str).collect();
    let entries = store
        .list_with_sizes_blocking(Some(&prefix))
        .map_err(|e| store_io("list", &prefix, e))?;
    Ok(entries
        .iter()
        .filter(|(key, _)| wanted.contains(key.as_str()))
        .map(|(_, size)| *size)
        .sum())
}

/// Read the partition's `manifest.json` through the [`Store`], discarding the
/// `ETag`. `Ok(None)` when absent (the pre-compaction / glob-fallback case).
pub(super) fn read_manifest(
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
pub(super) fn read_manifest_etag(
    store: &Store,
    key: &str,
) -> Result<Option<String>, CompactionError> {
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
pub(super) fn no_op_outcome(files_before: usize) -> CompactionOutcome {
    CompactionOutcome {
        files_before,
        rows: 0,
        rows_dropped: 0,
        committed: None,
        gc_failures: 0,
        bytes_read: 0,
        bytes_written: 0,
    }
}

/// A committed data object: a `*.parquet` key (so `*.parquet.tmp` and
/// `manifest.json` are excluded). The writer only ever emits `<uuid>.parquet`.
pub(super) fn is_committed_parquet(key: &str) -> bool {
    key.ends_with(".parquet")
}

/// True when `key` is an **immediate** child object of `prefix`
/// (`<prefix>/<name>` with no further `/`). `Store::list*` enumerates the whole
/// subtree, but a partition's files live directly under its prefix, so — like
/// the pre-RFC-0019 `read_dir` immediate-children scan — a nested object under
/// the prefix (a foreign file or a future sidecar layout) is **not** a
/// partition input and must not be folded in.
pub(super) fn is_immediate_child(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

/// The trailing path segment of a `/`-delimited object key.
pub(super) fn basename(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// Bare file names of `keys` (for a manifest's `files` list). Keys are valid
/// UTF-8 strings, so this is infallible.
pub(super) fn basenames(keys: &[String]) -> Vec<String> {
    keys.iter().map(|k| basename(k).to_owned()).collect()
}

/// The partition's data path relative to the store root, as a `/`-delimited
/// object-key prefix (`data/tenant_id=…/year=…/month=…/day=…/hour=…`) — the same
/// address [`Writer::open_in`] publishes files under.
pub(super) fn partition_data_prefix(partition: &PartitionKey) -> String {
    partition
        .data_path(Path::new(""))
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// The partition's `manifest.json` object key.
pub(super) fn manifest_key(partition: &PartitionKey) -> String {
    format!("{}/{MANIFEST_FILENAME}", partition_data_prefix(partition))
}

/// Map a [`StoreError`] from a listing / object operation onto
/// [`CompactionError::Io`], keeping the backend cause in the error chain.
pub(super) fn store_io(op: &'static str, key: &str, source: StoreError) -> CompactionError {
    CompactionError::Io {
        op,
        path: PathBuf::from(key),
        source: std::io::Error::other(source),
    }
}
