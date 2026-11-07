//! Routing on the in-process cluster, every node serving forwards with a
//! [`LeaderTarget`]: RFC0065.8's forwarding arm.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ourios_serving::AuthResolver;

use super::{Routed, Router};
use crate::replicated::forward::LeaderTarget;
use crate::replicated::layout::VotingLayout;
use crate::replicated::raft::cluster::{Cluster, name, write};
use crate::replicated::raft::transport::{
    ForwardTarget, Forwarded, IdentifyError, Refusal, Rejection,
};
use crate::replicated::raft::types::{Entry, TenantBatchBytes};
use crate::replicated::write::{COMMIT_TIMEOUT, GroupWriter};

const TOKEN: Option<&str> = Some("Bearer tok-edge");

fn layout(cluster: &Cluster) -> VotingLayout {
    let peers = (0..3).map(|i| name(cluster.id(i)));
    VotingLayout::new(peers, NonZeroU32::new(1).expect("non-zero"), 3)
}

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

/// A node's [`LeaderTarget`], counting the forwards it receives.
struct Counted {
    target: LeaderTarget,
    calls: Arc<AtomicUsize>,
}

impl ForwardTarget for Counted {
    fn commit(
        &self,
        export: Forwarded,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<u64, Refusal>> + Send>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.target.commit(export)
    }
}

/// Serve forwards on every node, its commits waiting up to `timeout`;
/// returns each node's count of forwards received.
fn serve_forwards(cluster: &Cluster, timeout: Duration) -> Vec<Arc<AtomicUsize>> {
    (0..3)
        .map(|i| {
            let host = &cluster.node(i).host;
            let writer = GroupWriter::new(layout(cluster), host).with_timeout(timeout);
            let calls = Arc::new(AtomicUsize::new(0));
            let target = Counted {
                target: LeaderTarget::new(writer, resolver()),
                calls: Arc::clone(&calls),
            };
            host.server().serve_forwards(Arc::new(target));
            calls
        })
        .collect()
}

/// A router on node `from`.
fn router(cluster: &Cluster, from: usize, commit_timeout: Duration) -> Router {
    let node = name(cluster.id(from));
    Router::new(
        cluster.peers().clone(),
        layout(cluster),
        node,
        commit_timeout,
    )
}

/// An export for `acme` carrying `marker`, with `credential`.
fn export(marker: &str, credential: Option<&str>) -> Forwarded {
    let payload = ourios_wal::TenantBatch::encode("acme", marker.as_bytes()).expect("batch");
    Forwarded {
        group: 0,
        batch: TenantBatchBytes::new(payload).expect("valid"),
        credential: credential.map(str::to_owned),
    }
}

/// How many of `log`'s entries carry `marker`.
fn copies(log: &[Entry], marker: &str) -> usize {
    let carries = |e: &&Entry| match &e.payload {
        openraft::EntryPayload::Normal(batch) => batch
            .as_bytes()
            .windows(marker.len())
            .any(|w| w == marker.as_bytes()),
        _ => false,
    };
    log.iter().filter(carries).count()
}

/// The leader of group 0, once it has committed an entry of its term, and
/// the two followers.
async fn roles(cluster: &Cluster) -> (usize, usize, usize) {
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 0).await;
    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    (leader, followers[0], followers[1])
}

/// A forward hinted at the leader commits there. One hinted at a follower
/// is retried at the leader that follower names and committed once, and so
/// is one with no hint at all, whichever voter placement order puts first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forward_reaches_the_leader_and_commits_once() {
    let cluster = Cluster::start(3, &[0]).await;
    let (leader, from, other) = roles(&cluster).await;
    serve_forwards(&cluster, COMMIT_TIMEOUT);
    let sender = router(&cluster, from, COMMIT_TIMEOUT);
    let leader_name = name(cluster.id(leader));
    let hints = [
        ("<hinted>", Some(leader_name.clone())),
        ("<stale>", Some(name(cluster.id(other)))),
        ("<unhinted>", None),
    ];
    for (marker, hint) in hints {
        let routed = sender.forward(export(marker, TOKEN), hint).await;
        assert!(
            matches!(routed, Routed::Committed(_)),
            "{marker}: {routed:?}"
        );
        let log = cluster.node(leader).log(0).await;
        assert_eq!(copies(&log, marker), 1, "{marker} committed once");
    }
    cluster.shutdown().await;
}

/// A hinted node that is down was never sent the export, so the router
/// moves on to the voters and the leader commits it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hinted_node_that_is_down_is_passed_over() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let (leader, from, other) = roles(&cluster).await;
    serve_forwards(&cluster, COMMIT_TIMEOUT);
    let dead = name(cluster.id(other));
    cluster.kill(other).await;
    // The pool still holds the connection the group's own traffic opened
    // to it, and a call that dies on an open connection may have been
    // sent. One failed call drops it, so the forward has to connect.
    let probed = cluster.peers().identify(&dead, COMMIT_TIMEOUT).await;
    assert!(
        matches!(probed, Err(IdentifyError::Unanswered(_))),
        "{probed:?}"
    );
    let routed = router(&cluster, from, COMMIT_TIMEOUT)
        .forward(export("<passed over>", TOKEN), Some(dead))
        .await;
    assert!(matches!(routed, Routed::Committed(_)), "{routed:?}");
    let log = cluster.node(leader).log(0).await;
    assert_eq!(copies(&log, "<passed over>"), 1);
    cluster.shutdown().await;
}

/// The leader's rejection comes back as it gave it, and nothing is
/// appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leaders_rejection_comes_back_unchanged() {
    let cluster = Cluster::start(3, &[0]).await;
    let (leader, from, _) = roles(&cluster).await;
    serve_forwards(&cluster, COMMIT_TIMEOUT);
    let routed = router(&cluster, from, COMMIT_TIMEOUT)
        .forward(export("<refused>", Some("Bearer tok-wrong")), None)
        .await;
    match routed {
        Routed::Rejected(rejected) => {
            assert_eq!(rejected.rejection, Rejection::Unauthenticated);
            assert_eq!(rejected.error_type, "unauthenticated");
            assert_eq!(rejected.message, "a valid bearer token is required");
        }
        other => panic!("expected the leader's rejection, got {other:?}"),
    }
    let log = cluster.node(leader).log(0).await;
    assert_eq!(copies(&log, "<refused>"), 0);
    cluster.shutdown().await;
}

/// A forward whose commit times out may still commit, so the router stops
/// at it with the retryable outcome and sends it nowhere else: the leader
/// was asked once, no other node at all, and its log holds the export
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncertain_forward_is_not_sent_again() {
    let cluster = Cluster::start(3, &[0]).await;
    let (leader, from, other) = roles(&cluster).await;
    let impatient = Duration::from_millis(300);
    let calls = serve_forwards(&cluster, impatient);
    for follower in [from, other] {
        cluster.node(follower).gate.close();
    }
    let hint = Some(name(cluster.id(leader)));
    let routed = router(&cluster, from, impatient)
        .forward(export("<uncertain>", TOKEN), hint)
        .await;
    assert!(matches!(routed, Routed::Unavailable(_)), "{routed:?}");
    let log = cluster.node(leader).log(0).await;
    assert_eq!(copies(&log, "<uncertain>"), 1, "appended once, not resent");
    let asked: Vec<_> = calls.iter().map(|c| c.load(Ordering::SeqCst)).collect();
    let expected: Vec<_> = (0..3).map(|i| usize::from(i == leader)).collect();
    assert_eq!(asked, expected, "only the leader was asked");
    for follower in [from, other] {
        cluster.node(follower).gate.open();
    }
    cluster.shutdown().await;
}

/// A hint naming a node outside the group's voters is passed over, so it
/// cannot take a try a voter needs: the first try goes to the first voter
/// in placement order, and every voter is still tried.
#[test]
fn a_hint_outside_the_groups_voters_is_passed_over() {
    let layout = VotingLayout::new(["a", "b", "c"], NonZeroU32::new(1).expect("one"), 3);
    let voters = layout.voters(0);
    let router = Router::new(
        crate::replicated::raft::transport::Peers::plaintext(),
        layout.clone(),
        "outside".to_owned(),
        COMMIT_TIMEOUT,
    );
    let first = router.next(Some("ghost".to_owned()), &voters, &[]);
    assert_eq!(first.as_deref(), Some(voters[0]));
    let mut tried = Vec::new();
    while let Some(next) = router.next(Some("ghost".to_owned()), &voters, &tried) {
        tried.push(next);
    }
    assert_eq!(tried, voters, "each voter once, in placement order");
}
