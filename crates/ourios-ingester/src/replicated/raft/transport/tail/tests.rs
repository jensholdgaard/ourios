//! `Tail`'s outcomes: how a tail replica answers each refusal, how the
//! querier reads each answer, and who the gate admits.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tonic::Status;

use super::{
    Clients, Outcome, Refusal, TailAnswer, TailQuery, TailRequest, TailTarget, Tailing,
    TemplateDefinition, classify, refusal, serve,
};
use crate::replicated::raft::cluster::{bind, serve as serve_on};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::transport::{Peers, ReplicationServer, VERSION};

fn request() -> TailRequest {
    TailRequest {
        version: VERSION,
        group: 4,
        tenant: "acme".to_owned(),
        from: 10,
        start_unix_nano: Some(1),
        end_unix_nano: Some(2),
    }
}

fn answer() -> TailAnswer {
    TailAnswer {
        read: 42,
        rows: vec![1, 2, 3],
        templates: vec![TemplateDefinition {
            template_id: 7,
            template_version: 2,
            template: "user <*> logged in".to_owned(),
        }],
        published: vec!["data/tenant_id=acme/x.parquet".to_owned()],
    }
}

/// A tail replica that answers every query the same way, checking what
/// reached it.
struct Fake(Result<TailAnswer, Refusal>);

impl TailTarget for Fake {
    fn tail(&self, query: TailQuery) -> Tailing {
        assert_eq!(
            query,
            TailQuery {
                group: 4,
                tenant: "acme".to_owned(),
                from: 10,
                range: Some((1, 2)),
            }
        );
        let answer = self.0.clone();
        Box::pin(async move { answer })
    }
}

#[test]
fn each_refusal_reads_back_as_the_outcome_it_proves() {
    let read = |r: Refusal| classify(&refusal(r));
    assert_eq!(read(Refusal::NotHosted), Outcome::NotHosted);
    assert_eq!(
        read(Refusal::TooOld { oldest: 12 }),
        Outcome::TooOld { oldest: 12 }
    );
    assert!(matches!(
        read(Refusal::Unavailable("no read point".to_owned())),
        Outcome::Unavailable(_)
    ));
    assert_eq!(read(Refusal::Denied), Outcome::Denied);
    assert_eq!(
        classify(&Status::unimplemented("no such method")),
        Outcome::NotHosted,
        "a node without Tail"
    );
}

#[tokio::test]
async fn another_version_and_a_missing_target_are_not_hosted() {
    let stale = TailRequest {
        version: VERSION + 1,
        ..request()
    };
    let target: Arc<dyn TailTarget> = Arc::new(Fake(Ok(answer())));
    let refused = serve(Some(Arc::clone(&target)), true, stale)
        .await
        .expect_err("another version");
    assert_eq!(classify(&refused), Outcome::NotHosted);
    let refused = serve(None, true, request()).await.expect_err("no target");
    assert_eq!(classify(&refused), Outcome::NotHosted);
}

/// A caller the gate does not admit gets no rows, whatever the target
/// would answer (RFC0065.13).
#[tokio::test]
async fn an_unadmitted_caller_is_denied_before_the_target() {
    let target: Arc<dyn TailTarget> = Arc::new(Fake(Ok(answer())));
    let refused = serve(Some(target), false, request())
        .await
        .expect_err("denied");
    assert_eq!(classify(&refused), Outcome::Denied);
}

#[test]
fn the_gate_admits_only_a_listed_identity() {
    let clients = Clients::only(["spiffe://cluster/querier", "querier.logs.svc"]);
    let names = |list: &[&str]| list.iter().map(|&n| n.to_owned()).collect::<BTreeSet<_>>();
    assert!(clients.admits(&names(&["spiffe://cluster/querier"])));
    assert!(clients.admits(&names(&["other.example", "querier.logs.svc"])));
    assert!(!clients.admits(&names(&["spiffe://cluster/receiver"])));
    assert!(!clients.admits(&names(&[])), "no certificate, no rows");
}

/// A node serving `fake` on loopback to anyone, and peers that name it
/// `n1`.
fn node(fake: Fake) -> (Peers, crate::replicated::raft::cluster::Serving) {
    let server = ReplicationServer::<GroupSm>::default();
    server.serve_tails(Arc::new(fake), Clients::anyone());
    let (incoming, addr) = bind();
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    (peers, serve_on(server, incoming))
}

const PATIENCE: Duration = Duration::from_secs(2);

#[tokio::test]
async fn a_tail_call_over_loopback_reads_each_answer() {
    for (fake, expected) in [
        (Fake(Ok(answer())), Outcome::Answered(answer())),
        (
            Fake(Err(Refusal::TooOld { oldest: 12 })),
            Outcome::TooOld { oldest: 12 },
        ),
        (Fake(Err(Refusal::NotHosted)), Outcome::NotHosted),
    ] {
        let (peers, _serving) = node(fake);
        let query = TailQuery {
            group: 4,
            tenant: "acme".to_owned(),
            from: 10,
            range: Some((1, 2)),
        };
        assert_eq!(peers.tail("n1", query, PATIENCE).await, expected);
    }
}

/// With no gate set, a plaintext caller presents no certificate and is
/// denied, as on a node configured without the tail service.
#[tokio::test]
async fn a_node_admits_no_one_until_its_clients_are_set() {
    let server = ReplicationServer::<GroupSm>::default();
    server.serve_tails(Arc::new(Fake(Ok(answer()))), Clients::only(["querier"]));
    let (incoming, addr) = bind();
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    let _serving = serve_on(server, incoming);
    let query = TailQuery {
        group: 4,
        tenant: "acme".to_owned(),
        from: 10,
        range: Some((1, 2)),
    };
    assert_eq!(peers.tail("n1", query, PATIENCE).await, Outcome::Denied);
}
