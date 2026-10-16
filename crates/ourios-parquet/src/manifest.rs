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

use std::collections::HashSet;
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

/// The manifest format this build reads and writes (RFC 0009 §3.4
/// late-write note). Any other format is rejected.
pub const MANIFEST_FORMAT: u32 = 2;

/// What compaction committed in one partition (RFC 0009 §3.4, late-write
/// note): the live compacted files and the flush files a won commit
/// consumed. Every other flush file in the partition is live, so a late
/// write is read without the writer touching the manifest; see
/// [`LiveSet`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_FORMAT`]. Absent in a manifest written before it, which
    /// is then rejected.
    #[serde(default)]
    pub format: u32,
    /// Monotonic generation, bumped on each atomic swap. Lets a reader
    /// confirm it read one consistent generation and a compactor
    /// detect a lost update.
    pub generation: u64,
    /// Live compacted files (`<uuid>.g<N>.parquet`), as bare file names
    /// relative to the partition directory. A compacted file present on
    /// disk but not listed here is never live.
    pub files: Vec<String>,
    /// Flush files a won commit copied into a compacted file. They are
    /// dead and awaiting GC. Append-only: a commit never drops a name.
    #[serde(default)]
    pub consumed: Vec<String>,
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
    /// The manifest is not [`MANIFEST_FORMAT`]: written before the RFC 0009
    /// late-write amendment, whose unnamed files cannot be told apart. No
    /// fallback reads it.
    UnsupportedFormat(u32),
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
            Self::UnsupportedFormat(found) => write!(
                f,
                "manifest format {found} is not supported (expected {MANIFEST_FORMAT})"
            ),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::InvalidFilename(_) | Self::DuplicateFilename(_) | Self::UnsupportedFormat(_) => {
                None
            }
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
    /// A [`MANIFEST_FORMAT`] manifest at `generation`.
    #[must_use]
    pub fn new(generation: u64, files: Vec<String>, consumed: Vec<String>) -> Self {
        Self {
            format: MANIFEST_FORMAT,
            generation,
            files,
            consumed,
        }
    }

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
            Some(bytes) => Self::from_json(&bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Parse and [`validate`](Self::validate) manifest JSON bytes fetched
    /// by the caller (for a reader that accounts for what it fetched).
    ///
    /// # Errors
    ///
    /// [`ManifestError::Parse`] if the bytes aren't manifest JSON, or the
    /// [`validate`](Self::validate) errors.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(ManifestError::Parse)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate the manifest: it is [`MANIFEST_FORMAT`], every `files`
    /// entry is a partition-local compacted file name, every `consumed`
    /// entry a partition-local flush file name, and no name is repeated.
    ///
    /// # Errors
    ///
    /// [`ManifestError::UnsupportedFormat`] for any other format,
    /// [`ManifestError::InvalidFilename`] for the first name of the wrong
    /// form, or [`ManifestError::DuplicateFilename`] for the first repeat.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.format != MANIFEST_FORMAT {
            return Err(ManifestError::UnsupportedFormat(self.format));
        }
        let mut seen = HashSet::with_capacity(self.files.len() + self.consumed.len());
        let entries = self
            .files
            .iter()
            .map(|name| (name, true))
            .chain(self.consumed.iter().map(|name| (name, false)));
        for (name, compacted) in entries {
            let right_form = is_partition_local_parquet(name)
                && compacted_generation(name).is_some() == compacted;
            if !right_form {
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
            Some((bytes, e_tag)) => Ok(Some((Self::from_json(&bytes)?, e_tag))),
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
    /// if the live set is invalid; [`ManifestError::Parse`] if serialization
    /// fails; [`ManifestError::Io`] on a non-precondition backend failure.
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

/// The manifest generation a compaction output was written for, parsed
/// from its name `<uuid>.g<generation>.parquet` (RFC 0009 §3.4 late-write
/// note). `None` for any other name, a flush file `<uuid>.parquet`
/// included. Only the canonical forms the writer emits parse: a
/// lowercase hyphenated UUID and a generation without sign or leading
/// zeros.
#[must_use]
pub fn compacted_generation(name: &str) -> Option<u64> {
    let (uuid, digits) = name.strip_suffix(".parquet")?.rsplit_once(".g")?;
    let canonical_uuid =
        uuid::Uuid::try_parse(uuid).is_ok_and(|parsed| parsed.hyphenated().to_string() == uuid);
    let generation: u64 = digits.parse().ok()?;
    (canonical_uuid && digits == generation.to_string()).then_some(generation)
}

/// The RFC 0009 §3.4 live-set rule for one partition, indexed once from its
/// manifest (or its absence). A compacted file (`<uuid>.g<N>.parquet`) is
/// live only when the manifest names it; any other `*.parquet` is a flush
/// file and is live unless the manifest lists it as consumed. With no
/// manifest, every flush file is live and no compacted file is.
#[derive(Debug)]
pub struct LiveSet<'m> {
    generation: u64,
    files: HashSet<&'m str>,
    consumed: HashSet<&'m str>,
}

impl<'m> LiveSet<'m> {
    /// Index `manifest`; `None` is a partition with no manifest.
    #[must_use]
    pub fn new(manifest: Option<&'m Manifest>) -> Self {
        let names = |list: Option<&'m Vec<String>>| {
            list.into_iter()
                .flatten()
                .map(String::as_str)
                .collect::<HashSet<_>>()
        };
        Self {
            generation: manifest.map_or(0, |m| m.generation),
            files: names(manifest.map(|m| &m.files)),
            consumed: names(manifest.map(|m| &m.consumed)),
        }
    }

    /// Whether the listed object named `name` holds live rows.
    #[must_use]
    pub fn is_live(&self, name: &str) -> bool {
        match compacted_generation(name) {
            Some(_) => self.files.contains(name),
            None => name.ends_with(".parquet") && !self.consumed.contains(name),
        }
    }

    /// Whether the listed object named `name` is dead and safe to delete:
    /// an interrupted `*.parquet.tmp`, a consumed flush file, or a compacted
    /// file the manifest does not name whose generation the manifest has
    /// reached. An unnamed compacted file for a later generation is kept,
    /// since its compaction may still commit it (RFC0009.17); a flush file
    /// is never an orphan unless consumed.
    #[must_use]
    pub fn is_orphan(&self, name: &str) -> bool {
        if name.ends_with(".parquet.tmp") {
            return true;
        }
        match compacted_generation(name) {
            Some(generation) => generation <= self.generation && !self.files.contains(name),
            None => self.consumed.contains(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacted_generation_parses_only_the_written_form() {
        let uuid = uuid::Uuid::now_v7();
        assert_eq!(compacted_generation(&format!("{uuid}.g7.parquet")), Some(7));
        assert_eq!(compacted_generation(&format!("{uuid}.g0.parquet")), Some(0));
        for name in [
            format!("{uuid}.parquet"),
            format!("{uuid}.g07.parquet"),
            format!("{uuid}.g+7.parquet"),
            format!("{uuid}.g.parquet"),
            format!("{uuid}.g7.parquet.tmp"),
            format!("{}.g7.parquet", uuid.simple()),
            format!(
                "{}.g7.parquet",
                uuid.hyphenated().to_string().to_uppercase()
            ),
            "manifest.json".to_owned(),
            "x.g7.parquet".to_owned(),
        ] {
            assert_eq!(compacted_generation(&name), None, "{name}");
        }
    }

    /// A compacted-file name for generation `n`.
    fn compacted(n: u64) -> String {
        format!("{}.g{n}.parquet", uuid::Uuid::now_v7())
    }

    /// A flush-file name.
    fn flush() -> String {
        format!("{}.parquet", uuid::Uuid::now_v7())
    }

    #[test]
    fn round_trips_through_json() {
        // Arrange
        let manifest = Manifest::new(7, vec![compacted(7), compacted(6)], vec![flush(), flush()]);

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
        let manifest = Manifest::new(3, vec![compacted(3)], Vec::new());
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
        let evil = Manifest::new(
            1,
            vec!["../../../etc/secrets.parquet".to_string()],
            Vec::new(),
        );
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
        let consumed = vec![flush()];
        let first = Manifest::new(1, Vec::new(), Vec::new());
        let second = Manifest::new(2, vec![compacted(2)], consumed);
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
        let bad = Manifest::new(1, vec!["../escape.parquet".to_string()], Vec::new());

        // Act
        let result = bad.write_atomic(dir.path());

        // Assert — rejected, and nothing was published.
        assert!(matches!(result, Err(ManifestError::InvalidFilename(_))));
        assert!(!dir.path().join(MANIFEST_FILENAME).exists());
    }

    #[test]
    fn validate_rejects_a_duplicate_file_name() {
        // Arrange — the same file named twice would double-count.
        let name = compacted(1);
        let manifest = Manifest::new(1, vec![name.clone(), name], Vec::new());

        // Act
        let result = manifest.validate();

        // Assert
        assert!(matches!(result, Err(ManifestError::DuplicateFilename(_))));
    }

    /// RFC0009.19 — a manifest written before the late-write amendment (no
    /// `format`) is rejected, and so is any other format.
    #[test]
    fn rfc0009_19_an_old_format_manifest_is_rejected() {
        let old = format!(r#"{{"generation":2,"files":["{}"]}}"#, flush());
        assert!(matches!(
            Manifest::from_json(old.as_bytes()),
            Err(ManifestError::UnsupportedFormat(0))
        ));
        let future = Manifest {
            format: MANIFEST_FORMAT + 1,
            ..Manifest::new(1, Vec::new(), Vec::new())
        };
        assert!(matches!(
            future.validate(),
            Err(ManifestError::UnsupportedFormat(3))
        ));
    }

    #[test]
    fn validate_keeps_each_list_to_its_file_class() {
        let flush_in_files = Manifest::new(1, vec![flush()], Vec::new());
        let compacted_in_consumed = Manifest::new(1, Vec::new(), vec![compacted(1)]);
        let name = flush();
        let twice = Manifest::new(1, Vec::new(), vec![name.clone(), name]);
        assert!(matches!(
            flush_in_files.validate(),
            Err(ManifestError::InvalidFilename(_))
        ));
        assert!(matches!(
            compacted_in_consumed.validate(),
            Err(ManifestError::InvalidFilename(_))
        ));
        assert!(matches!(
            twice.validate(),
            Err(ManifestError::DuplicateFilename(_))
        ));
    }

    #[test]
    fn live_set_reads_named_compacted_and_unconsumed_flush_files() {
        let (named, unnamed, consumed, late) = (compacted(2), compacted(1), flush(), flush());
        let manifest = Manifest::new(2, vec![named.clone()], vec![consumed.clone()]);
        let live = LiveSet::new(Some(&manifest));
        let verdicts: Vec<_> = [&named, &unnamed, &consumed, &late]
            .iter()
            .map(|name| live.is_live(name))
            .collect();
        assert_eq!(verdicts, [true, false, false, true]);

        let none = LiveSet::new(None);
        assert_eq!((none.is_live(&late), none.is_live(&named)), (true, false));
    }

    #[test]
    fn live_set_orphans_only_dead_files() {
        let (named, past, future, consumed, late) =
            (compacted(2), compacted(2), compacted(3), flush(), flush());
        let manifest = Manifest::new(2, vec![named.clone()], vec![consumed.clone()]);
        let live = LiveSet::new(Some(&manifest));
        let tmp = format!("{late}.tmp");
        let verdicts: Vec<_> = [&named, &past, &future, &consumed, &late, &tmp]
            .iter()
            .map(|name| live.is_orphan(name))
            .collect();
        assert_eq!(verdicts, [false, true, false, true, false, true]);

        let none = LiveSet::new(None);
        assert_eq!(
            (
                none.is_orphan(&late),
                none.is_orphan(&future),
                none.is_orphan(&tmp)
            ),
            (false, false, true)
        );
    }
}
