//! Startup crash fixture for the RFC0059.3 and RFC0059.6 kill legs.
//!
//! Not a product binary — declared as a `[[bin]]` only so the tests can
//! spawn it as a real OS process and `SIGKILL` it. It runs startup
//! recovery the way `serve` does, against a WAL root and a local store.
//!
//! Usage: `template_ids_crash_fixture <reserve|scan> <wal_root> <store_root>`.
//!
//! - `reserve`: recovery seats the high-water and reserves its blocks, the
//!   fixture prints `STARTED`, and then waits, minting nothing.
//! - `scan`: the store's first read of a data file — a footer read of the
//!   bootstrap scan — prints `SCANNING <key>` and never returns, so the
//!   kill lands mid-scan.
//!
//! It never exits on its own; the parent kills it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use ourios_config::MinerConfig;
use ourios_ingester::recovery;
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig};

fn say(line: &str) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}").expect("fixture: write");
    stdout.flush().expect("fixture: flush");
}

/// The WAL knobs the in-process restart of the RFC 0059 tests uses.
fn wal_config(root: PathBuf) -> WalConfig {
    WalConfig {
        root,
        batch_window_ms: 20,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 1,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

type BoxStream<T> = std::pin::Pin<Box<dyn futures_core::Stream<Item = T> + Send>>;

/// A local store whose first read under `data/` parks forever.
#[derive(Debug)]
struct PausingStore(Arc<dyn ObjectStore>);

impl std::fmt::Display for PausingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PausingStore({})", self.0)
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausingStore {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.0.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.0.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if location.as_ref().starts_with("data/") {
            say(&format!("SCANNING {location}"));
            std::future::pending::<()>().await;
        }
        self.0.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<object_store::Result<Key>>,
    ) -> BoxStream<object_store::Result<Key>> {
        self.0.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<object_store::Result<ObjectMeta>> {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }
}

/// Run startup recovery over `wal_root` and `store`, as `serve` does.
fn start(wal_root: &Path, store: Store) -> TemplateIds {
    let ids = TemplateIds::new(store).with_bootstrap_allowed(true);
    let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
    let mut wal = Wal::open(wal_config(wal_root.to_path_buf())).expect("fixture: Wal::open");
    recovery::recover(&mut wal, &wal_root.join("snapshots"), &mut miner, &ids)
        .expect("fixture: recover");
    ids
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut arg = |name| {
        args.next()
            .unwrap_or_else(|| panic!("fixture: missing <{name}>"))
    };
    let (mode, wal_root, store_root) = (arg("mode"), arg("wal_root"), arg("store_root"));
    let store = Store::local(&store_root).expect("fixture: store");
    let store = match mode.as_str() {
        "reserve" => store,
        "scan" => store.wrap_backend(|inner| Arc::new(PausingStore(inner))),
        other => panic!("fixture: unknown mode {other}"),
    };
    let _ids = start(Path::new(&wal_root), store);
    say("STARTED");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
