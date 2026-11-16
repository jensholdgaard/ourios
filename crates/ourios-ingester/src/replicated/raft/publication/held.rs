//! A store whose first put waits until the test releases it, then fails
//! without writing; every later call goes through.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use ourios_parquet::Store;

type BoxStream<T> = futures::stream::BoxStream<'static, T>;

/// The first put through the store returned with it, held.
pub(super) struct Held {
    /// Receives once that put has been called.
    pub(super) reached: mpsc::Receiver<()>,
    /// Lets it fail.
    pub(super) release: mpsc::Sender<()>,
}

/// `store` with its first put held until released.
pub(super) fn first_put_held(store: Store) -> (Store, Held) {
    let (reached_tx, reached) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let gate = Arc::new(Gate {
        taken: AtomicBool::new(false),
        reached: Mutex::new(reached_tx),
        release: Mutex::new(release_rx),
    });
    let wrapped = store.wrap_backend(move |inner| Arc::new(HeldStore { inner, gate }));
    (wrapped, Held { reached, release })
}

#[derive(Debug)]
struct Gate {
    taken: AtomicBool,
    reached: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}

#[derive(Debug)]
struct HeldStore {
    inner: Arc<dyn ObjectStore>,
    gate: Arc<Gate>,
}

impl std::fmt::Display for HeldStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for HeldStore {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        if !self.gate.taken.swap(true, Ordering::SeqCst) {
            self.gate.reached.lock().expect("reached").send(()).ok();
            self.gate.release.lock().expect("release").recv().ok();
            return Err(object_store::Error::Generic {
                store: "held",
                source: "the held put failed".into(),
            });
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<object_store::Result<Key>>,
    ) -> BoxStream<object_store::Result<Key>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}
