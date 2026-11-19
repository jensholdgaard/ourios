//! RFC0065.3's steady stream of exports, and what it requires of the store
//! once the stream stops: every acknowledged record live in Parquet, and
//! no `(tenant, ingest position)` twice.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_core::record::MinedRecord;
use ourios_parquet::{LiveSet, MANIFEST_FILENAME, Manifest, Reader, Store, compacted_generation};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::served::{SEQ, TENANT, numbered};

/// Exports sent at once, so a kill lands mid-batch.
const SENDERS: usize = 4;

/// The voters the stream sends to, by name: a killed one is taken out, a
/// restarted one put back at its new address.
#[derive(Clone, Default)]
pub(super) struct Targets(Arc<Mutex<BTreeMap<String, SocketAddr>>>);

impl Targets {
    pub(super) fn insert(&self, name: &str, grpc: SocketAddr) {
        self.lock().insert(name.to_owned(), grpc);
    }

    pub(super) fn remove(&self, name: &str) {
        self.lock().remove(name);
    }

    /// The `n`th live target, round robin.
    fn nth(&self, n: u64) -> Option<SocketAddr> {
        let targets = self.lock();
        let len = u64::try_from(targets.len()).ok().filter(|&len| len > 0)?;
        let at = usize::try_from(n % len).ok()?;
        targets.values().nth(at).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, SocketAddr>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The stream: each export one record with its own `seq`, stamped
/// `time_unix_nano`; the ones acknowledged.
pub(super) struct Stream {
    acked: Arc<Mutex<BTreeSet<u64>>>,
    /// The next `seq` a sender takes.
    next: Arc<AtomicU64>,
    senders: Vec<JoinHandle<()>>,
}

impl Stream {
    /// Send to `targets` from [`SENDERS`] tasks until `stop`.
    pub(super) fn start(
        targets: &Targets,
        time_unix_nano: u64,
        stop: &watch::Receiver<bool>,
    ) -> Self {
        let acked = Arc::new(Mutex::new(BTreeSet::new()));
        let next = Arc::new(AtomicU64::new(1));
        let senders = (0..SENDERS)
            .map(|_| {
                let send = Send {
                    targets: targets.clone(),
                    next: Arc::clone(&next),
                    acked: Arc::clone(&acked),
                    time_unix_nano,
                };
                tokio::spawn(send.run(stop.clone()))
            })
            .collect();
        Self {
            acked,
            next,
            senders,
        }
    }

    /// The first `seq` not yet sent: every export from it on is sent after
    /// this call.
    pub(super) fn unsent(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// Whether an export of `seq` `from` or later is acknowledged within
    /// `patience`: one sent after the moment `from` was [`Self::unsent`].
    pub(super) async fn acks_from(&self, from: u64, patience: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + patience;
        while tokio::time::Instant::now() < deadline {
            let acked = self
                .acked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .range(from..)
                .next()
                .is_some();
            if acked {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Wait for the senders, which stop once asked; the `seq`s acknowledged.
    pub(super) async fn finish(self) -> BTreeSet<u64> {
        for sender in self.senders {
            sender.await.expect("a sender");
        }
        Arc::try_unwrap(self.acked)
            .map(|acked| {
                acked
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .expect("every sender joined")
    }
}

/// One sender of the stream.
struct Send {
    targets: Targets,
    next: Arc<AtomicU64>,
    acked: Arc<Mutex<BTreeSet<u64>>>,
    time_unix_nano: u64,
}

impl Send {
    async fn run(self, mut stop: watch::Receiver<bool>) {
        while !*stop.borrow() {
            let seq = self.next.fetch_add(1, Ordering::Relaxed);
            let acknowledged = tokio::select! {
                acknowledged = self.send(seq) => acknowledged,
                _ = stop.wait_for(|stop| *stop) => return,
            };
            if acknowledged {
                self.acked
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(seq);
            } else {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    /// Whether the export of `seq` is acknowledged. A refused or lost one
    /// may still have committed; only an acknowledged one must survive.
    async fn send(&self, seq: u64) -> bool {
        let Some(grpc) = self.targets.nth(seq) else {
            return false;
        };
        let attempt = async {
            let mut client = LogsServiceClient::connect(format!("http://{grpc}"))
                .await
                .ok()?;
            let request = numbered(seq, self.time_unix_nano);
            client.export(request).await.ok()
        };
        matches!(
            tokio::time::timeout(Duration::from_secs(5), attempt).await,
            Ok(Some(_))
        )
    }
}

/// Wait until the live rows on `store` hold every `acked` record and no
/// `(tenant, ingest position)` twice, on two reads a compaction cycle apart
/// that found the same live files, failing after `patience` with what was
/// missing or doubled.
pub(super) async fn published(store: &Store, acked: &BTreeSet<u64>, patience: Duration) {
    let deadline = tokio::time::Instant::now() + patience;
    let mut confirming: Option<Vec<String>> = None;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        let store = store.clone();
        let live = tokio::task::spawn_blocking(move || live_rows(&store))
            .await
            .expect("read");
        match live.and_then(|live| settled(&live, acked).map(|()| live.into_keys().collect())) {
            Ok(files) if confirming.as_ref() == Some(&files) => return,
            Ok(files) => {
                confirming = Some(files);
                "settled once, its live files not confirmed".clone_into(&mut last);
            }
            Err(why) => {
                confirming = None;
                last = why;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("the acknowledged records did not settle in Parquet: {last}");
}

/// Wait until the group's checkpoint covers every acknowledged record, as
/// RFC0065.3 says of "the next checkpoint": `checkpoint.json` names a cut
/// at or past the highest log index an acknowledged row carries, and that
/// cut's bundle is written. The stream was acknowledged after the last
/// kill, so such a cut was written after it too. Fails after `patience`
/// with the last cut seen.
pub(super) async fn checkpointed(store: &Store, acked: &BTreeSet<u64>, patience: Duration) {
    use ourios_ingester::replicated::raft::bundle::{bundle_key, pointer};

    let reading = store.clone();
    let live = tokio::task::spawn_blocking(move || live_rows(&reading))
        .await
        .expect("read")
        .expect("the live rows");
    let highest = live
        .values()
        .flatten()
        .filter(|row| row.tenant_id.as_str() == TENANT)
        .filter(|row| seq_of(row).is_some_and(|seq| acked.contains(&seq)))
        .filter_map(|row| row.ingest_position)
        .max_by_key(|position| position.seq)
        .expect("acknowledged rows");
    let deadline = tokio::time::Instant::now() + patience;
    let mut last = None;
    while tokio::time::Instant::now() < deadline {
        let store = store.clone();
        let covered = tokio::task::spawn_blocking(move || {
            let bytes = store
                .get_blocking_opt(&pointer::key(highest.group))
                .ok()??;
            let cut = pointer::decode(&bytes, highest.group).ok()?;
            let bundled = store
                .get_blocking_opt(&bundle_key(highest.group, cut.seq))
                .ok()?
                .is_some();
            Some((cut.seq, bundled))
        })
        .await
        .expect("read");
        if let Some((seq, true)) = covered
            && seq >= highest.seq
        {
            return;
        }
        last = covered;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!(
        "no checkpoint covers the acknowledged records through {}: last cut {last:?}",
        highest.seq
    );
}

/// Whether the `live` files' rows hold every record [`TENANT`] was
/// acknowledged for, each position once.
fn settled(live: &BTreeMap<String, Vec<MinedRecord>>, acked: &BTreeSet<u64>) -> Result<(), String> {
    let rows: Vec<&MinedRecord> = live.values().flatten().collect();
    let stored: BTreeSet<u64> = rows
        .iter()
        .filter(|row| row.tenant_id.as_str() == TENANT)
        .filter_map(|row| seq_of(row))
        .collect();
    let missing: Vec<_> = acked.difference(&stored).take(10).collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} of {} acknowledged missing, e.g. {missing:?}",
            acked.difference(&stored).count(),
            acked.len()
        ));
    }
    let mut positions = HashSet::new();
    for row in rows {
        let position = row
            .ingest_position
            .ok_or_else(|| format!("a row without an ingest position: {:?}", seq_of(row)))?;
        if !positions.insert((row.tenant_id.clone(), position)) {
            return Err(format!("{position:?} appears twice"));
        }
    }
    Ok(())
}

fn seq_of(row: &MinedRecord) -> Option<u64> {
    let value = row
        .attributes
        .iter()
        .find(|kv| kv.key == SEQ)?
        .value
        .as_ref()?;
    match value.value.as_ref()? {
        Value::StringValue(seq) => seq.parse().ok(),
        _ => None,
    }
}

/// The rows of every live file under `data/` (RFC 0009 §3.4's live set,
/// per partition), once compaction has taken every flush file of a
/// partition published in more than one: a position published twice, by a
/// flusher and by the one that took over, is in two files until compaction
/// keeps one. Keyed by each file's key. Why not, before that or when a file
/// went while it was read, which a compaction's collection does.
fn live_rows(store: &Store) -> Result<BTreeMap<String, Vec<MinedRecord>>, String> {
    let keys = store
        .list_blocking(Some("data/"))
        .map_err(|e| e.to_string())?;
    let mut partitions: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (dir, name) in keys.iter().filter_map(|key| key.rsplit_once('/')) {
        partitions.entry(dir).or_default().push(name);
    }
    if partitions.is_empty() {
        return Err("nothing published under data/ yet".to_owned());
    }
    let mut live = BTreeMap::new();
    for (dir, names) in partitions {
        live.extend(partition_rows(store, dir, &names)?);
    }
    Ok(live)
}

/// The rows of partition `dir`'s live files among `names`.
fn partition_rows(
    store: &Store,
    dir: &str,
    names: &[&str],
) -> Result<BTreeMap<String, Vec<MinedRecord>>, String> {
    let manifest = Manifest::read_with_etag(store, &format!("{dir}/{MANIFEST_FILENAME}"))
        .map_err(|e| e.to_string())?
        .map(|(manifest, _)| manifest);
    let files: Vec<_> = names
        .iter()
        .filter(|name| name.ends_with(".parquet"))
        .collect();
    // As the querier resolves it: the compacted files the manifest names,
    // whether or not the listing caught them, and the listed flush files it
    // does not consume.
    let live = LiveSet::new(manifest.as_ref());
    let compacted = manifest
        .iter()
        .flat_map(|m| m.files.iter().map(String::as_str));
    let flush: Vec<&str> = files
        .into_iter()
        .copied()
        .filter(|name| compacted_generation(name).is_none() && live.is_live(name))
        .collect();
    // Settled once compaction has taken every flush file: a lone flush file
    // can hold no position twice, but one published beside a manifest, or
    // beside another, has not been compacted yet.
    if flush.len() > usize::from(manifest.is_none()) {
        return Err(format!("{dir} not compacted yet: {flush:?} uncompacted"));
    }
    let mut live = BTreeMap::new();
    for name in compacted.chain(flush) {
        let key = format!("{dir}/{name}");
        let rows = file_rows(store, &key)?;
        live.insert(key, rows);
    }
    Ok(live)
}

fn file_rows(store: &Store, key: &str) -> Result<Vec<MinedRecord>, String> {
    let bytes = store.get_blocking(key).map_err(|e| e.to_string())?;
    Reader::open_bytes(bytes.into())
        .and_then(Reader::read_all)
        .map_err(|e| e.to_string())
}
