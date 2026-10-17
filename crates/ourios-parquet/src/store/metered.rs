//! The [`ObjectStore`] a [`Store`](super::Store) hands out: its backend,
//! with every logical call recorded (see [`super::metrics`]).

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, Result,
};

use super::Store;
use super::metrics::{Call, Caller, Metrics, Observed, Operation};

/// What a [`Store`] constructor resolved, for [`Store::assemble`].
pub(super) struct Backend {
    pub(super) store: Arc<dyn ObjectStore>,
    pub(super) http: bool,
    pub(super) prefix: Path,
    pub(super) conditional_update: bool,
    pub(super) local_root: Option<std::path::PathBuf>,
    pub(super) metrics: Metrics,
}

impl Store {
    pub(super) fn assemble(backend: Backend) -> Self {
        let Backend {
            store,
            http,
            prefix,
            conditional_update,
            local_root,
            metrics,
        } = backend;
        Self {
            inner: Arc::clone(&store),
            backend: store,
            http,
            caller: Caller::default(),
            metrics,
            prefix,
            conditional_update,
            local_root,
            if_match_unquoted: Arc::default(),
        }
        .metered()
    }

    /// Rebuild [`Self::inner`] over the backend, for the current caller and
    /// metrics.
    pub(super) fn metered(self) -> Self {
        Self {
            inner: Arc::new(Metered {
                inner: Arc::clone(&self.backend),
                caller: self.caller,
                metrics: self.metrics.clone(),
                http: self.http,
            }),
            ..self
        }
    }

    /// This handle, with its calls attributed to `caller` in the storage
    /// metrics. A subsystem sets it where it takes its handle; the backend
    /// is shared, so the handle stays as cheap to make as a clone.
    #[must_use]
    pub fn with_caller(self, caller: Caller) -> Self {
        Self { caller, ..self }.metered()
    }

    /// The subsystem this handle's calls are attributed to.
    #[must_use]
    pub fn caller(&self) -> Caller {
        self.caller
    }

    #[cfg(test)]
    pub(super) fn with_metrics(self, metrics: Metrics) -> Self {
        Self { metrics, ..self }.metered()
    }
}

#[derive(Debug)]
pub(super) struct Metered {
    pub(super) inner: Arc<dyn ObjectStore>,
    pub(super) caller: Caller,
    pub(super) metrics: Metrics,
    /// Whether the backend is reached over HTTP, so its attempts are
    /// tracked for retries.
    pub(super) http: bool,
}

impl Metered {
    fn call(&self, op: Operation) -> Call {
        Call::start(&self.metrics, op, self.caller, self.http)
    }
}

impl fmt::Display for Metered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[async_trait]
impl ObjectStore for Metered {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let call = self.call(Operation::Put);
        let result = call.run(self.inner.put_opts(location, payload, opts)).await;
        call.end(&result);
        result
    }

    // Unmetered: Ourios writes every object with one PUT, and a multipart
    // upload's parts are sent long after this returns.
    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    /// A GET's call lasts until its body has been read, so a slow transfer
    /// shows as a slow GET. A local file body is read by the caller and is
    /// not timed.
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let op = match (&options.range, options.head) {
            (_, true) => Operation::Head,
            (Some(_), false) => Operation::GetRange,
            (None, false) => Operation::Get,
        };
        let call = self.call(op);
        match call.run(self.inner.get_opts(location, options)).await {
            Ok(GetResult {
                payload: GetResultPayload::Stream(body),
                meta,
                range,
                attributes,
                extensions,
            }) if op != Operation::Head => Ok(GetResult {
                payload: GetResultPayload::Stream(Observed::boxed(body, call)),
                meta,
                range,
                attributes,
                extensions,
            }),
            result => {
                call.end(&result);
                result
            }
        }
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        let call = self.call(Operation::GetRange);
        let result = call.run(self.inner.get_ranges(location, ranges)).await;
        call.end(&result);
        result
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        Observed::boxed(
            self.inner.delete_stream(locations),
            self.call(Operation::Delete),
        )
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        Observed::boxed(self.inner.list(prefix), self.call(Operation::List))
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        Observed::boxed(
            self.inner.list_with_offset(prefix, offset),
            self.call(Operation::List),
        )
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let mut call = self.call(Operation::ListDelimited);
        let result = call.run(self.inner.list_with_delimiter(prefix)).await;
        if let Ok(listing) = &result {
            call.listed(listing.objects.len());
        }
        call.end(&result);
        result
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        let call = self.call(Operation::Copy);
        let result = call.run(self.inner.copy_opts(from, to, options)).await;
        call.end(&result);
        result
    }

    // Forwarded so a backend's native rename is kept; it is recorded as the
    // copy it amounts to.
    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        let call = self.call(Operation::Copy);
        let result = call.run(self.inner.rename_opts(from, to, options)).await;
        call.end(&result);
        result
    }
}
