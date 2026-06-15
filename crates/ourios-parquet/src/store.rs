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

use std::future::Future;
use std::sync::Arc;

use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

/// Drive `fut` to completion synchronously — the bridge from the **sync**
/// storage API (`Writer`, `Reader`, `compaction`, the manifest) to async
/// `object_store` (compaction must reach S3 per RFC0013.3, so a local-only
/// `std::fs` shortcut won't do).
///
/// Everything happens on a **fresh OS thread**: a single-threaded runtime is
/// built, drives `fut`, and is dropped, all on a thread that never carries the
/// caller's tokio context. That makes the bridge safe from any call site —
/// including *inside* a runtime (a `#[tokio::test]` that opens the reader, or
/// a future async consumer), where calling `block_on` (or dropping a runtime)
/// on the caller's own thread would panic. [`std::thread::scope`] lets `fut`
/// borrow the caller's `self`/`key` while still running off-thread.
///
/// `fut` already yields a [`StoreError`] result, returned directly; the extra
/// error modes are building the bridge thread or its runtime
/// ([`StoreError::Runtime`]). A panic *inside* `fut` is not swallowed — it is
/// re-raised on the caller's thread via [`std::panic::resume_unwind`]. No
/// `enable_all()`: the local backend drives I/O via `spawn_blocking`, which
/// needs only the bare runtime; the S3 backend adds `enable_all()` and the
/// `net`/`io`/`time` tokio features in the slice that introduces it.
fn block_on_off_runtime<T>(
    fut: impl Future<Output = Result<T, StoreError>> + Send,
) -> Result<T, StoreError>
where
    T: Send,
{
    std::thread::scope(|s| {
        // `Builder::spawn_scoped` (not `Scope::spawn`) so OS thread-creation
        // failure surfaces as `StoreError::Runtime` rather than panicking.
        let handle = std::thread::Builder::new()
            .name("ourios-store-bridge".into())
            .spawn_scoped(s, || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .map_err(StoreError::Runtime)?;
                rt.block_on(fut)
            })
            .map_err(StoreError::Runtime)?;
        handle
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

/// A handle to the object store backing a tenant store's Parquet + manifest
/// objects, addressed by key under `prefix`. Wraps an [`ObjectStore`] so the
/// same code path targets `LocalFileSystem` or `AmazonS3` / S3-compatible.
///
/// **`red` caveat:** `prefix` is reserved and currently always empty, and
/// [`Store::object_store`] returns the raw backend with **no prefix
/// scoping**. Per-tenant/prefix isolation (RFC0013.5) is wired at `green` —
/// do **not** assume this type enforces isolation yet.
#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    /// Reserved key prefix (the store root). Always empty at `red`; honoured
    /// once the consumers migrate onto [`Store`] at `green`.
    prefix: ObjectPath,
}

/// Configuration for the S3 / S3-compatible backend (RFC0013.7). Populated
/// from RFC 0004 config at `green`; a placeholder here so the `red`
/// constructor signature is stable.
///
/// `Default` is a `red` placeholder only — it yields an **empty `bucket`**,
/// which is not valid; callers must set a non-empty `bucket` (the `green`
/// `s3()` will reject an empty one).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct S3Config {
    /// Bucket name (required; the empty `Default` is a `red` placeholder).
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
    /// The sync→async bridge thread or runtime could not be built (resource
    /// exhaustion). Surfaced by the `*_blocking` methods.
    Runtime(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "object-store backend: {e}"),
            Self::Unimplemented(what) => write!(f, "not implemented (RFC 0013 red): {what}"),
            Self::Runtime(e) => write!(f, "object-store bridge runtime: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(e) => Some(e),
            Self::Unimplemented(_) => None,
            Self::Runtime(e) => Some(e),
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
    // `red` stub: `cfg` is unused until the `green` AmazonS3 impl consumes
    // it (`needless_pass_by_value` fires because we never read it). The
    // signature is fixed now so consumers can be written against it.
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

    /// Resolve a `/`-delimited `key` to an absolute object path under the
    /// store prefix. At `red` the prefix is empty, so this is just the key;
    /// once prefix scoping is wired (RFC0013.5) the prefix segments lead.
    fn resolve(&self, key: &str) -> ObjectPath {
        self.prefix
            .parts()
            .chain(ObjectPath::from(key).parts())
            .collect()
    }

    /// Write `bytes` to `key`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the put fails.
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), StoreError> {
        self.inner
            .put(&self.resolve(key), PutPayload::from(bytes))
            .await
            .map_err(StoreError::Backend)?;
        Ok(())
    }

    /// Read the whole object at `key`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the object is missing or the read fails.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let got = self
            .inner
            .get(&self.resolve(key))
            .await
            .map_err(StoreError::Backend)?;
        let bytes = got.bytes().await.map_err(StoreError::Backend)?;
        Ok(bytes.to_vec())
    }

    /// Delete the object at `key`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the delete fails.
    pub async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.inner
            .delete(&self.resolve(key))
            .await
            .map_err(StoreError::Backend)
    }

    /// Blocking [`Self::get`] for the **sync** storage call sites (`Reader`,
    /// compaction). Safe to call from any thread, including inside a tokio
    /// runtime — the `block_on` runs off the caller's thread.
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::get`].
    pub fn get_blocking(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        block_on_off_runtime(self.get(key))
    }

    /// Blocking [`Self::put`] for the **sync** storage call sites (`Writer`,
    /// compaction). Safe to call from inside a tokio runtime (see
    /// [`Self::get_blocking`]).
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::put`].
    pub fn put_blocking(&self, key: &str, bytes: Vec<u8>) -> Result<(), StoreError> {
        block_on_off_runtime(self.put(key, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::Store;

    /// A byte object round-trips through the local backend, and a delete
    /// removes it. (Foundation for the RFC0013 consumer migration; the §5
    /// scenarios turn green as the writer/reader move onto `Store`.)
    #[tokio::test(flavor = "current_thread")]
    async fn local_store_put_get_delete_round_trip() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "data/tenant_id=t/year=2026/x.parquet";
        store.put(key, b"hello-ourios".to_vec()).await.expect("put");
        assert_eq!(store.get(key).await.expect("get"), b"hello-ourios");
        store.delete(key).await.expect("delete");
        assert!(store.get(key).await.is_err(), "object gone after delete");
    }

    /// The sync `*_blocking` bridge round-trips a byte object — the path the
    /// sync `Writer` / `Reader` / compaction take onto `Store`. Runs on a
    /// plain test thread (no ambient runtime), exercising `block_on`.
    #[test]
    fn blocking_bridge_put_get_round_trip() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "data/tenant_id=t/year=2026/x.parquet";
        store
            .put_blocking(key, b"hello-blocking".to_vec())
            .expect("put_blocking");
        assert_eq!(
            store.get_blocking(key).expect("get_blocking"),
            b"hello-blocking"
        );
    }

    /// The `*_blocking` bridge is safe to call from *within* a tokio runtime —
    /// some consumers (e.g. a `#[tokio::test]` that reads back via `Reader`)
    /// do exactly that. The `block_on` runs off the caller's thread, so it
    /// must not panic "runtime within a runtime".
    #[tokio::test(flavor = "current_thread")]
    async fn blocking_bridge_is_safe_inside_a_runtime() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "data/tenant_id=t/year=2026/x.parquet";
        store
            .put_blocking(key, b"inside-runtime".to_vec())
            .expect("put_blocking");
        assert_eq!(
            store.get_blocking(key).expect("get_blocking"),
            b"inside-runtime"
        );
    }
}
