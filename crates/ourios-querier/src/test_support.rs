//! A request-counting [`ObjectStore`] wrapper for tests that assert how much
//! of the store a query touches (#853): which prefixes it lists, how many keys
//! those listings return, and how many reads it issues.

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// One recorded backend request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    /// A recursive `list` under the prefix.
    List(String),
    /// A one-level `list_with_delimiter` under the prefix.
    ListDelimited(String),
    /// A `get_opts` of the key (whole object or a range).
    Get(String),
    /// A `get_ranges` of the key.
    GetRanges(String),
}

#[derive(Default)]
struct Log {
    calls: Vec<Call>,
    /// Keys yielded by recursive `list` streams, in yield order.
    listed_keys: Vec<String>,
}

/// Forwards every request to `inner`, recording the reads and listings.
#[derive(Clone)]
pub(crate) struct CountingStore {
    inner: Arc<dyn ObjectStore>,
    log: Arc<Mutex<Log>>,
}

impl CountingStore {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            log: Arc::default(),
        }
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    pub(crate) fn listed_keys(&self) -> Vec<String> {
        self.lock().listed_keys.clone()
    }

    pub(crate) fn reset(&self) {
        *self.lock() = Log::default();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, call: Call) {
        self.lock().calls.push(call);
    }
}

fn prefix_str(prefix: Option<&Path>) -> String {
    prefix.map(ToString::to_string).unwrap_or_default()
}

impl fmt::Debug for CountingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CountingStore({})", self.inner)
    }
}

impl fmt::Display for CountingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CountingStore({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.record(Call::Get(location.to_string()));
        self.inner.get_opts(location, options).await
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.record(Call::GetRanges(location.to_string()));
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.record(Call::List(prefix_str(prefix)));
        let log = Arc::clone(&self.log);
        self.inner
            .list(prefix)
            .inspect(move |item| {
                if let Ok(meta) = item {
                    log.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .listed_keys
                        .push(meta.location.to_string());
                }
            })
            .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.record(Call::ListDelimited(prefix_str(prefix)));
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}
