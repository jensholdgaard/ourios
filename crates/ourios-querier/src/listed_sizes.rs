//! Answers `DataFusion`'s per-file `HEAD` from the sizes file-set resolution
//! already listed (#1115). A per-file table URL makes `DataFusion` `HEAD` each
//! file to learn its size, once while inferring the schema and again while
//! planning the scan, although the resolving `LIST` returned every size.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::object_store::path::Path;
use datafusion::object_store::{
    Attributes, CopyOptions, Extensions, GetOptions, GetResult, GetResultPayload, ListResult,
    MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload,
    PutResult, Result,
};
use futures::StreamExt;
use futures::stream::BoxStream;

/// Forwards every call to `inner`, except a plain `HEAD` of a listed object,
/// which it answers from `sizes`. Anything it was not given, a conditional
/// `HEAD` included, still reaches the store, so a file deleted since the
/// listing fails where it did before.
#[derive(Debug)]
pub(crate) struct ListedSizes {
    inner: Arc<dyn ObjectStore>,
    sizes: HashMap<Path, u64>,
}

impl ListedSizes {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>, sizes: HashMap<Path, u64>) -> Self {
        Self { inner, sizes }
    }

    fn listed_head(&self, location: &Path, options: &GetOptions) -> Option<GetResult> {
        let plain_head = options.head
            && options.range.is_none()
            && options.version.is_none()
            && options.if_match.is_none()
            && options.if_none_match.is_none()
            && options.if_modified_since.is_none()
            && options.if_unmodified_since.is_none();
        let size = *self.sizes.get(location).filter(|_| plain_head)?;
        Some(GetResult {
            payload: GetResultPayload::Stream(futures::stream::empty().boxed()),
            // The epoch, as `DataFusion`'s own `PartitionedFile::new` uses for
            // a file whose size alone is known. It only keys `DataFusion`'s
            // per-session caches, and every query runs on a fresh session.
            meta: ObjectMeta {
                location: location.clone(),
                last_modified: chrono::DateTime::UNIX_EPOCH,
                size,
                e_tag: None,
                version: None,
            },
            range: 0..0,
            attributes: Attributes::default(),
            extensions: Extensions::default(),
        })
    }
}

impl fmt::Display for ListedSizes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ListedSizes({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for ListedSizes {
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
        match self.listed_head(location, &options) {
            Some(result) => Ok(result),
            None => self.inner.get_opts(location, options).await,
        }
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
    use std::collections::HashMap;
    use std::sync::Arc;

    use datafusion::object_store::memory::InMemory;
    use datafusion::object_store::path::Path;
    use datafusion::object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt as _};

    use super::ListedSizes;
    use crate::fetch_count::{CountingStore, FetchCounter};

    /// A listed size answers the `HEAD` without the store: the inner store
    /// holds nothing, yet the `HEAD` succeeds with the listed size.
    #[tokio::test]
    async fn a_listed_object_is_headed_without_the_store() {
        let path = Path::from("tenant/a.parquet");
        let store = ListedSizes::new(
            Arc::new(InMemory::new()),
            HashMap::from([(path.clone(), 4321)]),
        );

        let meta = store.head(&path).await.expect("head from the listing");

        assert_eq!(meta.location, path);
        assert_eq!(meta.size, 4321);
    }

    /// An object missing from the listing, a conditional `HEAD` and every
    /// read still reach the store.
    #[tokio::test]
    async fn everything_else_reaches_the_store() {
        let inner = Arc::new(InMemory::new());
        let listed = Path::from("tenant/a.parquet");
        let unlisted = Path::from("tenant/b.parquet");
        inner.put(&listed, vec![1u8; 10].into()).await.expect("put");
        let counter = Arc::new(FetchCounter::default());
        let store = ListedSizes::new(
            Arc::new(CountingStore::new(inner, Arc::clone(&counter))),
            HashMap::from([(listed.clone(), 10)]),
        );

        let missing = store.head(&unlisted).await;
        assert!(
            missing.is_err(),
            "an unlisted object is headed for real: {missing:?}"
        );

        let conditional = GetOptions {
            if_none_match: Some("*".to_owned()),
            ..GetOptions::default().with_head(true)
        };
        let conditional = store.get_opts(&listed, conditional).await;
        assert!(
            conditional.is_err(),
            "a conditional HEAD is the store's to answer: {conditional:?}",
        );

        let suffix = GetOptions {
            range: Some(GetRange::Suffix(4)),
            ..GetOptions::default()
        };
        store.get_opts(&listed, suffix).await.expect("ranged read");
        assert_eq!(counter.get(), 4, "a read goes to the store");
    }
}
