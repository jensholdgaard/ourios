//! The async graph phase of a sweep (RFC 0047 §3.3 / §3.6): after the
//! blocking pass, write the tuples it derived, then delete the tuples of
//! every erasure whose rows are gone and finish those erasures.

// The parent scope is this module's import surface, as for the other
// compactor submodules.
#[allow(clippy::wildcard_imports)]
use super::*;

use std::sync::Mutex;

use ourios_core::sync::lock_recover;

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

/// What `remove_markers` has done so far. It lives outside the blocking
/// task's unwind boundary: a marker removed before a later one panics is
/// gone for good, so only this sweep can report that erasure finished.
#[derive(Default)]
struct Progress {
    finished: Vec<usize>,
    errors: Vec<String>,
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
        let sink = std::mem::replace(audit_sink, Box::new(NoOpAuditSink::new()));
        let progress = Arc::new(Mutex::new(Progress::default()));
        let shared = Arc::clone(&progress);
        let (outcome, sink) = blocking_with_sink("erasure completion", sink, move |sink| {
            remove_markers(&store, sink, completions, &shared);
        })
        .await;
        *audit_sink = sink;
        // A marker the failed task did not remove stays in the `tuples`
        // phase, so the next sweep finishes that erasure.
        let Progress {
            finished,
            mut errors,
        } = std::mem::take(&mut *lock_recover(&progress));
        if let Err(e) = outcome {
            errors.push(format!("erase: {e} — retried next sweep"));
        }
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
/// this process removed it. Each outcome lands in `progress` as it
/// happens, so a panic further down the list loses nothing already done.
fn remove_markers(
    store: &Store,
    sink: &mut dyn AuditSink,
    completions: Vec<Completion>,
    progress: &Mutex<Progress>,
) {
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
                lock_recover(progress).finished.push(index);
                sink.emit(event);
            }
            Err(e) if e.is_not_found() => lock_recover(progress).finished.push(index),
            Err(e) => lock_recover(progress)
                .errors
                .push(format!("erase: remove marker {marker}: {e}")),
        }
    }
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink};
    use ourios_core::sync::lock_recover;
    use ourios_core::tenant::TenantId;
    use ourios_parquet::Store;

    use super::super::IngestError;
    use super::super::daemon::blocking_with_sink;
    use super::super::erasure::erasure_marker_key;
    use super::{Completion, Progress, remove_markers};

    /// Panics on its second `emit`, like a sink wedged mid-list.
    struct PanicOnSecondEmit(usize);

    impl AuditSink for PanicOnSecondEmit {
        fn emit(&mut self, _: AuditEvent) {
            self.0 += 1;
            assert!(self.0 < 2, "the second completion dies here");
        }
    }

    fn completion(index: usize, marker: &str) -> Completion {
        Completion {
            index,
            marker: marker.to_owned(),
            event: AuditEvent {
                tenant_id: TenantId::new("a"),
                timestamp: SystemTime::now(),
                payload: AuditPayload::ConversationErased {
                    conversation_id: format!("c-{index}"),
                    partitions_rewritten: 1,
                    rows_dropped: 1,
                    tuples_deleted: 1,
                },
            },
        }
    }

    #[tokio::test]
    async fn a_panic_on_the_second_marker_keeps_the_first_one_finished() {
        let bucket = tempfile::tempdir().expect("temp");
        let store = Store::local(bucket.path()).expect("local store");
        let markers = [
            erasure_marker_key("a", "c-0"),
            erasure_marker_key("a", "c-1"),
        ];
        for marker in &markers {
            store.put_blocking(marker, Vec::new()).expect("put marker");
        }
        let completions = vec![completion(0, &markers[0]), completion(1, &markers[1])];
        let progress = Arc::new(Mutex::new(Progress::default()));
        let shared = Arc::clone(&progress);

        let (outcome, _sink) = blocking_with_sink(
            "erasure completion",
            Box::new(PanicOnSecondEmit(0)),
            move |sink| remove_markers(&store, sink, completions, &shared),
        )
        .await;

        assert!(
            matches!(
                outcome,
                Err(IngestError::Task {
                    error_type: "panic",
                    ..
                })
            ),
            "the panic fails the task"
        );
        let progress = lock_recover(&progress);
        assert_eq!(
            progress.finished,
            vec![0, 1],
            "both markers were removed before the panic"
        );
        assert!(progress.errors.is_empty(), "{:?}", progress.errors);
        for marker in &markers {
            assert!(
                !bucket.path().join(marker).exists(),
                "{marker} was removed before the sink panicked"
            );
        }
    }
}
