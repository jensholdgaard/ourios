//! `Identify`: what a node answers, what the caller accepts, and the call
//! on the in-process cluster.

use std::time::Duration;

use tonic::Code;

use super::{
    HIGHEST_CONTROL_KIND, IdentifyError, IdentifyRequest, IdentifyResponse, Identity, identity_of,
    serve,
};
use crate::replicated::raft::cluster::{Cluster, name};
use crate::replicated::raft::transport::VERSION;
use crate::replicated::raft::types::NodeId;

const PATIENCE: Duration = Duration::from_secs(2);

fn answer(node_id: &str, replica: Vec<u8>, kind: u32) -> IdentifyResponse {
    IdentifyResponse {
        node_id: node_id.to_owned(),
        replica,
        highest_control_kind: kind,
    }
}

#[test]
fn a_node_refuses_another_version_and_has_no_answer_before_its_identity() {
    let identity = Identity::new("a".to_owned(), NodeId::from(1));
    let stale = IdentifyRequest {
        version: VERSION + 1,
    };
    let refused = serve(Some(identity.clone()), &stale).expect_err("another version");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    let current = IdentifyRequest { version: VERSION };
    let early = serve(None, &current).expect_err("no identity yet");
    assert_eq!(early.code(), Code::Unavailable);
    let answered = serve(Some(identity), &current).expect("an identity");
    assert_eq!(answered.node_id, "a");
    assert_eq!(
        answered.highest_control_kind,
        u32::from(HIGHEST_CONTROL_KIND)
    );
}

/// An answer is taken only for the `node_id` the caller asked, with a
/// well-formed UUID and a kind that fits a byte.
#[test]
fn an_answer_for_another_node_or_a_malformed_one_is_mismatched() {
    let uuid = NodeId::from(7).uuid().as_bytes().to_vec();
    let taken = identity_of("a", answer("a", uuid.clone(), 0)).expect("a's own answer");
    assert_eq!(taken.replica, NodeId::from(7));
    for wrong in [
        answer("b", uuid.clone(), 0),
        answer("a", vec![1, 2, 3], 0),
        answer("a", uuid, 256),
    ] {
        let refused = identity_of("a", wrong);
        assert!(
            matches!(refused, Err(IdentifyError::Mismatched(_))),
            "{refused:?}"
        );
    }
}

/// Over the cluster's peer pool, each node answers with the replica it
/// runs; a peer with no address, or one that is down, is unanswered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_node_identifies_itself_and_an_absent_one_is_unanswered() {
    let mut cluster = Cluster::start(3, &[0]).await;
    for i in 0..3 {
        let node_id = name(cluster.id(i));
        let identity = cluster.peers().identify(&node_id, PATIENCE).await;
        let expected = Identity::new(node_id, cluster.id(i));
        assert_eq!(identity, Ok(expected), "node {i}");
    }
    let unknown = cluster.peers().identify("nobody", PATIENCE).await;
    assert!(
        matches!(unknown, Err(IdentifyError::Unanswered(_))),
        "{unknown:?}"
    );
    let down = name(cluster.id(2));
    cluster.kill(2).await;
    let silent = cluster.peers().identify(&down, PATIENCE).await;
    assert!(
        matches!(silent, Err(IdentifyError::Unanswered(_))),
        "{silent:?}"
    );
    cluster.shutdown().await;
}
