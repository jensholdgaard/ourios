//! A store a test can take down, or pause at one call on the lock: at its
//! create, or just after its first read, so the paused start has read the
//! lock absent and resumes only when released. It can also fail one write
//! of the lock, before or after the write commits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::StreamExt as _;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use ourios_parquet::Store;
use tokio::sync::oneshot;

use super::LOG_IDS_KEY;

type BoxStream<T> = futures::stream::BoxStream<'static, T>;

/// Takes the store returned with it down and back up.
pub(super) struct Outage(Arc<AtomicBool>);

impl Outage {
    pub(super) fn begin(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(super) fn end(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The first call on the lock at a [`Point`] through the store returned
/// with it, held.
pub(super) struct Paused {
    /// Fires once that create has been called.
    pub(super) reached: oneshot::Receiver<()>,
    /// Lets it go on.
    pub(super) release: oneshot::Sender<()>,
}

/// Where a start is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Point {
    /// Before its create of the lock.
    CreateLock,
    /// After its first read of the lock has returned.
    AfterLockRead,
    /// After its create of the lock has committed, before it returns.
    AfterCreateLock,
}

/// One write of the lock that fails, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fault {
    /// The first create of the lock commits, and its caller sees an error.
    LoseCreateReply,
    /// The first compare-and-swap of the lock fails without committing.
    FailFirstSwap,
}

impl Fault {
    fn hits(self, location: &Key, mode: &PutMode) -> bool {
        let on_lock = location.as_ref() == LOG_IDS_KEY;
        match self {
            Self::LoseCreateReply => on_lock && matches!(mode, PutMode::Create),
            Self::FailFirstSwap => on_lock && matches!(mode, PutMode::Update(_)),
        }
    }
}

type Channels = (oneshot::Sender<()>, oneshot::Receiver<()>);

/// The points a start is held at, each once.
#[derive(Debug, Default)]
struct Hold(Mutex<Vec<(Point, Channels)>>);

impl Hold {
    /// Wait for the release if this call is the first at a held point.
    async fn at(&self, point: Point, location: &Key) {
        if location.as_ref() != LOG_IDS_KEY {
            return;
        }
        let held = {
            let mut points = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let at = points.iter().position(|(held, _)| *held == point);
            at.map(|at| points.remove(at).1)
        };
        if let Some((reached, release)) = held {
            let _ = reached.send(());
            let _ = release.await;
        }
    }
}

/// `store`, behind a switch that takes it down.
pub(super) fn with_outage(store: Store) -> (Store, Outage) {
    let down = Arc::new(AtomicBool::new(false));
    let wrapped = wrap(store, Arc::clone(&down), Hold::default(), None);
    (wrapped, Outage(down))
}

/// `store`, failing one write of the lock as `fault` says.
pub(super) fn faulty(store: Store, fault: Fault) -> Store {
    wrap(store, Arc::default(), Hold::default(), Some(fault))
}

/// `store`, pausing its first call on the lock at `point`.
pub(super) fn pausing(store: Store, point: Point) -> (Store, Paused) {
    pausing_faulty(store, point, None)
}

/// As [`pausing`], also failing one write of the lock as `fault` says.
pub(super) fn pausing_faulty(store: Store, point: Point, fault: Option<Fault>) -> (Store, Paused) {
    let (channels, paused) = paused();
    let hold = Hold(Mutex::new(vec![(point, channels)]));
    (wrap(store, Arc::default(), hold, fault), paused)
}

/// `store`, pausing its first call on the lock at `first`, then at `then`.
pub(super) fn pausing_twice(store: Store, first: Point, then: Point) -> (Store, [Paused; 2]) {
    let (first_channels, first_paused) = paused();
    let (then_channels, then_paused) = paused();
    let hold = Hold(Mutex::new(vec![
        (first, first_channels),
        (then, then_channels),
    ]));
    let wrapped = wrap(store, Arc::default(), hold, None);
    (wrapped, [first_paused, then_paused])
}

fn paused() -> (Channels, Paused) {
    let (reached_tx, reached) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    ((reached_tx, release_rx), Paused { reached, release })
}

fn wrap(store: Store, down: Arc<AtomicBool>, hold: Hold, fault: Option<Fault>) -> Store {
    let fault = Mutex::new(fault);
    store.wrap_backend(move |inner| {
        Arc::new(Gated {
            inner,
            down,
            hold,
            fault,
        })
    })
}

#[derive(Debug)]
struct Gated {
    inner: Arc<dyn ObjectStore>,
    down: Arc<AtomicBool>,
    hold: Hold,
    fault: Mutex<Option<Fault>>,
}

fn unreachable() -> object_store::Error {
    object_store::Error::Generic {
        store: "gated",
        source: "the object store is unreachable".into(),
    }
}

impl Gated {
    fn reachable(&self) -> object_store::Result<()> {
        if self.down.load(Ordering::SeqCst) {
            Err(unreachable())
        } else {
            Ok(())
        }
    }

    /// The fault this write hits, if it is the first it hits.
    fn fault_at(&self, location: &Key, mode: &PutMode) -> Option<Fault> {
        let mut fault = self.fault.lock().unwrap_or_else(PoisonError::into_inner);
        match *fault {
            Some(hit) if hit.hits(location, mode) => fault.take(),
            _ => None,
        }
    }

    fn failing<T: Send + 'static>(&self) -> Option<BoxStream<object_store::Result<T>>> {
        self.reachable()
            .err()
            .map(|e| futures::stream::once(async move { Err(e) }).boxed())
    }
}

impl std::fmt::Display for Gated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gated({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Gated {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.reachable()?;
        let create = matches!(opts.mode, PutMode::Create);
        if create {
            self.hold.at(Point::CreateLock, location).await;
        }
        let put = match self.fault_at(location, &opts.mode) {
            Some(Fault::FailFirstSwap) => Err(unreachable()),
            Some(Fault::LoseCreateReply) => self
                .inner
                .put_opts(location, payload, opts)
                .await
                .and_then(|_| Err(unreachable())),
            None => self.inner.put_opts(location, payload, opts).await,
        };
        if create {
            self.hold.at(Point::AfterCreateLock, location).await;
        }
        put
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
        let got = self.inner.get_opts(location, options).await;
        self.hold.at(Point::AfterLockRead, location).await;
        got
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
