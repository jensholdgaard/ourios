//! The bridge from the sync storage API to async `object_store`.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;

use futures::FutureExt;
use opentelemetry::context::FutureExt as _;
use tokio::runtime::Runtime;

use super::StoreError;
use super::metrics::{self, Metrics, Operation};

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
/// (see [`Store::s3`](super::Store::s3)).
pub(super) fn bridge_runtime() -> Result<&'static Runtime, StoreError> {
    static RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    match RT.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_BRIDGE_WORKERS);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("ourios-store-bridge")
            .max_blocking_threads(MAX_BRIDGE_BLOCKING_THREADS)
            .on_thread_start(metrics::bridge_thread_started)
            .thread_keep_alive(BLOCKING_THREAD_KEEP_ALIVE)
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
pub(super) const MAX_BRIDGE_WORKERS: usize = 4;

/// Upper bound on the bridge runtime's blocking pool. Each new S3
/// connection resolves its host there, so a burst that lands after the
/// idle threads have retired would otherwise start a thread per
/// connection. Lookups and local file I/O past the cap queue briefly.
pub(super) const MAX_BRIDGE_BLOCKING_THREADS: usize = 16;

/// How long an idle blocking-pool thread of the bridge runtime lives. The
/// local backend's file I/O and the S3 client's DNS lookups run on that pool,
/// and tokio's ten-second default retired a thread between any two calls
/// further apart than that, so a lightly loaded store started a thread per
/// call. Idle threads hold only their stacks, and the pool never exceeds its
/// peak concurrency.
const BLOCKING_THREAD_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(600);

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
pub(super) fn block_on_off_runtime<T>(
    metrics: &Metrics,
    op: Operation,
    fut: impl Future<Output = Result<T, StoreError>> + Send + 'static,
) -> Result<T, StoreError>
where
    T: Send + 'static,
{
    let call = metrics.get().bridge_call(op);
    match bridge(fut) {
        Ok(result) => {
            call.end(result.as_ref().err().map(metrics::store_error_type));
            result
        }
        Err(payload) => {
            call.end(Some("_OTHER"));
            std::panic::resume_unwind(payload)
        }
    }
}

/// [`block_on_off_runtime`]'s bridge, with a panic in `fut` returned
/// rather than re-raised.
fn bridge<T>(
    fut: impl Future<Output = Result<T, StoreError>> + Send + 'static,
) -> std::thread::Result<Result<T, StoreError>>
where
    T: Send + 'static,
{
    let rt = match bridge_runtime() {
        Ok(rt) => rt,
        Err(e) => return Ok(Err(e)),
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    // The caller's OTel context rides along, so a request `fut` makes inside a
    // query phase opens its CLIENT span under that phase (RFC 0038 §3.7.4).
    let cx = opentelemetry::Context::current();
    // The task, not a `JoinHandle`, carries the outcome back: awaiting a
    // `JoinHandle` needs an executor, and the caller may be inside a runtime.
    rt.spawn(
        async move {
            // The receiver outlives the send — the caller blocks on it below.
            let _ = tx.send(AssertUnwindSafe(fut).catch_unwind().await);
        }
        .with_context(cx),
    );
    rx.recv().unwrap_or_else(|std::sync::mpsc::RecvError| {
        Ok(Err(StoreError::Runtime(std::io::Error::other(
            "store bridge task dropped before completing",
        ))))
    })
}
