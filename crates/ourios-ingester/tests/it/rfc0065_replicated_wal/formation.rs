//! Group formation, wiped and unreadable voters, and log ids (RFC0065.24).

/// Scenario RFC0065.24 — a wiped disk rejoins as a new replica (B4, B6).
/// Given a voter restarted under its name on an empty volume while the
/// other two are up, then it joins as a learner under a new identity and
/// is promoted by one membership change that removes the old identity.
#[test]
#[ignore = "RFC0065.24 stub — implemented with replica identity and rejoin"]
fn rfc0065_24_a_wiped_voter_rejoins_under_a_new_identity() {
    todo!(
        "RFC0065.24 — learner first, promoted once caught up by one change that \
         removes the old identity, also across a leader change; never two voters \
         with one `node_id`, no core panics, every acked record present"
    );
}

/// Scenario RFC0065.24 — the leader-lost arm (B8).
/// When the wiped voter restarts while the leader is also lost, only one
/// original voter remains and the group fails closed.
#[test]
#[ignore = "RFC0065.24 stub — implemented with replica identity and rejoin"]
fn rfc0065_24_a_wiped_voter_without_a_quorum_fails_closed() {
    todo!(
        "RFC0065.24 — no election, no membership change and no ack until an \
         original quorum returns; its tenants get the retryable rejection"
    );
}

/// Scenario RFC0065.24 — the interrupted-formation arm (B4, B6), the
/// rejoin half. The formation half, the other two forming from the record
/// while the wiped first-ranked voter never initializes, is
/// `replicated::raft::formation::tests`.
#[test]
#[ignore = "RFC0065.24 stub — the rejoin half, implemented with replica identity and rejoin"]
fn rfc0065_24_a_formation_interrupted_by_a_wiped_voter_rejoins_by_replacement() {
    todo!(
        "RFC0065.24 — the restarted first-ranked voter joins under its new \
         identity by replacement, never by initializing a second membership"
    );
}

/// Scenario RFC0065.24 — the log-id arm (B13), the `ingest_group` half.
/// The record half is `replicated::raft::formation::format_2_tests::
/// rfc0065_24_the_formation_record_carries_the_groups_log_id`: a format-2
/// `formed.json` carrying a log id allocated from `log_ids.json`; an id
/// allocated and never recorded, standing in for a recording voter killed
/// between the allocation and its write, followed by a recorded later id;
/// two groups recorded under distinct ids; and a format-1 record forming
/// nothing.
#[test]
#[ignore = "RFC0065.24 stub — the ingest_group half, implemented with publication"]
fn rfc0065_24_the_formation_record_carries_the_groups_log_id() {
    todo!("RFC0065.24 — every replica of the group writes the recorded log id as ingest_group");
}

/// Scenario RFC0065.24 — the unreadable-disk arm (B4, B6). A node whose
/// `wal_root` exists but cannot be read fails startup and changes nothing
/// on it. The identity half is `replicated::replica_id::tests::
/// an_unreadable_identity_fails_naming_the_file`.
#[test]
#[ignore = "RFC0065.24 stub — the unreadable-disk arm, implemented with the server wiring"]
fn rfc0065_24_an_unreadable_wal_root_refuses_to_start() {
    todo!(
        "RFC0065.24 — startup fails naming the cause, no new replica.id is written, and \
         the vote log and group logs are byte-for-byte unchanged"
    );
}
