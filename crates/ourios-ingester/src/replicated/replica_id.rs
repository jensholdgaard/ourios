//! A replica's identity on its own disk (RFC 0065 §6.1, B4, B6).
//!
//! The first replicated start on a `wal_root` without [`REPLICA_ID_FILE`]
//! writes a fresh `UUIDv7` there; every later start reads it back. A file
//! that cannot be read or does not hold one fails startup rather than
//! being replaced: a node never rejoins under an identity whose vote it
//! may have lost, and joining as a new replica is its own step.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use uuid::{Uuid, Version};

use crate::snapshot_store::{self, SnapshotStoreError};

/// The identity file's name under `wal_root`.
pub const REPLICA_ID_FILE: &str = "replica.id";

/// This replica's identity, its Raft node id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaId(Uuid);

impl ReplicaId {
    #[must_use]
    pub fn uuid(self) -> Uuid {
        self.0
    }

    /// The file's bytes: the hyphenated lowercase UUID and a newline.
    fn encode(self) -> String {
        format!("{}\n", self.0.hyphenated())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        let uuid = text
            .strip_suffix('\n')
            .ok_or("no trailing newline")
            .and_then(|s| Uuid::try_parse(s).map_err(|_| "not a UUID"))?;
        match uuid.get_version() {
            Some(Version::SortRand) if Self(uuid).encode() == text => Ok(Self(uuid)),
            Some(Version::SortRand) => Err("not in canonical form".to_owned()),
            _ => Err("not a UUIDv7".to_owned()),
        }
    }
}

impl std::fmt::Display for ReplicaId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// Whether a start created its identity or found one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Created(ReplicaId),
    Existing(ReplicaId),
}

impl Identity {
    #[must_use]
    pub fn id(self) -> ReplicaId {
        match self {
            Self::Created(id) | Self::Existing(id) => id,
        }
    }
}

/// Why the identity could not be read or written.
#[derive(Debug)]
pub enum ReplicaIdError {
    /// A filesystem step failed.
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file exists but does not hold an identity.
    Malformed { path: PathBuf, detail: String },
    /// `wal_root` could not be made durable after the write.
    Sync {
        path: PathBuf,
        source: SnapshotStoreError,
    },
}

impl std::fmt::Display for ReplicaIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, path, source } if *op == READ => write!(
                f,
                "{} is unreadable ({source}); a replica never restarts under a regenerated \
                 identity (RFC 0065 §6.1)",
                path.display()
            ),
            Self::Io { op, path, source } => write!(f, "{op} {}: {source}", path.display()),
            Self::Malformed { path, detail } => write!(
                f,
                "{} does not hold a replica identity ({detail}); a replica never restarts \
                 under a regenerated identity (RFC 0065 §6.1)",
                path.display()
            ),
            Self::Sync { path, source } => {
                write!(f, "make {} durable: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ReplicaIdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sync { source, .. } => Some(source),
            Self::Malformed { .. } => None,
        }
    }
}

const READ: &str = "read";

/// Read this replica's identity from `wal_root`, or create it there on
/// the first start: temp file, fsync, rename, then fsync `wal_root` and
/// its parent.
///
/// # Errors
///
/// [`ReplicaIdError::Io`] or [`ReplicaIdError::Malformed`] naming the
/// file when it exists but cannot be read or decoded; otherwise a
/// failure creating it.
pub fn load_or_create(wal_root: &Path) -> Result<Identity, ReplicaIdError> {
    let path = wal_root.join(REPLICA_ID_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => ReplicaId::decode(&bytes)
            .map(Identity::Existing)
            .map_err(|detail| ReplicaIdError::Malformed { path, detail }),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            create(wal_root, &path, ReplicaId(Uuid::now_v7())).map(Identity::Created)
        }
        Err(source) => Err(ReplicaIdError::Io {
            op: READ,
            path,
            source,
        }),
    }
}

fn create(wal_root: &Path, path: &Path, id: ReplicaId) -> Result<ReplicaId, ReplicaIdError> {
    let io = |op: &'static str, at: &Path| {
        let path = at.to_path_buf();
        move |source| ReplicaIdError::Io { op, path, source }
    };
    let tmp = wal_root.join(format!("{REPLICA_ID_FILE}.tmp"));
    std::fs::create_dir_all(wal_root).map_err(io("create_dir_all", wal_root))?;
    let mut file = std::fs::File::create(&tmp).map_err(io("create", &tmp))?;
    file.write_all(id.encode().as_bytes())
        .map_err(io("write", &tmp))?;
    file.sync_all().map_err(io("fsync", &tmp))?;
    std::fs::rename(&tmp, path).map_err(io("rename", path))?;
    snapshot_store::fsync_root(wal_root).map_err(|source| ReplicaIdError::Sync {
        path: wal_root.to_path_buf(),
        source,
    })?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_start_creates_and_later_starts_reuse() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = dir.path().join("wal");
        let created = match load_or_create(&root).expect("create") {
            Identity::Created(id) => id,
            other @ Identity::Existing(_) => panic!("expected a new identity, got {other:?}"),
        };
        assert_eq!(created.uuid().get_version(), Some(Version::SortRand));
        let on_disk = std::fs::read_to_string(root.join(REPLICA_ID_FILE)).expect("read");
        assert_eq!(on_disk, format!("{created}\n"));
        assert!(!root.join("replica.id.tmp").exists());
        assert_eq!(
            load_or_create(&root).expect("reuse"),
            Identity::Existing(created)
        );
    }

    #[test]
    fn the_file_format_is_pinned() {
        let id = ReplicaId::decode(b"01890a5d-ac96-774b-bcce-b302099a8057\n").expect("decode");
        assert_eq!(id.encode(), "01890a5d-ac96-774b-bcce-b302099a8057\n");
    }

    #[test]
    fn a_malformed_identity_fails_naming_the_file_and_is_kept() {
        let cases: [&[u8]; 5] = [
            b"",
            b"not a uuid\n",
            b"01890a5d-ac96-774b-bcce-b302099a8057",
            b"01890A5D-AC96-774B-BCCE-B302099A8057\n",
            b"6ba7b810-9dad-41d1-80b4-00c04fd430c8\n",
        ];
        for bytes in cases {
            let dir = tempfile::TempDir::new().expect("temp");
            let path = dir.path().join(REPLICA_ID_FILE);
            std::fs::write(&path, bytes).expect("write");
            let err = load_or_create(dir.path()).expect_err("malformed");
            assert!(matches!(err, ReplicaIdError::Malformed { .. }), "{err}");
            assert!(
                err.to_string().contains(&path.display().to_string()),
                "{err}"
            );
            assert_eq!(std::fs::read(&path).expect("kept"), bytes);
        }
    }

    #[test]
    fn an_unreadable_identity_fails_naming_the_file() {
        let dir = tempfile::TempDir::new().expect("temp");
        let path = dir.path().join(REPLICA_ID_FILE);
        std::fs::create_dir(&path).expect("a directory where the file belongs");
        let err = load_or_create(dir.path()).expect_err("unreadable");
        assert!(
            matches!(&err, ReplicaIdError::Io { op, .. } if *op == READ),
            "{err}"
        );
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }
}
