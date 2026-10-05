//! The seated marker (RFC 0059 §3.5): a root's record that its snapshots
//! were written under template-id reservations.
//!
//! Snapshots a root wrote before it first seated against the high-water
//! hold ids from its pre-RFC counter, which another replica may have
//! issued since. A root without the marker therefore trusts its snapshots
//! only when its own start is the bootstrap that folds them into the
//! floor; otherwise every artefact is discarded before the marker is
//! written, so the marker never vouches for one of them.

use std::io::Write as _;
use std::path::Path;

use ourios_parquet::Store;

use super::{TemplateIdsError, read};
use crate::snapshot_store;

/// The marker's file name in the snapshots root. Not a `*.snap`, so the
/// snapshot loader never lists it.
pub const SEATED_MARKER: &str = "TEMPLATE_IDS_SEATED";

/// Whether a root's snapshot artefacts may be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotTrust {
    /// The root has seated before: every artefact was written under
    /// reservations.
    Seated,
    /// No high-water exists yet: this start bootstraps it and folds the
    /// restored ids into the floor.
    Bootstrap,
    /// The root never seated, but the store has: every artefact predates
    /// the high-water and is discarded.
    PredatesHighWater,
}

impl SnapshotTrust {
    /// Decide for the root at `snapshots_root` over `store`.
    ///
    /// # Errors
    ///
    /// [`TemplateIdsError`] when the marker cannot be checked or the
    /// high-water cannot be read; startup fails closed.
    pub fn of(snapshots_root: &Path, store: &Store) -> Result<Self, TemplateIdsError> {
        // Only a file is a marker: anything else at its path vouches for
        // nothing.
        match std::fs::metadata(snapshots_root.join(SEATED_MARKER)) {
            Ok(metadata) if metadata.is_file() => return Ok(Self::Seated),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(TemplateIdsError::Marker {
                    op: "stat(seated marker)",
                    source,
                });
            }
        }
        Ok(match read(store)? {
            Some(_) => Self::PredatesHighWater,
            None => Self::Bootstrap,
        })
    }

    /// Whether the artefacts restore.
    #[must_use]
    pub fn restores(self) -> bool {
        matches!(self, Self::Seated | Self::Bootstrap)
    }
}

/// Record that the root at `snapshots_root` seated above `high_water`:
/// temp file, fsync, rename, then fsync the root and its parent, the
/// snapshot store's own discipline.
///
/// # Errors
///
/// [`TemplateIdsError::Marker`] on any filesystem failure.
pub fn mark_seated(snapshots_root: &Path, high_water: u64) -> Result<(), TemplateIdsError> {
    let io = |op: &'static str| move |source| TemplateIdsError::Marker { op, source };
    std::fs::create_dir_all(snapshots_root).map_err(io("create_dir_all(snapshots root)"))?;
    let tmp = snapshots_root.join(format!("{SEATED_MARKER}.tmp"));
    let mut file = std::fs::File::create(&tmp).map_err(io("create(seated marker tmp)"))?;
    file.write_all(format!("{high_water}\n").as_bytes())
        .map_err(io("write(seated marker tmp)"))?;
    file.sync_all().map_err(io("fsync(seated marker tmp)"))?;
    std::fs::rename(&tmp, snapshots_root.join(SEATED_MARKER))
        .map_err(io("rename(seated marker)"))?;
    snapshot_store::fsync_root(snapshots_root).map_err(TemplateIdsError::Snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template_ids::{HIGH_WATER_KEY, encode};

    #[test]
    fn the_marker_decides_between_trust_bootstrap_and_discard() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path().join("snapshots");
        let store = Store::in_memory();
        assert_eq!(
            SnapshotTrust::of(&root, &store).expect("trust"),
            SnapshotTrust::Bootstrap
        );
        store.put_blocking(HIGH_WATER_KEY, encode(9)).expect("put");
        assert_eq!(
            SnapshotTrust::of(&root, &store).expect("trust"),
            SnapshotTrust::PredatesHighWater
        );
        mark_seated(&root, 9).expect("mark");
        assert_eq!(
            SnapshotTrust::of(&root, &store).expect("trust"),
            SnapshotTrust::Seated
        );
    }
}
