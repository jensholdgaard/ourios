//! The write path: the shipped Raft timings, fencing, routing, rebuild,
//! the simulation suite and the many-groups failover (RFC0065.1, .4, .8,
//! .9, .10, .27).

/// Scenario RFC0065.1 — the default-timings arm. A three-voter group on
/// `group_config()` unchanged, 50 ms heartbeat and 150–300 ms election,
/// commits and acknowledges exports and fails over when its leader stops.
#[test]
#[ignore = "RFC0065.1 stub — the default-timings arm, implemented with the timing keys"]
fn rfc0065_1_a_group_on_the_default_timings_acks_and_fails_over() {
    todo!(
        "RFC0065.1 — on the shipped timings, exports are acknowledged and a stopped \
         leader is replaced, in the CI suite"
    );
}

/// Scenario RFC0065.4 — a replaced writer cannot acknowledge (B5), the
/// clock arm: the resumed former writer, with its clock set arbitrarily,
/// commits nothing new.
#[test]
#[ignore = "RFC0065.4 stub — the clock arm, implemented with the seeded simulation suite"]
fn rfc0065_4_a_replaced_writer_with_any_clock_cannot_acknowledge() {
    todo!(
        "RFC0065.4 — with its clock set arbitrarily, the resumed former writer \
         commits nothing new: every success it sends is for a record the new \
         writer holds, other waiters get the retryable rejection"
    );
}

/// Scenario RFC0065.8 — routing and tenancy (B9, B11, B12).
/// Given two tenants in one group exporting to a node that does not hold
/// their group, then both are acked and mined in log-position order.
#[test]
#[ignore = "RFC0065.8 stub — implemented with forwarding on the in-process cluster"]
fn rfc0065_8_forwarded_exports_are_mined_in_position_order() {
    todo!(
        "RFC0065.8 — both forwarded exports are acked and mined in log position \
         order, one acked before the other was sent is mined first, and each \
         tenant's query returns only its own rows"
    );
}

/// Scenario RFC0065.9 — a corrupt replica does not recover from itself (B4).
/// Given a follower whose group log fails an integrity check, when it
/// restarts, then it serves nothing from it and rebuilds from a peer.
#[test]
#[ignore = "RFC0065.9 stub — implemented with log rebuild on the in-process cluster"]
fn rfc0065_9_a_corrupt_replica_rebuilds_from_a_healthy_one() {
    todo!(
        "RFC0065.9 — the corrupt follower serves nothing from its log, rebuilds \
         from a healthy replica, and no record is lost or duplicated"
    );
}

/// Scenario RFC0065.10 — the protocol holds its invariants under faults.
/// Given loss, reordering, partitions, crashes, segment-cut disk loss and
/// clock skew, when ≥ 1 000 seeds run in CI, then no run breaks B1, B5,
/// B6, B14 or B16; every failing seed is kept as a regression test.
#[test]
#[ignore = "RFC0065.10 stub — implemented with the seeded simulation suite"]
fn rfc0065_10_the_simulation_holds_the_invariants_over_1000_seeds() {
    todo!(
        "RFC0065.10 — at least 1 000 seeds of writes, membership changes, flusher \
         handoffs, checkpoints and tail reads under faults; no run breaks B1, B5, \
         B6, B14 or B16"
    );
}

/// Scenario RFC0065.10 — the fallback's model check.
/// With the §6.2–§6.4 fallback instead of `openraft`, a model of the whole
/// protocol is model-checked to a bounded depth. Applies only if the
/// fallback is built.
#[test]
#[ignore = "RFC0065.10 stub — applies only with the §6.2–§6.4 fallback; discharged with it"]
fn rfc0065_10_the_fallback_protocol_is_model_checked() {
    todo!("RFC0065.10 — the fallback protocol's model is checked to a bounded depth in CI");
}

/// Scenario RFC0065.27 — a node failure with many groups fails over like one (B7).
/// Given three voter nodes hosting 128 groups with durable fsync, when one
/// node is killed, then every group it led acknowledges on a new leader
/// within twice the single-group failover time `F` measured earlier in
/// the same run.
#[test]
#[ignore = "RFC0065.27 stub — implemented with the many-groups load run"]
fn rfc0065_27_a_node_failure_with_many_groups_fails_over_like_one() {
    todo!(
        "RFC0065.27 — every group the killed node led acknowledges on a new leader \
         within twice the single-group bound, none is left leaderless, and the \
         vote log took fewer fsyncs than vote writes"
    );
}
