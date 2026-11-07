//! A follower's ingest forwarded to its group's leader (RFC 0065 §6.8),
//! every node serving forwards with a [`LeaderTarget`].

use std::sync::Arc;
use std::time::Duration;

use ourios_serving::AuthResolver;

use super::{committed, export, followers, layout, ready_leader, rejected_as, writer};
use crate::receiver::ReceiveError;
use crate::replicated::forward::LeaderTarget;
use crate::replicated::ingest::ReplicatedIngest;
use crate::replicated::raft::cluster::{Cluster, name};
use crate::replicated::raft::transport::Rejection;
use crate::replicated::route::Router;
use crate::replicated::write::{COMMIT_TIMEOUT, WriteError};

const TOKEN: Option<&str> = Some("Bearer tok-edge");

fn resolver() -> AuthResolver {
    let store = ourios_core::auth::build_token_store(Some(&[ourios_core::auth::TokenSpec {
        name: Some("edge".to_owned()),
        token: Some("tok-edge".to_owned()),
        tenants: vec!["acme".to_owned()],
    }]))
    .expect("valid spec")
    .expect("enabled store");
    AuthResolver::static_only(Some(Arc::new(store)))
}

/// Serve forwards on every node, each commit waiting up to `timeout`.
fn serve_forwards(cluster: &Cluster, timeout: Duration) {
    for i in 0..3 {
        let target = LeaderTarget::new(writer(cluster, i).with_timeout(timeout), resolver());
        cluster
            .node(i)
            .host
            .server()
            .serve_forwards(Arc::new(target));
    }
}

/// Node `i`'s ingest, forwarding what it does not lead.
fn forwarding(cluster: &Cluster, i: usize, commit_timeout: Duration) -> ReplicatedIngest {
    let router = Router::new(
        cluster.peers().clone(),
        layout(cluster, 1),
        name(cluster.id(i)),
        commit_timeout,
    );
    ReplicatedIngest::new(writer(cluster, i)).with_router(router)
}

/// A follower forwards an export it does not lead, and it is acknowledged
/// at its position in the leader's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followers_export_commits_on_the_leader() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    serve_forwards(&cluster, COMMIT_TIMEOUT);
    let follower = followers(leader)[0];
    let ingested = forwarding(&cluster, follower, COMMIT_TIMEOUT)
        .ingest_with(export("forwarded"), TOKEN)
        .await;
    let position = committed(ingested.expect("acknowledged"));
    let log = cluster.node(leader).log(0).await;
    assert!(log.iter().any(|e| e.log_id.index == position.seq));
    cluster.shutdown().await;
}

/// The leader's rejection of a forwarded export reaches the follower's
/// client as the leader gave it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leaders_rejection_is_the_followers_answer() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    serve_forwards(&cluster, COMMIT_TIMEOUT);
    let follower = followers(leader)[0];
    let refused = forwarding(&cluster, follower, COMMIT_TIMEOUT)
        .ingest_with(export("refused"), Some("Bearer tok-wrong"))
        .await
        .expect_err("a bad credential");
    match &refused {
        ReceiveError::Forwarded(rejected) => {
            assert_eq!(rejected.rejection, Rejection::Unauthenticated);
            assert_eq!(refused.to_string(), "a valid bearer token is required");
        }
        other => panic!("expected the leader's rejection, got {other:?}"),
    }
    cluster.shutdown().await;
}

/// A forward that may have committed gets the retryable rejection, counted
/// as the group being unavailable; so does a follower with no router.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncommitted_forward_and_a_follower_without_a_router_are_unavailable() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let impatient = Duration::from_millis(300);
    serve_forwards(&cluster, impatient);
    let [follower, other] = followers(leader)[..] else {
        unreachable!("three voters")
    };
    let unrouted = ReplicatedIngest::new(writer(&cluster, follower))
        .ingest_with(export("unrouted"), TOKEN)
        .await
        .expect_err("no router");
    assert_eq!(rejected_as(&unrouted), "group_unavailable");

    for f in [follower, other] {
        cluster.node(f).gate.close();
    }
    let uncertain = forwarding(&cluster, follower, impatient)
        .ingest_with(export("uncertain"), TOKEN)
        .await
        .expect_err("the commit timed out");
    assert_eq!(rejected_as(&uncertain), "group_unavailable");
    assert!(
        matches!(
            uncertain,
            ReceiveError::Replication(WriteError::Unforwarded { group: 0, .. })
        ),
        "{uncertain:?}"
    );
    for f in [follower, other] {
        cluster.node(f).gate.open();
    }
    cluster.shutdown().await;
}

/// An export too large for a WAL frame is the permanent size rejection on
/// a follower too: it is refused before routing and never forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversize_export_is_refused_before_it_is_forwarded() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    serve_forwards(&cluster, COMMIT_TIMEOUT);
    let follower = followers(leader)[0];
    let before = cluster.node(leader).log(0).await.len();
    let line = "x".repeat(ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES + 1);
    let refused = forwarding(&cluster, follower, COMMIT_TIMEOUT)
        .ingest_with(export(&line), TOKEN)
        .await
        .expect_err("too large");
    assert!(
        matches!(
            refused,
            ReceiveError::Replication(WriteError::TooLarge { .. })
        ),
        "{refused:?}"
    );
    assert_eq!(
        cluster.node(leader).log(0).await.len(),
        before,
        "never forwarded"
    );
    cluster.shutdown().await;
}
