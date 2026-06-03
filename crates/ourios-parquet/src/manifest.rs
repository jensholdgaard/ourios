//! Per-partition manifest (RFC 0009 §3.4).
//!
//! Compaction can't atomically replace a partition's many small
//! `*.parquet` files with one consolidated file on object storage
//! (no atomic multi-object operation), so a directory glob would race
//! a compaction and double-count or miss rows. The **manifest** names
//! the authoritative live set of data files in a partition plus a
//! monotonically increasing generation; the read path (RFC 0007)
//! resolves a partition's files through it, and a compaction commits
//! by atomically swapping the manifest. A partition with no manifest
//! — every partition today, pre-compaction — falls back to "all
//! committed `*.parquet`", so the manifest is additive and
//! back-compatible (RFC 0009 §3.4 reader-first sequencing: the reader
//! learns the manifest before any compactor writes one).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Canonical manifest filename inside a partition directory.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// The live data-file set of one partition (RFC 0009 §3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Monotonic generation, bumped on each atomic swap. Lets a reader
    /// confirm it read one consistent generation and a compactor
    /// detect a lost update.
    pub generation: u64,
    /// Live data files, as bare file names (no path) relative to the
    /// partition directory. A reader joins each onto the partition
    /// dir; files present on disk but *not* listed here are orphans
    /// awaiting GC and MUST be ignored.
    pub files: Vec<String>,
}

/// Failure reading, parsing, or validating a [`Manifest`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ManifestError {
    /// The manifest file existed but could not be read.
    Io(std::io::Error),
    /// The manifest bytes were not valid manifest JSON.
    Parse(serde_json::Error),
    /// A `files` entry is not a partition-local `*.parquet` file name
    /// (it is absolute, contains path separators, or escapes the
    /// partition directory via `..`). A reader joins entries onto the
    /// partition dir, so accepting such a name would let a manifest
    /// point a query at files outside the tenant's partition —
    /// breaking tenant isolation (`CLAUDE.md` §3.7 / RFC0007.5). Bad
    /// manifests fail loudly rather than silently mis-resolving.
    InvalidFilename(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "read manifest: {e}"),
            Self::Parse(e) => write!(f, "parse manifest: {e}"),
            Self::InvalidFilename(name) => {
                write!(
                    f,
                    "manifest lists a non-partition-local file name: {name:?}"
                )
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::InvalidFilename(_) => None,
        }
    }
}

/// Whether `name` is a bare partition-local `*.parquet` file name:
/// exactly one path component, that component an ordinary name (no
/// `/`, no `..`, not absolute), with a lowercase `.parquet` extension.
/// The extension match is case-*sensitive* on purpose — the writer
/// only ever emits lowercase `.parquet`, and the glob fallback
/// (`resolve_live_files`) / `ListingOptions::with_file_extension`
/// match lowercase too, so a manifest naming `*.PARQUET` would be
/// "valid" yet inconsistent with the on-disk contract.
fn is_partition_local_parquet(name: &str) -> bool {
    use std::path::Component;
    let path = Path::new(name);
    let mut components = path.components();
    let single_normal =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    single_normal && path.extension().is_some_and(|ext| ext == "parquet")
}

impl Manifest {
    /// Read `<partition_dir>/manifest.json`.
    ///
    /// `Ok(None)` when the manifest is absent — the pre-compaction
    /// (and current) case, where the reader falls back to globbing
    /// `*.parquet`. `Ok(Some(_))` when present and parseable.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] if the file exists but can't be read, or its
    /// bytes aren't valid manifest JSON.
    pub fn read(partition_dir: &Path) -> Result<Option<Self>, ManifestError> {
        match std::fs::read(partition_dir.join(MANIFEST_FILENAME)) {
            Ok(bytes) => {
                let manifest: Self =
                    serde_json::from_slice(&bytes).map_err(ManifestError::Parse)?;
                manifest.validate()?;
                Ok(Some(manifest))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ManifestError::Io(e)),
        }
    }

    /// Reject any `files` entry that is not a partition-local
    /// `*.parquet` file name (see [`ManifestError::InvalidFilename`]).
    ///
    /// # Errors
    ///
    /// [`ManifestError::InvalidFilename`] for the first offending name.
    pub fn validate(&self) -> Result<(), ManifestError> {
        for name in &self.files {
            if !is_partition_local_parquet(name) {
                return Err(ManifestError::InvalidFilename(name.clone()));
            }
        }
        Ok(())
    }

    /// Serialize to the canonical JSON bytes the compactor writes and
    /// [`read`](Self::read) parses.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if serialization fails (not expected for
    /// this plain struct).
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        // Arrange
        let manifest = Manifest {
            generation: 7,
            files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
        };

        // Act
        let restored: Manifest =
            serde_json::from_slice(&manifest.to_json().expect("serialize")).expect("parse");

        // Assert
        assert_eq!(restored, manifest);
    }

    #[test]
    fn absent_manifest_is_none() {
        // Arrange
        let dir = tempfile::tempdir().expect("temp");

        // Act
        let read = Manifest::read(dir.path());

        // Assert
        assert_eq!(read.expect("read"), None);
    }

    #[test]
    fn reads_a_written_manifest() {
        // Arrange
        let dir = tempfile::tempdir().expect("temp");
        let manifest = Manifest {
            generation: 3,
            files: vec!["compacted.parquet".to_string()],
        };
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            manifest.to_json().unwrap(),
        )
        .expect("write");

        // Act
        let read = Manifest::read(dir.path());

        // Assert
        assert_eq!(read.expect("read"), Some(manifest));
    }

    #[test]
    fn malformed_manifest_is_a_parse_error() {
        // Arrange
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(dir.path().join(MANIFEST_FILENAME), b"not json").expect("write");

        // Act
        let read = Manifest::read(dir.path());

        // Assert
        assert!(matches!(read, Err(ManifestError::Parse(_))));
    }

    #[test]
    fn accepts_plain_parquet_names() {
        // Arrange — bare partition-local names a compactor would emit.
        let names = ["a.parquet", "01890000-0000-7000-8000-000000000000.parquet"];

        // Act & Assert (table-driven over the pure predicate)
        for name in names {
            assert!(
                is_partition_local_parquet(name),
                "{name} should be accepted"
            );
        }
    }

    #[test]
    fn rejects_path_escaping_or_non_parquet_names() {
        // Arrange — names that escape the partition or aren't parquet.
        let names = [
            "../escape.parquet", // parent escape
            "/abs/x.parquet",    // absolute
            "sub/x.parquet",     // nested
            "x.txt",             // wrong extension
            "x.PARQUET",         // non-canonical (uppercase) extension
            "x",                 // no extension
            "",                  // empty
            ".",                 // current dir
        ];

        // Act & Assert (table-driven over the pure predicate)
        for name in names {
            assert!(
                !is_partition_local_parquet(name),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn read_rejects_a_path_escaping_entry() {
        // Arrange — a hostile manifest on disk (serialized directly,
        // bypassing `validate`) whose entry escapes the partition.
        let dir = tempfile::tempdir().expect("temp");
        let evil = Manifest {
            generation: 1,
            files: vec!["../../../etc/secrets.parquet".to_string()],
        };
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            serde_json::to_vec(&evil).unwrap(),
        )
        .expect("write");

        // Act
        let read = Manifest::read(dir.path());

        // Assert
        assert!(matches!(read, Err(ManifestError::InvalidFilename(_))));
    }
}
