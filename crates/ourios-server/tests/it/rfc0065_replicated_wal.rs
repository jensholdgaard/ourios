//! RFC 0065 — replicated WAL and a queryable tail, the config,
//! served-binary and chart arms. See `docs/rfcs/0065-replicated-wal.md`
//! §5 and §8. The in-process cluster arms are in `ourios-ingester`'s
//! `tests/it/rfc0065_replicated_wal.rs`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC is
//! red; each names the slice that discharges it. RFC0065.1's layout arm is
//! a unit test in `src/main.rs`: the replicated backend needs a store with
//! conditional updates, and the local store these tests serve on has none.

/// Scenario RFC0065.1 — the Raft timings arm. Unset, every group runs on
/// the 50 ms heartbeat and 150–300 ms election timeout; set, on the
/// configured ones; a zero or mis-ordered value fails startup naming the key.
#[test]
#[ignore = "RFC0065.1 stub — the Raft timings arm, implemented with the timing keys"]
fn rfc0065_1_the_raft_timings_are_configured_and_checked() {
    todo!(
        "RFC0065.1 — defaults 50 ms and 150–300 ms, configured values reach every \
         group's openraft config, and 0, heartbeat >= election minimum or minimum >= \
         maximum fails startup naming the key"
    );
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
