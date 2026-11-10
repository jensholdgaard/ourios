//! RFC 0065 — replicated WAL and a queryable tail, the config,
//! served-binary and chart arms. See `docs/rfcs/0065-replicated-wal.md`
//! §5 and §8. The in-process cluster arms are in `ourios-ingester`'s
//! `tests/it/rfc0065_replicated_wal/`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC is
//! red; each names the slice that discharges it. RFC0065.1's layout arm is
//! a unit test in `src/main.rs`: the replicated backend needs a store with
//! conditional updates, and the local store these tests serve on has none.

use ourios_ingester::replicated::raft::group_config;
use ourios_server::config::file::parse;
use ourios_server::config::resolve::server_config_from_file;
use ourios_server::config::wal::{ReplicatedWal, WalBackend};

/// Scenario RFC0065.1 — the start arm. With `receiver.wal.backend:
/// replicated` and every field set, the server starts; today
/// `check_wal_backend` refuses it, as `only_the_local_wal_backend_starts`
/// pins until the backend is wired.
#[test]
#[ignore = "RFC0065.1 stub — the start arm, implemented with the server wiring"]
fn rfc0065_1_a_valid_replicated_config_starts() {
    todo!(
        "RFC0065.1 — a server configured with the replicated backend and every field set \
         starts and serves its groups"
    );
}

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
/// Given a three-node cluster ingesting steadily, when the acknowledging
/// replica is killed with `SIGKILL` mid-batch, then another takes over without an
/// operator and every acked record is in Parquet after the next
/// checkpoint. The multi-process crash test RFC 0064 §3.2 requires.
#[test]
#[ignore = "RFC0065.3 stub — implemented with the multi-process crash test"]
fn rfc0065_3_sigkill_of_the_leader_loses_no_acked_record() {
    todo!(
        "RFC0065.3 — another replica takes over without an operator, every acked \
         record is in Parquet after the next checkpoint, and after compaction no \
         `(tenant, ingest position)` appears twice"
    );
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
/// Given `receiver.wal.backend: replicated` and `receiver.replicas: 3`, then
/// the config carries the three headless-Service peers and `node_id` from
/// the pod name, the peer port is not on the public Service, and the peer
/// TLS secret is mounted. Lands in `deploy/helm/render-tests.sh`.
#[test]
#[ignore = "RFC0065.17 stub — implemented with the chart's replicated shape in render-tests.sh"]
fn rfc0065_17_the_chart_renders_the_replicated_shape() {
    todo!(
        "RFC0065.17 — three peers on the headless Service, `node_id` from the pod \
         name, no peer port on the public Service, the peer TLS secret mounted"
    );
}
