//! Per-tenant snapshot artefacts on local disk, WAL-adjacent
//! (RFC 0001 §6.9: the snapshot is a rebuildable
//! recovery-acceleration cache, not durable state — local disk is
//! legitimate because nothing the snapshot accelerates outlives the
//! WAL horizon, `CLAUDE.md` §3.6).
//!
//! One `<tenant>.snap` file per tenant under the snapshots root
//! (`<wal_root>/snapshots/`). The tenant id is percent-encoded into
//! the file stem with the same codec the Parquet store uses for
//! tenant directories ([`percent_encode_tenant`]), so any tenant id
//! is a safe single path segment and the stem decodes back without a
//! tenant field inside the payload.
//!
//! Writes are atomic (`.tmp` → write → fsync → rename → fsync parent
//! dir, mirroring the WAL's `CHECKPOINT` sidecar persistence) so a
//! crash mid-write leaves either the previous artefact or the new
//! one, never a torn file.

use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::Path;

use ourios_core::tenant::TenantId;
use ourios_miner::snapshot::{SnapshotError, SnapshotState, snapshot};
use ourios_parquet::{percent_decode_tenant, percent_encode_tenant};
use uuid::Uuid;

const EXTENSION: &str = "snap";

/// Failure writing or listing snapshot artefacts.
#[derive(Debug)]
#[non_exhaustive]
pub enum SnapshotStoreError {
    /// Filesystem I/O failure; `op` names the failing step.
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// The snapshot payload failed to encode ([`snapshot`]).
    Encode(SnapshotError),
}

impl std::fmt::Display for SnapshotStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "snapshot store I/O during {op}: {source}"),
            Self::Encode(e) => write!(f, "snapshot store encode: {e}"),
        }
    }
}

impl std::error::Error for SnapshotStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Encode(e) => Some(e),
        }
    }
}

/// Atomically persist one tenant's [`SnapshotState`] as
/// `<root>/<percent-encoded tenant>.snap`, creating `root` if absent.
/// Overwrites any previous artefact for the tenant — recovery loads
/// the latest snapshot per tenant (RFC 0001 §6.9 *Scope*).
///
/// # Errors
///
/// [`SnapshotStoreError::Encode`] if the payload fails to encode;
/// [`SnapshotStoreError::Io`] on any filesystem failure.
pub fn write(
    root: &Path,
    tenant: &TenantId,
    state: &SnapshotState,
) -> Result<(), SnapshotStoreError> {
    let io = |op: &'static str| move |source| SnapshotStoreError::Io { op, source };
    let bytes = snapshot(state).map_err(SnapshotStoreError::Encode)?;
    std::fs::create_dir_all(root).map_err(io("create_dir_all(snapshots root)"))?;
    let stem = percent_encode_tenant(tenant.as_str());
    // RFC 0052 §3.1: a **unique** temp name per attempt. One fixed
    // `.snap.tmp` could be truncated or interleaved by a concurrent
    // writer before either rename, and the two would install each
    // other's bytes — or one would unlink the temp the other was still
    // writing. The cut's mark does not make it unique (two writers can
    // carry the same one), so the name carries a `UUIDv7` instead.
    let tmp = root.join(format!(
        "{stem}.{}.{EXTENSION}.tmp",
        Uuid::now_v7().simple()
    ));
    let written = write_and_install(&tmp, root, &stem, &bytes);
    if written.is_err() {
        // The writer unlinks its own temp on any write, fsync or rename
        // failure, so a failing barrier cannot leak one file per attempt.
        drop(std::fs::remove_file(&tmp));
    }
    written?;
    File::open(root)
        .and_then(|dir| dir.sync_all())
        .map_err(io("fsync(snapshots root)"))?;
    Ok(())
}

fn write_and_install(
    tmp: &Path,
    root: &Path,
    stem: &str,
    bytes: &[u8],
) -> Result<(), SnapshotStoreError> {
    let io = |op: &'static str| move |source| SnapshotStoreError::Io { op, source };
    let mut file = File::create(tmp).map_err(io("create(snapshot tmp)"))?;
    file.write_all(bytes).map_err(io("write(snapshot tmp)"))?;
    file.sync_all().map_err(io("fsync(snapshot tmp)"))?;
    std::fs::rename(tmp, root.join(format!("{stem}.{EXTENSION}")))
        .map_err(io("rename(snapshot tmp -> snapshot)"))?;
    Ok(())
}

/// Fsync the snapshots root **and its parent** (RFC0052.13).
///
/// Startup calls this before any listed artefact is read as a horizon.
/// A failure is startup's to raise, not to swallow: reclamation may
/// already have removed the frames the listed snapshots cover, so a
/// horizon whose directory entry may not be durable must never govern
/// reclamation — and discarding the snapshots instead would throw away
/// state nothing else can rebuild.
///
/// An absent root is a cold start, not a failure.
///
/// # Errors
///
/// [`SnapshotStoreError::Io`] when either fsync fails.
pub fn fsync_root(root: &Path) -> Result<(), SnapshotStoreError> {
    let io = |op: &'static str| move |source| SnapshotStoreError::Io { op, source };
    match File::open(root) {
        // Fsyncing a *file* proves nothing about a directory entry, so
        // a root that is not a directory is a failed step rather than a
        // silently successful one — and it is the shape the
        // file-in-place-of-directory fixture produces (§6).
        Ok(handle) if !handle.metadata().is_ok_and(|meta| meta.is_dir()) => {
            return Err(SnapshotStoreError::Io {
                op: "fsync(snapshots root)",
                source: std::io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{} is not a directory", root.display()),
                ),
            });
        }
        Ok(dir) => dir.sync_all().map_err(io("fsync(snapshots root)"))?,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(SnapshotStoreError::Io {
                op: "open(snapshots root)",
                source,
            });
        }
    }
    let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(io("fsync(snapshots root parent)"))?;
    Ok(())
}

/// Remove every `*.snap.tmp` a previous process left, then list the
/// durable artefacts ([`load_all`]).
///
/// A temp file is a writer's private scratch: unlinking its own is the
/// writer's job while it lives, and this is what closes the gap left by
/// one that died mid-write.
///
/// # Errors
///
/// [`SnapshotStoreError::Io`] on any directory or file failure.
pub fn load_all_durable(root: &Path) -> Result<Vec<(TenantId, Vec<u8>)>, SnapshotStoreError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SnapshotStoreError::Io {
                op: "read_dir(snapshots root)",
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| SnapshotStoreError::Io {
            op: "read_dir entry",
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "tmp") {
            drop(std::fs::remove_file(&path));
        }
    }
    load_all(root)
}

/// List every snapshot artefact under `root`, decoding the tenant id
/// from each `*.snap` file stem. Returns the raw artefact bytes —
/// version dispatch belongs to `ourios_miner::snapshot::recover`. An
/// absent root is an empty store (cold start), not an error. Files
/// whose stem is not a canonical percent-encoding are not Ourios
/// artefacts and are skipped, like the compactor's store sweep skips
/// foreign directory names. Sorted by tenant for a deterministic
/// recovery order.
///
/// # Errors
///
/// [`SnapshotStoreError::Io`] on any directory or file read failure.
pub fn load_all(root: &Path) -> Result<Vec<(TenantId, Vec<u8>)>, SnapshotStoreError> {
    let io = |op: &'static str| move |source| SnapshotStoreError::Io { op, source };
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SnapshotStoreError::Io {
                op: "read_dir(snapshots root)",
                source,
            });
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io("read_dir entry"))?;
        // A directory named `*.snap` is not an artefact; skip it
        // rather than fail recovery on the read below.
        if !entry
            .file_type()
            .map_err(io("file_type(snapshot entry)"))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|e| e != EXTENSION) {
            continue;
        }
        let Some(tenant) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(percent_decode_tenant)
        else {
            continue;
        };
        let bytes = std::fs::read(&path).map_err(io("read(snapshot)"))?;
        out.push((TenantId::new(tenant), bytes));
    }
    out.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_miner::snapshot::{
        LeafRecord, StructuredTemplateRecord, TokenRecord, WalHighWater, load_snapshot,
    };

    fn state(template_id: u64) -> SnapshotState {
        SnapshotState {
            leaves: vec![LeafRecord {
                template: vec![
                    TokenRecord::Fixed("user".to_string()),
                    TokenRecord::Fixed("logged".to_string()),
                ],
                template_id,
                template_version: 1,
                severity_number: 9,
                scope_name: None,
                slot_types: vec![],
                provenance: vec![],
                upstream_associations: vec![],
                upstream_association_overflow: 0,
            }],
            structured_templates: vec![StructuredTemplateRecord {
                severity_number: 17,
                scope_name: None,
                event_name: None,
                template_id: template_id + 1,
            }],
            wal_high_water: Some(WalHighWater {
                segment: "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0".to_string(),
                byte: 64,
            }),
            adopted_templates: vec![],
        }
    }

    #[test]
    fn write_then_load_all_round_trips_including_encoded_tenants() {
        // Arrange — one plain tenant and one whose id needs
        // percent-encoding to be a single path segment.
        let tmp = tempfile::TempDir::new().expect("temp");
        let plain = TenantId::new("checkout");
        let spicy = TenantId::new("acme/EU=prod");

        // Act
        write(tmp.path(), &plain, &state(1)).expect("write plain");
        write(tmp.path(), &spicy, &state(3)).expect("write spicy");
        let loaded = load_all(tmp.path()).expect("load_all");

        // Assert — both tenants come back (sorted), each decoding to
        // the state written for it.
        let tenants: Vec<&str> = loaded.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(tenants, vec!["acme/EU=prod", "checkout"]);
        for (tenant, bytes) in &loaded {
            let decoded = load_snapshot(bytes).expect("artefact decodes");
            let expected = match tenant.as_str() {
                "checkout" => state(1),
                _ => state(3),
            };
            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn load_all_on_an_absent_dir_is_an_empty_store() {
        // Arrange — a path that does not exist (cold start).
        let tmp = tempfile::TempDir::new().expect("temp");
        let absent = tmp.path().join("snapshots");

        // Act + Assert
        assert!(load_all(&absent).expect("absent dir is empty").is_empty());
    }

    #[test]
    fn write_atomically_overwrites_the_previous_artefact() {
        // Arrange — an existing artefact for the tenant.
        let tmp = tempfile::TempDir::new().expect("temp");
        let tenant = TenantId::new("checkout");
        write(tmp.path(), &tenant, &state(1)).expect("first write");

        // Act — overwrite with a newer state.
        write(tmp.path(), &tenant, &state(7)).expect("overwrite");

        // Assert — exactly one artefact survives, carrying the newer
        // state, and no `.tmp` residue is left behind.
        let loaded = load_all(tmp.path()).expect("load_all");
        assert_eq!(loaded.len(), 1);
        assert_eq!(load_snapshot(&loaded[0].1).expect("decodes"), state(7));
        let residue: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(residue.is_empty(), "no tmp residue, got {residue:?}");
    }

    #[test]
    fn load_all_skips_foreign_files() {
        // Arrange — a non-`.snap` file and a `.snap` whose stem is
        // not a canonical percent-encoding (a space the encoder would
        // have escaped).
        let tmp = tempfile::TempDir::new().expect("temp");
        std::fs::write(tmp.path().join("README.md"), b"not a snapshot").expect("write");
        std::fs::write(tmp.path().join("not ours.snap"), b"junk").expect("write");
        write(tmp.path(), &TenantId::new("checkout"), &state(1)).expect("write");

        // Act + Assert
        let loaded = load_all(tmp.path()).expect("load_all");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.as_str(), "checkout");
    }

    #[test]
    fn load_all_skips_non_file_snap_entries() {
        // Arrange — a *directory* named like an artefact next to a
        // valid one; reading it as a file would abort recovery.
        let tmp = tempfile::TempDir::new().expect("temp");
        std::fs::create_dir(tmp.path().join("junk.snap")).expect("mkdir");
        write(tmp.path(), &TenantId::new("checkout"), &state(1)).expect("write");

        // Act + Assert
        let loaded = load_all(tmp.path()).expect("load_all");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.as_str(), "checkout");
    }
}
