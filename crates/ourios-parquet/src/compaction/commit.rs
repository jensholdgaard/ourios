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
/// a `*.parquet` object **not** named by it is dead — a superseded input the
/// post-commit GC never reached, or a compaction output (`<uuid>.g<N>.parquet`)
/// whose generation *N* the manifest has already reached without naming it —
/// and any `*.parquet.tmp` is an interrupted publish (absent on S3). Both are
/// safe to unlink. An output for a generation past the manifest's is kept: its
/// compaction may still commit it (RFC0009.17). With **no** manifest the glob
/// is the live set, so no `*.parquet` is an orphan and only stray
/// `*.parquet.tmp` are reclaimed.
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
    let manifest = read_manifest(store, partition)?;
    let keys = store
        .list_blocking(Some(&prefix))
        .map_err(|e| store_io("list", &prefix, e))?;
    let mut gc = OrphanGc::default();
    let orphans = keys
        .iter()
        .filter(|object| is_orphan(basename(object), manifest.as_ref()));
    for object in orphans {
        match unlink(store, object) {
            Unlink::Removed => gc.reclaimed += 1,
            Unlink::AlreadyGone => {}
            Unlink::Failed => gc.failures += 1,
        }
    }
    Ok(gc)
}

/// What deleting one orphan did.
enum Unlink {
    Removed,
    /// Not found: already reclaimed (S3 `DELETE` is idempotent; the local
    /// backend reports not-found).
    AlreadyGone,
    /// Left for a later pass.
    Failed,
}

fn unlink(store: &Store, object: &str) -> Unlink {
    match store.delete_blocking(object) {
        Ok(()) => Unlink::Removed,
        Err(e) if e.is_not_found() => Unlink::AlreadyGone,
        Err(_) => Unlink::Failed,
    }
}

/// Whether the object `name` in a partition is dead under `manifest`.
///
/// `.parquet.tmp` is always a dead interrupted publish. A `.parquet` is an
/// orphan only when a manifest names a set that excludes it (no manifest ⇒
/// glob ⇒ every `.parquet` is live), and a compaction output written for a
/// generation the manifest has not reached yet is kept: its compaction may
/// still be about to commit it (RFC0009.17). Anything else
/// (`manifest.json`, a future sidecar) is not ours.
fn is_orphan(name: &str, manifest: Option<&Manifest>) -> bool {
    if name.ends_with(".parquet.tmp") {
        return true;
    }
    let Some(manifest) = manifest else {
        return false;
    };
    let in_flight =
        compacted_generation(name).is_some_and(|generation| generation > manifest.generation);
    name.ends_with(".parquet") && !in_flight && !manifest.files.iter().any(|f| f == name)
}
