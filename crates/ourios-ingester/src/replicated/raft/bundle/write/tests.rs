use std::sync::{Arc, Mutex, PoisonError};

use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

use super::super::tests::{AT, LOG, SEQ, marked, sample};
use super::*;

const TENANTS: [&str; 2] = ["acme", "beta"];

fn snapshots() -> Vec<Vec<u8>> {
    TENANTS.iter().map(|tenant| marked(tenant)).collect()
}

fn keys() -> Vec<String> {
    let mut keys: Vec<String> = TENANTS
        .iter()
        .map(|tenant| snapshot_key(LOG, SEQ, &ourios_core::tenant::TenantId::new(*tenant)))
        .collect();
    keys.push(bundle_key(LOG, SEQ));
    keys.push(pointer::key(LOG));
    keys
}

fn stored(store: &Store, key: &str) -> Option<Vec<u8>> {
    store.get_blocking_opt(key).expect("store reads")
}

fn pointer_at(seq: u64) -> CutId {
    CutId { seq, ..AT }
}

fn put_pointer(store: &Store, cut: CutId) {
    store
        .put_blocking(&pointer::key(cut.log), pointer::encode(cut))
        .expect("pointer put");
}

/// Puts reach the inner store until `allow` of them have, then fail; the
/// keys every put reached are kept in order. Before the first put to
/// `checkpoint.json`, `race` writes another writer's object.
#[derive(Debug)]
struct Puts {
    inner: Arc<dyn ObjectStore>,
    allow: Option<usize>,
    reached: Arc<Mutex<Vec<String>>>,
    race: Mutex<Option<Vec<u8>>>,
}

impl fmt::Display for Puts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Puts({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Puts {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let raced = location
            .as_ref()
            .ends_with("checkpoint.json")
            .then(|| {
                self.race
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
            })
            .flatten();
        if let Some(bytes) = raced {
            self.inner
                .put_opts(location, PutPayload::from(bytes), PutOptions::default())
                .await
                .expect("the other writer's put");
        }
        {
            let mut reached = self.reached.lock().unwrap_or_else(PoisonError::into_inner);
            if self.allow.is_some_and(|allow| reached.len() >= allow) {
                return Err(object_store::Error::Generic {
                    store: "Puts",
                    source: "killed".into(),
                });
            }
            reached.push(location.to_string());
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// `plain` behind [`Puts`], and the keys its puts reach.
fn puts(
    plain: &Store,
    allow: Option<usize>,
    race: Option<CutId>,
) -> (Store, Arc<Mutex<Vec<String>>>) {
    let reached = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&reached);
    let store = plain.clone().wrap_backend(move |inner| {
        Arc::new(Puts {
            inner,
            allow,
            reached: seen,
            race: Mutex::new(race.map(pointer::encode)),
        })
    });
    (store, reached)
}

#[test]
fn every_snapshot_is_written_before_the_bundle_and_the_bundle_before_the_pointer() {
    let plain = Store::in_memory();
    let (store, reached) = puts(&plain, None, None);

    let published = write(&store, &sample(), &snapshots()).expect("writes");

    assert_eq!(published, Published::Advanced);
    assert_eq!(
        *reached.lock().unwrap_or_else(PoisonError::into_inner),
        keys()
    );
    for (key, bytes) in keys().iter().zip(snapshots()) {
        assert_eq!(stored(&plain, key), Some(bytes), "{key}");
    }
    assert_eq!(
        stored(&plain, &bundle_key(LOG, SEQ)),
        Some(sample().encode().expect("encodes"))
    );
    let pointer = stored(&plain, &pointer::key(LOG)).expect("pointer");
    assert_eq!(pointer::decode(&pointer, LOG), Ok(AT));
}

/// RFC0065.26: a flusher killed between any two of its bundle puts, or
/// before its `checkpoint.json` put, leaves `checkpoint.json` as it was,
/// and no `bundle.bin` is present before every `.snap` it names; a
/// retried put of identical bytes completes the checkpoint.
#[test]
fn a_writer_killed_between_any_two_puts_leaves_the_pointer_and_a_retry_completes() {
    let keys = keys();
    for allow in 0..keys.len() {
        let plain = Store::in_memory();
        put_pointer(&plain, pointer_at(100));
        let (store, _) = puts(&plain, Some(allow), None);

        let killed = write(&store, &sample(), &snapshots());

        assert!(
            matches!(killed, Err(WriteError::Store { .. })),
            "{allow}: {killed:?}"
        );
        let pointer = stored(&plain, &pointer::key(LOG)).expect("pointer");
        assert_eq!(
            pointer::decode(&pointer, LOG),
            Ok(pointer_at(100)),
            "{allow}"
        );
        if stored(&plain, &bundle_key(LOG, SEQ)).is_some() {
            for key in &keys[..TENANTS.len()] {
                assert!(stored(&plain, key).is_some(), "{allow}: {key}");
            }
        }
        assert_eq!(
            write(&plain, &sample(), &snapshots()).expect("the retry writes"),
            Published::Advanced,
            "{allow}"
        );
    }
}

#[test]
fn an_object_already_holding_other_bytes_is_kept_and_abandons_the_checkpoint() {
    for key in &keys()[..=TENANTS.len()] {
        let plain = Store::in_memory();
        put_pointer(&plain, pointer_at(100));
        plain.put_blocking(key, b"other".to_vec()).expect("put");

        let abandoned = write(&plain, &sample(), &snapshots());

        assert!(
            matches!(&abandoned, Err(WriteError::Differs { key: k }) if k == key),
            "{key}: {abandoned:?}"
        );
        assert_eq!(stored(&plain, key), Some(b"other".to_vec()), "{key}");
        let pointer = stored(&plain, &pointer::key(LOG)).expect("pointer");
        assert_eq!(pointer::decode(&pointer, LOG), Ok(pointer_at(100)), "{key}");
    }
}

/// RFC0065.26: `checkpoint.json` never moves back. A writer whose cut is
/// at or below the stored one leaves it and still has its bundle.
#[test]
fn a_pointer_at_or_above_the_cut_stays_and_the_bundle_is_still_complete() {
    for seq in [SEQ, 300] {
        let plain = Store::in_memory();
        put_pointer(&plain, pointer_at(seq));

        let published = write(&plain, &sample(), &snapshots()).expect("writes");

        assert_eq!(published, Published::Behind(pointer_at(seq)));
        let pointer = stored(&plain, &pointer::key(LOG)).expect("pointer");
        assert_eq!(pointer::decode(&pointer, LOG), Ok(pointer_at(seq)));
        assert!(stored(&plain, &bundle_key(LOG, SEQ)).is_some());
    }
}

#[test]
fn an_unreadable_pointer_is_never_replaced() {
    let plain = Store::in_memory();
    plain
        .put_blocking(&pointer::key(LOG), b"not json".to_vec())
        .expect("put");

    let failed = write(&plain, &sample(), &snapshots());

    assert!(
        matches!(failed, Err(WriteError::Unreadable(_))),
        "{failed:?}"
    );
    assert_eq!(
        stored(&plain, &pointer::key(LOG)),
        Some(b"not json".to_vec())
    );
}

#[test]
fn a_lost_race_rereads_and_decides_again() {
    for (racer, published) in [
        (pointer_at(300), Published::Behind(pointer_at(300))),
        (pointer_at(150), Published::Advanced),
    ] {
        let plain = Store::in_memory();
        put_pointer(&plain, pointer_at(100));
        let (store, _) = puts(&plain, None, Some(racer));

        assert_eq!(
            write(&store, &sample(), &snapshots()).expect("writes"),
            published
        );
        let expected = match published {
            Published::Advanced => AT,
            Published::Behind(stored) => stored,
        };
        let pointer = stored(&plain, &pointer::key(LOG)).expect("pointer");
        assert_eq!(pointer::decode(&pointer, LOG), Ok(expected));
    }
}

/// RFC0065.26: a new log incarnation over a bucket holding an old one's
/// objects checkpoints under its own log id from its first cut.
#[test]
fn a_new_log_incarnation_checkpoints_under_its_own_log_id() {
    let plain = Store::in_memory();
    put_pointer(
        &plain,
        CutId {
            log: 2,
            seq: 900,
            ..AT
        },
    );

    assert_eq!(
        write(&plain, &sample(), &snapshots()).expect("writes"),
        Published::Advanced
    );
    let pointer = stored(&plain, &pointer::key(LOG)).expect("pointer");
    assert_eq!(pointer::decode(&pointer, LOG), Ok(AT));
}

#[test]
fn snapshots_the_bundle_would_not_restore_write_nothing() {
    let mut wrong = snapshots();
    wrong[1] = marked("gamma");
    for given in [snapshots()[..1].to_vec(), wrong] {
        let plain = Store::in_memory();

        assert!(write(&plain, &sample(), &given).is_err());
        for key in keys() {
            assert_eq!(stored(&plain, &key), None, "{key}");
        }
    }
}
