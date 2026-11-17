use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_ingester::replicated::raft::host::HostRole;
use ourios_parquet::{PromotedAttributes, Store};
use ourios_server::config::wal::{ReplicatedWal, WalBackend};
use ourios_serving::AuthResolver;
use ourios_wal::WalConfig;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

use super::{Plan, plan};
use crate::receiver::ReceiverConfig;

/// A CA and a peer certificate it signed, written under `dir`, as the
/// `tls:` block of a replicated config.
fn peer_tls(dir: &Path) -> String {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().expect("key");
    let ca = params.self_signed(&ca_key).expect("ca");
    let issuer = Issuer::new(params, ca_key);
    let key = KeyPair::generate().expect("key");
    let leaf = CertificateParams::new(vec!["localhost".to_owned()])
        .expect("params")
        .signed_by(&key, &issuer)
        .expect("leaf");
    let write = |name: &str, pem: String| {
        let path = dir.join(name);
        std::fs::write(&path, pem).expect("write");
        path.display().to_string()
    };
    let (cert, key, ca) = (
        write("peer.crt", leaf.pem()),
        write("peer.key", key.serialize_pem()),
        write("ca.crt", ca.pem()),
    );
    format!(
        "      tls:\n        cert_file: {cert}\n        key_file: {key}\n        client_ca_file: {ca}\n"
    )
}

/// The replicated config of node `node_id` among voters a, b, c and the
/// tail replica t, over 4 groups.
fn replicated(node_id: &str, dir: &Path) -> ReplicatedWal {
    let yaml = format!(
        "\
storage:
  local:
    bucket_root: /data
receiver:
  enabled: true
  wal_root: /wal
  wal:
    backend: replicated
    replicated:
      node_id: {node_id}
      groups: 4
      peers: a=a:4320,b=b:4320,c=c:4320
      tail_peers: t=t:4320
      tail_clients: querier
{}",
        peer_tls(dir)
    );
    let file = ourios_server::config::file::parse(&yaml, &|_| None).expect("parses");
    let config = ourios_server::config::resolve::server_config_from_file(&file).expect("resolves");
    match config.receiver.expect("enabled").wal {
        WalBackend::Replicated(replicated) => *replicated,
        WalBackend::Local => panic!("a replicated backend"),
    }
}

fn receiver(wal_root: &Path) -> ReceiverConfig {
    ReceiverConfig {
        grpc_addr: "127.0.0.1:0".parse().expect("addr"),
        grpc_tls: None,
        http_addr: "127.0.0.1:0".parse().expect("addr"),
        http_tls: None,
        wal: WalConfig {
            root: wal_root.to_path_buf(),
            batch_window_ms: 100,
            segment_size_bytes: 128 * 1024 * 1024,
            segment_age_secs: 600,
            housekeeping_secs: 60,
            max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
            rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
            macos_full_fsync: false,
        },
        store: Store::in_memory(),
        promoted: PromotedAttributes::default(),
        auth: AuthResolver::static_only(None),
        graph_emitter: None,
        encode_workers: 1,
        miner: MinerConfig::default(),
        template_ids_allow_bootstrap: true,
        max_buffer_age: Duration::from_secs(60),
        max_request_bytes: ourios_ingester::receiver::MAX_REQUEST_BYTES,
    }
}

fn plan_of(node_id: &str) -> Plan {
    let dir = tempfile::tempdir().expect("tempdir");
    let wal_root = tempfile::tempdir().expect("tempdir");
    plan(
        &replicated(node_id, dir.path()),
        &receiver(wal_root.path()),
        Arc::new(AtomicBool::new(false)),
    )
    .expect("planned")
}

/// RFC 0065 §3.8, §6.1: a voter hosts the groups the voting layout places
/// on it, as a voter, and keeps no mined output; each group's tail
/// placement is the tail pool's.
#[test]
fn a_voter_hosts_its_layouts_groups() {
    let planned = plan_of("a");

    let layout = &planned.start.layout;
    assert_eq!(planned.start.groups, layout.groups_of("a"));
    assert!(matches!(planned.start.role, HostRole::Voter));
    assert!(planned.mined.is_none());
    assert_eq!(planned.start.node_id, "a");
    assert!(planned.start.allow_bootstrap);
    assert_eq!(planned.background.placed.len(), 4);
    for placed in planned.background.placed.values() {
        assert_eq!(placed, &["t".to_owned()]);
    }
}

/// RFC 0065 §6.7: a tail replica hosts the groups it is placed in, as a
/// tail with publication to the store, and receives their mined output.
#[test]
fn a_tail_replica_hosts_its_placed_groups_and_mines_them() {
    let planned = plan_of("t");

    assert_eq!(planned.start.groups, [0, 1, 2, 3]);
    let HostRole::Tail { publishing, .. } = &planned.start.role else {
        panic!("a tail replica");
    };
    let publishing = publishing.as_ref().expect("publishes");
    assert_eq!(publishing.voters.len(), 4);
    // An event count, as the local receiver's audit sink takes it.
    assert_eq!(
        publishing.audit_ceiling,
        crate::receiver::AUDIT_SINK_CEILING_EVENTS
    );
    assert!(planned.mined.is_some());
}
