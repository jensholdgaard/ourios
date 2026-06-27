//! Object-storage backend (RFC 0013) — the seam behind the writer, reader,
//! compaction, and audit sink so the RFC 0005 data + audit Parquet and the
//! RFC 0009 manifest live on local disk (dev/test) or an S3-compatible
//! bucket (production), without changing the on-disk layout.
//!
//! **Status: `green`, in progress (RFC 0013).** Landed: the [`Store`] type,
//! both backends ([`Store::local`] and [`Store::s3`] — S3-compatible via an
//! endpoint override), the byte I/O surface (async `put`/`get`/`delete` plus
//! the sync `*_blocking` bridge), create-if-absent conditional PUT
//! ([`Store::put_if_absent`]), and the reader + manifest consumers reading and
//! writing through the seam. Still to come: the manifest generation-swap CAS
//! (`If-Match`, RFC0013.3/.4), the writer's migration onto the seam, and the
//! live S3 acceptance tests (RFC0013.1/.7). The §5 scenarios are `#[ignore]`d
//! stubs in `tests/rfc0013_object_store.rs` and turn green as each lands.
//!
//! Per RFC 0013 §3.7 the backend is a **module here in `ourios-parquet`**
//! (not a new crate): `ourios-querier`, `-ingester`, and `-server` already
//! depend on this crate, so the type is visible to every storage consumer.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use futures::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion,
};
use tokio::runtime::Runtime;

/// The process-wide runtime that drives the async `object_store` calls behind
/// the sync storage API. Built once, lazily, via `get_or_init` so there is no
/// init-race that could drop a surplus runtime on a caller's thread (an
/// earlier manual `get`/`set` did, panicking when the loser was inside a tokio
/// runtime). The runtime lives for the process and is never dropped, so the
/// "drop a runtime in async context" hazard can't arise.
///
/// Multi-threaded(1-worker) so concurrent `block_on` from many bridge threads
/// (parallel queries / tests) is safe. `enable_all()` so the runtime carries
/// the IO + time drivers the `AmazonS3` backend's HTTP client needs; the local
/// backend uses only `spawn_blocking` and ignores them.
fn bridge_runtime() -> Result<&'static Runtime, StoreError> {
    static RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    match RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            // `enable_all` so the runtime carries the IO + time drivers the
            // `AmazonS3` backend's HTTP client (reqwest/hyper) needs; the local
            // backend ignores them.
            .enable_all()
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

/// Object bytes paired with the backend's `ETag` (the compare-and-swap token),
/// as returned by [`Store::get_with_etag`]. The `ETag` is `None` when the
/// backend doesn't expose one.
pub type EtaggedBytes = (Vec<u8>, Option<String>);

/// A handle to the object store backing a tenant store's Parquet + manifest
/// objects, addressed by key under `prefix`. Wraps an [`ObjectStore`] so the
/// same code path targets `LocalFileSystem` or `AmazonS3` / S3-compatible.
///
/// **`red` caveat:** `prefix` is reserved and currently always empty, and
/// [`Store::object_store`] returns the raw backend with **no prefix
/// scoping**. Per-tenant/prefix isolation (RFC0013.5) is wired at `green` —
/// do **not** assume this type enforces isolation yet.
#[derive(Clone, Debug)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    /// Reserved key prefix (the store root). Always empty at `red`; honoured
    /// once the consumers migrate onto [`Store`] at `green`.
    prefix: ObjectPath,
}

/// Non-secret addressing for the S3 / S3-compatible backend (RFC0013.7) —
/// bucket, endpoint, region, and key prefix. Credentials are resolved by
/// [`Store::s3`] from the AWS credential chain, not carried here.
///
/// `Default` yields an **empty `bucket`**, which is not valid; [`Store::s3`]
/// rejects it with [`StoreError::Config`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct S3Config {
    /// Bucket name (required; an empty value is rejected by [`Store::s3`]).
    pub bucket: String,
    /// Optional endpoint override for S3-compatible stores (`MinIO`, R2, …).
    pub endpoint: Option<String>,
    /// Region (AWS) — ignored by some S3-compatible stores.
    pub region: Option<String>,
    /// Key prefix within the bucket (the store root).
    pub prefix: Option<String>,
}

impl S3Config {
    /// Config for `bucket` (required); endpoint, region, and prefix start
    /// unset — add them with the `with_*` builders. The preferred way to build
    /// an `S3Config` (it's `#[non_exhaustive]`, so external callers can't use a
    /// struct literal; `S3Config::default()` plus setting the public fields
    /// also works, but `bucket` then defaults to the invalid empty string).
    #[must_use]
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            endpoint: None,
            region: None,
            prefix: None,
        }
    }

    /// Set the endpoint override for an S3-compatible store (Hetzner, R2,
    /// `LocalStack`, …).
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Set the region.
    #[must_use]
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the key prefix (the store root within the bucket).
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }
}

/// Which backend the process addresses, plus its addressing (RFC 0019). The
/// operator resolves this from config; [`StoreConfig::open`] constructs the
/// [`Store`].
///
/// Deliberately **exhaustive** (not `#[non_exhaustive]`, unlike the growable
/// public enums elsewhere): adding a backend variant should be a *compile
/// error* at every consumer (server, querier, compactor) so none silently
/// falls through to a wildcard. The usual `#[non_exhaustive]` semver tradeoff —
/// adding a variant breaks downstream `match`es — does not bite here: every
/// Ourios crate is internal (`publish = false`), so there is no external
/// downstream, and the compile-time forcing is precisely what we want.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreConfig {
    /// Local-filesystem backend rooted at the path (dev / test / CI).
    Local(std::path::PathBuf),
    /// S3 / S3-compatible backend — the data + audit store on object storage
    /// (`CLAUDE.md` §3.6, the production source of truth).
    S3(S3Config),
}

impl StoreConfig {
    /// Construct the [`Store`] for this backend.
    ///
    /// # Errors
    /// Propagates [`Store::local`] / [`Store::s3`] construction failures.
    pub fn open(&self) -> Result<Store, StoreError> {
        match self {
            Self::Local(root) => Store::local(root),
            Self::S3(cfg) => Store::s3(cfg.clone()),
        }
    }
}

/// Errors from constructing or addressing a [`Store`].
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// Backend construction failed (bad root, credentials, endpoint, …).
    Backend(object_store::Error),
    /// The sync→async bridge thread or runtime could not be built (resource
    /// exhaustion). Surfaced by the `*_blocking` methods.
    Runtime(std::io::Error),
    /// Backend configuration was invalid before any backend was constructed
    /// (e.g. an empty S3 bucket name).
    Config(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "object-store backend: {e}"),
            Self::Runtime(e) => write!(f, "object-store bridge runtime: {e}"),
            Self::Config(detail) => write!(f, "object-store config: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(e) => Some(e),
            Self::Runtime(e) => Some(e),
            Self::Config(_) => None,
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

    /// True if a conditional update (`If-Match`) failed its precondition —
    /// the object's `ETag` changed under us, i.e. a compare-and-swap lost the
    /// race (see [`Store::put_if_match`]).
    #[must_use]
    pub fn is_precondition(&self) -> bool {
        matches!(
            self,
            Self::Backend(object_store::Error::Precondition { .. })
        )
    }

    /// True if a create-if-absent (`If-None-Match`) failed because the object
    /// already exists (see [`Store::put_if_absent`]).
    #[must_use]
    pub fn is_already_exists(&self) -> bool {
        matches!(
            self,
            Self::Backend(object_store::Error::AlreadyExists { .. })
        )
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

    /// S3 / S3-compatible backend (RFC0013.1/.4/.7) — AWS S3, or any
    /// S3-compatible endpoint (Hetzner, R2, …) via [`S3Config::endpoint`].
    ///
    /// Credentials come from the standard AWS credential chain
    /// (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` env, shared profile, or
    /// instance metadata) — `cfg` only carries the non-secret addressing
    /// (bucket, endpoint, region, prefix). Threading explicit RFC 0004
    /// credentials is a later refinement. The backend keeps `object_store`'s
    /// default `S3ConditionalPut::ETagMatch`, the `If-Match` CAS the manifest
    /// generation-swap needs (RFC0013.3/.4).
    ///
    /// Construction does not contact the endpoint — credentials and
    /// connectivity are resolved on the first request.
    ///
    /// # Errors
    /// [`StoreError::Config`] if `cfg.bucket` is empty; [`StoreError::Backend`]
    /// if the `AmazonS3` backend cannot be built from `cfg`.
    pub fn s3(cfg: S3Config) -> Result<Self, StoreError> {
        let S3Config {
            bucket,
            endpoint,
            region,
            prefix,
        } = cfg;
        // Trim once and use the trimmed value for both the check and the
        // builder, so a whitespace-padded bucket can't pass validation and then
        // fail opaquely at request time.
        let bucket = bucket.trim().to_owned();
        if bucket.is_empty() {
            return Err(StoreError::Config(
                "S3 bucket name must not be empty".to_string(),
            ));
        }
        // Base off the AWS credential chain; explicit `cfg` fields override.
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
        if let Some(endpoint) = endpoint {
            // S3-compatible dev endpoints are often plain HTTP; object_store
            // refuses HTTP unless explicitly allowed.
            let allow_http = endpoint.starts_with("http://");
            builder = builder.with_endpoint(endpoint);
            if allow_http {
                builder = builder.with_allow_http(true);
            }
        }
        if let Some(region) = region {
            builder = builder.with_region(region);
        }
        let s3 = builder.build().map_err(StoreError::Backend)?;
        let prefix = prefix.map_or_else(ObjectPath::default, ObjectPath::from);
        Ok(Self {
            inner: Arc::new(s3),
            prefix,
        })
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

    /// Blocking [`Self::delete`] for the **sync** storage call sites (the
    /// compactor's orphan GC and post-commit input reclaim). Safe to call from
    /// inside a tokio runtime (see [`Self::get_blocking`]).
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::delete`].
    pub fn delete_blocking(&self, key: &str) -> Result<(), StoreError> {
        block_on_off_runtime(self.delete(key))
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

    /// List every object key under `prefix` (store-relative), recursively, in
    /// **lexicographic order**. The querier and compactor enumerate their
    /// partitions and files through this rather than reaching past the seam to
    /// `std::fs` (RFC 0019 §3.3) — so the same walk targets `LocalFileSystem`
    /// or S3. `prefix` is `None` to list the whole store.
    ///
    /// Keys are returned relative to the store's own prefix (the same form the
    /// `get`/`put` methods take); today that prefix is empty (RFC 0013 §3.7),
    /// so a key is the full object path. The order is enforced here by sorting,
    /// not inherited from the backend (neither `LocalFileSystem` nor S3
    /// guarantees stream order), so the contract is deterministic.
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, StoreError> {
        Ok(self
            .list_entries(prefix)
            .await?
            .into_iter()
            .map(|(key, _size)| key)
            .collect())
    }

    /// List every object under `prefix` (store-relative) as `(key, size)` pairs,
    /// recursively, in **lexicographic key order** — the size-bearing core of
    /// [`Self::list`]. The compactor's small-file candidate check needs each
    /// object's byte length, which the backend already reports in the listing
    /// (`ObjectMeta::size`), so it comes for free here rather than via a
    /// per-object `head`. Same tenant-isolation gating and key normalisation as
    /// [`Self::list`].
    async fn list_entries(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>, StoreError> {
        let scoped = prefix.map_or_else(|| self.prefix.clone(), |p| self.resolve(p));
        let metas: Vec<ObjectMeta> = self
            .inner
            .list(Some(&scoped))
            .try_collect()
            .await
            .map_err(StoreError::Backend)?;
        let root = &self.prefix;
        let mut entries: Vec<(String, u64)> = metas
            .into_iter()
            .filter_map(|m| {
                // The backend's `list` does **string**-prefix matching, so S3
                // can return a sibling (`tenant_id=ab/…` when asked for
                // `tenant_id=a`). `prefix_match` is **segment-wise**, so it
                // excludes that sibling — gate on it against the *requested*
                // prefix (`scoped`) to keep listing tenant-isolation-safe
                // (RFC0019.5), then strip the store `root` to the caller's key
                // space (the same keys `get`/`put` take).
                // `?` rejects an object not under the requested prefix; the
                // matched iterator isn't needed here (the key is built from the
                // `root` strip below), so bind it to `_` to mark the
                // `#[must_use]` value used.
                let _ = m.location.prefix_match(&scoped)?;
                let parts = m.location.prefix_match(root)?;
                let key = parts
                    .map(|p| p.as_ref().to_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                Some((key, m.size))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Blocking recursive key listing for the **sync** storage call sites — the
    /// bridge over the internal async `list`. Safe to call from inside a tokio
    /// runtime (see [`Self::get_blocking`]).
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// [`StoreError::Backend`] on a listing failure.
    pub fn list_blocking(&self, prefix: Option<&str>) -> Result<Vec<String>, StoreError> {
        block_on_off_runtime(self.list(prefix))
    }

    /// Blocking `(key, size)` listing for the **sync** storage call sites — the
    /// bridge over the internal async `list_entries`, used by the compactor to
    /// size small-file candidates without a per-object `head`. Same order +
    /// isolation contract as [`Self::list_blocking`].
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// [`StoreError::Backend`] on a listing failure.
    pub fn list_with_sizes_blocking(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        block_on_off_runtime(self.list_entries(prefix))
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

    /// Read the object at `key` together with its current `ETag` (the
    /// compare-and-swap token for a later [`Self::put_if_match`]); the `ETag`
    /// is `None` when the backend doesn't expose one.
    ///
    /// # Errors
    /// As [`Self::get`].
    pub async fn get_with_etag(&self, key: &str) -> Result<EtaggedBytes, StoreError> {
        let got = self
            .inner
            .get(&self.resolve(key))
            .await
            .map_err(StoreError::Backend)?;
        let e_tag = got.meta.e_tag.clone();
        let bytes = got.bytes().await.map_err(StoreError::Backend)?;
        Ok((bytes.to_vec(), e_tag))
    }

    /// Compare-and-swap write: replace `key` only if its current `ETag` still
    /// matches `e_tag` (`If-Match`). Used to publish a new manifest generation
    /// atomically without a `rename` (RFC0013.3/.4). Needs a backend that
    /// supports conditional update — S3-compatible stores do;
    /// `LocalFileSystem` does not.
    ///
    /// # Errors
    /// [`StoreError::Backend`] whose [`StoreError::is_precondition`] is true if
    /// the `ETag` no longer matches (the swap lost the race); otherwise as a
    /// failed put.
    pub async fn put_if_match(
        &self,
        key: &str,
        bytes: Vec<u8>,
        e_tag: &str,
    ) -> Result<(), StoreError> {
        let opts = PutOptions::from(PutMode::Update(UpdateVersion {
            e_tag: Some(e_tag.to_string()),
            version: None,
        }));
        self.inner
            .put_opts(&self.resolve(key), PutPayload::from(bytes), opts)
            .await
            .map_err(StoreError::Backend)?;
        Ok(())
    }

    /// Blocking [`Self::get_with_etag`], mapping a missing object to `None`
    /// (the manifest's "no manifest yet" case) for sync call sites.
    ///
    /// # Errors
    /// As [`Self::get_with_etag`], except a not-found object yields `Ok(None)`;
    /// plus [`StoreError::Runtime`] if the bridge runtime can't be built.
    pub fn get_with_etag_blocking_opt(
        &self,
        key: &str,
    ) -> Result<Option<EtaggedBytes>, StoreError> {
        match block_on_off_runtime(self.get_with_etag(key)) {
            Ok(pair) => Ok(Some(pair)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Blocking [`Self::put_if_match`] for the sync storage call sites.
    ///
    /// # Errors
    /// As [`Self::put_if_match`], plus [`StoreError::Runtime`] if the bridge
    /// runtime can't be built.
    pub fn put_if_match_blocking(
        &self,
        key: &str,
        bytes: Vec<u8>,
        e_tag: &str,
    ) -> Result<(), StoreError> {
        block_on_off_runtime(self.put_if_match(key, bytes, e_tag))
    }
}

#[cfg(test)]
mod tests {
    use super::{S3Config, Store, StoreError};

    /// `Store::s3` builds an `AmazonS3` backend from addressing config without
    /// contacting the endpoint (creds/connectivity resolve on first request),
    /// so construction succeeds offline for a valid bucket + S3-compatible
    /// endpoint.
    #[test]
    fn s3_constructs_from_a_valid_config() {
        let cfg = S3Config::new("ourios-test")
            .with_endpoint("https://s3.example.invalid")
            .with_region("eu-central-1")
            .with_prefix("ourios");
        let store = Store::s3(cfg).expect("s3 construct");
        assert_eq!(store.prefix().as_ref(), "ourios", "prefix is honoured");
    }

    /// An empty bucket is rejected up front with [`StoreError::Config`] rather
    /// than deferring to an opaque backend error.
    #[test]
    fn s3_rejects_an_empty_bucket() {
        let err = Store::s3(S3Config::default()).expect_err("empty bucket must fail");
        assert!(matches!(err, StoreError::Config(_)), "got {err:?}");
    }

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

    /// `list_blocking` enumerates keys under a prefix recursively, in
    /// lexicographic order, returning store-relative keys (the same key space
    /// as `get`/`put`) — the seam the querier/compactor walk instead of
    /// `std::fs` (RFC 0019 §3.3).
    #[test]
    fn list_blocking_enumerates_keys_under_a_prefix() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        for key in [
            "data/tenant_id=a/year=2026/h0.parquet",
            "data/tenant_id=a/year=2026/h1.parquet",
            // A string-prefix *sibling* of `tenant_id=a` — S3's string-prefix
            // `list` would surface this when asked for `tenant_id=a`; the
            // segment-wise filter must exclude it (tenant isolation, RFC0019.5).
            "data/tenant_id=ab/year=2026/h0.parquet",
            "data/tenant_id=b/year=2026/h0.parquet",
        ] {
            store.put_blocking(key, b"x".to_vec()).expect("put");
        }
        // Scoped to one tenant's prefix → only that tenant's objects, in the
        // guaranteed lexicographic order (asserted directly — no test-side sort,
        // so an ordering regression would fail here). The `tenant_id=ab` sibling
        // is excluded.
        assert_eq!(
            store
                .list_blocking(Some("data/tenant_id=a"))
                .expect("list a"),
            vec![
                "data/tenant_id=a/year=2026/h0.parquet".to_string(),
                "data/tenant_id=a/year=2026/h1.parquet".to_string(),
            ],
        );
        // No prefix → the whole store, all four objects, lexicographically
        // (note `tenant_id=a/` sorts before `tenant_id=ab/` — `/` < `b`).
        assert_eq!(
            store.list_blocking(None).expect("list all"),
            vec![
                "data/tenant_id=a/year=2026/h0.parquet".to_string(),
                "data/tenant_id=a/year=2026/h1.parquet".to_string(),
                "data/tenant_id=ab/year=2026/h0.parquet".to_string(),
                "data/tenant_id=b/year=2026/h0.parquet".to_string(),
            ],
        );
        // A prefix matching nothing → empty.
        assert!(
            store
                .list_blocking(Some("data/tenant_id=z"))
                .expect("list z")
                .is_empty(),
        );
    }

    /// `list_with_sizes_blocking` reports each object's byte length alongside
    /// the key, in the same lexicographic-by-key order and with the same
    /// segment-wise tenant isolation as `list_blocking` — the compactor sizes
    /// small-file candidates from this rather than a per-object `head`.
    #[test]
    fn list_with_sizes_reports_byte_lengths_in_key_order() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        // Distinct lengths so a size mismatch is visible; the `tenant_id=ab`
        // sibling must be excluded when scoping to `tenant_id=a`.
        store
            .put_blocking("data/tenant_id=a/year=2026/h0.parquet", vec![0u8; 3])
            .expect("put");
        store
            .put_blocking("data/tenant_id=a/year=2026/h1.parquet", vec![0u8; 7])
            .expect("put");
        store
            .put_blocking("data/tenant_id=ab/year=2026/h0.parquet", vec![0u8; 11])
            .expect("put");
        assert_eq!(
            store
                .list_with_sizes_blocking(Some("data/tenant_id=a"))
                .expect("list a"),
            vec![
                ("data/tenant_id=a/year=2026/h0.parquet".to_string(), 3),
                ("data/tenant_id=a/year=2026/h1.parquet".to_string(), 7),
            ],
        );
    }

    /// `delete_blocking` removes an object (the compactor's orphan/input GC),
    /// and a missing key surfaces as a `is_not_found` error rather than `Ok` —
    /// the compactor's GC loops must (and do) treat that as already-reclaimed,
    /// the same way they tolerate `ErrorKind::NotFound` on `std::fs::remove_file`.
    #[test]
    fn delete_blocking_removes_and_reports_missing_as_not_found() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "data/tenant_id=t/year=2026/x.parquet";
        store.put_blocking(key, b"x".to_vec()).expect("put");
        store.delete_blocking(key).expect("delete");
        assert_eq!(store.get_blocking_opt(key).expect("get_opt"), None);
        let err = store
            .delete_blocking(key)
            .expect_err("deleting an absent key is a not-found error");
        assert!(
            err.is_not_found(),
            "absent delete maps to not-found: {err:?}"
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
