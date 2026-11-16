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

use super::super::super::bundle::{Bundle, CutId, bundle_key, pointer, snapshot_key};
use super::super::super::flushing::Publishing;
use super::super::super::formation::{Formation, formed_key};
use super::super::super::mining::Mined;
use super::super::super::mining::tests::{batch_at, block, corpus, time};
use super::super::super::telemetry::Meters;
use super::super::super::types::{GroupEntry, NodeId};
use super::super::Shape;
use super::mining::{commit_all, mined_rows, with_learners};
use super::{Cluster, GROUP, name};
use crate::record_sink::FlushConfig;

const LOG: u32 = 9;

/// The voters of the cluster's groups, nodes 1 to 3, by name.
fn voters() -> Vec<String> {
    (1..=3).map(|n| name(NodeId::from(n))).collect()
}

/// A store holding `GROUP`'s `formed.json` with log id [`LOG`].
fn formed_store() -> Store {
    let store = Store::in_memory();
    let voters = voters();
    let names: Vec<&str> = voters.iter().map(String::as_str).collect();
    let members = (1..)
        .zip(&voters)
        .map(|(n, voter)| (voter.clone(), NodeId::from(n)))
        .collect();
    let formation = Formation::new(GROUP, LOG, members, &names).expect("formation");
    let bytes = formation.encode().expect("encode");
    store
        .put_blocking(&formed_key(GROUP), bytes)
        .expect("record");
    store
}

/// Publishing into `store`, flushing anything buffered for 50 ms.
fn publishing(store: Store) -> Publishing {
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
    }
}

/// Every row published to `store`'s data files, in position order.
fn published(store: &Store) -> Vec<MinedRecord> {
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let cut = loop {
        // An earlier checkpoint may cut before the first batch is mined.
        if let Some(cut) = pointer(&stores[0]).filter(|cut| cut.seq >= last) {
            break cut;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the flusher never checkpointed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let rows = published(&stores[0]);
    let through: Vec<_> = expected
        .iter()
        .filter(|row| row.ingest_position.is_some_and(|p| p.seq <= cut.seq))
        .collect();
    assert!(!through.is_empty(), "the cut passes mined rows");
    for row in through {
        assert!(rows.contains(row), "a row through the cut is published");
    }
    let bytes = stores[0]
        .get_blocking(&bundle_key(LOG, cut.seq))
        .expect("bundle.bin");
    let bundle = Bundle::decode(&bytes).expect("a bundle a replica reads");
    assert_eq!(bundle.cut(), cut);
    for entry in bundle.tenants() {
        let snap = stores[0]
            .get_blocking(&snapshot_key(LOG, cut.seq, &entry.tenant))
            .expect(".snap");
        bundle
            .snapshot(entry, &snap)
            .expect("a snapshot it restores");
    }
    assert_eq!(
        pointer(&stores[1]),
        None,
        "only the named replica checkpoints"
    );
    cluster.shutdown().await;
}
