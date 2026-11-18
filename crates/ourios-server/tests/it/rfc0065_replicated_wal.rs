//! RFC 0065 — replicated WAL and a queryable tail, the config,
//! served-binary and chart arms. See `docs/rfcs/0065-replicated-wal.md`
//! §5 and §8. The in-process cluster arms are in `ourios-ingester`'s
//! `tests/it/rfc0065_replicated_wal/`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC is
//! red; each names the slice that discharges it. RFC0065.1's layout arm is
//! a unit test in `src/main.rs`: the replicated backend needs a store with
//! conditional updates, and the local store these tests serve on has none.

use std::collections::BTreeMap;
use std::time::Duration;

use ourios_ingester::replicated::raft::group_config;
use ourios_server::config::file::parse;
use ourios_server::config::resolve::server_config_from_file;
use ourios_server::config::wal::{ReplicatedWal, WalBackend};

/// Scenario RFC0065.1 — the start arm. Three servers configured with
/// `receiver.wal.backend: replicated` and every field set start on an S3
/// store, form their group over mutual TLS, record the voting layout,
/// acknowledge an OTLP export, and stop cleanly on SIGTERM.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0065.1 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0065_1_a_valid_replicated_config_starts() {
    use served::{exported, free_addr, peer_tls, start};

    let bucket = "ourios-rfc0065-1";
    let (_localstack, endpoint, store) =
        crate::rfc0019_storage_backend::localstack_s3(bucket).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let tls = peer_tls(dir.path());
    let names = ["a", "b", "c"];
    let addrs: Vec<_> = names.iter().map(|_| free_addr()).collect();
    let peers = names
        .iter()
        .zip(&addrs)
        .map(|(name, addr)| format!("{name}={addr}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut servers = Vec::new();
    for (name, addr) in names.iter().zip(&addrs) {
        let leaves = format!(
            "      node_id: {name}\n      listen_addr: {addr}\n      groups: 1\n      \
             peers: {peers}\n      tail_peers: t=127.0.0.1:1\n      tail_clients: querier\n{tls}"
        );
        servers.push(start(dir.path(), name, (&endpoint, bucket), &leaves).await);
    }

    assert!(
        exported(servers[0].grpc).await,
        "an export is acknowledged once the group forms"
    );
    let keys = tokio::task::spawn_blocking(move || store.list_blocking(None).expect("list"))
        .await
        .expect("listed");
    assert!(
        keys.iter()
            .any(|key| key == ourios_ingester::replicated::layout::LAYOUT_KEY),
        "the voting layout is recorded: {keys:?}"
    );
    for server in servers {
        server.terminate_cleanly().await;
    }
}

/// Serving the binary with the replicated backend.
#[cfg(unix)]
mod served;

/// RFC0065.3: a leader killed mid-batch.
#[cfg(unix)]
mod crash;

/// A replicated `receiver.wal` with every required leaf and the given
/// timing lines, `${env:…}` resolved from `env`.
fn replicated_wal(timings: &str, env: &[(&str, &str)]) -> Result<ReplicatedWal, String> {
    let yaml = format!(
        "storage:\n  local:\n    bucket_root: /data\nreceiver:\n  enabled: true\n  \
         wal_root: /wal\n  wal:\n    backend: replicated\n    replicated:\n      \
         node_id: receiver-0\n      peers: receiver-0=r0.wal:4320,receiver-1=r1.wal:4320,\
         receiver-2=r2.wal:4320\n      tail_peers: tail-0=t0.wal:4320\n      \
         tail_clients: querier-a\n      tls:\n        cert_file: /tls/peer.crt\n        \
         key_file: /tls/peer.key\n        client_ca_file: /tls/ca.crt\n{timings}"
    );
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    };
    let file = parse(&yaml, &lookup).map_err(|e| e.to_string())?;
    match server_config_from_file(&file)?.receiver.map(|r| r.wal) {
        Some(WalBackend::Replicated(wal)) => Ok(*wal),
        other => Err(format!("not a replicated receiver: {other:?}")),
    }
}

/// The heartbeat and election timeouts of the `openraft` config every
/// group on the node is built with.
fn group_timings(wal: &ReplicatedWal) -> (u64, u64, u64) {
    let config = group_config(wal.raft_timings).expect("a valid group config");
    (
        config.heartbeat_interval,
        config.election_timeout_min,
        config.election_timeout_max,
    )
}

/// Scenario RFC0065.1 — the Raft timings arm. Unset, every group runs on
/// the 50 ms heartbeat and 150–300 ms election timeout; set, on the
/// configured ones; a zero or mis-ordered value fails startup naming the
/// key. The env front-end's arm is
/// `config::wal::tests::rfc0065_1_invalid_raft_timings_fail_naming_the_key`.
#[test]
fn rfc0065_1_the_raft_timings_are_configured_and_checked() {
    let unset = replicated_wal("", &[]).expect("the timings default");
    assert_eq!(group_timings(&unset), (50, 150, 300));

    let set = "      heartbeat_interval_ms: ${env:HEARTBEAT}\n      \
               election_timeout_min_ms: ${env:ELECTION_MIN}\n      \
               election_timeout_max_ms: 2000\n";
    let env = [("HEARTBEAT", "200"), ("ELECTION_MIN", "1000")];
    let configured = replicated_wal(set, &env).expect("configured timings resolve");
    assert_eq!(group_timings(&configured), (200, 1000, 2000));

    let invalid = [
        ("      heartbeat_interval_ms: 0\n", "heartbeat_interval_ms"),
        (
            "      election_timeout_min_ms: 0\n",
            "election_timeout_min_ms",
        ),
        (
            "      election_timeout_max_ms: 0\n",
            "election_timeout_max_ms",
        ),
        (
            "      heartbeat_interval_ms: 150\n",
            "heartbeat_interval_ms",
        ),
        (
            "      election_timeout_min_ms: 300\n",
            "election_timeout_min_ms",
        ),
    ];
    for (line, key) in invalid {
        let error = replicated_wal(line, &[]).expect_err(line);
        let named = format!("receiver.wal.replicated.{key} ");
        assert!(error.starts_with(&named), "{line:?}: {error}");
    }
}

/// Scenario RFC0065.3 — crash recovery on the replicated backend (B2, B7).
/// Given three voters and a tail replica ingesting steadily, when each voter
/// in turn is killed with `SIGKILL` mid-batch and restarted, then another
/// takes over without an operator each time, every acked record is in
/// Parquet after the next checkpoint, and after compaction no
/// `(tenant, ingest position)` appears twice. The multi-process crash test
/// RFC 0064 §3.2 requires.
///
/// Killing a follower moves no leadership, so killing every voter in turn
/// kills the leader at least once, whichever it is.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "RFC0065.3 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0065_3_sigkill_of_the_leader_loses_no_acked_record() {
    use crash::{Stream, Targets, published};
    use served::{free_addr, peer_tls, restart, start};

    let bucket = "ourios-rfc0065-3";
    let (_localstack, endpoint, store) =
        crate::rfc0019_storage_backend::localstack_s3(bucket).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let tls = peer_tls(dir.path());
    let voters = ["a", "b", "c"];
    let addrs: Vec<_> = (0..4).map(|_| free_addr()).collect();
    let peers = voters
        .iter()
        .zip(&addrs)
        .map(|(name, addr)| format!("{name}={addr}"))
        .collect::<Vec<_>>()
        .join(",");
    let tail = format!("t={}", addrs[3]);
    let mut servers = BTreeMap::new();
    for (name, addr) in voters.iter().chain(&["t"]).zip(&addrs) {
        let leaves = format!(
            "      node_id: {name}\n      listen_addr: {addr}\n      groups: 1\n      \
             peers: {peers}\n      tail_peers: {tail}\n      tail_clients: querier\n{tls}"
        );
        let server = start(dir.path(), name, (&endpoint, bucket), &leaves).await;
        servers.insert(*name, server);
    }
    let targets = Targets::default();
    for name in voters {
        targets.insert(name, servers[name].grpc);
    }
    assert!(served::exported(servers["a"].grpc).await, "the group forms");

    // Hours old, so their partitions are sealed and compacted during the test.
    let three_hours_ago = std::time::SystemTime::now() - Duration::from_secs(3 * 3_600);
    let stamped = three_hours_ago
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let stream = Stream::start(&targets, u64::try_from(stamped).expect("fits"), &stopping);
    for name in voters {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        targets.remove(name);
        servers.remove(name).expect("serving").kill().await;
        // Before it returns: if it led, one of the other two took over.
        let after_the_kill = stream.unsent();
        assert!(
            stream
                .acks_from(after_the_kill, Duration::from_secs(30))
                .await,
            "an export sent after {name} is killed is acknowledged without an operator"
        );
        let server = restart(dir.path(), name).await;
        targets.insert(name, server.grpc);
        servers.insert(name, server);
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    stop.send_replace(true);
    let acked = stream.finish().await;
    assert!(acked.len() > 50, "a steady stream: {} acked", acked.len());

    published(&store, &acked, Duration::from_secs(120)).await;
    for server in servers.into_values() {
        server.terminate_cleanly().await;
    }
}

/// Scenario RFC0065.11 — a record is queryable within the allowance, the
/// latency arm (B15). On the CI runner, ack to a query returning the record
/// is under 5 s across served binaries.
#[test]
#[ignore = "RFC0065.11 stub — implemented with the multi-process crash test's cluster"]
fn rfc0065_11_ack_to_queryable_is_under_five_seconds() {
    todo!(
        "RFC0065.11 — with the flush age at 300 s, the time from ack to a query \
         returning the record is under 5 s on the CI runner"
    );
}

/// Scenario RFC0065.14 — an unreachable tail is reported, the
/// `require_complete` arm (B17). The same query with `require_complete:
/// true` fails with HTTP `503` and an MCP tool error naming the group in
/// the §6.7 wire form, and returns no rows.
#[test]
#[ignore = "RFC0065.14 stub — implemented with `require_complete` on the query endpoint and MCP"]
fn rfc0065_14_require_complete_fails_when_the_tail_is_unavailable() {
    todo!(
        "RFC0065.14 — `require_complete: true` with the tail unavailable gives \
         HTTP 503 and an MCP tool error naming the group, and no rows"
    );
}

/// Scenario RFC0065.14 — the same holds for `parquet_unavailable` during a
/// store outage (RFC0065.21): `require_complete: true` fails retryably
/// naming what was unavailable, with no rows.
#[test]
#[ignore = "RFC0065.14 stub — implemented with `require_complete` on the query endpoint and MCP"]
fn rfc0065_14_require_complete_fails_when_parquet_is_unavailable() {
    todo!(
        "RFC0065.14 — `require_complete: true` during a store outage gives HTTP 503 \
         and an MCP tool error in the §6.7 wire form, and no rows"
    );
}

/// Scenario RFC0065.17 — the chart renders the replicated shape (B20, B21).
/// Given `receiver.wal.backend: replicated` and `receiver.replicas: 3`, the
/// receiver config the chart renders (pinned by `deploy/helm/render-tests.sh`
/// as `testdata/replicated-config.yaml`, which also checks the Services, the
/// pods' `POD_NAME` and the mounted peer secrets) resolves, for each pod,
/// to the node its name says: the three voters on the headless Service, and
/// the tail replica on its own.
#[test]
fn rfc0065_17_the_chart_renders_the_replicated_shape() {
    use ourios_server::config::wal::NodeRole;

    let rendered = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/helm/testdata/replicated-config.yaml"),
    )
    .expect("the pinned render");
    let resolve = |pod: &str| {
        let lookup = |name: &str| (name == "POD_NAME").then(|| pod.to_owned());
        let file = parse(&rendered, &lookup).expect("parses");
        match server_config_from_file(&file)
            .expect("resolves")
            .receiver
            .map(|r| r.wal)
        {
            Some(WalBackend::Replicated(wal)) => *wal,
            other => panic!("not a replicated receiver: {other:?}"),
        }
    };

    let voter = resolve("t-ourios-receiver-1");
    assert_eq!(voter.node_id, "t-ourios-receiver-1");
    assert!(matches!(voter.role, NodeRole::Voter));
    let peers: Vec<_> = voter
        .peers
        .iter()
        .map(|peer| format!("{}={}:{}", peer.id, peer.host, peer.port))
        .collect();
    assert_eq!(
        peers,
        (0..3)
            .map(|i| format!(
                "t-ourios-receiver-{i}=t-ourios-receiver-{i}.t-ourios-receiver-headless:4320"
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(voter.listen_addr.port(), 4320);
    assert!(matches!(
        resolve("t-ourios-wal-tail-0").role,
        NodeRole::TailReplica
    ));
}
