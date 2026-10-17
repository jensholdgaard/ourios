//! Manifest commit (backend-appropriate atomic swap) and orphan GC.
//! Split from the flat `compaction.rs` (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path — types, siblings, external crates — resolving
// unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

/// The manifest a commit replaces.
#[derive(Clone, Copy)]
pub(super) enum Base<'e> {
    /// None yet: the commit creates the first one.
    Absent,
    /// The manifest read before the rewrite, with its `ETag` (`None` if the
    /// backend exposes none).
    Current(Option<&'e str>),
}

/// Commit `manifest` to `key` in `store` with the backend-appropriate atomic
/// swap (CLAUDE.md §3.5 / RFC0009.3 — no torn read either way):
///
/// - **First commit (any backend):** a create-if-absent conditional PUT
///   ([`Manifest::publish_cas`] with no `ETag`); a peer that created the
///   manifest first makes it [`Published::Lost`] (RFC 0009 §3.4).
/// - **CAS-capable backend (S3, RFC0019.4):** an `If-Match` conditional PUT
///   against the base's `ETag`; a lost race returns [`Published::Lost`].
/// - **Local backend (RFC0019.7):** `LocalFileSystem` rejects
///   `PutMode::Update`, so a later commit is an atomic overwrite (it stages to
///   a temp object and renames it into place — the same swap
///   [`Manifest::write_atomic`] performed pre-RFC-0019). Last-writer-wins; one
///   compactor per local store (RFC0009.24), so it always wins.
pub(super) fn commit_manifest(
    store: &Store,
    key: &str,
    manifest: &Manifest,
    base: Base<'_>,
) -> Result<Published, CompactionError> {
    let expected = match base {
        Base::Absent => None,
        Base::Current(etag) if store.supports_conditional_update() => etag,
        Base::Current(_) => return overwrite_manifest(store, key, manifest),
    };
    manifest
        .publish_cas(store, key, expected)
        .map_err(CompactionError::Manifest)
}

/// The local backend's last-writer-wins commit over an existing manifest.
fn overwrite_manifest(
    store: &Store,
    key: &str,
    manifest: &Manifest,
) -> Result<Published, CompactionError> {
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
/// not name. Under the RFC 0009 §3.4 live-set rule ([`LiveSet::is_orphan`])
/// the dead files are: a flush file the manifest lists as consumed (an input
/// the post-commit GC never reached), a compaction output
/// (`<uuid>.g<N>.parquet`) whose generation *N* the manifest has reached
/// without naming it, and any `*.parquet.tmp` (an interrupted publish, absent
/// on S3). An unconsumed flush file is never an orphan — a late write is live
/// — and an output for a generation past the manifest's is kept, since its
/// compaction may still commit it (RFC0009.17).
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
    let live = LiveSet::new(manifest.as_ref());
    let mut gc = OrphanGc::default();
    let orphans = keys
        .iter()
        .filter(|object| live.is_orphan(basename(object)));
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
