//! The object store behind [`Hooks`], injecting at the high-water.

use std::sync::Arc;

use super::Hooks;

/// What another writer leaves in the high-water when it wins a race.
const WINNER: &[u8] = br#"{"reserved_through": 1}"#;

fn is_high_water(location: &object_store::path::Path) -> bool {
    location.as_ref() == ourios_ingester::template_ids::HIGH_WATER_KEY
}

fn take(flag: &std::sync::atomic::AtomicBool) -> bool {
    flag.swap(false, std::sync::atomic::Ordering::AcqRel)
}

pub(super) struct HookedStore {
    pub(super) inner: Arc<dyn object_store::ObjectStore>,
    pub(super) hooks: Hooks,
}

impl std::fmt::Debug for HookedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HookedStore({})", self.inner)
    }
}

impl std::fmt::Display for HookedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HookedStore({})", self.inner)
    }
}

impl HookedStore {
    /// Another actor deletes the high-water, once, if `flag` is armed, and
    /// the write in flight fails not-found.
    async fn delete_if(
        &self,
        flag: &std::sync::atomic::AtomicBool,
        location: &object_store::path::Path,
    ) -> object_store::Result<()> {
        if !take(flag) {
            return Ok(());
        }
        object_store::ObjectStoreExt::delete(self.inner.as_ref(), location).await?;
        Err(object_store::Error::NotFound {
            path: location.to_string(),
            source: "deleted between the read and the write".into(),
        })
    }

    /// Another writer creates the high-water, once, if `flag` is armed.
    async fn win_if(
        &self,
        flag: &std::sync::atomic::AtomicBool,
        location: &object_store::path::Path,
    ) -> object_store::Result<()> {
        if !take(flag) {
            return Ok(());
        }
        self.inner
            .put_opts(
                location,
                WINNER.to_vec().into(),
                object_store::PutOptions::default(),
            )
            .await
            .map(|_| ())
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for HookedStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.hooks.enter()?;
        if is_high_water(location) {
            self.hooks.count_down_put()?;
            self.hooks.high_water_put_gate.pass();
            self.delete_if(&self.hooks.delete_before_next_put, location)
                .await?;
        }
        let creating = matches!(opts.mode, object_store::PutMode::Create);
        if creating && is_high_water(location) {
            self.win_if(&self.hooks.race_the_create, location).await?;
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.hooks.enter()?;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if is_high_water(location) {
            self.hooks
                .high_water_reads
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.hooks.enter()?;
        if location.as_ref().starts_with("data/")
            && let Some(flag) = self.hooks.raise_on_data_read.get()
        {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        let got = self.inner.get_opts(location, options).await;
        let absent = matches!(got, Err(object_store::Error::NotFound { .. }));
        if absent && is_high_water(location) {
            self.win_if(&self.hooks.create_after_absent_read, location)
                .await?;
        }
        got
    }

    async fn get_ranges(
        &self,
        location: &object_store::path::Path,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<bytes::Bytes>> {
        self.hooks.enter()?;
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.hooks.enter()?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.hooks.enter()?;
        self.inner.copy_opts(from, to, options).await
    }
}
