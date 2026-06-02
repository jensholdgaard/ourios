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

/// Failure reading or parsing a [`Manifest`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ManifestError {
    /// The manifest file existed but could not be read.
    Io(std::io::Error),
    /// The manifest bytes were not valid manifest JSON.
    Parse(serde_json::Error),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "read manifest: {e}"),
            Self::Parse(e) => write!(f, "parse manifest: {e}"),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
        }
    }
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
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(ManifestError::Parse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ManifestError::Io(e)),
        }
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
        let m = Manifest {
            generation: 7,
            files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
        };
        let bytes = m.to_json().expect("serialize");
        let back: Manifest = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(m, back);
    }

    #[test]
    fn absent_manifest_is_none() {
        let dir = tempfile::tempdir().expect("temp");
        assert_eq!(Manifest::read(dir.path()).expect("read"), None);
    }

    #[test]
    fn reads_a_written_manifest() {
        let dir = tempfile::tempdir().expect("temp");
        let m = Manifest {
            generation: 3,
            files: vec!["compacted.parquet".to_string()],
        };
        std::fs::write(dir.path().join(MANIFEST_FILENAME), m.to_json().unwrap()).expect("write");
        assert_eq!(Manifest::read(dir.path()).expect("read"), Some(m));
    }

    #[test]
    fn malformed_manifest_is_a_parse_error() {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(dir.path().join(MANIFEST_FILENAME), b"not json").expect("write");
        assert!(matches!(
            Manifest::read(dir.path()),
            Err(ManifestError::Parse(_))
        ));
    }
}
