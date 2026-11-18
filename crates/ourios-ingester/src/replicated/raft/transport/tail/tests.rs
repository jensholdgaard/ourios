//! `Tail` and `TailScan`: how a tail replica answers each refusal, how the
//! querier reads each answer, who the gate admits, and how a result larger
//! than one message arrives a page at a time.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tonic::Status;

use super::super::wire::MAX_MESSAGE_BYTES;
use super::{
    Clients, Outcome, PAGE_BYTES, Refusal, ScanPage, ScanQuery, Scanning, TailPoint, TailQuery,
    TailRequest, TailTarget, Tailing, TemplateDefinition, WirePosition, call, classify, refusal,
    serve,
};
use crate::replicated::raft::cluster::{bind, serve as serve_on};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::transport::{Peers, ReplicationServer, TAIL, VERSION};

fn query() -> TailQuery {
    TailQuery {
        group: 4,
        tenant: "acme".to_owned(),
        from: 10,
    }
}

fn point() -> TailPoint {
    TailPoint {
        read: 42,
        published: vec!["data/tenant_id=acme/x.parquet".to_owned()],
    }
}

fn scan(after: Option<WirePosition>) -> ScanQuery {
    ScanQuery {
        group: 4,
        tenant: "acme".to_owned(),
        from: 10,
        read: 42,
        range: Some((1, 2)),
        after,
    }
}

fn template() -> TemplateDefinition {
    TemplateDefinition {
        template_id: 7,
        template_version: 2,
        template: "user <*> logged in".to_owned(),
    }
}

/// Pages of `PAGE` bytes each, `PAGES` of them, ending at position `n`.
const PAGES: u64 = 3;
const PAGE: usize = PAGE_BYTES - 1024;

/// A tail replica that answers every read the same way, its scan paged
/// over [`PAGES`] pages, checking what reached it.
struct Fake(Result<TailPoint, Refusal>);

impl TailTarget for Fake {
    fn tail(&self, query: TailQuery) -> Tailing {
        assert_eq!(query, self::query());
        let answer = self.0.clone();
        Box::pin(async move { answer })
    }

    fn scan(&self, query: ScanQuery) -> Scanning {
        assert_eq!(
            ScanQuery {
                after: None,
                ..query.clone()
            },
            scan(None)
        );
        let at = query.after.map_or(0, |after| after.seq);
        let answer = self.0.clone().map(|_| ScanPage {
            rows: vec![u8::try_from(at).expect("small"); PAGE],
            templates: vec![template()],
            next: (at + 1 < PAGES).then_some(WirePosition {
                seq: at + 1,
                index: 0,
            }),
        });
        Box::pin(async move { answer })
    }
}

#[test]
fn each_refusal_reads_back_as_the_outcome_it_proves() {
    let read = |r: Refusal| classify::<()>(&refusal(r));
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
        classify::<()>(&Status::unimplemented("no such method")),
        Outcome::NotHosted,
        "a node without Tail"
    );
}

#[tokio::test]
async fn another_version_and_a_missing_target_are_not_hosted() {
    let stale = TailRequest {
        version: VERSION + 1,
        ..query().request()
    };
    let target: Arc<dyn TailTarget> = Arc::new(Fake(Ok(point())));
    let refused = serve(Some(Arc::clone(&target)), stale)
        .await
        .expect_err("another version");
    assert_eq!(classify::<()>(&refused), Outcome::NotHosted);
    let refused = serve(None, query().request()).await.expect_err("no target");
    assert_eq!(classify::<()>(&refused), Outcome::NotHosted);
}

#[test]
fn the_gate_admits_only_a_listed_identity() {
    let clients = Clients::only(["spiffe://cluster/querier", "querier.logs.svc"]);
    let names = |list: &[&str]| list.iter().map(|&n| n.to_owned()).collect::<BTreeSet<_>>();
    assert!(clients.admits(&names(&["spiffe://cluster/querier"])));
    assert!(clients.admits(&names(&["other.example", "querier.logs.svc"])));
    assert!(!clients.admits(&names(&["spiffe://cluster/receiver"])));
    assert!(!clients.admits(&names(&[])), "no certificate, no rows");
    assert!(clients.lists(&names(&["querier.logs.svc"])));
    assert!(!clients.lists(&names(&["spiffe://cluster/receiver"])));
    let anyone = Clients::anyone();
    assert!(anyone.admits(&names(&[])) && !anyone.lists(&names(&["querier"])));
}

/// A node serving `fake` on loopback to `clients`, and peers that name it
/// `n1`.
fn node(fake: Fake, clients: Clients) -> (Peers, crate::replicated::raft::cluster::Serving) {
    let server = ReplicationServer::<GroupSm>::default();
    server.serve_tails(Arc::new(fake), clients);
    let (incoming, addr) = bind();
    let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
    (peers, serve_on(server, incoming))
}

const PATIENCE: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_tail_call_over_loopback_reads_each_answer() {
    for (fake, expected) in [
        (Fake(Ok(point())), Outcome::Answered(point())),
        (
            Fake(Err(Refusal::TooOld { oldest: 12 })),
            Outcome::TooOld { oldest: 12 },
        ),
        (Fake(Err(Refusal::NotHosted)), Outcome::NotHosted),
    ] {
        let (peers, _serving) = node(fake, Clients::anyone());
        assert_eq!(peers.tail("n1", query(), PATIENCE).await, expected);
    }
}

/// A result larger than one message arrives whole, a page at a time: each
/// page within [`PAGE_BYTES`], the next asked from where it ended.
#[tokio::test]
async fn a_tail_larger_than_one_message_arrives_a_page_at_a_time() {
    let (peers, _serving) = node(Fake(Ok(point())), Clients::anyone());
    let mut after = None;
    let mut received = Vec::new();
    loop {
        let Outcome::Answered(page) = peers.tail("n1", scan(after), PATIENCE).await else {
            panic!("a page");
        };
        assert!(page.rows.len() <= PAGE_BYTES);
        assert_eq!(page.templates, vec![template()]);
        received.extend(page.rows);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert!(
        received.len() > MAX_MESSAGE_BYTES,
        "{} bytes is not over one message",
        received.len()
    );
    assert_eq!(
        received.len(),
        PAGE * usize::try_from(PAGES).expect("small")
    );
}

/// A node whose clients do not include the caller denies both calls, and
/// before decoding: another protocol version, or a body that does not
/// decode, still reads as denied (RFC0065.13).
#[tokio::test]
async fn an_unadmitted_caller_is_denied_before_decoding() {
    let (peers, _serving) = node(Fake(Ok(point())), Clients::only(["querier"]));
    assert_eq!(peers.tail("n1", query(), PATIENCE).await, Outcome::Denied);
    assert_eq!(
        peers.tail("n1", scan(None), PATIENCE).await,
        Outcome::Denied
    );

    let channel = peers_channel(&peers);
    let other_version = TailRequest {
        version: VERSION + 1,
        ..query().request()
    };
    let (outcome, _) =
        call::<_, super::TailResponse>(channel.clone(), (TAIL, other_version), PATIENCE).await;
    assert_eq!(outcome, Outcome::Denied);
    let undecodable = raw_call(channel, vec![0xff, 0xff, 0xff]).await;
    assert_eq!(classify::<()>(&undecodable), Outcome::Denied);
}

/// A channel to the node `peers` name `n1`.
fn peers_channel(peers: &Peers) -> tonic::transport::Channel {
    peers.resolve("n1").1.expect("channel")
}

/// The bytes of a message, sent and read as they are.
#[derive(Clone, Copy, Default)]
struct Raw;

impl tonic::codec::Codec for Raw {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = Raw;
    type Decoder = Raw;
    fn encoder(&mut self) -> Raw {
        Raw
    }
    fn decoder(&mut self) -> Raw {
        Raw
    }
}

impl tonic::codec::Encoder for Raw {
    type Item = Vec<u8>;
    type Error = Status;
    fn encode(
        &mut self,
        item: Vec<u8>,
        dst: &mut tonic::codec::EncodeBuf<'_>,
    ) -> Result<(), Status> {
        use bytes::BufMut as _;
        dst.put_slice(&item);
        Ok(())
    }
}

impl tonic::codec::Decoder for Raw {
    type Item = Vec<u8>;
    type Error = Status;
    fn decode(&mut self, src: &mut tonic::codec::DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Status> {
        use bytes::Buf as _;
        Ok(Some(src.copy_to_bytes(src.remaining()).to_vec()))
    }
}

/// Send `body` as a `Tail` request's message, undecoded; the refusal.
async fn raw_call(channel: tonic::transport::Channel, body: Vec<u8>) -> Status {
    use tonic::codegen::http::uri::PathAndQuery;

    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.expect("ready");
    grpc.unary(
        tonic::Request::new(body),
        PathAndQuery::from_static(TAIL),
        Raw,
    )
    .await
    .expect_err("refused")
}

/// A call the gate denies is still traced (RFC 0065 §6.9): its server span
/// records `PERMISSION_DENIED`, the child of the caller's client span.
#[tokio::test]
async fn a_denied_call_records_its_server_span() {
    use opentelemetry::trace::SpanKind;

    use crate::replicated::raft::transport::tests::{
        assert_child_of_client, attr, capture, of_kind, peer_spans,
    };

    let (exporter, tracing, _guard) = capture();
    let server = ReplicationServer::<GroupSm>::traced_by(tracing.clone());
    server.serve_tails(Arc::new(Fake(Ok(point()))), Clients::only(["querier"]));
    let (incoming, addr) = bind();
    let _serving = serve_on(server, incoming);
    let peers = Peers::traced_by(tracing).with_addresses([("n1".to_owned(), addr.to_string())]);
    assert_eq!(peers.tail("n1", query(), PATIENCE).await, Outcome::Denied);

    let spans = peer_spans(&exporter);
    let server = of_kind(&spans, &SpanKind::Server);
    assert_eq!(
        attr(&server, "rpc.response.status_code").as_deref(),
        Some("PERMISSION_DENIED")
    );
    assert_child_of_client(&spans);
}

/// A node admits its tail's clients as its host opens and serves its tail
/// target later: serving the target keeps the clients it admitted.
#[tokio::test]
async fn a_target_served_later_keeps_the_admitted_clients() {
    for (clients, expected) in [
        (Clients::only(["querier"]), Outcome::Denied),
        (Clients::anyone(), Outcome::Answered(point())),
    ] {
        let server = ReplicationServer::<GroupSm>::default();
        server.admit_tail_clients(clients);
        server.serve_tail_target(Arc::new(Fake(Ok(point()))));
        let (incoming, addr) = bind();
        let peers = Peers::plaintext().with_addresses([("n1".to_owned(), addr.to_string())]);
        let _serving = serve_on(server, incoming);
        assert_eq!(peers.tail("n1", query(), PATIENCE).await, expected);
    }
}
