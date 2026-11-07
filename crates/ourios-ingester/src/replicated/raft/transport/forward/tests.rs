//! `Forward`'s outcomes: how a node answers each refusal, how the sender
//! reads each answer, and the deadlines of a call over loopback.

use std::sync::Arc;
use std::time::Duration;

use tonic::{Code, Status};

use super::{
    Committing, ForwardRequest, ForwardTarget, Forwarded, Outcome, Refusal, Rejected, Rejection,
    classify, refusal, serve,
};
use crate::replicated::raft::cluster::{bind, serve as serve_on};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::transport::{Peers, ReplicationServer, VERSION};

fn request() -> ForwardRequest {
    ForwardRequest {
        version: VERSION,
        group: 0,
        payload: ourios_wal::TenantBatch::encode("acme", b"record").expect("batch"),
        credential: Some("Bearer tok".to_owned()),
    }
}

/// A node that answers every forward the same way, or never.
enum Fake {
    Answer(Result<u64, Refusal>),
    Hang,
}

impl ForwardTarget for Fake {
    fn commit(&self, export: Forwarded) -> Committing {
        assert_eq!(export.credential.as_deref(), Some("Bearer tok"));
        assert_eq!(export.group, 0);
        match self {
            Self::Answer(answer) => {
                let answer = answer.clone();
                Box::pin(async move { answer })
            }
            Self::Hang => Box::pin(std::future::pending()),
        }
    }
}

/// A leader's rejection with its message and details, as its client
/// would have got them.
fn denied(rejection: Rejection) -> Rejected {
    Rejected {
        rejection,
        error_type: "permission_denied".to_owned(),
        message: "the token edge may not write the tenant acme".to_owned(),
        details: vec![0x08, 0x07, 0x12, 0x02, b'n', b'o'],
    }
}

#[test]
fn each_refusal_reads_back_as_the_outcome_it_proves() {
    let read = |r: Refusal| classify(&refusal(r));
    assert_eq!(
        read(Refusal::NotLeader {
            leader: Some("ourios-receiver-2".to_owned())
        }),
        Outcome::NotLeader {
            leader: Some("ourios-receiver-2".to_owned())
        }
    );
    let unknown = read(Refusal::NotLeader { leader: None });
    assert_eq!(unknown, Outcome::NotLeader { leader: None });
    assert_eq!(read(Refusal::NotHosted), Outcome::NotHosted);
    for rejection in [
        Rejection::Unauthenticated,
        Rejection::Unavailable,
        Rejection::PermissionDenied,
        Rejection::TooLarge,
    ] {
        let rejected = denied(rejection);
        assert_eq!(
            read(Refusal::Rejected(rejected.clone())),
            Outcome::Rejected(rejected)
        );
    }
    let failed = read(Refusal::Failed("the commit timed out".to_owned()));
    assert!(matches!(failed, Outcome::Uncertain(_)), "{failed:?}");
}

/// Only a marked refusal proves nothing was appended. An unmarked status,
/// whatever its code, may follow an append, and so may a refusal marked
/// rejected with a code no rejection has.
#[test]
fn an_unmarked_status_is_uncertain() {
    for code in [
        Code::Unavailable,
        Code::NotFound,
        Code::Internal,
        Code::PermissionDenied,
    ] {
        let outcome = classify(&Status::new(code, "lost"));
        assert!(
            matches!(outcome, Outcome::Uncertain(_)),
            "{code:?}: {outcome:?}"
        );
    }
    let odd = super::refused(Status::internal("odd"), "rejected");
    assert!(matches!(classify(&odd), Outcome::Uncertain(_)));
    let older = Status::unimplemented("no such method");
    assert_eq!(
        classify(&older),
        Outcome::NotHosted,
        "a node without Forward"
    );
}

#[tokio::test]
async fn a_node_refuses_another_version_and_a_missing_target_as_not_hosted() {
    let stale = ForwardRequest {
        version: VERSION + 1,
        ..request()
    };
    let target: Arc<dyn ForwardTarget> = Arc::new(Fake::Answer(Ok(1)));
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
    server.serve_forwards(Arc::new(fake));
    let (incoming, addr) = bind();
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    (peers, serve_on(server, incoming))
}

const COMMIT: Duration = Duration::from_millis(200);

#[tokio::test]
async fn a_forward_over_loopback_reads_each_answer() {
    let hint = Some("n2".to_owned());
    for (fake, expected) in [
        (Fake::Answer(Ok(42)), Outcome::Committed(42)),
        (
            Fake::Answer(Err(Refusal::NotLeader {
                leader: hint.clone(),
            })),
            Outcome::NotLeader {
                leader: hint.clone(),
            },
        ),
        (
            Fake::Answer(Err(Refusal::Rejected(denied(Rejection::PermissionDenied)))),
            Outcome::Rejected(denied(Rejection::PermissionDenied)),
        ),
    ] {
        let (peers, _serving) = node(fake);
        assert_eq!(peers.forward("n1", request(), COMMIT).await, expected);
    }
    let (peers, _serving) = node(Fake::Answer(Err(Refusal::Failed("lost".to_owned()))));
    let failed = peers.forward("n1", request(), COMMIT).await;
    assert!(matches!(failed, Outcome::Uncertain(_)), "{failed:?}");
}

/// A sent call that never answers is uncertain once the commit timeout
/// plus the connect timeout has passed, not before.
#[tokio::test]
async fn a_sent_call_that_never_answers_is_uncertain_at_its_deadline() {
    let (peers, _serving) = node(Fake::Hang);
    let started = tokio::time::Instant::now();
    let outcome = peers.forward("n1", request(), COMMIT).await;
    assert!(matches!(outcome, Outcome::Uncertain(_)), "{outcome:?}");
    assert!(
        started.elapsed() >= COMMIT,
        "gave up early: {:?}",
        started.elapsed()
    );
}

/// A call to a node nobody listens at, or with no address, was never
/// sent.
#[tokio::test]
async fn a_call_that_cannot_connect_was_never_sent() {
    let (incoming, addr) = bind();
    drop(incoming);
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    let refused = peers.forward("n1", request(), COMMIT).await;
    assert!(matches!(refused, Outcome::Unreached(_)), "{refused:?}");
    let unknown = peers.forward("n9", request(), COMMIT).await;
    assert!(matches!(unknown, Outcome::Unreached(_)), "{unknown:?}");
}
