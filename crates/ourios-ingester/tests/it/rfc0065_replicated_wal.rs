//! RFC 0065 — replicated WAL and a queryable tail, the in-process cluster
//! arms. See `docs/rfcs/0065-replicated-wal.md` §5 and §8: three receivers
//! and a querier on the in-memory store, a simulated network, a
//! controllable clock, and crash loss that cuts each segment back to the
//! last `Wal::sync`. The served-binary, config and chart arms are in
//! `ourios-server`'s `tests/it/rfc0065_replicated_wal.rs`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC is
//! red; each names the slice that discharges it.
//!
//! RFC0065.2, RFC0065.5, RFC0065.6, RFC0065.7, RFC0065.3's takeover arm,
//! RFC0065.4's paused-writer arm, RFC0065.15's write arm, RFC0065.24's
//! formation and unreadable-record arms, RFC0065.29's kind-gate arm and
//! RFC0065.30 run on the crate-internal cluster harness, so they are unit
//! tests: `replicated::write::tests`, `replicated::raft::cluster::tests`,
//! `replicated::raft::formation::tests` and `replicated::raft::flusher::tests`.

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

/// Scenario RFC0065.11 — a record is queryable within the allowance (B14).
/// Given the flush age at 300 s, when an export is acked and a query
/// starts afterwards, then it returns the records with no flush run.
#[test]
#[ignore = "RFC0065.11 stub — implemented with the tail service"]
fn rfc0065_11_an_acked_record_is_queryable_before_any_flush() {
    todo!(
        "RFC0065.11 — with the flush age at 300 s, a query started after the ack \
         returns the export's records with no flush having run"
    );
}

/// Scenario RFC0065.12 — the tail and Parquet count a record once (B16, B19).
/// Given a test clock over flushes, checkpoints, failovers and compaction,
/// when the same `count` runs at random points, then it equals the records
/// acked before it started (a `proptest` over the event order).
#[test]
#[ignore = "RFC0065.12 stub — implemented with the tail merge"]
fn rfc0065_12_tail_and_parquet_count_a_record_once() {
    todo!(
        "RFC0065.12 — proptest over event order: every `count` returns the number \
         of records acked before it started"
    );
}

/// Scenario RFC0065.13 — the tail keeps tenancy and visibility (B12, B18).
/// Given two tenants in one group and an RFC 0047 masked attribute, then
/// each sees only its own tail rows, masked exactly as in Parquet results.
#[test]
#[ignore = "RFC0065.13 stub — implemented with the tail service"]
fn rfc0065_13_tail_rows_keep_tenancy_and_masking() {
    todo!(
        "RFC0065.13 — each tenant sees only its own tail rows, and the masked \
         attribute is masked exactly as in Parquet results"
    );
}

/// Scenario RFC0065.13 — the tail keeps tenancy and visibility, the caller arm.
/// A `Tail` or `TailScan` call from a certificate not in `tail_clients`, a
/// voter's or tail replica's own included, gets `PERMISSION_DENIED`.
#[test]
#[ignore = "RFC0065.13 stub — implemented with the peer protocol's mTLS gate"]
fn rfc0065_13_tail_calls_outside_tail_clients_are_denied() {
    todo!(
        "RFC0065.13 — `Tail` and `TailScan` from a certificate not in \
         `tail_clients` get PERMISSION_DENIED and no rows"
    );
}

/// Scenario RFC0065.14 — an unreachable tail is reported (B17).
/// Given a group whose tail cannot be read, when a tenant in it is queried,
/// then the response carries the Parquet results and names the tail as
/// unavailable. The `require_complete` arms are served-binary tests.
#[test]
#[ignore = "RFC0065.14 stub — implemented with the tail merge"]
fn rfc0065_14_an_unreachable_tail_is_named_in_the_response() {
    todo!(
        "RFC0065.14 — the response carries the Parquet results and names the tail \
         as unavailable; it never returns fewer records without saying so"
    );
}

/// Scenario RFC0065.15 — a store outage does not stop acks (B10), the
/// recovery arm. Given a healthy three-node cluster, when the object store
/// becomes unreachable and returns, then failover and checkpoints resume.
#[test]
#[ignore = "RFC0065.15 stub — implemented with checkpoints on the in-process cluster"]
fn rfc0065_15_failover_and_checkpoints_resume_after_a_store_outage() {
    todo!("RFC0065.15 — failover and checkpoints resume once the store returns");
}

/// Scenario RFC0065.16 — the local backend is unchanged, the column arm.
/// Given `backend: local`, then written Parquet files have no `ingest_*`
/// columns. The suites arm is the RFC 0008, 0014 and 0052 suites passing
/// unchanged in CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0065_16_local_backend_files_have_no_ingest_columns() {
    use crate::ingest_support::{pooled_wal_pipeline, request, resource_logs};

    let tmp = tempfile::TempDir::new().expect("temp");
    let store = tmp.path().join("store");
    let (pipeline, sink) = pooled_wal_pipeline(&tmp.path().join("wal"), &store, 2);
    let tenant = ourios_core::tenant::TenantId::new("checkout");
    for export in [
        request(vec![resource_logs(
            "checkout",
            &["user 1 logged in", "user 2 logged in"],
        )]),
        request(vec![resource_logs(
            "billing",
            &["charge 9 EUR accepted", ""],
        )]),
    ] {
        pipeline
            .ingest(export, tenant.clone())
            .await
            .expect("local ingest acks");
    }
    pipeline.quiesce_encodes();
    sink.flush_all();
    drop(pipeline);

    let files = data_files(&store.join("data"));
    assert!(!files.is_empty(), "the local backend wrote Parquet");
    for path in files {
        assert_no_ingest_columns(&path);
    }
}

/// Every `*.parquet` file under `root`.
fn data_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).expect("read dir").flatten();
        let (subdirs, plain): (Vec<_>, Vec<_>) =
            entries.map(|e| e.path()).partition(|p| p.is_dir());
        dirs.extend(subdirs);
        files.extend(
            plain
                .into_iter()
                .filter(|p| p.extension().is_some_and(|x| x == "parquet")),
        );
    }
    files
}

/// The data file at `path` has no `ingest_*` leaf, and its rows read back
/// with no position.
fn assert_no_ingest_columns(path: &std::path::Path) {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = std::fs::File::open(path).expect("open parquet");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("metadata");
    let leaves: Vec<String> = builder
        .parquet_schema()
        .columns()
        .iter()
        .map(|c| c.path().string())
        .collect();
    assert!(
        leaves.iter().all(|leaf| !leaf.starts_with("ingest_")),
        "{} has an ingest column: {leaves:?}",
        path.display(),
    );
    let rows = ourios_parquet::Reader::open_file(path)
        .expect("open_file")
        .read_all()
        .expect("read_all");
    assert!(rows.iter().all(|r| r.ingest_position.is_none()));
}

/// Scenario RFC0065.18 — tail queries do not slow ingest (B23).
/// Given one tail replica per group at a fixed ingest rate, when tail
/// queries saturate it and then it is paused, then ack p99 stays within
/// 10 % and throughput within 5 % of the same run's query-free baseline.
/// A load run (§8).
#[test]
#[ignore = "RFC0065.18 stub — implemented with the tail load runs"]
fn rfc0065_18_tail_queries_do_not_slow_ingest() {
    todo!(
        "RFC0065.18 — ack latency p99 and ingest throughput stay within the noise \
         band of the run without queries, and no ack waits on the paused tail replica"
    );
}

/// Scenario RFC0065.19 — tail capacity scales on its own (B24).
/// Given one tail replica under saturating load, when a tail peer is added
/// and `tail_replicas` raised to 2, then tail throughput rises while the
/// voters, the ack quorum and ack latency do not change. A load run (§8).
#[test]
#[ignore = "RFC0065.19 stub — implemented with the tail load runs"]
fn rfc0065_19_tail_capacity_scales_on_its_own() {
    todo!(
        "RFC0065.19 — tail query throughput rises with a second tail replica; the \
         voting set, ack quorum and ack latency are unchanged"
    );
}

/// Scenario RFC0065.20 — every tail replica mines the same rows (B11, B16).
/// Given the flusher and another tail replica across an id-block
/// reservation, a flusher change, a restart from a checkpoint snapshot and
/// a second tenant, then their rows are identical position by position.
#[test]
#[ignore = "RFC0065.20 stub — implemented with mining on tail replicas"]
fn rfc0065_20_every_tail_replica_mines_the_same_rows() {
    todo!(
        "RFC0065.20 — compared position by position, every row of the two tail \
         replicas is identical, `template_id` included"
    );
}

/// Scenario RFC0065.20 — the audit-time arm (B11). Every audit event the
/// two miners emit is identical, stamped with the `time` of the batch that
/// emitted it, also on a restored replica and a new flusher, whatever each
/// replica's clock says.
#[test]
#[ignore = "RFC0065.20 stub — the audit-time arm, implemented with the timed batch record"]
fn rfc0065_20_every_tail_replica_stamps_the_same_audit_times() {
    todo!(
        "RFC0065.20 — with each replica's clock hours apart from the others and the \
         leader's, every audit event is identical on every tail replica, the restored \
         one and the new flusher, its timestamp the emitting batch's `time`"
    );
}

/// Scenario RFC0065.21 — a long store outage is absorbed and searchable (B25, B19).
/// Given a backlog sized `N`, when the store is unreachable until the
/// backlog reaches half of `N`, then every export is acked, a query over
/// the outage returns every record, selective-query bytes do not grow
/// with the backlog, and the 50 % alert has fired.
#[test]
#[ignore = "RFC0065.21 stub — implemented with the backlog limit"]
fn rfc0065_21_a_long_outage_is_absorbed_and_searchable() {
    todo!(
        "RFC0065.21 — at half of N every export is acked, the whole outage is \
         queryable, a selective query's bytes read do not grow with the backlog, \
         and the 50 % alert has fired"
    );
}

/// Scenario RFC0065.21 — the limit arm (B25, B3).
/// When the backlog reaches the limit, exports get the retryable rejection
/// and nothing already acknowledged is lost.
#[test]
#[ignore = "RFC0065.21 stub — implemented with the backlog limit"]
fn rfc0065_21_at_the_backlog_limit_exports_are_rejected_retryably() {
    todo!(
        "RFC0065.21 — at the limit exports get the retryable rejection and no \
         acknowledged record is lost"
    );
}

/// Scenario RFC0065.22 — a backlog drains while ingest continues (B26).
/// Given RFC0065.21's backlog, when the store returns and ingest continues,
/// then it drains in no longer than it took to build, and a `count` over
/// the window is the same before, during and after.
#[test]
#[ignore = "RFC0065.22 stub — implemented with the backlog limit"]
fn rfc0065_22_a_backlog_drains_while_ingest_continues() {
    todo!(
        "RFC0065.22 — the backlog drains in no longer than it took to build, and \
         a `count` over the window is unchanged before, during and after"
    );
}

/// Scenario RFC0065.23 — the leader never mines (B23, B11).
/// Given two tail replicas per group, when the flusher is killed mid-flush,
/// then the leader's ack latency is unaffected and the other tail replica
/// takes over flushing without replaying the log.
#[test]
#[ignore = "RFC0065.23 stub — implemented with the flusher handoff"]
fn rfc0065_23_the_leader_never_mines() {
    todo!(
        "RFC0065.23 — leader ack latency unaffected, the other tail replica becomes \
         the flusher without replaying the log, and after compaction no \
         `(tenant, ingest position)` appears twice"
    );
}

/// Scenario RFC0065.23 — the kept-output arms (B11). The new flusher
/// publishes what it kept above the applied cut, audit events first; a
/// former flusher's late files are counted once; and a flusher with a gap
/// replays from the bundle in slices before it publishes or proposes.
#[test]
#[ignore = "RFC0065.23 stub — the kept-output arms, implemented with publication from the flusher"]
fn rfc0065_23_a_flusher_publishes_what_it_kept_and_replays_a_gap() {
    todo!(
        "RFC0065.23 — the new flusher publishes its kept rows and audit events above \
         the applied cut, each template's events before the first file using it, and \
         keeps nothing at or below an applied cut; a former flusher's late files are \
         counted once; past the audit ceiling or with the tail volume full, a tail \
         replica that takes over or the flusher itself drops, counts and records a gap, \
         then publishes nothing and proposes no cut until a slice-by-slice replay from \
         the bundle (each slice's audit events durable before its rows, memory at most \
         one slice) yields the same bytes as a flusher that dropped nothing; a tail \
         replica past a checkpoint at or above its gap replays nothing; nothing is \
         published while the store is unreachable; a checkpoint that applies during the \
         replay (from 100 toward 200, at 120 when 150 applies) restarts it from the new \
         cut's bundle without reading a purged entry, ends it when the cut reaches the \
         gap, and every record is still counted once"
    );
}

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

/// Scenario RFC0065.24 — the log-id arm (B13). The group's `formed.json`
/// is format 2 and records a log id allocated from `log_ids.json`, which
/// every replica writes as `ingest_group`.
#[test]
#[ignore = "RFC0065.24 stub — the log-id arm, implemented with log ids"]
fn rfc0065_24_the_formation_record_carries_the_groups_log_id() {
    todo!(
        "RFC0065.24 — formed.json is format 2 with a log id from log_ids.json, every \
         replica writes it as ingest_group, a recording voter killed between the \
         allocation and its write records a later id and no id is recorded twice, and a \
         format-1 record forms nothing"
    );
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

/// Scenario RFC0065.25 — a forced takeover reports its loss and reuses no position (B8, B13).
/// Given the other two voters lost for good with acked records above the
/// checkpoint, some flushed, when `ourios wal force-leader` runs on the
/// survivor, then it prints the possibly-lost range before acting and
/// reuses no position, template id or template version.
#[test]
#[ignore = "RFC0065.25 stub — implemented with `ourios wal force-leader`"]
fn rfc0065_25_a_forced_takeover_reports_loss_and_reuses_nothing() {
    todo!(
        "RFC0065.25 — prints the possibly-lost range first; the survivor leads \
         alone; its first position is above every published one; minted ids are \
         above the id high-water; a re-widened template gets a version above \
         every published one"
    );
}

/// Scenario RFC0065.26 — a delayed former flusher cannot move a checkpoint back.
/// Given a new flusher checkpointed at `seq` 200 and replicas purged below
/// it, when the former flusher writes and proposes `seq` 100, then
/// `checkpoint.json` still names 200 and the leader rejects the proposal.
#[test]
#[ignore = "RFC0065.26 stub — implemented with checkpoint entries"]
fn rfc0065_26_a_delayed_former_flusher_cannot_move_a_checkpoint_back() {
    todo!(
        "RFC0065.26 — `checkpoint.json` still names 200, the leader rejects the \
         `seq` 100 proposal, and a restarting tail replica restores from 200"
    );
}

/// Scenario RFC0065.26 — the interrupted-bundle arm. A flusher killed
/// between bundle puts, or before its `checkpoint.json` put, proposes
/// nothing and leaves `checkpoint.json` as it was; a conflicting object is
/// never replaced.
#[test]
#[ignore = "RFC0065.26 stub — the interrupted-bundle arm, implemented with the checkpoint bundle"]
fn rfc0065_26_an_interrupted_bundle_proposes_nothing() {
    todo!(
        "RFC0065.26 — a flusher killed between any two bundle puts or before the \
         checkpoint.json put leaves checkpoint.json unchanged and proposes nothing; no \
         bundle.bin precedes the .snap objects it names; an object present with other \
         bytes, or a malformed checkpoint.json, is never replaced and the checkpoint fails \
         with nothing purged; an identical retried put completes it; a new log \
         incarnation over an old one's objects checkpoints under its own log id"
    );
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

/// Scenario RFC0065.28 — the tail never resurrects an erased row, store reachable.
/// Given published rows above the checkpoint and a flusher paused before
/// its published entry commits, when an erasure completes and the flusher
/// resumes, then no erased row is returned: the rewritten file answers.
#[test]
#[ignore = "RFC0065.28 stub — implemented with erasure entries"]
fn rfc0065_28_no_erased_row_returns_with_the_store_reachable() {
    todo!(
        "RFC0065.28 — with the store reachable, the rewritten file answers for the \
         erased conversation and no tail replica serves its rows"
    );
}

/// Scenario RFC0065.28 — the tail never resurrects an erased row, store outage.
/// During an outage the rewritten file is named in `parquet_unavailable`
/// and no tail replica serves the erased rows.
#[test]
#[ignore = "RFC0065.28 stub — implemented with erasure entries"]
fn rfc0065_28_no_erased_row_returns_during_a_store_outage() {
    todo!(
        "RFC0065.28 — during the outage the rewritten file is named in \
         `parquet_unavailable` and no tail replica serves the erased rows"
    );
}

/// Scenario RFC0065.29 — a log entry a replica cannot read stops it (B11),
/// the log arm. Given a flusher assignment and then an unknown control kind,
/// or an assignment whose body is not 16 bytes, when the replica opens the
/// group's log, then it reads the first UUID and refuses the rest.
/// One `0x04` entry record at `(1, index)`.
fn record(index: u64, body: ourios_wal::EntryBody<'_>) -> Vec<u8> {
    let log_id = ourios_wal::RaftLogId { term: 1, index };
    ourios_wal::ReplicatedRecord::Entry { log_id, body }
        .encode()
        .expect("encode")
}

/// A flusher assignment at `index` naming `replica`, then `edit`ed.
fn assignment(index: u64, replica: [u8; 16], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let control = ourios_wal::Control::FlusherAssignment(replica);
    let mut frame = record(index, ourios_wal::EntryBody::Control(control));
    edit(&mut frame);
    frame
}

/// Open group 0's log from a WAL holding a blank entry, then `frames`.
fn open_holding(
    frames: &[&[u8]],
) -> (
    std::io::Result<ourios_ingester::replicated::raft::log_store::GroupLog>,
    tempfile::TempDir,
) {
    use ourios_ingester::replicated::raft::log_store::{GroupLog, group_dir};
    use ourios_ingester::replicated::raft::vote_log::{self, VoteLog};

    let dir = tempfile::tempdir().expect("tempdir");
    let config = ourios_wal::WalConfig {
        root: group_dir(dir.path(), 0),
        batch_window_ms: 2,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    };
    let mut wal = ourios_wal::Wal::open(config.clone()).expect("the group WAL");
    let blank = record(0, ourios_wal::EntryBody::Blank);
    for frame in std::iter::once(&blank[..]).chain(frames.iter().copied()) {
        wal.append(ourios_wal::FrameKind::ReplicatedEntry, frame)
            .expect("append");
    }
    wal.sync().expect("sync");
    drop(wal);
    vote_log::create(dir.path()).expect("vote log");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    (GroupLog::open(config, 0, votes), dir)
}

#[tokio::test]
async fn rfc0065_29_an_unreadable_control_entry_stops_the_open() {
    use openraft::EntryPayload;
    use openraft::storage::RaftLogReader as _;
    use ourios_ingester::replicated::raft::types::GroupEntry;
    use ourios_wal::Control;

    let flusher = *uuid::Uuid::from_u128(0x0190_b3c8_1a2b_7c3d_9e4f_5060_7080_a0b0).as_bytes();
    let first = assignment(1, flusher, |_| {});
    let (opened, _dir) = open_holding(&[&first]);
    let mut log = opened.expect("a log holding a flusher assignment opens");
    let entries = log.try_get_log_entries(..).await.expect("read");
    let assigned = entries.iter().find_map(|e| match &e.payload {
        EntryPayload::Normal(GroupEntry::Control(Control::FlusherAssignment(replica))) => {
            Some(*replica)
        }
        _ => None,
    });
    assert_eq!(assigned, Some(flusher), "the first assignment's UUID");

    let unknown_kind = assignment(2, [7; 16], |f| f[17] = Control::HIGHEST_KIND + 1);
    let short_body = assignment(2, [7; 16], |f| f.truncate(f.len() - 1));
    let long_body = assignment(2, [7; 16], |f| f.push(0));
    let kind = format!("control kind 0x{:02x}", Control::HIGHEST_KIND + 1);
    for (unreadable, why) in [
        (&unknown_kind, kind.as_str()),
        (&short_body, "body is 32 byte(s); expected 33"),
        (&long_body, "body is 34 byte(s); expected 33"),
    ] {
        let (refused, _dir) = open_holding(&[&first, unreadable]);
        let Err(error) = refused else {
            panic!("{why}: the entry did not stop the open");
        };
        assert!(error.to_string().contains(why), "{error}");
    }
}

/// Scenario RFC0065.29 — the batch-record arm (B11): a batch under tag
/// `0x02`, or under tag `0x05` with a `time` above `2^63 - 1`, stops the
/// open at that record.
#[test]
#[ignore = "RFC0065.29 stub — the batch-record arm, implemented with the timed batch record"]
fn rfc0065_29_an_untimed_or_out_of_range_batch_refuses_the_log() {
    todo!(
        "RFC0065.29 — a group WAL holding a batch under tag 0x02, or under tag 0x05 \
         with a time above 2^63 - 1, is refused at that record"
    );
}

/// Scenario RFC0065.29 — the batch-record arm (B11): a log holding a timed
/// batch opens with its time, and a batch under tag `0x02`, or under tag
/// `0x05` with a `time` above `2^63 - 1`, stops the open at that record.
#[tokio::test]
async fn rfc0065_29_an_untimed_or_out_of_range_batch_stops_the_open() {
    use openraft::EntryPayload;
    use openraft::storage::RaftLogReader as _;
    use ourios_ingester::replicated::raft::types::GroupEntry;
    use ourios_wal::BatchTime;

    let payload = ourios_wal::TenantBatch::encode("acme", b"export").expect("batch");
    let time = BatchTime::from_nanos(1_700_000_000_000_000_000).expect("in range");
    let timed = |index| {
        record(
            index,
            ourios_wal::EntryBody::Batch {
                time,
                batch: &payload,
            },
        )
    };
    let first = timed(1);
    let (opened, _dir) = open_holding(&[&first]);
    let mut log = opened.expect("a log holding a timed batch opens");
    let entries = log.try_get_log_entries(..).await.expect("read");
    let read = entries.iter().find_map(|e| match &e.payload {
        EntryPayload::Normal(GroupEntry::Batch(batch)) => Some(batch.time),
        _ => None,
    });
    assert_eq!(read, Some(time), "the batch's time");

    let mut untimed = vec![0x02];
    untimed.extend_from_slice(&timed(2)[1..17]);
    untimed.extend_from_slice(&payload);
    let mut late = timed(2);
    late[17..25].copy_from_slice(&(BatchTime::MAX.nanos() + 1).to_le_bytes());
    for (unreadable, why) in [
        (&untimed, "unknown ReplicatedEntry tag 0x02"),
        (&late, "is above 2^63 - 1"),
    ] {
        let (refused, _dir) = open_holding(&[&first, unreadable]);
        let Err(error) = refused else {
            panic!("{why}: the batch did not stop the open");
        };
        assert!(error.to_string().contains(why), "{error}");
    }
}

/// Scenario RFC0065.29 — the undecodable-batch arm (B11): a committed batch
/// whose export does not decode stops the group's miner at that entry on
/// every tail replica alike, while the group keeps acknowledging.
#[test]
#[ignore = "RFC0065.29 stub — the undecodable-batch arm, implemented with mining on tail replicas"]
fn rfc0065_29_an_undecodable_batch_stops_the_groups_miner() {
    todo!(
        "RFC0065.29 — the flusher, another tail replica and one restored before the \
         entry all stop there: none mines, publishes, proposes a cut at or above it or \
         serves a tail row past it, each logs the group, log id and decode error, a \
         query reports the group in `tail_unavailable`, and acks and log replication \
         continue"
    );
}

/// A miner configuration entry at `index` carrying `config`.
fn miner_config(index: u64, config: ourios_wal::MinerConfigV1) -> Vec<u8> {
    let control = ourios_wal::Control::MinerConfig(config);
    record(index, ourios_wal::EntryBody::Control(control))
}

fn default_miner_config() -> ourios_wal::MinerConfigV1 {
    ourios_wal::MinerConfigV1 {
        similarity_threshold: 0.7,
        similarity_floor: 0.4,
        param_byte_limit: 256,
        prefix_depth: 2,
        max_node_children: 100,
        max_templates: 20_000,
        max_line_tokens: 512,
        upstream_templates: ourios_wal::UpstreamTemplates::Ignore,
        upstream_template_byte_limit: 8192,
        upstream_association_limit: 4,
    }
}

/// Scenario RFC0065.31, the refused-configuration arm: a miner
/// configuration body that `MinerConfig` refuses stops the open as
/// RFC0065.29's unreadable entry does, as does one whose body is not 28
/// bytes or whose `upstream_templates` is not 0, 1 or 2.
#[tokio::test]
async fn rfc0065_31_a_miner_configuration_minerconfig_refuses_stops_the_open() {
    let accepted = miner_config(1, default_miner_config());
    let (opened, _dir) = open_holding(&[&accepted]);
    opened.expect("a log holding a valid configuration opens");

    let refused = |edit: fn(&mut ourios_wal::MinerConfigV1)| {
        let mut config = default_miner_config();
        edit(&mut config);
        miner_config(2, config)
    };
    let above_one = refused(|c| c.similarity_threshold = 1.5);
    let floor_above = refused(|c| c.similarity_floor = 0.9);
    let over_ceiling = refused(|c| c.param_byte_limit = 1025);
    let too_deep = refused(|c| c.prefix_depth = 9);
    let no_children = refused(|c| c.max_node_children = 0);
    let mut short_body = accepted.clone();
    short_body.pop();
    let mut long_body = accepted.clone();
    long_body.push(0);
    let mut unknown_mode = accepted.clone();
    unknown_mode[17 + 1 + 21] = 3;
    for (unreadable, why) in [
        (&above_one, "similarity_threshold"),
        (&floor_above, "similarity_floor"),
        (&over_ceiling, "param_byte_limit"),
        (&too_deep, "prefix_depth"),
        (&no_children, "max_node_children"),
        (&short_body, "body is 44 byte(s); expected 45"),
        (&long_body, "body is 46 byte(s); expected 45"),
        (&unknown_mode, "upstream_templates 3"),
    ] {
        let (refused, _dir) = open_holding(&[&accepted, unreadable]);
        let Err(error) = refused else {
            panic!("{why}: the entry did not stop the open");
        };
        assert!(error.to_string().contains(why), "{error}");
    }
}

/// Scenario RFC0065.31 — a checkpoint snapshot holds the group at its cut
/// (B11). Only the flusher's checkpoint with a cut above the last applies;
/// its snapshot is at the cut and holds the membership, flusher, highest
/// kind, miner configuration and latest id block as of it, and a replica
/// installing it catches up to the state of one that applied the whole log.
#[test]
#[ignore = "RFC0065.31 stub — the first arm's remaining clause, mining under the \
            configuration, lands with restoring from the bundle; the And-arms have stubs \
            of their own"]
fn rfc0065_31_a_checkpoint_snapshot_holds_the_group_at_its_cut() {
    todo!(
        "RFC0065.31 — remaining arm: the installer mines the lines after a \
         similarity_threshold change into the same templates as a replica that mined \
         through it. Covered: a leader whose own prefix_depth differs appends the log's, \
         also when elected after a checkpoint purged every configuration entry \
         (raft::flusher::tests::config, the in-process cluster, as the steward is not yet \
         wired into the served role), checkpoints \
         not by the flusher at their position or with a cut not above the last are \
         ignored, the snapshot is at the cut with its membership, flusher, kind, miner \
         configuration and latest id block, the installer and a restart match a full \
         replay, configuration included, a block below the previous applied one is ignored \
         (after (0, 100] and an ignored (0, 50], (50, 150] too), also by a replica whose \
         log is purged below that block (raft::cluster::tests, raft::state_machine::tests, \
         raft::checkpoint::tests), a configuration changing prefix_depth or \
         max_node_children from the group's first stops the apply \
         (raft::state_machine::tests), one MinerConfig refuses stops the open \
         (rfc0065_31_a_miner_configuration_minerconfig_refuses_stops_the_open, \
         raft::codec::tests), the leader's control view holds the configuration in force \
         (raft::tests), and a version-1 snapshot record fails the open \
         (raft::snapshot_record::tests)"
    );
}

/// Scenario RFC0065.31 — the bundle arm (B11). The bundle at the cut holds
/// every tenant's version-3 snapshot marked `{log, seq}`, the allocator's
/// range and blocks and the configuration in force; a malformed bundle
/// fails the restore closed.
#[test]
#[ignore = "RFC0065.31 stub — the bundle arm, implemented with the checkpoint bundle and restore"]
fn rfc0065_31_the_bundle_at_the_cut_restores_the_same_miner() {
    todo!(
        "RFC0065.31 — a replica restored from the bundle mints the same next template id \
         and rows as one that mined through the cut; every malformed field or missing \
         object fails the restore closed and no restored allocator issues 0; a local \
         version-3 snapshot loads as before while one marked {{log, seq}} is discarded"
    );
}

/// Scenario RFC0065.31 — the restart arm (B11). A restarting tail replica
/// restores from its snapshot record's cut, not a later `checkpoint.json`,
/// and one restarting during a store outage keeps taking the log.
#[test]
#[ignore = "RFC0065.31 stub — the restart arm, implemented with restoring from the bundle"]
fn rfc0065_31_a_restarting_tail_replica_restores_from_its_records_cut() {
    todo!(
        "RFC0065.31 — restores from the record's cut even when checkpoint.json names a \
         later one and mines only after it; during a store outage it serves no tail, does \
         not flush, reports why, keeps taking the log while the group acks, and restores \
         once the store returns"
    );
}

/// Scenario RFC0065.31 — the bootstrap-lock arm (B11). Only the start whose
/// create of `log_ids.json` succeeds bootstraps the high-water object, and
/// no start re-creates a deleted lock or object.
#[test]
#[ignore = "RFC0065.31 stub — the bootstrap-lock arm, implemented with log ids"]
fn rfc0065_31_one_replicated_start_bootstraps_the_high_water() {
    todo!(
        "RFC0065.31 — one start bootstraps and seeds before any log id; a paused start \
         does not re-create a deleted object; a killed bootstrapper finishes on restart; \
         a deleted lock fails with LogIdsDeleted and no log id is allocated twice; a \
         formed replica opens with the lock unreadable; a deleted object stops every \
         group's block appends"
    );
}

/// Scenario RFC0065.31 — the first-configuration arm (B11). A checkpoint
/// whose cut has no miner configuration in force is ignored and refused,
/// and the batches before the first configuration are mined from the WAL.
#[test]
#[ignore = "RFC0065.31 stub — the first-configuration arm, implemented with the checkpoint \
            barrier"]
fn rfc0065_31_a_cut_before_the_first_configuration_is_ignored() {
    todo!(
        "RFC0065.31 — every replica ignores the checkpoint and purges nothing, the leader \
         refuses the proposal as stale, and once the configuration applies every tail \
         replica mines the earlier batches, read back from the WAL, into the same rows; a \
         checkpoint applied during that read-back restarts a slow replica's miner from \
         the cut's bundle, after which it mines the same rows"
    );
}

/// Scenario RFC0065.32 — the flusher's proposals and reports reach the
/// leader (B11). The leader tops id blocks up to three counting the
/// flusher's unstarted ones, appends only the flusher's checkpoints above
/// the last, refuses the rest by name, and a flusher refused by a leader
/// whose assignment the next leader discards reports to that next leader.
#[test]
#[ignore = "RFC0065.32 stub — implemented with the Propose call"]
fn rfc0065_32_the_flushers_proposals_and_reports_reach_the_leader() {
    todo!(
        "RFC0065.32 — unstarted plus logged blocks reach three and no purge adds more; \
         the flusher's checkpoint is appended and answered with its log id; the former \
         flusher gets not_flusher, a cut not above the last or not committed or with \
         another highest kind gets stale, another kind or length or proposer gets \
         malformed, a non-leader gets not_leader with the hint; a resend leaves one \
         applied checkpoint; a not_flusher refusal from a leader whose assignment is \
         discarded does not silence the flusher toward the next leader; a report delayed \
         past a checkpoint that purged the blocks it caused is refused stale and \
         reserves nothing, and the fresh report keeps at most three unstarted blocks"
    );
}

/// Scenario RFC0065.32 — the abandoned-write arm (B11). A flusher whose
/// `checkpoint.json` write finds an equal or later `seq` still proposes, so
/// the checkpoint advances and never passes `checkpoint.json`.
#[test]
#[ignore = "RFC0065.32 stub — the abandoned-write arm, implemented with the checkpoint barrier"]
fn rfc0065_32_an_abandoned_checkpoint_write_still_proposes() {
    todo!(
        "RFC0065.32 — the current flusher proposes after finding a former flusher's later \
         seq or its own equal one and every replica applies it; an unreadable object \
         proposes nothing; checkpoint.json is always at or above every applied cut; a \
         restarting tail replica restores from the applied cut's bundle; a query from \
         checkpoint.json's later cut counts every record once"
    );
}
