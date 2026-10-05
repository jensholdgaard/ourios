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

use std::fmt;
use std::future::Future;
use std::sync::{Arc, OnceLock};

use std::panic::AssertUnwindSafe;

use futures::{FutureExt, TryStreamExt};
use object_store::ClientConfigKey;
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::client::SpawnedReqwestConnector;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{
    GetOptions, GetRange, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload,
    UpdateVersion,
};
use tokio::runtime::Runtime;

/// The process-wide runtime that drives the async `object_store` calls behind
/// the sync storage API. Built once, lazily, via `get_or_init` so there is no
/// init-race that could drop a surplus runtime on a caller's thread (an
/// earlier manual `get`/`set` did, panicking when the loser was inside a tokio
/// runtime). The runtime lives for the process and is never dropped, so the
/// "drop a runtime in async context" hazard can't arise.
///
/// Its workers are the long-lived threads every bridged future is polled on
/// (see [`block_on_off_runtime`]), so they are sized to the host but capped at
/// [`MAX_BRIDGE_WORKERS`]: the work is object-store I/O, and the local backend
/// moves its file I/O onto tokio's blocking pool anyway. The S3 backend's
/// HTTP connections also live here, whichever runtime issues the request
/// (see [`Store::s3`]).
fn bridge_runtime() -> Result<&'static Runtime, StoreError> {
    static RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    match RT.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_BRIDGE_WORKERS);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("ourios-store-bridge")
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

/// Upper bound on the bridge runtime's worker threads.
const MAX_BRIDGE_WORKERS: usize = 4;

/// Idle connections the S3 client keeps per host once a burst of concurrent
/// requests is over; the rest close instead of holding a descriptor each.
/// When credentials come from the chain, an `AWS_POOL_MAX_IDLE_PER_HOST` in
/// the environment takes precedence (explicit credentials read no `AWS_*`).
const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 32;

/// Whether `tag` (an `ETag` without its quotes) is safe to send unquoted in
/// `If-Match`: non-empty ASCII letters, digits and hyphens, the shape of S3
/// and Ceph `ETag`s (a hex digest, `-N` for a multipart upload). Anything
/// else could change the header's meaning, not just its spelling: `*` would
/// become a wildcard matching any existing object, and a comma a list.
fn is_plain_opaque_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Drive `fut` to completion synchronously — the bridge from the **sync**
/// storage API (`Writer`, `Reader`, `compaction`, the manifest) to async
/// `object_store` (compaction must reach S3 per RFC0013.3, so a local-only
/// `std::fs` shortcut won't do).
///
/// `fut` is spawned onto the shared [`bridge_runtime`] and the caller blocks
/// on a plain `std` channel for its result. Nothing here enters or drives a
/// tokio runtime on the caller's thread, so it is safe from any call site —
/// a plain thread, a `spawn_blocking` closure, or *inside* a runtime (the
/// querier resolving manifests on its async task, a `#[tokio::test]`), where
/// `Handle::block_on` would panic. A current-thread caller blocked here does
/// not stall `fut`, which runs on the bridge runtime's own workers.
///
/// The future is polled on those long-lived workers, never on a thread made
/// for the call: an earlier design spawned a scoped OS thread per call, and a
/// query making thousands of store calls churned thousands of threads (and
/// their allocator arenas, which glibc does not hand back). The cost is that
/// `fut` must be `'static`; the `*_blocking` methods clone the cheap `Store`
/// handle and own their keys to satisfy that.
///
/// `fut` already yields a [`StoreError`] result, returned directly; the extra
/// error mode is building the bridge runtime ([`StoreError::Runtime`]). A
/// panic *inside* `fut` is not swallowed — it is re-raised on the caller's
/// thread via [`std::panic::resume_unwind`].
fn block_on_off_runtime<T>(
    fut: impl Future<Output = Result<T, StoreError>> + Send + 'static,
) -> Result<T, StoreError>
where
    T: Send + 'static,
{
    let rt = bridge_runtime()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    // The task, not a `JoinHandle`, carries the outcome back: awaiting a
    // `JoinHandle` needs an executor, and the caller may be inside a runtime.
    rt.spawn(async move {
        // The receiver outlives the send — the caller blocks on it below.
        let _ = tx.send(AssertUnwindSafe(fut).catch_unwind().await);
    });
    match rx.recv() {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) => std::panic::resume_unwind(payload),
        Err(std::sync::mpsc::RecvError) => Err(StoreError::Runtime(std::io::Error::other(
            "store bridge task dropped before completing",
        ))),
    }
}

/// Object bytes paired with the backend's `ETag` (the compare-and-swap token),
/// as returned by [`Store::get_with_etag`]. The `ETag` is `None` when the
/// backend doesn't expose one.
pub type EtaggedBytes = (Vec<u8>, Option<String>);

/// One `/`-delimited level of a listing, as returned by
/// [`Store::list_delimited_blocking`]: store-relative keys, each list sorted
/// and unique.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DelimitedListing {
    /// Objects directly under the listed prefix (not in a child "directory").
    pub objects: Vec<String>,
    /// The immediate child common-prefixes ("directories"), without a
    /// trailing `/`.
    pub common_prefixes: Vec<String>,
}

/// The tail of an object, from [`Store::get_suffix`].
#[derive(Debug, Clone)]
pub struct Suffix {
    /// Up to the requested number of the object's last bytes.
    pub bytes: bytes::Bytes,
    /// The whole object's size.
    pub object_size: u64,
}

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
    /// Whether the backend supports a conditional update (`If-Match` CAS), the
    /// manifest generation-swap ([`crate::Manifest::publish_cas`]) the compactor
    /// commits with on S3. `false` for `LocalFileSystem`, which rejects
    /// `PutMode::Update` (see [`Self::supports_conditional_update`]).
    conditional_update: bool,
}

/// Addressing for the S3 / S3-compatible backend (RFC0013.7) — bucket,
/// endpoint, region, and key prefix — plus optional explicit S3 credentials
/// (RFC 0019 §3.4). When the credential fields are set, [`Store::s3`] applies them
/// to the builder; when they are unset, credentials fall back to the standard
/// chain ([`AmazonS3Builder::from_env`] — static `AWS_*` keys, a shared profile,
/// IRSA, or instance metadata).
///
/// `Default` yields an **empty `bucket`**, which is not valid; [`Store::s3`]
/// rejects it with [`StoreError::Config`].
///
/// The credential fields are **secret**: the manual [`fmt::Debug`] impl redacts
/// their values (showing only presence), so a `Debug` rendering of an
/// `S3Config` never leaks a key (RFC 0019 §3.4 / RFC0019.6).
#[derive(Clone, Default, PartialEq, Eq)]
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
    /// Explicit static access key id (**secret**, `OURIOS_S3_ACCESS_KEY_ID`).
    /// Paired with [`Self::secret_access_key`]; setting one without the other is
    /// rejected by [`Store::s3`].
    pub access_key_id: Option<String>,
    /// Explicit static secret access key (**secret**,
    /// `OURIOS_S3_SECRET_ACCESS_KEY`).
    pub secret_access_key: Option<String>,
    /// Explicit session token for temporary credentials (**secret**,
    /// `OURIOS_S3_SESSION_TOKEN`); valid only alongside the static key pair.
    pub session_token: Option<String>,
}

impl fmt::Debug for S3Config {
    /// Redacts the credential fields — a `Debug` rendering shows only whether a
    /// credential is present, never its value (RFC 0019 §3.4 / RFC0019.6).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redact = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("access_key_id", &redact(&self.access_key_id))
            .field("secret_access_key", &redact(&self.secret_access_key))
            .field("session_token", &redact(&self.session_token))
            .finish()
    }
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
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
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

    /// Set the explicit static access key id (**secret**). Pair with
    /// [`Self::with_secret_access_key`]; [`Store::s3`] rejects one without the
    /// other.
    #[must_use]
    pub fn with_access_key_id(mut self, access_key_id: impl Into<String>) -> Self {
        self.access_key_id = Some(access_key_id.into());
        self
    }

    /// Set the explicit static secret access key (**secret**).
    #[must_use]
    pub fn with_secret_access_key(mut self, secret_access_key: impl Into<String>) -> Self {
        self.secret_access_key = Some(secret_access_key.into());
        self
    }

    /// Set the explicit session token for temporary credentials (**secret**);
    /// valid only alongside the static key pair.
    #[must_use]
    pub fn with_session_token(mut self, session_token: impl Into<String>) -> Self {
        self.session_token = Some(session_token.into());
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
    /// The sync→async bridge runtime could not be built (resource
    /// exhaustion), or its task was lost. Surfaced by the `*_blocking` methods.
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
            // `LocalFileSystem` rejects `PutMode::Update`, so it has no `If-Match`
            // CAS; the compactor commits the manifest with an atomic overwrite here.
            conditional_update: false,
        })
    }

    /// An in-process backend with `If-Match` support, for tests and
    /// tools that need the conditional-update path without an S3 endpoint.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(object_store::memory::InMemory::new()),
            prefix: ObjectPath::default(),
            conditional_update: true,
        }
    }

    /// S3 / S3-compatible backend (RFC0013.1/.4/.7) — AWS S3, or any
    /// S3-compatible endpoint (Hetzner, R2, …) via [`S3Config::endpoint`].
    ///
    /// Credentials resolve **explicit-over-chain** (RFC 0019 §3.4): when
    /// `cfg.access_key_id` / `cfg.secret_access_key` (and optionally
    /// `cfg.session_token`) are set they build a **clean** `AmazonS3Builder` (so
    /// no ambient chain credential bleeds in); otherwise credentials fall back to
    /// the standard chain ([`AmazonS3Builder::from_env`] — static `AWS_*` env,
    /// shared profile, IRSA, or instance metadata). Blank credential values are
    /// trimmed and treated as unset. The static access key and secret are a pair:
    /// setting one without the other, or a session token without that pair, is
    /// rejected (the error names only the missing/offending field, never a value
    /// — RFC 0019 §3.4). The backend
    /// keeps `object_store`'s default `S3ConditionalPut::ETagMatch`, the
    /// `If-Match` CAS the manifest generation-swap needs (RFC0013.3/.4).
    ///
    /// Construction does not contact the endpoint — credentials and
    /// connectivity are resolved on the first request.
    ///
    /// # Errors
    /// [`StoreError::Config`] if `cfg.bucket` is empty or the explicit
    /// credential fields are a partial set; [`StoreError::Backend`] if the
    /// `AmazonS3` backend cannot be built from `cfg`; [`StoreError::Runtime`]
    /// if the bridge runtime its connections run on cannot be built.
    pub fn s3(cfg: S3Config) -> Result<Self, StoreError> {
        let S3Config {
            bucket,
            endpoint,
            region,
            prefix,
            access_key_id,
            secret_access_key,
            session_token,
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
        // Normalize the explicit credential fields: trim and treat an empty or
        // whitespace-only value as unset, so a blank reads consistently across
        // every caller of `S3Config` (matching the server's env parsing) and
        // can't spuriously trip the partial-set check below (RFC 0019 §3.4).
        let normalize =
            |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        let access_key_id = normalize(access_key_id);
        let secret_access_key = normalize(secret_access_key);
        let session_token = normalize(session_token);
        // The static access key and its secret are a pair; a session token is
        // meaningless without them. Reject a partial set rather than silently
        // falling back to the chain on an operator typo. The message names only
        // the offending key, never a value (RFC 0019 §3.4).
        match (&access_key_id, &secret_access_key) {
            (Some(_), None) => {
                return Err(StoreError::Config(
                    "OURIOS_S3_SECRET_ACCESS_KEY must be set (the static access key and secret access key are required together)".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(StoreError::Config(
                    "OURIOS_S3_ACCESS_KEY_ID must be set (the static access key and secret access key are required together)".to_string(),
                ));
            }
            (None, None) if session_token.is_some() => {
                return Err(StoreError::Config(
                    "OURIOS_S3_SESSION_TOKEN requires the static access key and secret access key (a session token is valid only with the static key pair)".to_string(),
                ));
            }
            _ => {}
        }
        // Explicit credentials build from a **clean** builder so no ambient
        // chain credential (e.g. an `AWS_SESSION_TOKEN` `from_env` would pick up)
        // bleeds into the explicit static-key pair — the `with_*` setters do not
        // clear an inherited token. `from_env()` (the standard chain: static
        // `AWS_*`, shared profile, IRSA, instance metadata) is used only as the
        // fallback when no explicit pair is given (RFC 0019 §3.4).
        let mut builder = match (access_key_id, secret_access_key) {
            (Some(access_key_id), Some(secret_access_key)) => {
                let mut explicit = AmazonS3Builder::new()
                    .with_bucket_name(bucket)
                    .with_access_key_id(access_key_id)
                    .with_secret_access_key(secret_access_key);
                if let Some(session_token) = session_token {
                    explicit = explicit.with_token(session_token);
                }
                explicit
            }
            // The validation above guarantees the remaining case is "no explicit
            // credentials" (a lone key, lone secret, or lone token is rejected).
            _ => AmazonS3Builder::from_env().with_bucket_name(bucket),
        };
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
        let pool_cap = AmazonS3ConfigKey::Client(ClientConfigKey::PoolMaxIdlePerHost);
        if builder.get_config_value(&pool_cap).is_none() {
            builder = builder.with_config(pool_cap, MAX_IDLE_CONNECTIONS_PER_HOST.to_string());
        }
        // reqwest runs each pooled connection on the runtime that issued the
        // request, and a connection only notices the store closing it while
        // that runtime polls it. Callers' runtimes stall (a current-thread
        // caller between requests, busy `DataFusion` workers), which left
        // connections in `CLOSE_WAIT` and handed them to later requests
        // (#791). The bridge runtime is never a caller's, so it always does.
        let s3 = builder
            .with_http_connector(SpawnedReqwestConnector::new(
                bridge_runtime()?.handle().clone(),
            ))
            .build()
            .map_err(StoreError::Backend)?;
        let prefix = prefix.map_or_else(ObjectPath::default, ObjectPath::from);
        Ok(Self {
            inner: Arc::new(s3),
            prefix,
            // The backend keeps object_store's default `S3ConditionalPut::ETagMatch`,
            // so the `If-Match` CAS the manifest generation-swap needs is available.
            conditional_update: true,
        })
    }

    /// Whether this backend supports a conditional update (`If-Match` CAS) — the
    /// atomic manifest generation-swap [`crate::Manifest::publish_cas`] needs
    /// (RFC0013.3/.4). S3-compatible backends do; `LocalFileSystem` rejects
    /// `PutMode::Update`, so the compactor commits there with an atomic overwrite
    /// instead (the local backend stages to a temp object and renames it into
    /// place — last-writer-wins, RFC0019.7 keeping the local commit byte-for-byte
    /// unchanged). A caller branches on this to pick the backend-appropriate swap.
    #[must_use]
    pub fn supports_conditional_update(&self) -> bool {
        self.conditional_update
    }

    /// The underlying [`ObjectStore`], for handing to `DataFusion`'s table
    /// providers on the read path (RFC 0013 §2.2 — the querier registers the
    /// same store rather than local file paths).
    #[must_use]
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.inner)
    }

    /// This store with its backend replaced by `wrap(backend)` — for layering
    /// an instrumenting or fault-injecting [`ObjectStore`] over the real one.
    /// The prefix and the conditional-update capability are kept, so the
    /// wrapper must forward every call it does not mean to change.
    #[must_use]
    pub fn wrap_backend(
        self,
        wrap: impl FnOnce(Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore>,
    ) -> Self {
        Self {
            inner: wrap(self.inner),
            ..self
        }
    }

    /// The store's root key prefix.
    #[must_use]
    pub fn prefix(&self) -> &ObjectPath {
        &self.prefix
    }

    /// Resolve a `/`-delimited `key` to an absolute object path under the
    /// store prefix.
    ///
    /// Keys are already path-safe by construction (RFC 0005 §3.4
    /// `percent_encode_tenant`, fixed partition names, UUID file names), so
    /// they are *parsed* — stored verbatim as the object key and, on the
    /// local backend, as the directory name — rather than re-encoded.
    /// `ObjectPath::from` would escape the `%` of an encoded tenant a second
    /// time (`a%2Fb` → `a%252Fb`), putting the object where neither the
    /// local querier's `tenant_id=<enc>` join nor `percent_decode_tenant`
    /// would find it — invisible for plain tenant ids, fatal for the RFC 0045
    /// composite ones.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if `key` is not a valid object path (an
    /// empty segment, `.`/`..`, a control character or raw `/` inside a
    /// segment) — a programming error at the call site, surfaced rather
    /// than silently re-encoded.
    fn resolve(&self, key: &str) -> Result<ObjectPath, StoreError> {
        let path = ObjectPath::parse(key)
            .map_err(|source| StoreError::Backend(object_store::Error::InvalidPath { source }))?;
        Ok(self.prefix.parts().chain(path.parts()).collect())
    }

    /// Write `bytes` to `key`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the put fails.
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), StoreError> {
        self.inner
            .put(&self.resolve(key)?, PutPayload::from(bytes))
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
            .get(&self.resolve(key)?)
            .await
            .map_err(StoreError::Backend)?;
        let bytes = got.bytes().await.map_err(StoreError::Backend)?;
        Ok(bytes.to_vec())
    }

    /// Read up to the last `len` bytes of the object at `key`, with the
    /// object's whole size — one ranged `GET`, which is how a Parquet footer
    /// is read without fetching the file.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the object is missing or the read fails.
    pub async fn get_suffix(&self, key: &str, len: u64) -> Result<Suffix, StoreError> {
        let options = GetOptions::default().with_range(Some(GetRange::Suffix(len)));
        let got = self
            .inner
            .get_opts(&self.resolve(key)?, options)
            .await
            .map_err(StoreError::Backend)?;
        let object_size = got.meta.size;
        let bytes = got.bytes().await.map_err(StoreError::Backend)?;
        Ok(Suffix { bytes, object_size })
    }

    /// Delete the object at `key`.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the delete fails.
    pub async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.inner
            .delete(&self.resolve(key)?)
            .await
            .map_err(StoreError::Backend)
    }

    /// Blocking [`Self::delete`] for the **sync** storage call sites (the
    /// compactor's orphan GC and post-commit input reclaim). Safe to call from
    /// inside a tokio runtime (see [`Self::get_blocking`]).
    ///
    /// **Missing-key behaviour is backend-dependent** (this bridge adds no
    /// existence check): `LocalFileSystem` maps an absent key to a
    /// [`is_not_found`](StoreError::is_not_found) error, while S3 DELETE is
    /// idempotent and returns success. The compactor's GC loops treat *both* as
    /// "already reclaimed" — they match `is_not_found` and otherwise count a
    /// failure — so the difference is invisible to them; do not rely on a
    /// uniform not-found for an absent key.
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::delete`] (and see the missing-key note above).
    pub fn delete_blocking(&self, key: &str) -> Result<(), StoreError> {
        let (store, key) = (self.clone(), key.to_owned());
        block_on_off_runtime(async move { store.delete(&key).await })
    }

    /// Blocking [`Self::get`] for the **sync** storage call sites (`Reader`,
    /// compaction). Safe to call from any thread, including inside a tokio
    /// runtime — the future runs on the bridge runtime, not the caller's thread.
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::get`].
    pub fn get_blocking(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let (store, key) = (self.clone(), key.to_owned());
        block_on_off_runtime(async move { store.get(&key).await })
    }

    /// Blocking [`Self::get_suffix`] for the sync call sites. Safe to call
    /// from inside a tokio runtime (see [`Self::get_blocking`]).
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::get_suffix`].
    pub fn get_suffix_blocking(&self, key: &str, len: u64) -> Result<Suffix, StoreError> {
        let (store, key) = (self.clone(), key.to_owned());
        block_on_off_runtime(async move { store.get_suffix(&key, len).await })
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
        let scoped = match prefix {
            Some(p) => self.resolve(p)?,
            None => self.prefix.clone(),
        };
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
        let (store, prefix) = (self.clone(), prefix.map(str::to_owned));
        block_on_off_runtime(async move { store.list(prefix.as_deref()).await })
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
        let (store, prefix) = (self.clone(), prefix.map(str::to_owned));
        block_on_off_runtime(async move { store.list_entries(prefix.as_deref()).await })
    }

    /// List the **immediate child common-prefixes** under `prefix` (the
    /// `/`-delimited "directories" one level down), store-relative, sorted, and
    /// deduplicated — a one-level roll-up via `ObjectStore::list_with_delimiter`,
    /// **not** a recursive walk. The compactor enumerates tenants with this
    /// (`data/` → `data/tenant_id=…`) so it reads one listing page rather than
    /// every object under `data/` (an S3-scale concern). `prefix` is `None` to
    /// roll up the store root.
    ///
    /// Keys are returned relative to the store's own prefix (the same form
    /// [`Self::list_blocking`] returns), and the order is enforced here by
    /// sorting, not inherited from the backend. Tenant isolation is the same
    /// **segment-wise** prefix scope as [`Self::list_blocking`]
    /// (RFC0019.5): a string-prefix sibling of the requested prefix is excluded.
    /// `LocalFileSystem` and S3 both surface subdirectories as common-prefixes.
    async fn list_common_prefixes(&self, prefix: Option<&str>) -> Result<Vec<String>, StoreError> {
        Ok(self.list_delimited(prefix).await?.common_prefixes)
    }

    /// One `/`-delimited level under `prefix`: the objects directly under it
    /// and its immediate child common-prefixes, each store-relative, sorted and
    /// deduplicated, with the same segment-wise scope as [`Self::list_blocking`].
    async fn list_delimited(&self, prefix: Option<&str>) -> Result<DelimitedListing, StoreError> {
        let scoped = match prefix {
            Some(p) => self.resolve(p)?,
            None => self.prefix.clone(),
        };
        let result = self
            .inner
            .list_with_delimiter(Some(&scoped))
            .await
            .map_err(StoreError::Backend)?;
        let root = &self.prefix;
        // Same segment-wise gating as `list_entries`: exclude a string-prefix
        // sibling, then strip the store `root` to the caller's key space. `?`
        // rejects a path not under the requested prefix; the matched iterator
        // isn't needed (the key is built from the `root` strip), so bind it
        // to `_`.
        let relative = |p: &ObjectPath| -> Option<String> {
            let _ = p.prefix_match(&scoped)?;
            let parts = p.prefix_match(root)?;
            Some(
                parts
                    .map(|s| s.as_ref().to_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            )
        };
        let mut objects: Vec<String> = result
            .objects
            .iter()
            .filter_map(|m| relative(&m.location))
            .collect();
        let mut common_prefixes: Vec<String> =
            result.common_prefixes.iter().filter_map(relative).collect();
        objects.sort();
        objects.dedup();
        common_prefixes.sort();
        common_prefixes.dedup();
        Ok(DelimitedListing {
            objects,
            common_prefixes,
        })
    }

    /// Blocking one-level delimited listing for the **sync** storage call
    /// sites — the objects directly under `prefix` plus its immediate child
    /// common-prefixes, from one `list_with_delimiter` request. The querier
    /// walks a tenant's Hive time levels with this so a windowed query lists
    /// only the subtrees its window can reach, not the tenant's whole history.
    /// Safe to call from inside a tokio runtime (see [`Self::get_blocking`]).
    /// Same order + isolation contract as [`Self::list_blocking`]; a prefix
    /// matching nothing is an empty listing.
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// [`StoreError::Backend`] on a listing failure.
    pub fn list_delimited_blocking(
        &self,
        prefix: Option<&str>,
    ) -> Result<DelimitedListing, StoreError> {
        let (store, prefix) = (self.clone(), prefix.map(str::to_owned));
        block_on_off_runtime(async move { store.list_delimited(prefix.as_deref()).await })
    }

    /// Blocking immediate-child common-prefix listing for the **sync** storage
    /// call sites — the bridge over the internal async `list_common_prefixes`,
    /// used by the compactor's one-level tenant enumeration. Safe to call from
    /// inside a tokio runtime (see [`Self::get_blocking`]). Same order +
    /// isolation contract as [`Self::list_blocking`].
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// [`StoreError::Backend`] on a listing failure.
    pub fn list_common_prefixes_blocking(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<String>, StoreError> {
        let (store, prefix) = (self.clone(), prefix.map(str::to_owned));
        block_on_off_runtime(async move { store.list_common_prefixes(prefix.as_deref()).await })
    }

    /// Blocking [`Self::put`] for the **sync** storage call sites (`Writer`,
    /// compaction). Safe to call from inside a tokio runtime (see
    /// [`Self::get_blocking`]).
    ///
    /// # Errors
    /// [`StoreError::Runtime`] if the bridge runtime can't be built;
    /// otherwise as [`Self::put`].
    pub fn put_blocking(&self, key: &str, bytes: Vec<u8>) -> Result<(), StoreError> {
        let (store, key) = (self.clone(), key.to_owned());
        block_on_off_runtime(async move { store.put(&key, bytes).await })
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
                &self.resolve(key)?,
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
        let (store, key) = (self.clone(), key.to_owned());
        block_on_off_runtime(async move { store.put_if_absent(&key, bytes).await })
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
            .get(&self.resolve(key)?)
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
    /// A `412` against a quoted `ETag` is retried once with the quotes
    /// stripped: some S3-compatible stores (Ceph RGW-based ones) return the
    /// quoted form from `GET` but honour `If-Match` only unquoted, so every
    /// swap would otherwise lose. The retry cannot weaken the swap — it
    /// still succeeds only if the object is unchanged — because it is sent
    /// only for a plain opaque tag (letters, digits and hyphens), whose
    /// unquoted spelling cannot become a wildcard or a list.
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
        let path = self.resolve(key)?;
        let payload = PutPayload::from(bytes);
        let put = |e_tag: &str| {
            let opts = PutOptions::from(PutMode::Update(UpdateVersion {
                e_tag: Some(e_tag.to_string()),
                version: None,
            }));
            self.inner.put_opts(&path, payload.clone(), opts)
        };
        let unquoted = e_tag
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|tag| is_plain_opaque_tag(tag));
        match (put(e_tag).await, unquoted) {
            (Ok(_), _) => Ok(()),
            (Err(object_store::Error::Precondition { .. }), Some(unquoted)) => {
                put(unquoted).await.map(|_| ()).map_err(StoreError::Backend)
            }
            (Err(e), _) => Err(StoreError::Backend(e)),
        }
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
        let (store, key) = (self.clone(), key.to_owned());
        match block_on_off_runtime(async move { store.get_with_etag(&key).await }) {
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
        let (store, key, e_tag) = (self.clone(), key.to_owned(), e_tag.to_owned());
        block_on_off_runtime(async move { store.put_if_match(&key, bytes, &e_tag).await })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;

    use super::{MAX_BRIDGE_WORKERS, S3Config, Store, StoreError, block_on_off_runtime};

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

    /// RFC0019.8 — a full explicit credential pair (and an optional session
    /// token) is accepted and applied to the builder; construction stays offline.
    #[test]
    fn s3_accepts_a_valid_credential_pair() {
        let cfg = S3Config::new("ourios-test")
            .with_access_key_id("AKIAEXAMPLE")
            .with_secret_access_key("s3cr3t")
            .with_session_token("token");
        Store::s3(cfg).expect("explicit credential pair");
    }

    /// RFC0019.8 — a partial credential set fails fast with [`StoreError::Config`]
    /// naming only the offending key, never a value (RFC 0019 §3.4): an
    /// access key without its secret, a secret without its key, or a session
    /// token without the pair.
    #[test]
    fn s3_rejects_a_partial_credential_set() {
        let secret_val = "s3cr3t-value-must-not-leak";
        let cases = [
            S3Config::new("b").with_access_key_id("AKIAEXAMPLE"),
            S3Config::new("b").with_secret_access_key(secret_val),
            S3Config::new("b").with_session_token("token"),
        ];
        for cfg in cases {
            let err = Store::s3(cfg).expect_err("partial credential set must fail");
            assert!(matches!(err, StoreError::Config(_)), "got {err:?}");
            let msg = err.to_string();
            assert!(
                !msg.contains(secret_val),
                "the error must not echo a credential value, got {msg:?}",
            );
        }
    }

    /// RFC0019.8 — blank/whitespace-only credential values are trimmed and read
    /// as unset (consistent with the server's env parsing), so they don't
    /// spuriously trip the partial-set fail-fast; the config falls back to the
    /// chain and constructs offline.
    #[test]
    fn s3_treats_blank_credentials_as_unset() {
        let cfg = S3Config::new("b")
            .with_access_key_id("   ")
            .with_secret_access_key("")
            .with_session_token("  ");
        Store::s3(cfg).expect("blank credentials read as unset, not a partial set");
    }

    /// RFC0019.8 / §3.4 — `S3Config`'s `Debug` redacts credential values, so a
    /// config logged or surfaced in an error never leaks a secret. Presence is
    /// still visible (so misconfig is diagnosable) but the value is not.
    #[test]
    fn s3config_debug_redacts_credentials() {
        let access = "AKIA-do-not-leak";
        let secret = "secret-do-not-leak";
        let token = "token-do-not-leak";
        let cfg = S3Config::new("b")
            .with_access_key_id(access)
            .with_secret_access_key(secret)
            .with_session_token(token);
        let rendered = format!("{cfg:?}");
        for v in [access, secret, token] {
            assert!(
                !rendered.contains(v),
                "Debug leaked a credential value: {rendered}",
            );
        }
        assert!(
            rendered.contains("<redacted>"),
            "Debug should mark credential presence, got {rendered}",
        );
        // A config with no credentials shows them absent (None), not redacted.
        let bare = format!("{:?}", S3Config::new("b"));
        assert!(
            !bare.contains("<redacted>"),
            "absent credentials are not redacted, got {bare}",
        );
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

    /// RFC 0005 §3.4 / RFC 0045 §3.2 — a percent-encoded tenant key is stored
    /// verbatim: on the local backend the directory is `tenant_id=<enc>`
    /// exactly (what the local querier joins and `percent_decode_tenant`
    /// inverts), and listing returns the same key `put` took.
    #[test]
    fn encoded_tenant_keys_are_stored_verbatim_and_round_trip() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let enc = crate::percent_encode_tenant("cluster1/flux%cd");
        assert_eq!(enc, "cluster1%2Fflux%25cd");
        let key = format!("data/tenant_id={enc}/year=2026/x.parquet");
        store.put_blocking(&key, b"row".to_vec()).expect("put");

        assert!(
            dir.path()
                .join("data")
                .join(format!("tenant_id={enc}"))
                .join("year=2026")
                .join("x.parquet")
                .is_file(),
            "the on-disk directory is the once-encoded tenant"
        );
        assert_eq!(
            store.list_blocking(Some("data/")).expect("list"),
            vec![key.clone()],
            "listing returns the key put took"
        );
        assert_eq!(
            store
                .list_common_prefixes_blocking(Some("data/"))
                .expect("prefixes"),
            vec![format!("data/tenant_id={enc}")]
        );
        assert_eq!(store.get_blocking(&key).expect("get"), b"row");
        assert!(matches!(
            store.put_blocking("data/../x", Vec::new()),
            Err(StoreError::Backend(_))
        ));
    }

    // RFC 0005 §3.4 / RFC 0045 §3.2 in property form: for any tenant id —
    // reserved, unreserved and multi-byte UTF-8 characters mixed — the
    // once-encoded key survives put → list → get on the local backend and
    // the on-disk directory is exactly `tenant_id=<enc>`.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
        #[test]
        fn encoded_tenant_keys_round_trip_for_any_tenant(
            tenant in "[a-z0-9._~/%=:+ éß日]{1,12}",
        ) {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let store = Store::local(dir.path()).expect("local store");
            let enc = crate::percent_encode_tenant(&tenant);
            let key = format!("data/tenant_id={enc}/year=2026/x.parquet");
            store.put_blocking(&key, tenant.as_bytes().to_vec()).expect("put");
            let on_disk = dir
                .path()
                .join("data")
                .join(format!("tenant_id={enc}"))
                .join("year=2026")
                .join("x.parquet");
            proptest::prop_assert!(on_disk.is_file(), "missing {}", on_disk.display());
            proptest::prop_assert_eq!(
                store.list_blocking(Some("data/")).expect("list"),
                vec![key.clone()]
            );
            proptest::prop_assert_eq!(
                store.get_blocking(&key).expect("get"),
                tenant.as_bytes().to_vec()
            );
            proptest::prop_assert_eq!(
                crate::percent_decode_tenant(&enc),
                Some(tenant.clone())
            );
        }
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

    /// `list_common_prefixes_blocking` rolls up to the **immediate** child
    /// "directories" under a prefix (one level, not a recursive walk) —
    /// store-relative, sorted, deduplicated. The compactor enumerates tenants
    /// with this (`data/` → `data/tenant_id=…`) rather than scanning every
    /// object under `data/`.
    #[test]
    fn list_common_prefixes_rolls_up_immediate_children() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        for key in [
            "data/tenant_id=a/year=2026/month=04/day=02/hour=10/h0.parquet",
            "data/tenant_id=a/year=2026/month=04/day=02/hour=11/h1.parquet",
            "data/tenant_id=ab/year=2026/h0.parquet",
            "data/tenant_id=b/year=2026/h0.parquet",
        ] {
            store.put_blocking(key, b"x".to_vec()).expect("put");
        }
        // Under `data/`: the three tenant dirs, one level down only (no deeper
        // segments), deduplicated across each tenant's many objects, sorted.
        assert_eq!(
            store
                .list_common_prefixes_blocking(Some("data"))
                .expect("roll up data"),
            vec![
                "data/tenant_id=a".to_string(),
                "data/tenant_id=ab".to_string(),
                "data/tenant_id=b".to_string(),
            ],
        );
        // Scoped to one tenant → its immediate `year=…` child only (segment-wise
        // scope excludes the `tenant_id=ab` sibling).
        assert_eq!(
            store
                .list_common_prefixes_blocking(Some("data/tenant_id=a"))
                .expect("roll up tenant a"),
            vec!["data/tenant_id=a/year=2026".to_string()],
        );
        // A prefix matching nothing → empty.
        assert!(
            store
                .list_common_prefixes_blocking(Some("data/tenant_id=z"))
                .expect("roll up z")
                .is_empty(),
        );
    }

    /// `list_delimited_blocking` returns one level: the objects directly under
    /// the prefix plus its immediate child "directories", never a deeper key —
    /// with the same segment-wise tenant scope as `list_blocking`.
    #[test]
    fn list_delimited_returns_one_level_of_objects_and_prefixes() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        for key in [
            "data/tenant_id=a/stray.parquet",
            "data/tenant_id=a/year=2025/month=12/day=31/hour=23/h0.parquet",
            "data/tenant_id=a/year=2026/month=04/day=02/hour=10/h1.parquet",
            "data/tenant_id=ab/other.parquet",
            "data/tenant_id=ab/year=2027/h2.parquet",
        ] {
            store.put_blocking(key, b"x".to_vec()).expect("put");
        }

        let level = store
            .list_delimited_blocking(Some("data/tenant_id=a"))
            .expect("list tenant a");

        assert_eq!(
            level,
            super::DelimitedListing {
                objects: vec!["data/tenant_id=a/stray.parquet".to_string()],
                common_prefixes: vec![
                    "data/tenant_id=a/year=2025".to_string(),
                    "data/tenant_id=a/year=2026".to_string(),
                ],
            },
        );
        assert_eq!(
            store
                .list_delimited_blocking(Some("data/tenant_id=z"))
                .expect("list absent tenant"),
            super::DelimitedListing::default(),
            "a prefix matching nothing is an empty listing, not an error",
        );
    }

    /// `wrap_backend` routes every call through the wrapper while keeping the
    /// store's key space: a key written through the wrapped store is readable
    /// through the original backend at the same key.
    #[test]
    fn wrap_backend_routes_calls_through_the_wrapper() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let wrapped_with = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&wrapped_with);
        let wrapped = store.clone().wrap_backend(move |inner| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            inner
        });

        wrapped
            .put_blocking("data/tenant_id=a/k.parquet", b"v".to_vec())
            .expect("put through the wrapper");

        assert_eq!(wrapped_with.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            store
                .get_blocking("data/tenant_id=a/k.parquet")
                .expect("get through the original"),
            b"v".to_vec(),
        );
        assert_eq!(
            wrapped.supports_conditional_update(),
            store.supports_conditional_update(),
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

    /// `delete_blocking` removes an object (the compactor's orphan/input GC). On
    /// the **local** backend a missing key surfaces as a `is_not_found` error
    /// (S3 DELETE is idempotent instead — see the method doc); the compactor's
    /// GC treats either as already-reclaimed, the same way it tolerates
    /// `ErrorKind::NotFound` on `std::fs::remove_file`.
    #[test]
    fn delete_blocking_removes_and_local_missing_is_not_found() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "data/tenant_id=t/year=2026/x.parquet";
        store.put_blocking(key, b"x".to_vec()).expect("put");
        store.delete_blocking(key).expect("delete");
        assert_eq!(store.get_blocking_opt(key).expect("get_opt"), None);
        let err = store
            .delete_blocking(key)
            .expect_err("local backend: absent key is a not-found error");
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

    /// Run one bridged call that records the thread its future is polled on.
    fn bridged_call_records_thread(seen: &Arc<Mutex<HashSet<ThreadId>>>) {
        let seen = Arc::clone(seen);
        let got = block_on_off_runtime(async move {
            seen.lock()
                .expect("thread-id set")
                .insert(std::thread::current().id());
            Ok(7_u8)
        })
        .expect("bridged call");
        assert_eq!(got, 7);
    }

    fn distinct(seen: &Arc<Mutex<HashSet<ThreadId>>>) -> usize {
        seen.lock().expect("thread-id set").len()
    }

    /// The bridge polls futures on a bounded set of long-lived threads, not a
    /// fresh OS thread per call: a query making thousands of blocking store
    /// calls must not create thousands of threads.
    #[test]
    fn sequential_bridged_calls_reuse_a_bounded_set_of_threads() {
        const CALLS: usize = 64;
        let seen = Arc::new(Mutex::new(HashSet::new()));
        for _ in 0..CALLS {
            bridged_call_records_thread(&seen);
        }
        assert!(
            distinct(&seen) <= MAX_BRIDGE_WORKERS,
            "{CALLS} sequential calls ran on {} threads",
            distinct(&seen)
        );
    }

    /// Concurrent callers share the same bounded set of bridge threads.
    #[test]
    fn concurrent_bridged_calls_reuse_a_bounded_set_of_threads() {
        const CALLERS: usize = 16;
        const CALLS_EACH: usize = 16;
        let seen = Arc::new(Mutex::new(HashSet::new()));
        std::thread::scope(|s| {
            for _ in 0..CALLERS {
                s.spawn(|| {
                    for _ in 0..CALLS_EACH {
                        bridged_call_records_thread(&seen);
                    }
                });
            }
        });
        assert!(
            distinct(&seen) <= MAX_BRIDGE_WORKERS,
            "{} concurrent calls ran on {} threads",
            CALLERS * CALLS_EACH,
            distinct(&seen)
        );
    }

    fn bridge_round_trip_from_here(label: &str) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let key = "data/tenant_id=t/year=2026/x.parquet";
        store
            .put_blocking(key, label.as_bytes().to_vec())
            .expect("put_blocking");
        assert_eq!(
            store.get_blocking(key).expect("get_blocking"),
            label.as_bytes()
        );
        assert_eq!(
            store.list_blocking(Some("data")).expect("list_blocking"),
            vec![key.to_owned()]
        );
        let seen = Arc::new(Mutex::new(HashSet::new()));
        for _ in 0..16 {
            bridged_call_records_thread(&seen);
        }
        assert!(distinct(&seen) <= MAX_BRIDGE_WORKERS);
    }

    /// Callable from a multi-thread runtime's worker without panicking
    /// ("runtime within a runtime") or deadlocking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_bridge_is_safe_inside_a_multi_thread_runtime() {
        bridge_round_trip_from_here("multi-thread");
    }

    /// Callable from a current-thread runtime, whose only thread is the caller:
    /// the bridged future must not need that thread to make progress.
    #[tokio::test(flavor = "current_thread")]
    async fn blocking_bridge_is_safe_inside_a_current_thread_runtime() {
        bridge_round_trip_from_here("current-thread");
    }

    /// Callable from a `spawn_blocking` closure, which carries the runtime's
    /// context without being one of its workers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_bridge_is_safe_inside_spawn_blocking() {
        tokio::task::spawn_blocking(|| bridge_round_trip_from_here("spawn-blocking"))
            .await
            .expect("spawn_blocking task");
    }

    /// A panic inside the bridged future is re-raised on the caller's thread
    /// with its original payload, and the bridge keeps working afterwards.
    #[test]
    fn a_panic_in_the_bridged_future_propagates_to_the_caller() {
        let fail = true;
        let caught = std::panic::catch_unwind(|| {
            block_on_off_runtime(async move {
                assert!(!fail, "bridged future panicked");
                Ok(())
            })
        })
        .expect_err("the panic reaches the caller");
        let msg = caught
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| caught.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(msg.contains("bridged future panicked"), "payload: {msg:?}");
        assert_eq!(
            block_on_off_runtime(async { Ok(1_u8) }).expect("after panic"),
            1
        );
    }
}
