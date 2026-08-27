//! RFC 0047 §3.6 erasure — request markers, pending-marker listing, and
//! the read→delete application the sweep runs. Split from the flat
//! `compactor.rs` (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path resolving unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

/// One pending RFC 0047 §3.6 erasure: a durable request marker in the
/// store (`erasure/tenant_id=<enc>/conversation=<enc>`, written by an
/// operator through [`request_erasure`]) naming the conversation to
/// remove from a tenant. The marker's body carries the phase, so a
/// sweep interrupted after the rewrite retries only the tuple deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureRequest {
    /// The tenant the conversation lives in.
    pub tenant: String,
    /// The raw conversation id (the promoted-column value).
    pub conversation_id: String,
    /// The marker object's key.
    pub marker: String,
    /// Where the request stands.
    pub phase: ErasurePhase,
}

/// The two phases of an erasure — rows first, tuples after (RFC 0047
/// §3.6: a dangling tuple is harmless, a dangling row is a leak).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasurePhase {
    /// The Parquet rewrite has not completed for every partition.
    Rows,
    /// Rows are gone; the graph tuples remain to be deleted.
    Tuples,
}

/// What one sweep did for one erasure request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureOutcome {
    /// The request as it was when the sweep picked it up.
    pub request: ErasureRequest,
    /// Partitions rewritten this sweep (`0` when the rows phase was
    /// already done).
    pub partitions_rewritten: u64,
    /// Rows dropped this sweep.
    pub rows_dropped: u64,
    /// The phase after this sweep's blocking pass: `Tuples` once every
    /// partition rewrote cleanly, else still `Rows` (retried next sweep).
    pub phase: ErasurePhase,
    /// Graph tuples deleted (set by the async phase; `None` until then).
    pub tuples_deleted: Option<usize>,
    /// Whether the marker was removed — the erasure is complete.
    pub finished: bool,
}

/// The object-store prefix of erasure request markers.
pub const ERASURE_PREFIX: &str = "erasure/";
/// The object-store prefix of backfill lock markers (RFC 0048 §3.4).
pub const BACKFILL_PREFIX: &str = "backfill/";
pub(super) const ERASURE_PHASE_ROWS: &[u8] = br#"{"phase":"rows"}"#;
pub(super) const ERASURE_PHASE_TUPLES: &[u8] = br#"{"phase":"tuples"}"#;

/// The marker key for erasing `conversation_id` from `tenant`.
#[must_use]
pub fn erasure_marker_key(tenant: &str, conversation_id: &str) -> String {
    format!(
        "{ERASURE_PREFIX}tenant_id={}/conversation={}",
        percent_encode_tenant(tenant),
        percent_encode_tenant(conversation_id)
    )
}

/// Request the erasure of `conversation_id` from `tenant` (RFC 0047 §3.6):
/// writes the durable marker the next sweep acts on. Idempotent —
/// create-if-absent, so a repeated request never resets an erasure already
/// in flight (one whose rows are gone and whose marker is in the `tuples`
/// phase).
///
/// # Errors
///
/// [`IngestError::Io`] when the marker cannot be written.
pub fn request_erasure(
    store: &Store,
    tenant: &str,
    conversation_id: &str,
) -> Result<(), IngestError> {
    let key = erasure_marker_key(tenant, conversation_id);
    match store.put_if_absent_blocking(&key, ERASURE_PHASE_ROWS.to_vec()) {
        Ok(()) => Ok(()),
        Err(e) if e.is_already_exists() => Ok(()),
        Err(e) => Err(store_error("put erasure marker", &key, &e)),
    }
}

/// The pending erasure requests, in deterministic (lexicographic key)
/// order.
///
/// # Errors
///
/// [`IngestError::Io`] when the marker prefix cannot be listed or a marker
/// cannot be read — a transient store error never regresses a marker's
/// phase.
pub fn pending_erasures(store: &Store) -> Result<Vec<ErasureRequest>, IngestError> {
    pending_erasures_under(store, ERASURE_PREFIX)
}

/// The pending erasure requests **of one tenant** — the same listing
/// scoped to `erasure/tenant_id=<enc>/`, so a caller interested in one
/// tenant does not pay for every other tenant's markers (RFC 0048 §3.4:
/// backfill checks this twice, before and under its lock).
///
/// # Errors
///
/// See [`pending_erasures`].
pub fn pending_erasures_for(
    store: &Store,
    tenant: &str,
) -> Result<Vec<ErasureRequest>, IngestError> {
    let prefix = format!(
        "{ERASURE_PREFIX}tenant_id={}/",
        percent_encode_tenant(tenant)
    );
    pending_erasures_under(store, &prefix)
}

pub(super) fn pending_erasures_under(
    store: &Store,
    prefix: &str,
) -> Result<Vec<ErasureRequest>, IngestError> {
    let mut keys = store
        .list_blocking(Some(prefix))
        .map_err(|e| store_error("list erasure markers", prefix, &e))?;
    keys.sort();
    let mut requests = Vec::new();
    for key in keys {
        let Some(rest) = key.strip_prefix(ERASURE_PREFIX) else {
            continue;
        };
        let Some((tenant_segment, conversation_segment)) = rest.split_once('/') else {
            continue;
        };
        let (Some(tenant), Some(conversation)) = (
            tenant_segment
                .strip_prefix("tenant_id=")
                .and_then(percent_decode_tenant),
            conversation_segment
                .strip_prefix("conversation=")
                .and_then(percent_decode_tenant),
        ) else {
            continue;
        };
        let body = store
            .get_blocking_opt(&key)
            .map_err(|e| store_error("read erasure marker", &key, &e))?;
        // A marker removed between the listing and this read is finished.
        let Some(body) = body else {
            continue;
        };
        // The marker body is `{"phase": "rows" | "tuples"}`, parsed
        // leniently (whitespace, key order) — operator tooling writes these.
        let phase = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(marker) if marker["phase"].as_str().map(str::trim) == Some("tuples") => {
                ErasurePhase::Tuples
            }
            _ => ErasurePhase::Rows,
        };
        requests.push(ErasureRequest {
            tenant,
            conversation_id: conversation,
            marker: key,
            phase,
        });
    }
    Ok(requests)
}

/// The RFC 0047 §3.6 erasure pass (rows phase).
pub(super) fn erase_pending(
    store: &Store,
    now_unix_nanos: u64,
    promoted: &PromotedAttributes,
    erasure_match: Option<&ErasureMatch<'_>>,
    report: &mut SweepReport,
) -> Result<(), IngestError> {
    for request in pending_erasures(store)? {
        // RFC 0048 §3.4: backfill and erasure exclude each other — a
        // partition read before this erasure and written after it would
        // recreate the tuples. The lock is read **per request**, right
        // before acting, never from a snapshot: with backfill's
        // re-check-under-lock on the other side, every interleaving is
        // covered — a backfill that acquired before this read is seen
        // here (defer), and one that acquires after it sees this marker
        // under its own lock (refuse + release).
        if backfill_lock_held(store, &request.tenant)? {
            report.erasures_deferred.push(request.marker.clone());
            tracing::info!(
                "erasure of tenant {:?} conversation {:?} deferred: backfill in progress",
                request.tenant,
                request.conversation_id,
            );
            continue;
        }
        let mut outcome = ErasureOutcome {
            request: request.clone(),
            partitions_rewritten: 0,
            rows_dropped: 0,
            phase: request.phase,
            tuples_deleted: None,
            finished: false,
        };
        if request.phase == ErasurePhase::Rows {
            let Some(matches) = erasure_match else {
                report.errors.push(format!(
                    "erase {:?} {:?}: no conversation column configured \
                     (auth.openfga.visibility.objects) — request left pending",
                    request.tenant, request.conversation_id
                ));
                report.erasures.push(outcome);
                continue;
            };
            let id = request.conversation_id.as_str();
            let drop = |record: &MinedRecord| matches(record, id);
            let mut clean = true;
            let partitions = match hour_partitions(store, &request.tenant) {
                Ok(partitions) => partitions,
                Err(e) => {
                    report
                        .errors
                        .push(format!("erase {:?}: list partitions: {e}", request.tenant));
                    report.erasures.push(outcome);
                    continue;
                }
            };
            for partition in partitions {
                let mut row_hooks = RowHooks {
                    observe: None,
                    drop: Some(&drop),
                };
                match compact_partition_hooked(store, &partition, promoted, &mut row_hooks) {
                    Ok(o) => {
                        if let Some(committed) = &o.committed {
                            outcome.partitions_rewritten += 1;
                            outcome.rows_dropped += o.rows_dropped;
                            // An erasure rewrite is a compaction like any
                            // other for the sweep's IO accounting and audit
                            // trail (RFC 0009 §3.6): it reads and writes the
                            // partition and commits a generation.
                            report.partitions_compacted += 1;
                            report.files_compacted += to_u64(o.files_before);
                            report.rows_compacted += o.rows;
                            report.bytes_read = report.bytes_read.saturating_add(o.bytes_read);
                            report.compacted_files.push(CompactedFile {
                                tenant: request.tenant.clone(),
                                bytes: o.bytes_written,
                            });
                            report.compaction_events.push(compaction_audit_event(
                                &request.tenant,
                                now_unix_nanos,
                                &partition,
                                committed,
                                o.rows,
                            ));
                            report.gc_failures += o.gc_failures;
                        }
                    }
                    Err(e) => {
                        clean = false;
                        report.errors.push(format!(
                            "erase {:?} {:?} {:04}-{:02}-{:02}T{:02}: {e}",
                            request.tenant,
                            request.conversation_id,
                            partition.year,
                            partition.month,
                            partition.day,
                            partition.hour,
                        ));
                    }
                }
            }
            if clean {
                match store.put_blocking(&request.marker, ERASURE_PHASE_TUPLES.to_vec()) {
                    Ok(()) => outcome.phase = ErasurePhase::Tuples,
                    Err(e) => report.errors.push(format!(
                        "erase {:?} {:?}: advance marker: {e}",
                        request.tenant, request.conversation_id
                    )),
                }
            }
        }
        report.erasures.push(outcome);
    }
    Ok(())
}
