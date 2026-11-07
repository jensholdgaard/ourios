//! An object store that can be made unreachable: while down, every call
//! fails as a lost connection would.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt as _;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use ourios_parquet::Store;

type BoxStream<T> = futures::stream::BoxStream<'static, T>;

/// Switches the store returned with it.
pub(super) struct Outage(Arc<AtomicBool>);

impl Outage {
    pub(super) fn begin(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(super) fn end(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// `store`, behind a switch that takes it down.
pub(super) fn over(store: Store) -> (Store, Outage) {
    let down = Arc::new(AtomicBool::new(false));
    let switch = Arc::clone(&down);
    let wrapped = store.wrap_backend(move |inner| {
        Arc::new(OutageStore {
            inner,
            down: switch,
        })
    });
    (wrapped, Outage(down))
}

#[derive(Debug)]
struct OutageStore {
    inner: Arc<dyn ObjectStore>,
    down: Arc<AtomicBool>,
}

fn unreachable() -> object_store::Error {
    object_store::Error::Generic {
        store: "outage",
        source: "the object store is unreachable".into(),
    }
}

impl OutageStore {
    fn reachable(&self) -> object_store::Result<()> {
        if self.down.load(Ordering::SeqCst) {
            Err(unreachable())
        } else {
            Ok(())
        }
    }

    fn failing<T: Send + 'static>(&self) -> Option<BoxStream<object_store::Result<T>>> {
        self.reachable()
            .err()
            .map(|e| futures::stream::once(async move { Err(e) }).boxed())
    }
}

impl std::fmt::Display for OutageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OutageStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for OutageStore {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.reachable()?;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.reachable()?;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.reachable()?;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<object_store::Result<Key>>,
    ) -> BoxStream<object_store::Result<Key>> {
        self.failing()
            .unwrap_or_else(|| self.inner.delete_stream(locations))
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<object_store::Result<ObjectMeta>> {
        self.failing().unwrap_or_else(|| self.inner.list(prefix))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.reachable()?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.reachable()?;
        self.inner.copy_opts(from, to, options).await
    }
}
