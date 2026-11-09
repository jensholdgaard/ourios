//! Mining on tail replicas and forced takeover (RFC0065.20, .23, .25).

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
