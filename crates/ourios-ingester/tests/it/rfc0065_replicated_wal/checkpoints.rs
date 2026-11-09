//! Checkpoints, bundles, restore and the flusher's proposals
//! (RFC0065.15, .26, .31, .32).

/// Scenario RFC0065.15 — a store outage does not stop acks (B10), the
/// recovery arm. Given a healthy three-node cluster, when the object store
/// becomes unreachable and returns, then failover and checkpoints resume.
#[test]
#[ignore = "RFC0065.15 stub — implemented with checkpoints on the in-process cluster"]
fn rfc0065_15_failover_and_checkpoints_resume_after_a_store_outage() {
    todo!("RFC0065.15 — failover and checkpoints resume once the store returns");
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
