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
/// the first start. Creation writes a uniquely named temp file, fsyncs
/// it and publishes it with a hard link, which fails if another start
/// published first; that start's identity is then the one returned.
/// Either way `wal_root` is fsynced before returning, so an identity a
/// crashed start left visible but not yet durable is made durable too.
///
/// # Errors
///
/// [`ReplicaIdError::Io`] or [`ReplicaIdError::Malformed`] naming the
/// file when it exists but cannot be read or decoded; otherwise a
/// failure creating it or making it durable.
pub fn load_or_create(wal_root: &Path) -> Result<Identity, ReplicaIdError> {
    let identity = match load_existing(wal_root)? {
        Some(id) => Identity::Existing(id),
        None => create(
            wal_root,
            &wal_root.join(REPLICA_ID_FILE),
            ReplicaId(Uuid::now_v7()),
        )?,
    };
    snapshot_store::fsync_root(wal_root).map_err(|source| ReplicaIdError::Sync {
        path: wal_root.to_path_buf(),
        source,
    })?;
    Ok(identity)
}

/// Read this replica's identity from `wal_root`, `None` if it has none
/// yet. Nothing is created or written, not even `wal_root`.
///
/// # Errors
///
/// [`ReplicaIdError::Io`] or [`ReplicaIdError::Malformed`] naming the
/// file when it exists but cannot be read or decoded.
pub fn load_existing(wal_root: &Path) -> Result<Option<ReplicaId>, ReplicaIdError> {
    read(&wal_root.join(REPLICA_ID_FILE))
}

fn read(path: &Path) -> Result<Option<ReplicaId>, ReplicaIdError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            ReplicaId::decode(&bytes)
                .map(Some)
                .map_err(|detail| ReplicaIdError::Malformed {
                    path: path.to_path_buf(),
                    detail,
                })
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ReplicaIdError::Io {
            op: READ,
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn create(wal_root: &Path, path: &Path, id: ReplicaId) -> Result<Identity, ReplicaIdError> {
    prepare_root(wal_root)?;
    // The vote log exists before the identity does, so a later start that
    // finds `replica.id` but no `votes.log` knows the votes were lost.
    crate::replicated::raft::vote_log::create(wal_root).map_err(|source| ReplicaIdError::Io {
        op: "create(votes.log)",
        path: wal_root.join(crate::replicated::raft::vote_log::VOTES_FILE),
        source,
    })?;
    let tmp = wal_root.join(format!("{REPLICA_ID_FILE}.{}.tmp", Uuid::now_v7().simple()));
    let published = write_synced(&tmp, id).and_then(|()| publish(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    if published? {
        return Ok(Identity::Created(id));
    }
    read(path)?
        .map(Identity::Existing)
        .ok_or_else(|| ReplicaIdError::Io {
            op: READ,
            path: path.to_path_buf(),
            source: ErrorKind::NotFound.into(),
        })
}

/// Create `wal_root` and fsync every directory that made, as the WAL
/// does for its own root.
fn prepare_root(wal_root: &Path) -> Result<(), ReplicaIdError> {
    let path = wal_root.to_path_buf();
    match ourios_wal::prepare_root(wal_root) {
        Ok(()) => Ok(()),
        Err(ourios_wal::OpenError::Io { op, source }) => {
            Err(ReplicaIdError::Io { op, path, source })
        }
        Err(other) => Err(ReplicaIdError::Io {
            op: "create_dir_all",
            path,
            source: std::io::Error::other(format!("{other:?}")),
        }),
    }
}

fn write_synced(tmp: &Path, id: ReplicaId) -> Result<(), ReplicaIdError> {
    let io = |op: &'static str| {
        let path = tmp.to_path_buf();
        move |source| ReplicaIdError::Io { op, path, source }
    };
    let mut file = std::fs::File::create_new(tmp).map_err(io("create"))?;
    file.write_all(id.encode().as_bytes())
        .map_err(io("write"))?;
    file.sync_all().map_err(io("fsync"))
}

/// `true` if this start published `tmp` as `path`, `false` if another
/// start already had.
fn publish(tmp: &Path, path: &Path) -> Result<bool, ReplicaIdError> {
    match std::fs::hard_link(tmp, path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(source) => Err(ReplicaIdError::Io {
            op: "link",
            path: path.to_path_buf(),
            source,
        }),
    }
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
        assert_eq!(leftover_temp_files(&root), 0);
        assert_eq!(
            load_or_create(&root).expect("reuse"),
            Identity::Existing(created)
        );
    }

    fn leftover_temp_files(root: &Path) -> usize {
        std::fs::read_dir(root)
            .expect("list")
            .filter(|entry| {
                let name = entry.as_ref().expect("entry").file_name();
                name.to_string_lossy().ends_with(".tmp")
            })
            .count()
    }

    #[test]
    fn the_first_start_creates_the_vote_log_with_the_identity() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = dir.path().join("wal");
        load_or_create(&root).expect("create");
        let votes = crate::replicated::raft::vote_log::VoteLog::open(&root);
        assert!(votes.is_ok(), "a first start leaves an openable vote log");
    }

    #[test]
    fn a_start_that_loses_the_race_returns_the_published_identity() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = dir.path();
        let path = root.join(REPLICA_ID_FILE);
        let first = ReplicaId(Uuid::now_v7());
        let second = ReplicaId(Uuid::now_v7());
        assert_eq!(
            create(root, &path, first).expect("win"),
            Identity::Created(first)
        );
        assert_eq!(
            create(root, &path, second).expect("lose"),
            Identity::Existing(first)
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            first.encode()
        );
        assert_eq!(leftover_temp_files(root), 0);
    }

    #[test]
    fn overlapping_first_starts_agree_on_one_identity() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = dir.path().join("wal");
        let barrier = std::sync::Barrier::new(8);
        let ids: Vec<Identity> = std::thread::scope(|scope| {
            let starts: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        load_or_create(&root).expect("start")
                    })
                })
                .collect();
            starts
                .into_iter()
                .map(|s| s.join().expect("join"))
                .collect()
        });
        let created = ids
            .iter()
            .filter(|id| matches!(id, Identity::Created(_)))
            .count();
        assert_eq!(created, 1, "{ids:?}");
        let on_disk = read(&root.join(REPLICA_ID_FILE))
            .expect("read")
            .expect("present");
        assert!(ids.iter().all(|id| id.id() == on_disk), "{ids:?}");
    }

    #[test]
    fn a_missing_wal_root_and_its_ancestors_are_created() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = dir.path().join("new-parent").join("wal");
        let id = load_or_create(&root).expect("create").id();
        assert_eq!(read(&root.join(REPLICA_ID_FILE)).expect("read"), Some(id));
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

    /// Run `load` on a `wal_root` whose identity is a directory, assert it
    /// fails naming the file, and return that `wal_root`.
    fn assert_an_unreadable_identity_fails<T: std::fmt::Debug>(
        load: impl Fn(&Path) -> Result<T, ReplicaIdError>,
    ) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("temp");
        let path = dir.path().join(REPLICA_ID_FILE);
        std::fs::create_dir(&path).expect("a directory where the file belongs");
        let err = load(dir.path()).expect_err("unreadable");
        assert!(
            matches!(&err, ReplicaIdError::Io { op, .. } if *op == READ),
            "{err}"
        );
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
        dir
    }

    #[test]
    fn an_unreadable_identity_fails_naming_the_file() {
        assert_an_unreadable_identity_fails(load_or_create);
    }

    fn entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .expect("list")
            .map(|entry| {
                let name = entry.expect("entry").file_name();
                name.to_string_lossy().into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn loading_without_an_identity_creates_nothing() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = dir.path().join("wal");
        assert_eq!(load_existing(&root).expect("absent"), None);
        assert!(!root.exists(), "wal_root was created");
        assert_eq!(load_existing(dir.path()).expect("absent"), None);
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }

    #[test]
    fn loading_an_existing_identity_writes_nothing() {
        let dir = tempfile::TempDir::new().expect("temp");
        let path = dir.path().join(REPLICA_ID_FILE);
        let id = ReplicaId(Uuid::now_v7());
        std::fs::write(&path, id.encode()).expect("write");
        assert_eq!(load_existing(dir.path()).expect("present"), Some(id));
        assert_eq!(entries(dir.path()), [REPLICA_ID_FILE]);
    }

    #[test]
    fn loading_a_malformed_identity_fails_naming_the_file_and_keeps_it() {
        let dir = tempfile::TempDir::new().expect("temp");
        let path = dir.path().join(REPLICA_ID_FILE);
        std::fs::write(&path, b"garbage").expect("write");
        let err = load_existing(dir.path()).expect_err("malformed");
        assert!(matches!(err, ReplicaIdError::Malformed { .. }), "{err}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
        assert_eq!(std::fs::read(&path).expect("kept"), b"garbage");
        assert_eq!(entries(dir.path()), [REPLICA_ID_FILE]);
    }

    #[test]
    fn loading_an_unreadable_identity_fails_naming_the_file() {
        let dir = assert_an_unreadable_identity_fails(load_existing);
        assert_eq!(entries(dir.path()), [REPLICA_ID_FILE]);
    }
}
