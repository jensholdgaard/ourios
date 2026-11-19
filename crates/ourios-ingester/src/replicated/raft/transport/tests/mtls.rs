//! Mutual TLS between peers (B21): replication runs over it, and a
//! caller or a server without a certificate the peer CA signed is
//! refused.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::RPCError;
use openraft::network::v2::RaftNetworkV2 as _;
use openraft::raft::VoteRequest;
use ourios_serving::tls::{ALPN_GRPC, TlsMinVersion, TlsSettings};
use ourios_serving::tls_serve::{LISTENER_GRPC, reloading_acceptor};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::server::TcpIncoming;

use super::super::super::cluster::Serving;
use super::super::super::state_machine::GroupSm;
use super::super::super::types::{NodeId, Vote};
use super::super::tail::Clients;
use super::super::{ForwardRequest, Outcome, Peers, ReplicationServer};
use super::{
    GROUP, Node, Replica, bind, client_to, leader, listeners, option, vote_request, write,
};

/// A certificate authority minted for one test.
pub(in crate::replicated) struct Ca {
    issuer: Issuer<'static, KeyPair>,
    pem: String,
    der: rustls::pki_types::CertificateDer<'static>,
}

impl Ca {
    pub(in crate::replicated) fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("ca");
        Self {
            pem: cert.pem(),
            der: cert.der().clone(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A node's settings: a certificate for the loopback address this CA
    /// signed, with this CA as the peer CA.
    pub(in crate::replicated) fn node(&self, dir: &Path, name: &str) -> TlsSettings {
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .expect("params")
            .signed_by(&key, &self.issuer)
            .expect("leaf");
        self.settings(dir, name, (&cert, &key))
    }

    /// Settings over `cert` and its `key`, written under `dir` as `name`,
    /// with this CA as the peer CA.
    fn settings(
        &self,
        dir: &Path,
        name: &str,
        (cert, key): (&rcgen::Certificate, &KeyPair),
    ) -> TlsSettings {
        let cert_file = dir.join(format!("{name}.crt"));
        let key_file = dir.join(format!("{name}.key"));
        let ca_file = dir.join(format!("{name}-ca.crt"));
        std::fs::write(&cert_file, cert.pem()).expect("write cert");
        std::fs::write(&key_file, key.serialize_pem()).expect("write key");
        std::fs::write(&ca_file, &self.pem).expect("write ca");
        TlsSettings {
            cert_file,
            key_file,
            client_ca_file: Some(ca_file),
            min_version: TlsMinVersion::V1_2,
            reload_interval: None,
        }
    }
}

impl Ca {
    /// A client's settings: a certificate this CA signed naming `uri` as
    /// its only subject alternative name, as a querier's does.
    fn client(&self, dir: &Path, name: &str, uri: &str) -> TlsSettings {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![rcgen::SanType::URI(uri.try_into().expect("uri"))];
        let cert = params.signed_by(&key, &self.issuer).expect("leaf");
        self.settings(dir, name, (&cert, &key))
    }
}

fn serve_tls(
    server: ReplicationServer<GroupSm>,
    incoming: TcpIncoming,
    settings: &TlsSettings,
) -> Serving {
    let acceptor = reloading_acceptor(settings, ALPN_GRPC, LISTENER_GRPC).expect("acceptor");
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        server
            .serve(incoming, acceptor, shutdown)
            .await
            .expect("serve");
    });
    (stop, serving)
}

async fn start_tls(n: u64, incoming: TcpIncoming, settings: &TlsSettings, peers: Peers) -> Node {
    let replica = Replica::new(n, &[GROUP]);
    let server = ReplicationServer::default();
    Node::open_on(replica, &peers, server, |server| {
        serve_tls(server, incoming, settings)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_replicate_over_mutual_tls() {
    let ca = Ca::new();
    let certs = TempDir::new().expect("tempdir");
    let (listeners, members, addresses) = listeners(2);
    let mut nodes = Vec::new();
    for (n, incoming) in listeners {
        let settings = ca.node(certs.path(), &format!("node-{n}"));
        let peers = Peers::mutual_tls(&settings).expect("peers");
        nodes.push(
            start_tls(
                n,
                incoming,
                &settings,
                peers.with_addresses(addresses.clone()),
            )
            .await,
        );
    }
    nodes[0]
        .raft(GROUP)
        .initialize(members)
        .await
        .expect("initialize");

    let first = leader(GROUP, &nodes.iter().collect::<Vec<_>>(), None).await;
    let at = nodes.iter().position(|n| n.id == first).expect("leader");
    let index = write(nodes[at].raft(GROUP), 1).await;
    for node in &nodes {
        node.applied_through(GROUP, index).await;
    }
    for node in &mut nodes {
        node.kill().await;
    }
}

/// A client that trusts `ca` for the server and presents no certificate.
fn anonymous(ca: &Ca) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der.clone()).expect("root");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    config
}

/// `refused` is refused a vote by a TLS node served with `server`, which
/// keeps its vote, and a caller with `trusted` is then granted one.
async fn refused_then_granted(server: &TlsSettings, refused: &Peers, trusted: &Peers) {
    let (incoming, addr) = bind();
    let peers = Peers::mutual_tls(server).expect("peers");
    let mut node = start_tls(1, incoming, server, peers).await;
    let vote_before = node.raft(GROUP).metrics().borrow_watched().vote;
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);

    let mut stranger = client_to(refused, node.id, addr).await;
    let result = stranger.vote(higher.clone(), option()).await;
    // A refused client certificate reaches the caller as a TLS alert after
    // its handshake, a transport error rather than a refused connection.
    assert!(
        matches!(result, Err(RPCError::Unreachable(_) | RPCError::Network(_))),
        "{result:?}"
    );
    assert_eq!(
        node.raft(GROUP).metrics().borrow_watched().vote,
        vote_before
    );

    let mut peer = client_to(trusted, node.id, addr).await;
    let granted = peer.vote(higher, option()).await.expect("vote");
    assert!(granted.vote_granted);
    node.kill().await;
}

/// #1175: a refused client certificate is an unreachable peer, which
/// `openraft` backs off from, not a network error it retries sooner. It
/// arrives after the handshake, as a TLS alert, and tonic reports it as
/// `Unknown`.
#[tokio::test]
async fn a_refused_certificate_reads_as_an_unreachable_peer() {
    let (ca, other) = (Ca::new(), Ca::new());
    let certs = TempDir::new().expect("tempdir");
    let server = ca.node(certs.path(), "server");
    let rogue = TlsSettings {
        client_ca_file: server.client_ca_file.clone(),
        ..other.node(certs.path(), "rogue")
    };
    let (incoming, addr) = bind();
    let peers = Peers::mutual_tls(&server).expect("peers");
    let mut node = start_tls(1, incoming, &server, peers).await;
    let rogue = Peers::mutual_tls(&rogue).expect("peers");
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);

    let result = client_to(&rogue, node.id, addr)
        .await
        .vote(higher, option())
        .await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_))),
        "{result:?}"
    );
    node.kill().await;
}

/// #1175: a node's client certificate follows its files, as its
/// listener's does: once a refused certificate is replaced by one the peer
/// CA signed, the node's next connection presents it and is let in, with
/// no restart and no new pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotated_client_certificate_is_presented_without_a_restart() {
    let (ca, other) = (Ca::new(), Ca::new());
    let certs = TempDir::new().expect("tempdir");
    let server = ca.node(certs.path(), "server");
    let (incoming, addr) = bind();
    let mut node = start_tls(
        1,
        incoming,
        &server,
        Peers::mutual_tls(&server).expect("peers"),
    )
    .await;
    let rotating = TlsSettings {
        client_ca_file: server.client_ca_file.clone(),
        reload_interval: Some(Duration::from_millis(50)),
        ..other.node(certs.path(), "rotating")
    };
    let client = Peers::mutual_tls(&rotating).expect("peers");
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);
    let refused = client_to(&client, node.id, addr)
        .await
        .vote(higher.clone(), option())
        .await;
    assert!(refused.is_err(), "the other CA's certificate is refused");

    let signed = ca.node(certs.path(), "signed");
    std::fs::copy(&signed.cert_file, &rotating.cert_file).expect("rotate cert");
    std::fs::copy(&signed.key_file, &rotating.key_file).expect("rotate key");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let vote = client_to(&client, node.id, addr)
            .await
            .vote(higher.clone(), option())
            .await;
        if vote.is_ok_and(|granted| granted.vote_granted) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the rotated certificate was never presented"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node.kill().await;
}

#[tokio::test]
async fn a_caller_without_a_certificate_is_refused() {
    let ca = Ca::new();
    let certs = TempDir::new().expect("tempdir");
    let server = ca.node(certs.path(), "server");
    let trusted = Peers::mutual_tls(&ca.node(certs.path(), "peer")).expect("peers");
    refused_then_granted(
        &server,
        &Peers::from_client_config(anonymous(&ca)),
        &trusted,
    )
    .await;
}

#[tokio::test]
async fn a_caller_certificate_from_another_ca_is_refused() {
    let (ca, other) = (Ca::new(), Ca::new());
    let certs = TempDir::new().expect("tempdir");
    let server = ca.node(certs.path(), "server");
    let trusted = Peers::mutual_tls(&ca.node(certs.path(), "peer")).expect("peers");
    let rogue = TlsSettings {
        client_ca_file: server.client_ca_file.clone(),
        ..other.node(certs.path(), "rogue")
    };
    let rogue = Peers::mutual_tls(&rogue).expect("peers");
    refused_then_granted(&server, &rogue, &trusted).await;
}

#[tokio::test]
async fn a_server_certificate_from_another_ca_is_refused() {
    let (ca, other) = (Ca::new(), Ca::new());
    let certs = TempDir::new().expect("tempdir");
    let caller = ca.node(certs.path(), "caller");
    let server = TlsSettings {
        client_ca_file: caller.client_ca_file.clone(),
        ..other.node(certs.path(), "server")
    };
    let refused = Peers::mutual_tls(&caller).expect("peers");
    let trusting = TlsSettings {
        client_ca_file: other.node(certs.path(), "trusting").client_ca_file,
        ..ca.node(certs.path(), "peer")
    };
    let trusting = Peers::mutual_tls(&trusting).expect("peers");
    refused_then_granted(&server, &refused, &trusting).await;
}

#[test]
fn a_pool_needs_a_peer_ca() {
    let ca = Ca::new();
    let certs = TempDir::new().expect("tempdir");
    let settings = TlsSettings {
        client_ca_file: None,
        ..ca.node(certs.path(), "node")
    };
    let refused = Peers::mutual_tls(&settings).err().expect("no peer CA");
    assert!(refused.contains("client_ca_file"), "{refused}");
}

#[tokio::test]
async fn a_peer_that_never_answers_the_handshake_is_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let holding = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let ca = Ca::new();
    let certs = TempDir::new().expect("tempdir");
    let peers = Peers::mutual_tls(&ca.node(certs.path(), "caller")).expect("peers");

    let mut client = client_to(&peers, NodeId::from(1), addr).await;
    let started = tokio::time::Instant::now();
    let result = client.vote(vote_request(), option()).await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_) | RPCError::Network(_))),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    holding.abort();
}

/// A forward whose connection fails the caller's own handshake was never
/// sent: a server certificate from another CA, or a peer that never
/// answers the handshake within the connect timeout.
#[tokio::test]
async fn a_forward_that_fails_the_handshake_was_never_sent() {
    let (ca, other) = (Ca::new(), Ca::new());
    let certs = TempDir::new().expect("tempdir");
    let caller = ca.node(certs.path(), "caller");
    let server = TlsSettings {
        client_ca_file: caller.client_ca_file.clone(),
        ..other.node(certs.path(), "server")
    };
    let (incoming, addr) = bind();
    let peers = Peers::mutual_tls(&server).expect("peers");
    let mut node = start_tls(1, incoming, &server, peers).await;
    let request = || ForwardRequest {
        version: super::super::VERSION,
        group: GROUP,
        payload: ourios_wal::TenantBatch::encode("acme", b"record").expect("batch"),
        credential: None,
    };
    let commit = Duration::from_millis(200);
    let caller = Peers::mutual_tls(&caller).expect("peers");
    let rogue = caller
        .clone()
        .with_addresses([("n1".to_owned(), addr.to_string())]);
    let refused = rogue.forward("n1", request(), commit).await;
    assert!(matches!(refused, Outcome::Unreached(_)), "{refused:?}");
    node.kill().await;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let silent = listener.local_addr().expect("addr");
    let holding = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let waiting = caller.with_addresses([("n2".to_owned(), silent.to_string())]);
    let started = tokio::time::Instant::now();
    let unanswered = waiting.forward("n2", request(), commit).await;
    assert!(
        matches!(unanswered, Outcome::Unreached(_)),
        "{unanswered:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    holding.abort();
}

/// A forward whose caller certificate the server refuses is uncertain, not
/// never sent: the refusal comes as an alert after the caller's side of
/// the handshake has finished, by when the request may have been written.
/// Reading it as never sent would let the sender resubmit an export that
/// might have committed.
#[tokio::test]
async fn a_forward_whose_caller_certificate_is_refused_is_uncertain() {
    let (ca, other) = (Ca::new(), Ca::new());
    let certs = TempDir::new().expect("tempdir");
    let server = ca.node(certs.path(), "server");
    let (incoming, addr) = bind();
    let peers = Peers::mutual_tls(&server).expect("peers");
    let mut node = start_tls(1, incoming, &server, peers).await;
    let rogue = TlsSettings {
        client_ca_file: server.client_ca_file.clone(),
        ..other.node(certs.path(), "rogue")
    };
    let rogue = Peers::mutual_tls(&rogue)
        .expect("peers")
        .with_addresses([("n1".to_owned(), addr.to_string())]);
    let request = ForwardRequest {
        version: super::super::VERSION,
        group: GROUP,
        payload: ourios_wal::TenantBatch::encode("acme", b"record").expect("batch"),
        credential: None,
    };
    let outcome = rogue
        .forward("n1", request, Duration::from_millis(200))
        .await;
    assert!(matches!(outcome, Outcome::Uncertain(_)), "{outcome:?}");
    node.kill().await;
}

/// A tail that answers every read with no rows.
struct Fixed;

impl super::super::tail::TailTarget for Fixed {
    fn tail(&self, _: super::super::tail::TailQuery) -> super::super::tail::Tailing {
        Box::pin(async {
            Ok(super::super::tail::TailPoint {
                read: 1,
                published: Vec::new(),
            })
        })
    }

    fn scan(&self, _: super::super::tail::ScanQuery) -> super::super::tail::Scanning {
        Box::pin(async {
            Ok(super::super::tail::ScanPage {
                rows: Vec::new(),
                templates: Vec::new(),
                next: None,
            })
        })
    }
}

/// RFC0065.13: over mutual TLS a tail replica serves only a caller whose
/// certificate names one of its `tail_clients`; another replica, whose
/// certificate the same CA signed, is denied.
#[tokio::test]
async fn only_a_listed_client_reads_the_tail_over_mutual_tls() {
    use super::super::tail::{Outcome as TailOutcome, TailQuery};

    let ca = Ca::new();
    let dir = TempDir::new().expect("temp");
    let server = ReplicationServer::<GroupSm>::default();
    server.serve_tails(Arc::new(Fixed), Clients::only(["spiffe://cluster/querier"]));
    let (incoming, addr) = bind();
    let _serving = serve_tls(server, incoming, &ca.node(dir.path(), "tail"));
    let peers = |tls: &TlsSettings| {
        Peers::mutual_tls(tls)
            .expect("peers")
            .with_addresses([("t".to_owned(), addr.to_string())])
    };
    let query = || TailQuery {
        group: 0,
        tenant: "acme".to_owned(),
        from: 0,
    };
    let patience = Duration::from_secs(5);

    let querier = peers(&ca.client(dir.path(), "querier", "spiffe://cluster/querier"));
    let read = querier.tail("t", query(), patience).await;
    assert!(matches!(read, TailOutcome::Answered(_)), "{read:?}");
    let replica = peers(&ca.node(dir.path(), "replica"));
    assert_eq!(
        replica.tail("t", query(), patience).await,
        TailOutcome::Denied
    );
}

/// A tail client's certificate reads the tail and nothing else: the peer
/// CA signed it, but a querier may not vote or append (RFC 0065 B21).
#[tokio::test]
async fn a_tail_client_cannot_call_the_consensus_rpcs() {
    a_querier_cannot_vote(|server| {
        server.serve_tails(Arc::new(Fixed), Clients::only([QUERIER]));
    })
    .await;
}

/// The same holds on a node that serves no tail, a voter's: its
/// `tail_clients` alone restrict the listed callers.
#[tokio::test]
async fn a_tail_client_cannot_vote_where_no_tail_is_served() {
    a_querier_cannot_vote(|server| server.admit_tail_clients(Clients::only([QUERIER]))).await;
}

const QUERIER: &str = "spiffe://cluster/querier";

/// A node whose replication server `restrict` set up refuses a vote from
/// the querier's certificate, and grants one from a replica's.
async fn a_querier_cannot_vote(restrict: impl FnOnce(&ReplicationServer<GroupSm>)) {
    let ca = Ca::new();
    let dir = TempDir::new().expect("temp");
    let settings = ca.node(dir.path(), "n1");
    let (incoming, addr) = bind();
    let peers = Peers::mutual_tls(&settings).expect("peers");
    let server = ReplicationServer::default();
    restrict(&server);
    let mut node = Node::open_on(Replica::new(1, &[GROUP]), &peers, server, |server| {
        serve_tls(server, incoming, &settings)
    })
    .await;
    let vote_before = node.raft(GROUP).metrics().borrow_watched().vote;
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);

    let querier = Peers::mutual_tls(&ca.client(dir.path(), "querier", QUERIER)).expect("peers");
    let refused = client_to(&querier, node.id, addr)
        .await
        .vote(higher.clone(), option())
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        node.raft(GROUP).metrics().borrow_watched().vote,
        vote_before
    );

    let replica = Peers::mutual_tls(&ca.node(dir.path(), "n2")).expect("peers");
    let granted = client_to(&replica, node.id, addr)
        .await
        .vote(higher, option())
        .await
        .expect("vote");
    assert!(granted.vote_granted);
    node.kill().await;
}
