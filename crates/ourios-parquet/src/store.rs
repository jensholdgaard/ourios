//! Object-storage backend (RFC 0013) — the seam behind the writer, reader,
//! compaction, and audit sink so the RFC 0005 data + audit Parquet and the
//! RFC 0009 manifest live on local disk (dev/test) or an S3-compatible
//! bucket (production), without changing the on-disk layout.
//!
//! **Status: `red` (RFC 0013).** This is the skeleton the `green` work fills:
//! the [`Store`] type and its constructors exist and the `LocalFileSystem`
//! backend is wired, but the S3 backend, the conditional-PUT atomic publish
//! (RFC0013.3/.4), and the migration of the writer/reader/compaction/audit
//! consumers from `bucket_root: &Path` onto [`Store`] are not done. The §5
//! acceptance scenarios are encoded as `#[ignore]`d stubs in
//! `tests/rfc0013_object_store.rs` and turn green as the backend lands.
//!
//! Per RFC 0013 §3.7 the backend is a **module here in `ourios-parquet`**
//! (not a new crate): `ourios-querier`, `-ingester`, and `-server` already
//! depend on this crate, so the type is visible to every storage consumer.

use std::sync::Arc;

use object_store::ObjectStore;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;

/// A handle to the object store backing a tenant store's Parquet + manifest
/// objects, addressed by key under `prefix`. Wraps an [`ObjectStore`] so the
/// same code path targets `LocalFileSystem` or `AmazonS3` / S3-compatible.
#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
}

/// Configuration for the S3 / S3-compatible backend (RFC0013.7). Populated
/// from RFC 0004 config at `green`; a placeholder here so the `red`
/// constructor signature is stable.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct S3Config {
    /// Bucket name.
    pub bucket: String,
    /// Optional endpoint override for S3-compatible stores (`MinIO`, R2, …).
    pub endpoint: Option<String>,
    /// Region (AWS) — ignored by some S3-compatible stores.
    pub region: Option<String>,
    /// Key prefix within the bucket (the store root).
    pub prefix: Option<String>,
}

/// Errors from constructing or addressing a [`Store`].
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// Backend construction failed (bad root, credentials, endpoint, …).
    Backend(object_store::Error),
    /// A backend constructor not yet implemented at this RFC 0013 stage.
    /// Returned (rather than panicking) so an accidental call fails
    /// gracefully while `red`.
    Unimplemented(&'static str),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "object-store backend: {e}"),
            Self::Unimplemented(what) => write!(f, "not implemented (RFC 0013 red): {what}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(e) => Some(e),
            Self::Unimplemented(_) => None,
        }
    }
}

impl Store {
    /// Local-filesystem backend rooted at `root` (dev / test / CI). Preserves
    /// today's on-disk layout — the RFC 0005 Hive keys become paths under
    /// `root`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if `root` cannot be opened as an
    /// `object_store` `LocalFileSystem` (e.g. it does not exist).
    pub fn local(root: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let fs = LocalFileSystem::new_with_prefix(root).map_err(StoreError::Backend)?;
        Ok(Self {
            inner: Arc::new(fs),
            prefix: ObjectPath::default(),
        })
    }

    /// S3 / S3-compatible backend (RFC0013.1/.4/.7).
    ///
    /// `red`: not yet built — the `green` implementation constructs an
    /// `object_store::aws::AmazonS3` (behind the `aws` feature) from `cfg`
    /// and the RFC 0004 credentials.
    ///
    /// # Errors
    /// At `red`, always [`StoreError::Unimplemented`] — returned rather than
    /// panicking so an accidental call (e.g. from another workspace crate)
    /// fails gracefully. At `green` this becomes [`StoreError::Backend`] if
    /// the `object_store` `AmazonS3` backend cannot be constructed (bad
    /// endpoint, credentials, or bucket).
    #[allow(clippy::needless_pass_by_value, unused_variables)]
    pub fn s3(cfg: S3Config) -> Result<Self, StoreError> {
        // RFC0013 green: build AmazonS3 from cfg + RFC 0004 creds.
        Err(StoreError::Unimplemented(
            "RFC0013 green: AmazonS3 / S3-compatible backend",
        ))
    }

    /// The underlying [`ObjectStore`], for handing to `DataFusion`'s table
    /// providers on the read path (RFC 0013 §2.2 — the querier registers the
    /// same store rather than local file paths).
    #[must_use]
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.inner)
    }

    /// The store's root key prefix.
    #[must_use]
    pub fn prefix(&self) -> &ObjectPath {
        &self.prefix
    }
}
