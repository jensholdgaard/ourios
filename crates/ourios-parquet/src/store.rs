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
use std::sync::{Arc, OnceLock};

use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use tokio::runtime::Runtime;

/// The process-wide runtime that drives the async `object_store` calls behind
/// the sync storage API. Built once, lazily, via `get_or_init` so there is no
/// init-race that could drop a surplus runtime on a caller's thread (an
/// earlier manual `get`/`set` did, panicking when the loser was inside a tokio
/// runtime). The runtime lives for the process and is never dropped, so the
/// "drop a runtime in async context" hazard can't arise.
///
/// Multi-threaded(1-worker) so concurrent `block_on` from many bridge threads
/// (parallel queries / tests) is safe. No `enable_all()`: the local backend
/// drives I/O via `spawn_blocking`, which needs only the bare runtime; the S3
/// backend adds `enable_all()` plus the `net`/`io`/`time` tokio features in the
/// slice that introduces it.
fn bridge_runtime() -> Result<&'static Runtime, StoreError> {
    static RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    match RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
    }) {
        Ok(rt) => Ok(rt),
        // Build failure is cached (a permanent resource exhaustion); rebuild a
        // fresh `io::Error` since it isn't `Clone`.
        Err(e) => Err(StoreError::Runtime(std::io::Error::new(
            e.kind(),
            e.to_string(),
        ))),
    }
}

/// Drive `fut` to completion synchronously — the bridge from the **sync**
/// storage API (`Writer`, `Reader`, `compaction`, the manifest) to async
/// `object_store` (compaction must reach S3 per RFC0013.3, so a local-only
/// `std::fs` shortcut won't do).
///
/// `block_on` runs on a **fresh OS thread** (the shared [`bridge_runtime`] is
/// driven from there), not the caller's. A plain thread never carries the
/// caller's tokio context, so this is safe from any call site — including
/// *inside* a runtime (e.g. the querier resolving manifests on its async task,
/// or a `#[tokio::test]`), where `block_on` on the caller's own thread would
/// panic. [`std::thread::scope`] lets `fut` borrow the caller's `self`/`key`
/// while still running off-thread. Reusing the shared runtime keeps the
/// per-call cost to one thread spawn (no per-call runtime build), which matters
/// on the query path (`resolve_live_files` reads one manifest per partition).
///
/// `fut` already yields a [`StoreError`] result, returned directly; the extra
/// error modes are building the bridge thread or runtime
/// ([`StoreError::Runtime`]). A panic *inside* `fut` is not swallowed — it is
/// re-raised on the caller's thread via [`std::panic::resume_unwind`].
fn block_on_off_runtime<T>(
    fut: impl Future<Output = Result<T, StoreError>> + Send,
) -> Result<T, StoreError>
where
    T: Send,
{
    let rt = bridge_runtime()?;
    std::thread::scope(|s| {
        // `Builder::spawn_scoped` (not `Scope::spawn`) so OS thread-creation
        // failure surfaces as `StoreError::Runtime` rather than panicking.
        let handle = std::thread::Builder::new()
            .name("ourios-store-bridge".into())
            .spawn_scoped(s, || rt.block_on(fut))
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

impl StoreError {
    /// True if this is a "no such object" backend error — the caller may
    /// treat the object as absent (see [`Store::get_blocking_opt`]).
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Backend(object_store::Error::NotFound { .. }))
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

    /// Write `bytes` to `key` only if no object exists there
    /// (create-if-absent — `If-None-Match: *`). The local-testable half of
    /// RFC 0013 conditional PUT; the compare-and-swap half (`If-Match`) needs
    /// an S3 backend, since `LocalFileSystem` rejects `PutMode::Update`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if an object already exists at `key`, or the
    /// put otherwise fails.
    pub async fn put_if_absent(&self, key: &str, bytes: Vec<u8>) -> Result<(), StoreError> {
        self.inner
            .put_opts(
                &self.resolve(key),
                PutPayload::from(bytes),
                PutOptions::from(PutMode::Create),
            )
            .await
            .map_err(StoreError::Backend)?;
        Ok(())
    }

    /// Read the object at `key`, mapping a missing object to `None` rather
    /// than an error — for sync call sites where absence is expected (e.g. a
    /// partition with no manifest yet).
    ///
    /// # Errors
    /// As [`Self::get_blocking`], except a not-found object yields `Ok(None)`.
    pub fn get_blocking_opt(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self.get_blocking(key) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Blocking [`Self::put_if_absent`] for the sync storage call sites.
    ///
    /// # Errors
    /// As [`Self::put_if_absent`], plus [`StoreError::Runtime`] if the bridge
    /// runtime can't be built.
    pub fn put_if_absent_blocking(&self, key: &str, bytes: Vec<u8>) -> Result<(), StoreError> {
        block_on_off_runtime(self.put_if_absent(key, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::{Store, StoreError};

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

    /// `get_blocking_opt` maps a missing object to `None` (the manifest's
    /// "no manifest yet" case) and yields the bytes when present.
    #[test]
    fn get_blocking_opt_maps_missing_to_none() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        assert_eq!(
            store.get_blocking_opt("manifest.json").expect("get_opt"),
            None,
            "absent object is None, not an error"
        );
        store
            .put_blocking("manifest.json", b"{}".to_vec())
            .expect("put");
        assert_eq!(
            store.get_blocking_opt("manifest.json").expect("get_opt"),
            Some(b"{}".to_vec()),
        );
    }

    /// `put_if_absent` (create-if-absent) writes when the key is free and
    /// refuses to clobber an existing object — the local-testable half of
    /// RFC 0013 conditional PUT.
    #[test]
    fn put_if_absent_refuses_to_clobber() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "manifest.json";
        store
            .put_if_absent_blocking(key, b"first".to_vec())
            .expect("first create");
        let err = store
            .put_if_absent_blocking(key, b"second".to_vec())
            .expect_err("create over an existing object must fail");
        assert!(matches!(err, StoreError::Backend(_)), "got {err:?}");
        assert_eq!(
            store.get_blocking(key).expect("get"),
            b"first",
            "the original object is untouched"
        );
    }
}
