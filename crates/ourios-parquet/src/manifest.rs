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

use crate::store::{Store, StoreError};

/// Map a [`StoreError`] from the manifest seam onto [`ManifestError::Io`],
/// keeping the backend cause in the error chain.
fn store_io(err: StoreError) -> ManifestError {
    ManifestError::Io(std::io::Error::other(err))
}

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
    /// A file name appears more than once. The compactor joins each
    /// entry to the partition dir and reads it, so a duplicate would
    /// read the same file twice and double-count its rows — the live
    /// set must be unique.
    DuplicateFilename(String),
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
            Self::DuplicateFilename(name) => {
                write!(f, "manifest lists a duplicate file name: {name:?}")
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::InvalidFilename(_) | Self::DuplicateFilename(_) => None,
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
    /// Read `<partition_dir>/manifest.json` through the object-storage
    /// [`Store`] seam (RFC 0013).
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
        // A missing partition directory means no manifest (matches the prior
        // `std::fs` NotFound→None). Checked up front because `Store::local`
        // canonicalises its root and would error on an absent directory.
        if !partition_dir.try_exists().map_err(ManifestError::Io)? {
            return Ok(None);
        }
        let store = Store::local(partition_dir).map_err(store_io)?;
        match store
            .get_blocking_opt(MANIFEST_FILENAME)
            .map_err(store_io)?
        {
            Some(bytes) => {
                let manifest: Self =
                    serde_json::from_slice(&bytes).map_err(ManifestError::Parse)?;
                manifest.validate()?;
                Ok(Some(manifest))
            }
            None => Ok(None),
        }
    }

    /// Validate the live set: every entry is a partition-local
    /// `*.parquet` name and no name is repeated.
    ///
    /// # Errors
    ///
    /// [`ManifestError::InvalidFilename`] for the first non-local name,
    /// or [`ManifestError::DuplicateFilename`] for the first repeat.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut seen = std::collections::HashSet::with_capacity(self.files.len());
        for name in &self.files {
            if !is_partition_local_parquet(name) {
                return Err(ManifestError::InvalidFilename(name.clone()));
            }
            if !seen.insert(name.as_str()) {
                return Err(ManifestError::DuplicateFilename(name.clone()));
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

    /// Atomically (re)write the partition's manifest through the
    /// object-storage [`Store`] seam (RFC 0013). The `Store` put is the
    /// **commit point** (RFC 0009 §3.4): the local backend stages to a
    /// private temp object and renames it into place, so a *concurrent
    /// reader* observes either the old manifest or the new one, never a
    /// partial write. The manifest is validated before any bytes are
    /// written, so an invalid set is never published.
    ///
    /// On the local backend this is last-writer-wins `Overwrite`, atomic but
    /// **not** crash-durable (no `fsync`) — the same contract as before the
    /// seam, and the same caveat as the `Writer`. That is safe by
    /// construction: a reader with no manifest falls back to the `*.parquet`
    /// glob, and a compaction that crashed before this commit left its inputs
    /// intact, so the worst case is reverting to the prior generation, never
    /// data loss. Compare-and-swap (generation CAS, `If-Match`) lands with the
    /// S3 backend — `LocalFileSystem` does not support conditional update.
    ///
    /// # Errors
    ///
    /// [`ManifestError::InvalidFilename`] if any entry isn't a
    /// partition-local `*.parquet` name; [`ManifestError::Io`] on a write
    /// failure.
    pub fn write_atomic(&self, partition_dir: &Path) -> Result<(), ManifestError> {
        self.validate()?;
        let bytes = self.to_json().map_err(ManifestError::Parse)?;
        let store = Store::local(partition_dir).map_err(store_io)?;
        store
            .put_blocking(MANIFEST_FILENAME, bytes)
            .map_err(store_io)?;
        Ok(())
    }

    /// Read the manifest at `key` in `store` together with its `ETag` — the
    /// compare-and-swap token for [`Self::publish_cas`]. `Ok(None)` when
    /// absent (the pre-compaction case). The `ETag` is `None` only if the
    /// backend doesn't expose one (S3-compatible stores do).
    ///
    /// # Errors
    ///
    /// [`ManifestError::Io`] on a read failure, [`ManifestError::Parse`] /
    /// validation errors on a corrupt manifest.
    pub fn read_with_etag(
        store: &Store,
        key: &str,
    ) -> Result<Option<(Self, Option<String>)>, ManifestError> {
        match store.get_with_etag_blocking_opt(key).map_err(store_io)? {
            Some((bytes, e_tag)) => {
                let manifest: Self =
                    serde_json::from_slice(&bytes).map_err(ManifestError::Parse)?;
                manifest.validate()?;
                Ok(Some((manifest, e_tag)))
            }
            None => Ok(None),
        }
    }

    /// Publish this manifest to `key` in `store` as a generation swap via
    /// **conditional PUT** — no `rename` anywhere on the path (RFC0013.4).
    /// `expected` is the `ETag` of the generation being replaced (from
    /// [`Self::read_with_etag`]); pass `None` for the first publish, which
    /// create-if-absents instead. Returns [`Published::Lost`] when another
    /// writer published first — the create/compare-and-swap precondition
    /// failed (RFC0013.3); the caller re-reads and retries or no-ops.
    ///
    /// Requires a backend that supports conditional update (S3-compatible);
    /// `LocalFileSystem` does not (use [`Self::write_atomic`] there).
    ///
    /// # Errors
    ///
    /// [`ManifestError::InvalidFilename`] / [`ManifestError::DuplicateFilename`]
    /// if the live set is invalid; [`ManifestError::Io`] on a non-precondition
    /// backend failure.
    pub fn publish_cas(
        &self,
        store: &Store,
        key: &str,
        expected: Option<&str>,
    ) -> Result<Published, ManifestError> {
        self.validate()?;
        let bytes = self.to_json().map_err(ManifestError::Parse)?;
        let result = match expected {
            None => store.put_if_absent_blocking(key, bytes),
            Some(e_tag) => store.put_if_match_blocking(key, bytes, e_tag),
        };
        match result {
            Ok(()) => Ok(Published::Won),
            Err(e) if e.is_precondition() || e.is_already_exists() => Ok(Published::Lost),
            Err(e) => Err(store_io(e)),
        }
    }
}

/// Outcome of a compare-and-swap manifest publish ([`Manifest::publish_cas`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// This writer's generation was published.
    Won,
    /// Another writer published first (the create / compare-and-swap
    /// precondition failed). The caller re-reads the new generation and
    /// retries, or no-ops if that generation already covers its work.
    Lost,
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

    #[test]
    fn write_atomic_round_trips_and_overwrites() {
        // Arrange
        let dir = tempfile::tempdir().expect("temp");
        let first = Manifest {
            generation: 1,
            files: vec!["a.parquet".to_string()],
        };
        let second = Manifest {
            generation: 2,
            files: vec!["compacted.parquet".to_string()],
        };
        first.write_atomic(dir.path()).expect("write first");

        // Act — the second write swaps the manifest in place.
        second.write_atomic(dir.path()).expect("write second");

        // Assert — the latest generation wins, no `.tmp` left behind.
        assert_eq!(Manifest::read(dir.path()).expect("read"), Some(second));
        assert!(!dir.path().join(format!("{MANIFEST_FILENAME}.tmp")).exists());
    }

    #[test]
    fn write_atomic_rejects_an_invalid_entry_before_writing() {
        // Arrange
        let dir = tempfile::tempdir().expect("temp");
        let bad = Manifest {
            generation: 1,
            files: vec!["../escape.parquet".to_string()],
        };

        // Act
        let result = bad.write_atomic(dir.path());

        // Assert — rejected, and nothing was published.
        assert!(matches!(result, Err(ManifestError::InvalidFilename(_))));
        assert!(!dir.path().join(MANIFEST_FILENAME).exists());
    }

    #[test]
    fn validate_rejects_a_duplicate_file_name() {
        // Arrange — the same file named twice would double-count.
        let manifest = Manifest {
            generation: 1,
            files: vec!["a.parquet".to_string(), "a.parquet".to_string()],
        };

        // Act
        let result = manifest.validate();

        // Assert
        assert!(matches!(result, Err(ManifestError::DuplicateFilename(_))));
    }
}
