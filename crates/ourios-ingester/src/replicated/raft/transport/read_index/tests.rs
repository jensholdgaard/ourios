//! `ReadIndex`'s outcomes: how a node answers each refusal, how the tail
//! replica reads each answer, and the deadline of a call over loopback.

use std::sync::Arc;
use std::time::Duration;

use tonic::{Code, Status};

use super::{
    Outcome, ReadIndexRequest, ReadIndexTarget, Reading, Refusal, classify, log_id, refusal, serve,
};
use crate::replicated::raft::cluster::{bind, serve as serve_on};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::transport::{Peers, ReplicationServer, VERSION};

fn request() -> ReadIndexRequest {
    ReadIndexRequest {
        version: VERSION,
        group: 4,
    }
}

/// A leader that answers every read the same way, or never.
enum Fake {
    Answer(Result<u64, Refusal>),
    Hang,
}

impl ReadIndexTarget for Fake {
    fn read_index(&self, group: u32) -> Reading {
        assert_eq!(group, 4);
        match self {
            Self::Answer(answer) => {
                let answer = answer.clone().map(|index| log_id(2, index));
                Box::pin(async move { answer })
            }
            Self::Hang => Box::pin(std::future::pending()),
        }
    }
}

#[test]
fn each_refusal_reads_back_as_the_outcome_it_proves() {
    let read = |r: Refusal| classify(&refusal(r));
    assert_eq!(read(Refusal::NotHosted), Outcome::NotHosted);
    let hint = Some("n2".to_owned());
    assert_eq!(
        read(Refusal::NotLeader {
            leader: hint.clone()
        }),
        Outcome::NotLeader { leader: hint }
    );
    assert!(matches!(
        read(Refusal::Unconfirmed("no quorum".to_owned())),
        Outcome::Unavailable(_)
    ));
    assert_eq!(
        classify(&Status::unimplemented("no such method")),
        Outcome::NotHosted,
        "a node without ReadIndex"
    );
    for code in [Code::Internal, Code::Unknown] {
        let outcome = classify(&Status::new(code, "lost"));
        assert!(
            matches!(outcome, Outcome::Unavailable(_)),
            "{code:?}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn a_node_refuses_another_version_and_a_missing_target_as_not_hosted() {
    let stale = ReadIndexRequest {
        version: VERSION + 1,
        ..request()
    };
    let target: Arc<dyn ReadIndexTarget> = Arc::new(Fake::Answer(Ok(1)));
    let refused = serve(Some(target), stale)
        .await
        .expect_err("another version");
    assert_eq!(classify(&refused), Outcome::NotHosted);
    let refused = serve(None, request()).await.expect_err("no target");
    assert_eq!(classify(&refused), Outcome::NotHosted);
}

/// A node serving `fake` on loopback, and peers that name it `n1`.
fn node(fake: Fake) -> (Peers, crate::replicated::raft::cluster::Serving) {
    let server = ReplicationServer::<GroupSm>::default();
    server.serve_read_indexes(Arc::new(fake));
    let (incoming, addr) = bind();
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    (peers, serve_on(server, incoming))
}

const PATIENCE: Duration = Duration::from_millis(200);

#[tokio::test]
async fn a_read_over_loopback_reads_each_answer() {
    let hint = Some("n2".to_owned());
    for (fake, expected) in [
        (Fake::Answer(Ok(42)), Outcome::Read(log_id(2, 42))),
        (
            Fake::Answer(Err(Refusal::NotLeader {
                leader: hint.clone(),
            })),
            Outcome::NotLeader { leader: hint },
        ),
        (Fake::Answer(Err(Refusal::NotHosted)), Outcome::NotHosted),
    ] {
        let (peers, _serving) = node(fake);
        assert_eq!(peers.read_index("n1", 4, PATIENCE).await, expected);
    }
    let (peers, _serving) = node(Fake::Answer(Err(Refusal::Unconfirmed(
        "no quorum".to_owned(),
    ))));
    let unconfirmed = peers.read_index("n1", 4, PATIENCE).await;
    assert!(
        matches!(unconfirmed, Outcome::Unavailable(_)),
        "{unconfirmed:?}"
    );
}

/// A leader that never confirms leaves the read unavailable at its
/// deadline, never a possibly stale read point.
#[tokio::test]
async fn a_read_that_never_answers_is_unavailable_at_its_deadline() {
    let (peers, _serving) = node(Fake::Hang);
    let started = tokio::time::Instant::now();
    let outcome = peers.read_index("n1", 4, PATIENCE).await;
    assert!(matches!(outcome, Outcome::Unavailable(_)), "{outcome:?}");
    assert!(started.elapsed() >= PATIENCE, "not before its deadline");
}

/// A node that cannot be reached is unreached, and so unavailable too.
#[tokio::test]
async fn a_read_that_cannot_connect_is_unreached() {
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), "127.0.0.1:1".to_owned())]);
    let outcome = peers.read_index("n1", 4, PATIENCE).await;
    assert!(matches!(outcome, Outcome::Unreached(_)), "{outcome:?}");
}
