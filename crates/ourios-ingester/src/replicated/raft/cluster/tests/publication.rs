//! Publication from the tail replica a flusher assignment names, on the
//! in-process cluster (RFC 0065 §6.5, §6.7): it publishes every row of
//! the group under the log id `formed.json` records, those mined before
//! the assignment and after it, and the other tail replica publishes
//! nothing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ourios_core::record::{IngestPosition, MinedRecord};
use ourios_parquet::{PromotedAttributes, Reader, Store};
use ourios_wal::Control;
use tokio::sync::mpsc;

use super::super::super::bundle::{Bundle, CutId, bundle_key, pointer, snapshot_key};
use super::super::super::flushing::Publishing;
use super::super::super::formation::{Formation, formed_key};
use super::super::super::host::GroupMined;
use super::super::super::mining::Mined;
use super::super::super::mining::tests::{batch_at, block, corpus, time};
use super::super::super::proposals::Proposals;
use super::super::super::telemetry::Meters;
use super::super::super::types::GroupEntry;
use super::super::Shape;
use super::super::cluster_node;
use super::mining::{commit_all, mined_rows, with_learners};
use super::{Cluster, GROUP, name};
use crate::record_sink::FlushConfig;

pub(super) const LOG: u32 = 9;

/// The voters of the cluster's groups, nodes 1 to 3, by name.
fn voters() -> Vec<String> {
    (1..=3).map(|n| name(cluster_node(n))).collect()
}

/// A store holding `GROUP`'s `formed.json` with log id [`LOG`].
pub(super) fn formed_store() -> Store {
    let store = Store::in_memory();
    form(&store);
    store
}

/// Record `GROUP`'s `formed.json` in `store`, with log id [`LOG`].
pub(super) fn form(store: &Store) {
    let voters = voters();
    let names: Vec<&str> = voters.iter().map(String::as_str).collect();
    let members = (1..)
        .zip(&voters)
        .map(|(n, voter)| (voter.clone(), cluster_node(n)))
        .collect();
    let formation = Formation::new(GROUP, LOG, members, &names).expect("formation");
    let bytes = formation.encode().expect("encode");
    store
        .put_blocking(&formed_key(GROUP), bytes)
        .expect("record");
}

/// Publishing into `store`, flushing anything buffered for 50 ms.
pub(super) fn publishing(store: Store) -> Publishing {
    Publishing {
        store,
        voters: BTreeMap::from([(GROUP, voters())]),
        flush: FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_millis(50),
            ceiling_bytes: usize::MAX,
        },
        promoted: PromotedAttributes::default(),
        audit_ceiling: 1 << 20,
        #[cfg(feature = "openfga")]
        graph_emitter: None,
    }
}

/// Every row published to `store`'s data files, in position order.
pub(super) fn published(store: &Store) -> Vec<MinedRecord> {
    let keys = store.list_blocking(Some("data")).expect("list");
    let mut rows: Vec<_> = keys
        .iter()
        .filter(|key| key.ends_with(".parquet"))
        .flat_map(|key| {
            let bytes = store.get_blocking(key).expect("get");
            let reader = Reader::open_bytes(bytes.into()).expect("open");
            reader.read_all().expect("read")
        })
        .collect();
    rows.sort_by_key(|row| row.ingest_position);
    rows
}

/// What `mined` holds, as published under [`LOG`].
fn under_log(mined: &[Mined]) -> Vec<MinedRecord> {
    mined
        .iter()
        .flat_map(|m| &m.rows)
        .map(|row| MinedRecord {
            ingest_position: Some(IngestPosition {
                group: LOG,
                seq: row.position.seq,
                index: row.position.index,
            }),
            ..row.record.clone()
        })
        .collect()
}

/// RFC 0065 §6.7 on a running group: the tail replica a flusher
/// assignment names publishes the rows it mined before the assignment and
/// those after, every one under the group's log id, and the other tail
/// replica publishes none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_named_tail_replica_publishes_every_row_under_the_log_id() {
    let stores = [formed_store(), formed_store()];
    let shape = Shape {
        publishing: stores
            .iter()
            .cloned()
            .map(publishing)
            .map(Arc::new)
            .collect(),
        ..Shape::from((3, 2))
    };
    let (cluster, mut mined) = Cluster::start_mining(shape, &[GROUP], |_| Meters::global()).await;
    let leader = with_learners(&cluster, 3..5).await;
    let lines = corpus();
    let batches = |range: std::ops::Range<usize>| {
        let chunks = lines[range].chunks(5).enumerate();
        chunks
            .map(|(n, chunk)| batch_at(time(n as u64 + 1), "acme", chunk))
            .collect::<Vec<_>>()
    };
    let named = *cluster.id(3).uuid().as_bytes();
    let mut entries = vec![block(0, 10_000)];
    entries.extend(batches(0..30));
    entries.push(GroupEntry::Control(Control::FlusherAssignment(named)));
    entries.extend(batches(30..60));
    commit_all(&cluster, leader, &entries).await;
    let expected = under_log(&mined_rows(&mut mined[0], 60).await);
    mined_rows(&mut mined[1], 60).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while published(&stores[0]).len() < expected.len() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the flusher never published"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(published(&stores[0]), expected);
    assert_eq!(
        published(&stores[1]),
        [],
        "only the named replica publishes"
    );
    cluster.shutdown().await;
}

/// The cut `checkpoint.json` in `store` names for [`LOG`], once there is one.
fn pointer(store: &Store) -> Option<CutId> {
    let bytes = store.get_blocking_opt(&pointer::key(LOG)).expect("read")?;
    Some(pointer::decode(&bytes, LOG).expect("a readable checkpoint.json"))
}

/// The checkpoint `checkpoint.json` in `store` names once it reaches
/// `last`: an earlier one may cut before the first batch is mined.
pub(super) async fn checkpointed_through(store: &Store, last: u64) -> CutId {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(cut) = pointer(store).filter(|cut| cut.seq >= last) {
            return cut;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the flusher never checkpointed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `cut`'s bundle in `store` is one a replica restores: `bundle.bin`
/// names the cut and admits every tenant's `.snap`.
fn assert_restorable(store: &Store, cut: CutId) {
    let bytes = store
        .get_blocking(&bundle_key(LOG, cut.seq))
        .expect("bundle.bin");
    let bundle = Bundle::decode(&bytes).expect("a bundle a replica reads");
    assert_eq!(bundle.cut(), cut);
    for entry in bundle.tenants() {
        let snap = store
            .get_blocking(&snapshot_key(LOG, cut.seq, &entry.tenant))
            .expect(".snap");
        bundle
            .snapshot(entry, &snap)
            .expect("a snapshot it restores");
    }
}

/// RFC 0065 §6.5 on a running group: the named flusher runs RFC 0052's
/// barrier, publishing every row through its cut and then writing a
/// bundle for the cut that a replica would restore and `checkpoint.json`
/// naming it; the other tail replica writes neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_named_flusher_checkpoints_its_group() {
    let stores = [formed_store(), formed_store()];
    let shape = Shape {
        publishing: stores
            .iter()
            .cloned()
            .map(publishing)
            .map(Arc::new)
            .collect(),
        ..Shape::from((3, 2))
    };
    let (cluster, mut mined) = Cluster::start_mining(shape, &[GROUP], |_| Meters::global()).await;
    let leader = with_learners(&cluster, 3..5).await;
    let lines = corpus();
    let named = *cluster.id(3).uuid().as_bytes();
    let mut entries = vec![
        block(0, 10_000),
        GroupEntry::Control(Control::FlusherAssignment(named)),
    ];
    let chunks = lines[..30].chunks(5).enumerate();
    entries.extend(chunks.map(|(n, chunk)| batch_at(time(n as u64 + 1), "acme", chunk)));
    commit_all(&cluster, leader, &entries).await;
    let expected = under_log(&mined_rows(&mut mined[0], 30).await);
    mined_rows(&mut mined[1], 30).await;

    let last = expected
        .iter()
        .filter_map(|row| row.ingest_position)
        .map(|position| position.seq)
        .max()
        .expect("mined rows");
    let cut = checkpointed_through(&stores[0], last).await;
    let rows = published(&stores[0]);
    let through: Vec<_> = expected
        .iter()
        .filter(|row| row.ingest_position.is_some_and(|p| p.seq <= cut.seq))
        .collect();
    assert!(!through.is_empty(), "the cut passes mined rows");
    for row in through {
        assert!(rows.contains(row), "a row through the cut is published");
    }
    assert_restorable(&stores[0], cut);
    assert_eq!(
        pointer(&stores[1]),
        None,
        "only the named replica checkpoints"
    );
    cluster.shutdown().await;
}

/// A running group whose nodes all serve proposals, its named flusher
/// node 3 having mined thirty rows; its stores, and the log id of its last
/// mined entry.
async fn checkpointing_group() -> (Cluster, [Store; 2], u64) {
    let (cluster, stores, last, _) = reporting_group().await;
    (cluster, stores, last)
}

/// As [`checkpointing_group`], with its leader and each node's `Propose`
/// target.
async fn reporting_group() -> (Cluster, [Store; 2], u64, (usize, Vec<Arc<Proposals>>)) {
    let (cluster, stores, last, proposals, _) = mining_group().await;
    (cluster, stores, last, proposals)
}

/// As [`reporting_group`], with what each tail replica mines.
pub(super) async fn mining_group() -> (
    Cluster,
    [Store; 2],
    u64,
    (usize, Vec<Arc<Proposals>>),
    Vec<mpsc::Receiver<GroupMined>>,
) {
    mining_group_over([formed_store(), formed_store()]).await
}

/// As [`mining_group`], its tail replicas publishing to `stores`.
pub(super) async fn mining_group_over(
    stores: [Store; 2],
) -> (
    Cluster,
    [Store; 2],
    u64,
    (usize, Vec<Arc<Proposals>>),
    Vec<mpsc::Receiver<GroupMined>>,
) {
    let shape = Shape {
        publishing: stores
            .iter()
            .cloned()
            .map(publishing)
            .map(Arc::new)
            .collect(),
        ..Shape::from((3, 2))
    };
    let (cluster, mut mined) = Cluster::start_mining(shape, &[GROUP], |_| Meters::global()).await;
    let mut targets = Vec::new();
    for i in 0..5 {
        let host = &cluster.node(i).host;
        let target = Arc::new(Proposals::new(host));
        host.server().serve_proposals(Arc::clone(&target) as _);
        targets.push(target);
    }
    let leader = with_learners(&cluster, 3..5).await;
    let named = *cluster.id(3).uuid().as_bytes();
    let mut entries = vec![
        block(0, 10_000),
        GroupEntry::Control(Control::FlusherAssignment(named)),
    ];
    let lines = corpus();
    let chunks = lines[..30].chunks(5).enumerate();
    entries.extend(chunks.map(|(n, chunk)| batch_at(time(n as u64 + 1), "acme", chunk)));
    commit_all(&cluster, leader, &entries).await;
    let expected = under_log(&mined_rows(&mut mined[0], 30).await);
    mined_rows(&mut mined[1], 30).await;
    let last = expected
        .iter()
        .filter_map(|row| row.ingest_position)
        .map(|position| position.seq)
        .max()
        .expect("mined rows");
    (cluster, stores, last, (leader, targets), mined)
}

/// The `bundle.bin` keys `store` holds for [`LOG`].
fn bundles(store: &Store) -> Vec<String> {
    let keys = store
        .list_blocking(Some(&format!("wal/replicated/logs/{LOG}/checkpoints/")))
        .expect("list");
    keys.into_iter()
        .filter(|key| key.ends_with("bundle.bin"))
        .collect()
}

/// Every node applies a checkpoint at or past `seq`; fails after 20 s.
pub(super) async fn applied_everywhere(cluster: &Cluster, seq: u64) {
    for i in 0..5 {
        let sm = cluster.node(i).host.state_machine(GROUP).expect("hosted");
        let mut cuts = sm.checkpoints();
        let applied = tokio::time::timeout(
            Duration::from_secs(20),
            cuts.wait_for(|applied| applied.is_some_and(|c| c.index >= seq)),
        )
        .await;
        assert!(matches!(applied, Ok(Ok(_))), "node {i} never applied {seq}");
    }
}

/// RFC 0065 §6.5, §6.8 on a running group: once the named flusher has
/// written a checkpoint, it proposes the checkpoint entry to the leader,
/// and every replica applies it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_named_flushers_checkpoint_applies_on_every_replica() {
    let (cluster, stores, last) = checkpointing_group().await;

    let cut = checkpointed_through(&stores[0], last).await;

    applied_everywhere(&cluster, cut.seq).await;
    cluster.shutdown().await;
}

/// A group that takes nothing after its checkpoint applies writes no
/// further one: the checkpoint entry itself changes nothing a bundle
/// holds, so barrier after barrier cuts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_group_writes_no_further_checkpoint() {
    let (cluster, stores, last) = checkpointing_group().await;
    let cut = checkpointed_through(&stores[0], last).await;
    applied_everywhere(&cluster, cut.seq).await;

    let before = bundles(&stores[0]);

    // Three barrier intervals, at the one-second floor.
    tokio::time::sleep(Duration::from_millis(3_500)).await;

    assert_eq!(pointer(&stores[0]), Some(cut), "checkpoint.json unchanged");
    assert_eq!(bundles(&stores[0]), before, "no further bundle");
    cluster.shutdown().await;
}

/// RFC 0065 §6.8: once its checkpoint applies, the named flusher reports
/// to its leader from the checkpoint entry, past the cut, which only a
/// report can carry: a checkpoint's own proposal reports from its cut, and
/// an idle group writes no further checkpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_named_flusher_reports_to_its_leader() {
    let (cluster, stores, last, (leader, targets)) = reporting_group().await;
    let reports = targets[leader].reports(GROUP).expect("hosted");
    let flusher = cluster.id(3);
    let reported_from = |at_least: u64| {
        let reports = reports.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                let kept = reports.latest();
                if kept.is_some_and(|r| r.proposer == flusher && r.mined.index >= at_least) {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no report from {at_least}: {kept:?}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    let cut = checkpointed_through(&stores[0], last).await;
    applied_everywhere(&cluster, cut.seq).await;
    reported_from(cut.seq + 1).await;
    cluster.shutdown().await;
}
