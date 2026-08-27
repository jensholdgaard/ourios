//! Manifest commit (backend-appropriate atomic swap) and orphan GC.
//! Split from the flat `compaction.rs` (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path — types, siblings, external crates — resolving
// unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

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
pub(super) fn commit_manifest(
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
