//! The seated marker (RFC 0059 §3.5): a root's record that its snapshots
//! were written under template-id reservations.
//!
//! Snapshots a root wrote before it first seated against the high-water
//! hold ids from its pre-RFC counter, which another replica may have
//! issued since. A root without the marker therefore trusts its snapshots
//! only when its own start is the bootstrap that folds them into the
//! floor; otherwise every artefact is discarded before the marker is
//! written, so the marker never vouches for one of them.
//!
//! The marker also records the highest reservation the root has seen
//! (`max_reserved_seen`), raised before each new block is used. A start
//! that finds the high-water below it fails closed: the object was
//! rolled back to an older copy, which RFC 0059 §3.1 does not support.

use std::io::Write as _;
use std::path::Path;

use ourios_parquet::Store;

use super::{TemplateIdsError, read};
use crate::snapshot_store;

/// The marker's file name in the snapshots root. Not a `*.snap`, so the
/// snapshot loader never lists it. Its body is JSON
/// `{"version": 1, "seated_above": N, "max_reserved_seen": M}`, with
/// `N <= M` (RFC 0059 §3.5).
pub const SEATED_MARKER: &str = "TEMPLATE_IDS_SEATED";
const MARKER_VERSION: u64 = 1;

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
    /// [`TemplateIdsError`] when the high-water cannot be read, and fails
    /// closed on a marker that is unreadable or malformed
    /// ([`TemplateIdsError::MarkerInvalid`]); on a seated root whose
    /// high-water is gone ([`TemplateIdsError::HighWaterDeleted`]), since a
    /// bootstrap then would see only published ids and could reseat below
    /// a block another receiver still holds; and on a high-water below a
    /// reservation this root already saw
    /// ([`TemplateIdsError::HighWaterRolledBack`]).
    pub fn of(snapshots_root: &Path, store: &Store) -> Result<Self, TemplateIdsError> {
        let marker = read_marker(snapshots_root)?;
        match (marker, read(store)?) {
            (Some(_), None) => Err(TemplateIdsError::HighWaterDeleted),
            (Some(marker), Some(high_water))
                if high_water.reserved_through < marker.max_reserved_seen =>
            {
                Err(TemplateIdsError::HighWaterRolledBack {
                    seen: marker.max_reserved_seen,
                    found: high_water.reserved_through,
                })
            }
            (Some(_), Some(_)) => Ok(Self::Seated),
            (None, Some(_)) => Ok(Self::PredatesHighWater),
            (None, None) => Ok(Self::Bootstrap),
        }
    }

    /// Whether the artefacts restore.
    #[must_use]
    pub fn restores(self) -> bool {
        matches!(self, Self::Seated | Self::Bootstrap)
    }
}

/// A root's seated marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Marker {
    /// The high-water the root first seated above.
    pub(super) seated_above: u64,
    /// The highest reservation the root has made usable.
    pub(super) max_reserved_seen: u64,
}

/// The root's marker, or `None` when it has none.
pub(super) fn read_marker(snapshots_root: &Path) -> Result<Option<Marker>, TemplateIdsError> {
    let bytes = match std::fs::read(snapshots_root.join(SEATED_MARKER)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TemplateIdsError::Marker {
                op: "read(seated marker)",
                source,
            });
        }
    };
    let invalid = |detail: String| TemplateIdsError::MarkerInvalid { detail };
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
    let field = |name: &str| {
        let value = body
            .get(name)
            .ok_or_else(|| invalid(format!("no `{name}`")))?;
        value
            .as_u64()
            .ok_or_else(|| invalid(format!("`{name}` is {value}, not a u64")))
    };
    match body.get("version").and_then(serde_json::Value::as_u64) {
        Some(MARKER_VERSION) => {}
        version => {
            return Err(invalid(format!(
                "version {version:?}, not {MARKER_VERSION}"
            )));
        }
    }
    let marker = Marker {
        seated_above: field("seated_above")?,
        max_reserved_seen: field("max_reserved_seen")?,
    };
    match marker {
        Marker {
            seated_above,
            max_reserved_seen,
        } if seated_above > max_reserved_seen => Err(invalid(format!(
            "seated above {seated_above}, past the {max_reserved_seen} it has seen"
        ))),
        marker => Ok(Some(marker)),
    }
}

/// Record that the root at `snapshots_root` seated above `high_water`,
/// having seen no reservation above it.
///
/// # Errors
///
/// [`TemplateIdsError::Marker`] on any filesystem failure.
pub fn mark_seated(snapshots_root: &Path, high_water: u64) -> Result<(), TemplateIdsError> {
    write_marker(
        snapshots_root,
        Marker {
            seated_above: high_water,
            max_reserved_seen: high_water,
        },
    )
}

/// Write `marker` durably: temp file, fsync, rename, then fsync the root
/// and its parent, the snapshot store's own discipline.
pub(super) fn write_marker(snapshots_root: &Path, marker: Marker) -> Result<(), TemplateIdsError> {
    let io = |op: &'static str| move |source| TemplateIdsError::Marker { op, source };
    std::fs::create_dir_all(snapshots_root).map_err(io("create_dir_all(snapshots root)"))?;
    let tmp = snapshots_root.join(format!("{SEATED_MARKER}.tmp"));
    let mut file = std::fs::File::create(&tmp).map_err(io("create(seated marker tmp)"))?;
    let body = serde_json::json!({
        "version": MARKER_VERSION,
        "seated_above": marker.seated_above,
        "max_reserved_seen": marker.max_reserved_seen,
    });
    file.write_all(body.to_string().as_bytes())
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

    #[test]
    fn a_seated_root_whose_high_water_is_gone_fails_closed() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path().join("snapshots");
        mark_seated(&root, 9).expect("mark");
        let err = SnapshotTrust::of(&root, &Store::in_memory()).expect_err("deleted");
        assert!(matches!(err, TemplateIdsError::HighWaterDeleted), "{err}");
    }

    #[test]
    fn every_unusable_marker_fails_closed() {
        let store = Store::in_memory();
        store.put_blocking(HIGH_WATER_KEY, encode(9)).expect("put");
        for body in [
            &b""[..],
            br#"{"version": 1, "seated_ab"#,
            b"9\n",
            br#"{"version": 1}"#,
            br#"{"version": 2, "seated_above": 3}"#,
            br#"{"version": 1, "seated_above": -3}"#,
            br#"{"version": 1, "seated_above": 10}"#,
            br#"{"version": 1, "seated_above": 3}"#,
            br#"{"version": 1, "seated_above": 3, "max_reserved_seen": "9"}"#,
            br#"{"version": 1, "seated_above": 5, "max_reserved_seen": 4}"#,
        ] {
            let tmp = tempfile::TempDir::new().expect("temp");
            let root = tmp.path().join("snapshots");
            std::fs::create_dir_all(&root).expect("root");
            std::fs::write(root.join(SEATED_MARKER), body).expect("marker");
            let err = SnapshotTrust::of(&root, &store).expect_err("never trusted");
            assert!(
                matches!(err, TemplateIdsError::MarkerInvalid { .. }),
                "{body:?}: {err}"
            );
        }
    }

    #[test]
    fn a_high_water_below_what_the_root_saw_is_a_rollback() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path().join("snapshots");
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(3_000))
            .expect("put");
        write_marker(
            &root,
            Marker {
                seated_above: 0,
                max_reserved_seen: 3_000,
            },
        )
        .expect("mark");
        assert_eq!(
            SnapshotTrust::of(&root, &store).expect("current"),
            SnapshotTrust::Seated
        );
        store
            .put_blocking(HIGH_WATER_KEY, encode(2_999))
            .expect("stale copy");
        let err = SnapshotTrust::of(&root, &store).expect_err("rolled back");
        assert!(
            matches!(
                err,
                TemplateIdsError::HighWaterRolledBack {
                    seen: 3_000,
                    found: 2_999
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn the_marker_round_trips_both_fields() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let marker = Marker {
            seated_above: 7,
            max_reserved_seen: 2_007,
        };
        write_marker(tmp.path(), marker).expect("write");
        assert_eq!(read_marker(tmp.path()).expect("read"), Some(marker));
    }

    #[test]
    fn a_directory_at_the_marker_path_fails_closed() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path().join("snapshots");
        std::fs::create_dir_all(root.join(SEATED_MARKER)).expect("dir");
        let err = SnapshotTrust::of(&root, &Store::in_memory()).expect_err("unreadable");
        assert!(matches!(err, TemplateIdsError::Marker { .. }), "{err}");
    }
}
