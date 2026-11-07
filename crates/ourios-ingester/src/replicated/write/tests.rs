//! The write path on the in-process cluster: RFC0065.2's replication
//! arms, RFC0065.15's write arm, and how each `client_write` outcome
//! becomes a rejection. "Visible" is committed and applied on every
//! voter until the tail exists.

mod failover;
mod forwarding;
mod outage;
mod spans;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use openraft::EntryPayload;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_core::tenant::TenantId;
use ourios_parquet::Store;

use super::{GroupWriter, WriteError};
use crate::receiver::{Export, ReceiveError};
use crate::replicated::ingest::{Ingested, ReplicatedIngest};
use crate::replicated::layout::{LayoutCheck, VotingLayout, record_or_check};
use crate::replicated::raft::cluster::{Cluster, batch, name, write};
use crate::replicated::raft::gate::SyncGate;
use crate::replicated::raft::types::TenantBatchBytes;

fn layout(cluster: &Cluster, groups: u32) -> VotingLayout {
    let peers = (0..3).map(|i| name(cluster.id(i)));
    VotingLayout::new(peers, NonZeroU32::new(groups).expect("non-zero"), 3)
}

fn writer(cluster: &Cluster, node: usize) -> GroupWriter {
    GroupWriter::new(layout(cluster, 1), &cluster.node(node).host)
}

fn export(line: &str) -> Export<'static> {
    let record = LogRecord {
        body: Some(AnyValue {
            value: Some(Value::StringValue(line.to_owned())),
        }),
        ..Default::default()
    };
    let request = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![record],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    Export {
        request,
        tenant: TenantId::new("acme"),
        binding: None,
        lenient_json: false,
    }
}

/// The leader of group 0, once it has committed an entry of its term.
async fn ready_leader(cluster: &Cluster) -> usize {
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 0).await;
    leader
}

fn followers(leader: usize) -> Vec<usize> {
    (0..3).filter(|&i| i != leader).collect()
}

async fn held(gate: &Arc<SyncGate>) {
    let gate = Arc::clone(gate);
    tokio::task::spawn_blocking(move || gate.wait_for_writer())
        .await
        .expect("a writer reaches the gate");
}

fn committed(ingested: Ingested) -> super::GroupPosition {
    match ingested {
        Ingested::Committed { position, .. } => position,
        Ingested::Empty => panic!("the export had a record"),
    }
}

fn rejected_as(error: &ReceiveError) -> &'static str {
    match error {
        ReceiveError::Replication(e) => e.error_type(),
        other => panic!("expected a replication rejection, got {other}"),
    }
}

/// RFC0065.2 (B1): with both followers' fsyncs held, the export is not
/// acknowledged; it is once one of them fsyncs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_2_an_ack_waits_for_a_second_voters_fsync() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let [first, second] = followers(leader)[..] else {
        unreachable!("three voters")
    };
    for f in [first, second] {
        cluster.node(f).gate.close();
    }
    let ingest = Arc::new(ReplicatedIngest::new(writer(&cluster, leader)));
    let sending = Arc::clone(&ingest);
    let acked = tokio::spawn(async move { sending.ingest(export("held")).await });
    for f in [first, second] {
        held(&cluster.node(f).gate).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !acked.is_finished(),
        "acknowledged before a second voter fsynced"
    );

    let fsynced = cluster.node(first).gate.passed();
    cluster.node(first).gate.open();
    let position = committed(acked.await.expect("join").expect("acknowledged"));
    assert!(cluster.node(first).gate.passed() > fsynced);
    let log = cluster.node(first).log(0).await;
    assert!(log.iter().any(|e| e.log_id.index == position.seq));
    cluster.node(second).gate.open();
    cluster.shutdown().await;
}

/// RFC0065.2 (B3): with both followers down the export gets the retryable
/// rejection, and its entry, committed once they return, is visible on
/// every voter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_2_without_a_quorum_the_export_is_rejected_retryably() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let [first, second] = followers(leader)[..] else {
        unreachable!("three voters")
    };
    cluster.kill(first).await;
    cluster.kill(second).await;
    let writer = writer(&cluster, leader).with_timeout(Duration::from_millis(500));
    let rejected = ReplicatedIngest::new(writer)
        .ingest(export("stranded"))
        .await
        .expect_err("no quorum");
    assert_eq!(rejected_as(&rejected), "not_enough_replicas");

    let stranded = cluster.node(leader).log(0).await.pop().expect("an entry");
    assert!(matches!(stranded.payload, EntryPayload::Normal(_)));
    // One follower at a time: the first cannot win an election against
    // the leader's longer log, so the stranded entry is the one that
    // commits.
    cluster.restart(first).await;
    cluster
        .node(leader)
        .applied_through(0, stranded.log_id.index)
        .await;
    cluster.restart(second).await;
    for node in 0..3 {
        cluster
            .node(node)
            .applied_through(0, stranded.log_id.index)
            .await;
        let log = cluster.node(node).log(0).await;
        assert!(log.contains(&stranded), "node {node} holds the entry");
    }
    cluster.shutdown().await;
}

/// RFC0065.15 (B10), the write arm: exports are acknowledged while the
/// object store is unreachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_15_exports_are_acknowledged_during_a_store_outage() {
    let cluster = Cluster::start(3, &[0]).await;
    let (store, outage) = outage::over(Store::in_memory());
    let configured = layout(&cluster, 1);
    let check = |store: Store, layout: VotingLayout| {
        tokio::task::spawn_blocking(move || record_or_check(&store, &layout))
    };
    let recorded = check(store.clone(), configured.clone())
        .await
        .expect("join");
    assert!(matches!(recorded, Ok(LayoutCheck::Recorded)));
    outage.begin();
    let during = check(store.clone(), configured.clone())
        .await
        .expect("join");
    assert!(during.is_err(), "the store is unreachable");

    let leader = ready_leader(&cluster).await;
    let ingest = ReplicatedIngest::new(writer(&cluster, leader));
    for n in 0..5 {
        let line = format!("during the outage {n}");
        let ingested = ingest.ingest(export(&line)).await;
        committed(ingested.expect("acknowledged during the outage"));
    }
    outage.end();
    let after = check(store, configured).await.expect("join");
    assert!(matches!(after, Ok(LayoutCheck::Matches)));
    cluster.shutdown().await;
}

/// A follower refuses before appending and names the leader, the hint
/// forwarding (§6.8) will follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_rejects_naming_the_leader() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let follower = followers(leader)[0];
    let refused = writer(&cluster, follower).commit("acme", batch(1)).await;
    match refused {
        Err(e @ WriteError::NotLeader { group: 0, .. }) => {
            assert_eq!(e.error_type(), "group_unavailable");
            let WriteError::NotLeader { leader: hint, .. } = e else {
                unreachable!()
            };
            assert_eq!(hint, Some(name(cluster.id(leader))));
        }
        other => panic!("expected NotLeader, got {other:?}"),
    }
    cluster.shutdown().await;
}

/// A tenant whose group this node does not vote in is refused, and so is
/// one whose group's Raft has stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unhosted_or_stopped_group_is_unavailable() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let two_groups = layout(&cluster, 2);
    let tenant = (0..100)
        .map(|n| format!("tenant-{n}"))
        .find(|t| two_groups.group_of(t) == 1)
        .expect("a tenant in group 1");
    let unhosted = GroupWriter::new(two_groups, &cluster.node(leader).host)
        .commit(&tenant, batch(1))
        .await;
    assert!(matches!(unhosted, Err(WriteError::NotHosted { group: 1 })));

    let hosted = writer(&cluster, leader);
    cluster.kill(leader).await;
    let stopped = hosted.commit("acme", batch(2)).await;
    match stopped {
        Err(e @ WriteError::Raft { group: 0, .. }) => {
            assert_eq!(e.error_type(), "group_unavailable");
        }
        other => panic!("expected a Raft failure, got {other:?}"),
    }
    cluster.shutdown().await;
}

/// A batch whose `TenantOtlpBatch` payload is exactly `len` bytes.
fn batch_of(tenant: &str, len: usize) -> TenantBatchBytes {
    let prefix = ourios_wal::TenantBatch::encode(tenant, b"")
        .expect("prefix")
        .len();
    let payload = ourios_wal::TenantBatch::encode(tenant, &vec![0; len - prefix]).expect("batch");
    TenantBatchBytes::new(payload).expect("valid")
}

/// RFC 0061 §3.2: an export whose entry would not fit one WAL frame is
/// refused before it is proposed, so the group's log never sees it and
/// the next export commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entry_over_the_wal_frame_is_refused_and_the_group_keeps_committing() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let writer = writer(&cluster, leader);
    let limit = ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES;
    match writer.commit("acme", batch_of("acme", limit + 1)).await {
        Err(WriteError::TooLarge {
            len,
            limit: refused,
        }) => {
            assert_eq!((len, refused), (limit + 1, limit));
        }
        other => panic!("expected the size rejection, got {other:?}"),
    }
    let position = writer
        .commit("acme", batch(1))
        .await
        .expect("a later export commits");
    assert_eq!(position.group, 0);
    cluster.shutdown().await;
}

/// A binding whose one token may write `tenants`.
fn binding(tenants: &[&str]) -> ourios_serving::AuthBinding {
    let store = ourios_core::auth::build_token_store(Some(&[ourios_core::auth::TokenSpec {
        name: Some("edge".to_owned()),
        token: Some("tok-edge".to_owned()),
        tenants: tenants.iter().map(|t| (*t).to_owned()).collect(),
    }]))
    .expect("valid spec")
    .expect("enabled store");
    ourios_serving::authenticate_bearer(Some(&store), Some("Bearer tok-edge"))
        .expect("known token")
        .expect("bound")
}

/// RFC 0026 §3.2 on the replicated path: an export whose tenant its
/// binding does not allow is denied before anything is proposed, and one
/// the binding allows commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_binding_is_checked_before_the_export_is_proposed() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let ingest = ReplicatedIngest::new(writer(&cluster, leader));
    let before = cluster.node(leader).log(0).await.len();

    let other = binding(&["globex"]);
    let denied = Export {
        binding: Some(&other),
        ..export("denied")
    };
    match ingest.ingest(denied).await {
        Err(ReceiveError::TenantDenied { tenant, .. }) => assert_eq!(tenant.as_str(), "acme"),
        other => panic!("expected the binding denial, got {other:?}"),
    }
    assert_eq!(
        cluster.node(leader).log(0).await.len(),
        before,
        "a denied export is never proposed"
    );

    let own = binding(&["acme"]);
    let allowed = Export {
        binding: Some(&own),
        ..export("allowed")
    };
    committed(
        ingest
            .ingest(allowed)
            .await
            .expect("an allowed export commits"),
    );
    cluster.shutdown().await;
}
