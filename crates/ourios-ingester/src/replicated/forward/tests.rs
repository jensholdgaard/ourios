//! The leader's side of `Forward` on the in-process cluster: what it
//! commits, what it refuses before appending, and a forward over the wire.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use ourios_serving::AuthResolver;

use super::LeaderTarget;
use crate::replicated::layout::VotingLayout;
use crate::replicated::raft::cluster::{Cluster, name, write};
use crate::replicated::raft::transport::{
    ForwardRequest, ForwardTarget, Forwarded, Outcome, Refusal, Rejection, VERSION,
};
use crate::replicated::raft::types::TenantBatchBytes;
use crate::replicated::write::GroupWriter;

fn layout(cluster: &Cluster, groups: u32) -> VotingLayout {
    let peers = (0..3).map(|i| name(cluster.id(i)));
    VotingLayout::new(peers, NonZeroU32::new(groups).expect("non-zero"), 3)
}

/// A resolver whose one token, `tok-edge`, may write `tenants`.
fn resolver(tenants: &[&str]) -> AuthResolver {
    let store = ourios_core::auth::build_token_store(Some(&[ourios_core::auth::TokenSpec {
        name: Some("edge".to_owned()),
        token: Some("tok-edge".to_owned()),
        tenants: tenants.iter().map(|t| (*t).to_owned()).collect(),
    }]))
    .expect("valid spec")
    .expect("enabled store");
    AuthResolver::static_only(Some(Arc::new(store)))
}

fn target(cluster: &Cluster, node: usize, tenants: &[&str]) -> LeaderTarget {
    let writer = GroupWriter::new(layout(cluster, 1), &cluster.node(node).host);
    LeaderTarget::new(writer, resolver(tenants))
}

fn payload(tenant: &str, len: usize) -> Vec<u8> {
    ourios_wal::TenantBatch::encode(tenant, &vec![b'x'; len]).expect("batch")
}

fn export(tenant: &str, credential: Option<&str>) -> Forwarded {
    Forwarded {
        group: 0,
        batch: TenantBatchBytes::new(payload(tenant, 8)).expect("valid"),
        credential: credential.map(str::to_owned),
    }
}

/// The leader of group 0, once it has committed an entry of its term.
async fn ready_leader(cluster: &Cluster) -> usize {
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 0).await;
    leader
}

fn rejected_as(refusal: Result<u64, Refusal>) -> (Rejection, String) {
    match refusal {
        Err(Refusal::Rejected(rejected)) => (rejected.rejection, rejected.error_type),
        other => panic!("expected the leader's rejection, got {other:?}"),
    }
}

/// The leader commits an export whose credential may write its tenant,
/// and refuses one without a valid credential, one outside the binding
/// and one too large for a WAL frame, each before appending anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leader_commits_a_bound_export_and_refuses_the_rest_before_appending() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let target = target(&cluster, leader, &["acme"]);
    let before = cluster.node(leader).log(0).await.len();

    let refused = [
        target.commit(export("acme", None)).await,
        target
            .commit(export("acme", Some("Bearer tok-wrong")))
            .await,
        target
            .commit(export("globex", Some("Bearer tok-edge")))
            .await,
    ];
    let expected = [
        (Rejection::Unauthenticated, "unauthenticated"),
        (Rejection::Unauthenticated, "unauthenticated"),
        (Rejection::PermissionDenied, "permission_denied"),
    ];
    for (refusal, (rejection, error_type)) in refused.into_iter().zip(expected) {
        assert_eq!(rejected_as(refusal), (rejection, error_type.to_owned()));
    }
    let limit = ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES;
    let oversize = Forwarded {
        batch: TenantBatchBytes::new(payload("acme", limit)).expect("valid"),
        ..export("acme", Some("Bearer tok-edge"))
    };
    let (rejection, _) = rejected_as(target.commit(oversize).await);
    assert_eq!(rejection, Rejection::TooLarge);
    assert_eq!(
        cluster.node(leader).log(0).await.len(),
        before,
        "nothing refused was appended"
    );

    let seq = target
        .commit(export("acme", Some("Bearer tok-edge")))
        .await
        .expect("committed");
    let log = cluster.node(leader).log(0).await;
    assert!(log.iter().any(|e| e.log_id.index == seq));
    cluster.shutdown().await;
}

/// A follower does not forward a forwarded export on: it refuses it as not
/// the leader, naming the leader. An export for a group its tenant does not
/// map to is refused as not hosted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_names_the_leader_and_a_wrong_group_is_not_hosted() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    let from_follower = target(&cluster, follower, &["acme"])
        .commit(export("acme", Some("Bearer tok-edge")))
        .await;
    let hint = Some(name(cluster.id(leader)));
    assert_eq!(from_follower, Err(Refusal::NotLeader { leader: hint }));

    let elsewhere = Forwarded {
        group: 1,
        ..export("acme", Some("Bearer tok-edge"))
    };
    let wrong_group = target(&cluster, leader, &["acme"]).commit(elsewhere).await;
    assert_eq!(wrong_group, Err(Refusal::NotHosted));
    cluster.shutdown().await;
}

/// Over the wire: a follower's forward to the leader, whose server runs
/// the target, commits; the same forward to a follower is refused as not
/// the leader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forward_to_the_leader_commits_and_one_to_a_follower_names_it() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    for i in 0..3 {
        let installed = Arc::new(target(&cluster, i, &["acme"]));
        cluster.node(i).host.server().serve_forwards(installed);
    }
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    let request = || ForwardRequest {
        version: VERSION,
        group: 0,
        payload: payload("acme", 8),
        credential: Some("Bearer tok-edge".to_owned()),
    };
    let commit = crate::replicated::write::COMMIT_TIMEOUT;
    let peers = cluster.peers();
    let at_leader = peers
        .forward(&name(cluster.id(leader)), request(), commit)
        .await;
    let Outcome::Committed(seq) = at_leader else {
        panic!("expected a commit, got {at_leader:?}");
    };
    let log = cluster.node(leader).log(0).await;
    assert!(log.iter().any(|e| e.log_id.index == seq));

    // The commit needs one follower's ack, not this one's, so it may not
    // yet have heard from the leader it is to name.
    cluster
        .node(follower)
        .raft(0)
        .wait(Some(Duration::from_secs(10)))
        .current_leader(cluster.id(leader), "the follower follows the leader")
        .await
        .expect("following");
    let at_follower = peers
        .forward(&name(cluster.id(follower)), request(), commit)
        .await;
    let hint = Some(name(cluster.id(leader)));
    assert_eq!(at_follower, Outcome::NotLeader { leader: hint });
    cluster.shutdown().await;
}

/// A commit that may have happened stays uncertain. With both followers'
/// fsyncs held, the leader stays leader but its commit times out after
/// appending, which it answers as failed, and the sender reads that over
/// the wire as uncertain, never as a refusal it could retry and append
/// twice. A commit to a stopped group fails the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_that_may_have_happened_stays_uncertain() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let impatient = || {
        let writer = GroupWriter::new(layout(&cluster, 1), &cluster.node(leader).host)
            .with_timeout(Duration::from_millis(300));
        LeaderTarget::new(writer, resolver(&["acme"]))
    };
    let (direct, served, stopped) = (impatient(), impatient(), impatient());
    cluster
        .node(leader)
        .host
        .server()
        .serve_forwards(Arc::new(served));
    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    for &follower in &followers {
        cluster.node(follower).gate.close();
    }

    let failed = direct.commit(export("acme", Some("Bearer tok-edge"))).await;
    assert!(matches!(failed, Err(Refusal::Failed(_))), "{failed:?}");
    let request = ForwardRequest {
        version: VERSION,
        group: 0,
        payload: payload("acme", 8),
        credential: Some("Bearer tok-edge".to_owned()),
    };
    let leader_name = name(cluster.id(leader));
    let over_the_wire = cluster
        .peers()
        .forward(&leader_name, request, Duration::from_millis(300))
        .await;
    assert!(
        matches!(over_the_wire, Outcome::Uncertain(_)),
        "{over_the_wire:?}"
    );

    for &follower in &followers {
        cluster.node(follower).gate.open();
    }
    cluster.kill(leader).await;
    let after_stop = stopped
        .commit(export("acme", Some("Bearer tok-edge")))
        .await;
    assert!(
        matches!(after_stop, Err(Refusal::Failed(_))),
        "{after_stop:?}"
    );
    cluster.shutdown().await;
}
