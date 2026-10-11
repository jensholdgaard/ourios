//! Per-session storage-read accounting: every byte a query session's table
//! reads fetch through its object stores, footer reads during schema
//! inference included.
//!
//! The scan's own `bytes_scanned` metric (`QueryStats::bytes_read`) only
//! sees the executed plan. Per-file schema inference in
//! `register_listing_table` reads every file's footer before a plan
//! exists, and on a many-file query that is most of what the query
//! fetches. Counting at the object-store seam the session reads through
//! sees both.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use datafusion::object_store::path::Path;
use datafusion::object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};
use datafusion::prelude::SessionContext;
use futures::stream::BoxStream;

/// The bytes one session has fetched. Shared by every store wrapper
/// registered on the session, and carried as a session-config extension
/// so the registration sites and the reader find the same counter.
#[derive(Debug, Default)]
pub(crate) struct FetchCounter(AtomicU64);

impl FetchCounter {
    fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The counter `ctx` was built with ([`crate::exec::session`]).
pub(crate) fn counter(ctx: &SessionContext) -> Option<Arc<FetchCounter>> {
    ctx.state().config().get_extension::<FetchCounter>()
}

/// The bytes `ctx`'s stores have fetched so far; `0` for a session built
/// without a counter.
pub(crate) fn fetched(ctx: &SessionContext) -> u64 {
    counter(ctx).map_or(0, |c| c.get())
}

/// `inner`, counting into `ctx`'s counter when it has one.
pub(crate) fn counted(ctx: &SessionContext, inner: Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
    match counter(ctx) {
        Some(counter) => Arc::new(CountingStore { inner, counter }),
        None => inner,
    }
}

/// Forwards every call to `inner`, adding the bytes each read returns to
/// `counter`. A HEAD transfers no object bytes and is not counted.
#[derive(Debug)]
pub(crate) struct CountingStore {
    inner: Arc<dyn ObjectStore>,
    counter: Arc<FetchCounter>,
}

impl CountingStore {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>, counter: Arc<FetchCounter>) -> Self {
        Self { inner, counter }
    }
}

impl fmt::Display for CountingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FetchCounting({})", self.inner)
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
        let head = options.head;
        let result = self.inner.get_opts(location, options).await?;
        if !head {
            self.counter
                .add(result.range.end.saturating_sub(result.range.start));
        }
        Ok(result)
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        let parts = self.inner.get_ranges(location, ranges).await?;
        self.counter
            .add(parts.iter().map(|b| b.len() as u64).sum::<u64>());
        Ok(parts)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::object_store::memory::InMemory;
    use datafusion::object_store::path::Path;
    use datafusion::object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt as _};

    use super::{CountingStore, FetchCounter};

    #[tokio::test]
    async fn counts_bytes_read_and_not_heads() {
        let inner = Arc::new(InMemory::new());
        let path = Path::from("a.parquet");
        inner.put(&path, vec![7u8; 100].into()).await.expect("put");
        let counter = Arc::new(FetchCounter::default());
        let store = CountingStore::new(inner, Arc::clone(&counter));

        store.head(&path).await.expect("head");
        assert_eq!(counter.get(), 0, "a HEAD fetches no object bytes");

        let suffix = GetOptions {
            range: Some(GetRange::Suffix(8)),
            ..GetOptions::default()
        };
        store.get_opts(&path, suffix).await.expect("get suffix");
        assert_eq!(counter.get(), 8, "a ranged GET counts its range");

        store
            .get_ranges(&path, &[0..10, 20..25])
            .await
            .expect("get ranges");
        assert_eq!(counter.get(), 23);

        store
            .get(&path)
            .await
            .expect("get")
            .bytes()
            .await
            .expect("body");
        assert_eq!(counter.get(), 123, "a whole GET counts the object");
    }
}
