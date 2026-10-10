//! The async graph phase of a sweep (RFC 0047 §3.3 / §3.6): after the
//! blocking pass, write the tuples it derived, then delete the tuples of
//! every erasure whose rows are gone and finish those erasures.

// The parent scope is this module's import surface, as for the other
// compactor submodules.
#[allow(clippy::wildcard_imports)]
use super::*;

/// The graph a sweep feeds, and the store its erasure markers live in.
pub(super) struct GraphPhase<'a> {
    pub(super) store: &'a Store,
    pub(super) emitter: &'a GraphEmitter,
}

/// A finished erasure's marker, and the audit event its removal writes.
struct Completion {
    index: usize,
    marker: String,
    event: AuditEvent,
}

impl GraphPhase<'_> {
    /// Write the derived tuples, then delete the tuples of every erasure in
    /// the `Tuples` phase and finish those erasures.
    pub(super) async fn run(
        &self,
        report: &mut SweepReport,
        audit_sink: &mut Box<dyn AuditSink>,
        tuples: GraphTuples,
    ) {
        if !tuples.is_empty() {
            match self.emitter.emit(&tuples).await {
                Ok(written) => report.graph_tuples_emitted += written.tuples,
                Err(e) => report.errors.push(format!("graph emit: {e}")),
            }
        }
        let completions = self.erase_tuples(report).await;
        if !completions.is_empty() {
            self.finish_erasures(report, audit_sink, completions).await;
        }
    }

    /// Delete the tuples of every erasure whose rows are gone; the
    /// erasures whose deletion succeeded.
    async fn erase_tuples(&self, report: &mut SweepReport) -> Vec<Completion> {
        let mut completions = Vec::new();
        for (index, outcome) in report.erasures.iter_mut().enumerate() {
            if outcome.phase != ErasurePhase::Tuples {
                continue;
            }
            let request = &outcome.request;
            match self
                .emitter
                .erase_conversation(&request.tenant, &request.conversation_id)
                .await
            {
                Ok(deleted) => {
                    outcome.tuples_deleted = Some(deleted);
                    completions.push(Completion {
                        index,
                        marker: request.marker.clone(),
                        event: erased_event(outcome, deleted),
                    });
                }
                Err(e) => report.errors.push(format!(
                    "erase {:?} {:?}: graph tuples: {e} — retried next sweep",
                    request.tenant, request.conversation_id
                )),
            }
        }
        completions
    }

    /// Back on the blocking pool for the audit `put`s and the marker
    /// deletes — after the tuples are gone.
    async fn finish_erasures(
        &self,
        report: &mut SweepReport,
        audit_sink: &mut Box<dyn AuditSink>,
        completions: Vec<Completion>,
    ) {
        let store = self.store.clone();
        let mut sink = std::mem::replace(audit_sink, Box::new(NoOpAuditSink::new()));
        let (sink, finished, errors) = tokio::task::spawn_blocking(move || {
            let (finished, errors) = remove_markers(&store, sink.as_mut(), completions);
            (sink, finished, errors)
        })
        .await
        .expect("erasure completion task should not panic");
        *audit_sink = sink;
        for index in finished {
            log_finished(&mut report.erasures[index]);
        }
        report.errors.extend(errors);
    }
}

/// The `conversation_erased` audit event for `outcome`, with `deleted`
/// tuples gone.
fn erased_event(outcome: &ErasureOutcome, deleted: usize) -> AuditEvent {
    let request = &outcome.request;
    AuditEvent {
        tenant_id: TenantId::new(&request.tenant),
        timestamp: SystemTime::now(),
        payload: AuditPayload::ConversationErased {
            conversation_id: request.conversation_id.clone(),
            partitions_rewritten: outcome.partitions_rewritten,
            rows_dropped: outcome.rows_dropped,
            tuples_deleted: to_u64(deleted),
        },
    }
}

/// Remove each completed erasure's marker, writing its audit event when
/// this process removed it; the indexes finished, and the removal errors.
fn remove_markers(
    store: &Store,
    sink: &mut dyn AuditSink,
    completions: Vec<Completion>,
) -> (Vec<usize>, Vec<String>) {
    let mut finished = Vec::new();
    let mut errors = Vec::new();
    for Completion {
        index,
        marker,
        event,
    } in completions
    {
        // The marker removal is the at-most-once transition: only the
        // process that removes it writes the audit event. A marker
        // already gone was finished (and audited) elsewhere; a failed
        // delete leaves the marker in the `tuples` phase — the next
        // sweep repeats the (idempotent) tuple deletion and retries.
        match store.delete_blocking(&marker) {
            Ok(()) => {
                sink.emit(event);
                finished.push(index);
            }
            Err(e) if e.is_not_found() => finished.push(index),
            Err(e) => errors.push(format!("erase: remove marker {marker}: {e}")),
        }
    }
    (finished, errors)
}

/// Mark `outcome` finished, with the RFC 0048 §3.3 completion event:
/// completion is observable in the logs too, one structured event per
/// finished erasure.
fn log_finished(outcome: &mut ErasureOutcome) {
    outcome.finished = true;
    // `tuples_deleted` is set on the same path that queued the marker
    // removal; a `None` here would be a regression worth seeing in the
    // log rather than a silent 0 (and never worth a panic — §6.5's
    // no-unwrap rule holds in the sweep).
    let tuples_deleted = outcome
        .tuples_deleted
        .map_or_else(|| "unknown".to_string(), |n| n.to_string());
    tracing::info!(
        name: ourios_semconv::EVENT_OURIOS_COMPACTION_ERASURE_COMPLETED,
        "conversation erasure completed: tenant {:?} conversation {:?}, {} rows dropped, {} tuples deleted",
        outcome.request.tenant,
        outcome.request.conversation_id,
        outcome.rows_dropped,
        tuples_deleted,
    );
}
