//! `Propose`'s wire: how a node answers each refusal, how the flusher
//! reads each answer, what a request must hold, and a call over loopback.

use std::sync::Arc;
use std::time::Duration;

use ourios_wal::Control;
use tonic::{Code, Status};
use uuid::Uuid;

use super::{
    Outcome, ProposeRequest, ProposeTarget, Proposed, ProposedCheckpoint, Proposing, Refusal,
    Report, classify, log_id, parse, refusal, serve,
};
use crate::replicated::raft::cluster::{bind, serve as serve_on};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::transport::wire::WireLogId;
use crate::replicated::raft::transport::{Peers, ReplicationServer, VERSION};
use crate::replicated::raft::types::NodeId;

fn flusher() -> NodeId {
    NodeId::from(Uuid::from_u128(7))
}

fn report() -> Report {
    Report {
        proposer: flusher(),
        mined: Some(log_id(3, 250)),
        unstarted: 2,
        checkpoint: Some(ProposedCheckpoint {
            cut: log_id(3, 200),
            highest_kind: 4,
        }),
    }
}

#[test]
fn a_report_reads_back_from_its_request() {
    let request = report().request(9);
    assert_eq!(request.version, VERSION);
    assert_eq!(request.group, 9);
    assert_eq!(request.proposer, flusher().uuid().as_bytes().to_vec());
    assert_eq!(parse(&request), Ok(report()));
    let bare = Report {
        mined: None,
        checkpoint: None,
        ..report()
    };
    let request = bare.request(9);
    assert_eq!(request.entry, None);
    assert_eq!(request.mined, None);
    assert_eq!(parse(&request), Ok(bare));
}

/// RFC 0065 §6.8 check 3: the proposer is 16 bytes, and an entry is a
/// checkpoint whose proposer is the request's.
#[test]
fn a_request_is_malformed_unless_it_carries_a_checkpoint_of_its_proposer() {
    let request = report().request(9);
    let other = Control::Checkpoint {
        proposer: [9; 16],
        cut: ourios_wal::RaftLogId {
            term: 3,
            index: 200,
        },
        highest_kind: 4,
    };
    let block = Control::IdBlock {
        after: 0,
        through: 100,
    };
    for malformed in [
        ProposeRequest {
            proposer: vec![1; 15],
            ..request.clone()
        },
        ProposeRequest {
            entry: Some(other.to_bytes()),
            ..request.clone()
        },
        ProposeRequest {
            entry: Some(block.to_bytes()),
            ..request.clone()
        },
        ProposeRequest {
            entry: Some(vec![0x02, 1, 2]),
            ..request.clone()
        },
    ] {
        assert!(parse(&malformed).is_err(), "{malformed:?}");
    }
}

#[test]
fn each_refusal_reads_back_as_the_outcome_it_proves() {
    let read = |r: Refusal| classify(&refusal(r));
    let hint = Some("ourios-receiver-2".to_owned());
    assert_eq!(
        read(Refusal::NotLeader {
            leader: hint.clone()
        }),
        Outcome::NotLeader { leader: hint }
    );
    assert_eq!(
        read(Refusal::NotLeader { leader: None }),
        Outcome::NotLeader { leader: None }
    );
    assert_eq!(read(Refusal::NotHosted), Outcome::NotHosted);
    assert_eq!(
        read(Refusal::Malformed("short".to_owned())),
        Outcome::Malformed("short".to_owned())
    );
    assert_eq!(read(Refusal::NotFlusher), Outcome::NotFlusher);
    assert_eq!(
        read(Refusal::Stale("behind".to_owned())),
        Outcome::Stale("behind".to_owned())
    );
    let failed = read(Refusal::Failed("lost".to_owned()));
    assert!(matches!(failed, Outcome::Uncertain(_)), "{failed:?}");
}

/// RFC 0065 §6.8: each refusal is the status code the RFC names.
#[test]
fn each_refusal_has_its_status_code() {
    for (refused, code) in [
        (Refusal::NotHosted, Code::NotFound),
        (Refusal::NotLeader { leader: None }, Code::Unavailable),
        (Refusal::Malformed(String::new()), Code::InvalidArgument),
        (Refusal::NotFlusher, Code::FailedPrecondition),
        (Refusal::Stale(String::new()), Code::FailedPrecondition),
    ] {
        assert_eq!(refusal(refused.clone()).code(), code, "{refused:?}");
    }
}

#[test]
fn an_unmarked_status_is_uncertain() {
    for code in [Code::Unavailable, Code::NotFound, Code::Internal] {
        let outcome = classify(&Status::new(code, "lost"));
        assert!(
            matches!(outcome, Outcome::Uncertain(_)),
            "{code:?}: {outcome:?}"
        );
    }
    assert_eq!(
        classify(&Status::unimplemented("no such method")),
        Outcome::NotHosted,
        "a node without Propose"
    );
}

/// A node that answers every proposal the same way, or never, checking
/// what reached it.
enum Fake {
    Answer(Result<Option<u64>, Refusal>),
    Hang,
}

impl ProposeTarget for Fake {
    fn propose(&self, proposed: Proposed) -> Proposing {
        assert_eq!(proposed.group, 9);
        assert_eq!(proposed.report, Ok(report()));
        match self {
            Self::Answer(answer) => {
                let answer = answer.clone().map(|index| index.map(|i| log_id(3, i)));
                Box::pin(async move { answer })
            }
            Self::Hang => Box::pin(std::future::pending()),
        }
    }
}

/// The target takes a malformed request too, to refuse it after checking
/// it hosts and leads the group.
#[tokio::test]
async fn a_malformed_request_still_reaches_the_target() {
    struct Echo;
    impl ProposeTarget for Echo {
        fn propose(&self, proposed: Proposed) -> Proposing {
            let why = proposed.report.expect_err("malformed");
            Box::pin(async move { Err(Refusal::Malformed(why)) })
        }
    }
    let malformed = ProposeRequest {
        proposer: vec![1],
        ..report().request(9)
    };
    let refused = serve(Some(Arc::new(Echo)), malformed)
        .await
        .expect_err("malformed");
    assert!(matches!(classify(&refused), Outcome::Malformed(_)));
}

#[tokio::test]
async fn a_node_refuses_another_version_and_a_missing_target_as_not_hosted() {
    let stale = ProposeRequest {
        version: VERSION + 1,
        ..report().request(9)
    };
    let target: Arc<dyn ProposeTarget> = Arc::new(Fake::Answer(Ok(None)));
    let refused = serve(Some(target), stale).await.expect_err("version");
    assert_eq!(classify(&refused), Outcome::NotHosted);
    let refused = serve(None, report().request(9))
        .await
        .expect_err("no target");
    assert_eq!(classify(&refused), Outcome::NotHosted);
}

/// A node serving `fake` on loopback, and peers that name it `n1`.
fn node(fake: Fake) -> (Peers, crate::replicated::raft::cluster::Serving) {
    let server = ReplicationServer::<GroupSm>::default();
    server.serve_proposals(Arc::new(fake));
    let (incoming, addr) = bind();
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    (peers, serve_on(server, incoming))
}

const COMMIT: Duration = Duration::from_millis(200);

#[tokio::test]
async fn a_proposal_over_loopback_reads_each_answer() {
    for (fake, expected) in [
        (
            Fake::Answer(Ok(Some(260))),
            Outcome::Accepted {
                appended: Some(log_id(3, 260)),
            },
        ),
        (Fake::Answer(Ok(None)), Outcome::Accepted { appended: None }),
        (Fake::Answer(Err(Refusal::NotFlusher)), Outcome::NotFlusher),
        (
            Fake::Answer(Err(Refusal::Stale("behind".to_owned()))),
            Outcome::Stale("behind".to_owned()),
        ),
    ] {
        let (peers, _serving) = node(fake);
        assert_eq!(
            peers.propose("n1", report().request(9), COMMIT).await,
            expected
        );
    }
}

#[tokio::test]
async fn a_sent_proposal_that_never_answers_is_uncertain_at_its_deadline() {
    let (peers, _serving) = node(Fake::Hang);
    let started = tokio::time::Instant::now();
    let outcome = peers.propose("n1", report().request(9), COMMIT).await;
    assert!(matches!(outcome, Outcome::Uncertain(_)), "{outcome:?}");
    assert!(started.elapsed() >= COMMIT, "gave up early");
}

#[tokio::test]
async fn a_proposal_that_cannot_connect_was_never_sent() {
    let (incoming, addr) = bind();
    drop(incoming);
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    let refused = peers.propose("n1", report().request(9), COMMIT).await;
    assert!(matches!(refused, Outcome::Unreached(_)), "{refused:?}");
}

/// A `WireLogId` carries the term and index the appended entry has.
#[test]
fn an_appended_log_id_keeps_its_term_and_index() {
    let wire = WireLogId::from(&log_id(3, 260));
    assert_eq!((wire.term, wire.index), (3, 260));
}
