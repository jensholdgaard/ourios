//! Sending a proposal to three voters on loopback, each answering from a
//! script: where it goes, when it moves on, and when it stops.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openraft::vote::leader_id_std::CommittedLeaderId;
use uuid::Uuid;

use super::{Delivered, Proposer};
use crate::replicated::raft::cluster::{Serving, bind, serve};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::transport::propose::{
    ProposeTarget, Proposed, ProposedCheckpoint, Refusal, Report,
};
use crate::replicated::raft::transport::{Peers, ReplicationServer};
use crate::replicated::raft::types::{LogId, NodeId};

const GROUP: u32 = 4;
const VOTERS: [&str; 3] = ["v1", "v2", "v3"];
const COMMIT: Duration = Duration::from_millis(200);

fn id(index: u64) -> LogId {
    LogId {
        leader_id: CommittedLeaderId::new(2),
        index,
    }
}

fn report() -> Report {
    Report {
        proposer: NodeId::from(Uuid::from_u128(7)),
        mined: Some(id(20)),
        unstarted: 1,
        checkpoint: Some(ProposedCheckpoint {
            cut: id(18),
            highest_kind: 4,
        }),
    }
}

/// What a node answers one proposal.
type Answer = Result<Option<LogId>, Refusal>;

/// One node's answers, in order, the last repeating; and how many calls
/// reached it.
struct Scripted {
    answers: Mutex<VecDeque<Answer>>,
    calls: Arc<Mutex<usize>>,
}

impl ProposeTarget for Scripted {
    fn propose(
        &self,
        proposed: Proposed,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<LogId>, Refusal>> + Send>>
    {
        assert_eq!(proposed.group, GROUP);
        assert_eq!(proposed.report, Ok(report()));
        *self.calls.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        let answer = match answers.len() {
            0 | 1 => answers.front().cloned().expect("an answer"),
            _ => answers.pop_front().expect("an answer"),
        };
        Box::pin(async move { answer })
    }
}

/// Three voters answering from `scripts`, `None` for one nobody listens
/// at; the peers naming them, how many calls reached each, and their
/// servers.
fn voters(scripts: [Option<Vec<Answer>>; 3]) -> (Peers, Vec<Arc<Mutex<usize>>>, Vec<Serving>) {
    let mut addresses = Vec::new();
    let mut calls = Vec::new();
    let mut serving = Vec::new();
    for (name, script) in VOTERS.into_iter().zip(scripts) {
        let (incoming, addr) = bind();
        addresses.push((name.to_owned(), addr.to_string()));
        let counted = Arc::new(Mutex::new(0));
        calls.push(Arc::clone(&counted));
        match script {
            Some(answers) => {
                let server = ReplicationServer::<GroupSm>::default();
                server.serve_proposals(Arc::new(Scripted {
                    answers: Mutex::new(answers.into()),
                    calls: counted,
                }));
                serving.push(serve(server, incoming));
            }
            None => drop(incoming),
        }
    }
    (Peers::plaintext().with_addresses(addresses), calls, serving)
}

fn proposer(peers: Peers) -> Proposer {
    let voters = VOTERS.iter().map(|v| (*v).to_owned()).collect();
    Proposer::new(peers, GROUP, voters, COMMIT)
}

fn count(calls: &[Arc<Mutex<usize>>]) -> Vec<usize> {
    calls
        .iter()
        .map(|c| *c.lock().unwrap_or_else(PoisonError::into_inner))
        .collect()
}

fn not_leader(hint: Option<&str>) -> Answer {
    Err(Refusal::NotLeader {
        leader: hint.map(str::to_owned),
    })
}

/// The leader the flusher's log names takes it at once.
#[tokio::test]
async fn the_named_leader_takes_it_first() {
    let accept = Some(vec![Ok(Some(id(21)))]);
    let (peers, calls, _serving) = voters([accept.clone(), accept.clone(), accept]);

    let delivered = proposer(peers).send(&report(), Some("v2".to_owned())).await;

    assert_eq!(
        delivered,
        Delivered::Accepted {
            appended: Some(id(21))
        }
    );
    assert_eq!(count(&calls), [0, 1, 0]);
}

/// RFC 0065 §6.8: after `not_leader` it goes to the hint, after
/// `not_hosted` or a call never sent to the voters in placement order.
#[tokio::test]
async fn it_follows_the_hint_then_the_voters_in_order() {
    let (peers, calls, _serving) = voters([
        Some(vec![Err(Refusal::NotHosted)]),
        Some(vec![Ok(None)]),
        Some(vec![not_leader(Some("v1"))]),
    ]);

    let delivered = proposer(peers.clone())
        .send(&report(), Some("v3".to_owned()))
        .await;

    assert_eq!(delivered, Delivered::Accepted { appended: None });
    assert_eq!(count(&calls), [1, 1, 1], "v3, its hint v1, then v2");

    let (peers, calls, _serving) = voters([None, Some(vec![Ok(None)]), Some(vec![Ok(None)])]);
    let delivered = proposer(peers).send(&report(), None).await;
    assert_eq!(delivered, Delivered::Accepted { appended: None });
    assert_eq!(count(&calls), [0, 1, 0], "v1 unreachable, then v2");
}

/// Unlike a forwarded export, a proposal is sent again after an uncertain
/// outcome, to the same node.
#[tokio::test]
async fn it_resends_after_an_uncertain_outcome() {
    let uncertain = Err(Refusal::Failed("lost".to_owned()));
    let (peers, calls, _serving) = voters([
        Some(vec![uncertain, Ok(Some(id(21)))]),
        Some(vec![Ok(None)]),
        Some(vec![Ok(None)]),
    ]);

    let delivered = proposer(peers).send(&report(), Some("v1".to_owned())).await;

    assert_eq!(
        delivered,
        Delivered::Accepted {
            appended: Some(id(21))
        }
    );
    assert_eq!(count(&calls), [2, 0, 0]);
}

/// `not_flusher`, `stale` and `malformed` end the sending: no other node is
/// tried.
#[tokio::test]
async fn a_leaders_refusal_ends_the_sending() {
    for (refusal, expected) in [
        (Refusal::NotFlusher, Delivered::NotFlusher),
        (
            Refusal::Stale("behind".to_owned()),
            Delivered::Stale("behind".to_owned()),
        ),
        (
            Refusal::Malformed("short".to_owned()),
            Delivered::Malformed("short".to_owned()),
        ),
    ] {
        let accept = Some(vec![Ok(None)]);
        let (peers, calls, _serving) = voters([Some(vec![Err(refusal)]), accept.clone(), accept]);

        let delivered = proposer(peers).send(&report(), Some("v1".to_owned())).await;

        assert_eq!(delivered, expected);
        assert_eq!(count(&calls), [1, 0, 0]);
    }
}

/// With every voter refusing as not hosted, or uncertain answers that
/// never end, it stops after a bounded number of calls.
#[tokio::test]
async fn it_gives_up_after_a_bounded_number_of_calls() {
    let not_hosted = Some(vec![Err(Refusal::NotHosted)]);
    let (peers, calls, _serving) = voters([not_hosted.clone(), not_hosted.clone(), not_hosted]);
    let delivered = proposer(peers).send(&report(), None).await;
    assert!(
        matches!(delivered, Delivered::Undelivered(_)),
        "{delivered:?}"
    );
    assert_eq!(count(&calls), [1, 1, 1]);

    let lost = Some(vec![Err(Refusal::Failed("lost".to_owned()))]);
    let (peers, calls, _serving) = voters([lost.clone(), lost.clone(), lost]);
    let delivered = proposer(peers).send(&report(), Some("v1".to_owned())).await;
    assert!(
        matches!(delivered, Delivered::Undelivered(_)),
        "{delivered:?}"
    );
    assert_eq!(count(&calls).iter().sum::<usize>(), 2 * VOTERS.len());
}

/// A hint naming a node outside the group's voters is passed over.
#[tokio::test]
async fn a_hint_that_is_no_voter_is_passed_over() {
    let accept = Some(vec![Ok(None)]);
    let (peers, calls, _serving) = voters([accept.clone(), accept.clone(), accept]);

    let delivered = proposer(peers)
        .send(&report(), Some("elsewhere".to_owned()))
        .await;

    assert_eq!(delivered, Delivered::Accepted { appended: None });
    assert_eq!(count(&calls), [1, 0, 0]);
}
