//! RFC 0047 §3.6 erasure — request markers, pending-marker listing, and
//! the read→delete application the sweep runs. Split from the flat
//! `compactor.rs` (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path resolving unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;
use std::collections::HashMap;

use ourios_parquet::partition_snapshot;

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
    /// When the store last wrote the marker, by the store's clock (Unix
    /// nanoseconds): the request instant `erasure_settle` is measured from.
    pub requested_at_unix_nanos: u64,
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
    let mut markers = store
        .list_objects_blocking(Some(prefix))
        .map_err(|e| store_error("list erasure markers", prefix, &e))?;
    markers.sort_by(|a, b| a.key.cmp(&b.key));
    let mut requests = Vec::new();
    for marker in markers {
        let key = marker.key;
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
            requested_at_unix_nanos: marker.last_modified_unix_nanos,
        });
    }
    Ok(requests)
}

/// The RFC 0047 §3.6 erasure pass (rows phase).
pub(super) fn erase_pending(
    sweep: &mut Sweep<'_, '_>,
    erasure_match: Option<&ErasureMatch<'_>>,
    settle_nanos: u64,
) -> Result<(), IngestError> {
    for request in pending_erasures(sweep.store)? {
        // RFC 0048 §3.4: backfill and erasure exclude each other — a
        // partition read before this erasure and written after it would
        // recreate the tuples. The lock is read **per request**, right
        // before acting, never from a snapshot: with backfill's
        // re-check-under-lock on the other side, every interleaving is
        // covered — a backfill that acquired before this read is seen
        // here (defer), and one that acquires after it sees this marker
        // under its own lock (refuse + release).
        if backfill_lock_held(sweep.store, &request.tenant)? {
            sweep.report.erasures_deferred.push(request.marker.clone());
            tracing::info!(
                "erasure of tenant {:?} conversation {:?} deferred: backfill in progress",
                request.tenant,
                request.conversation_id,
            );
            continue;
        }
        if request.phase == ErasurePhase::Rows && !settled(sweep, &request, settle_nanos) {
            sweep.report.erasures_deferred.push(request.marker.clone());
            tracing::info!(
                "erasure of tenant {:?} conversation {:?} deferred: requested less than \
                 erasure_settle ago",
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
            match erasure_match {
                Some(matches) => erase_rows(sweep, matches, &mut outcome),
                None => sweep.report.errors.push(format!(
                    "erase {:?} {:?}: no conversation column configured \
                     (auth.openfga.visibility.objects) — request left pending",
                    request.tenant, request.conversation_id
                )),
            }
        }
        sweep.report.erasures.push(outcome);
    }
    Ok(())
}

/// Whether `request` is older than `settle_nanos` by the sweep's clock, so
/// the receivers have published what they acknowledged before it (RFC 0009
/// §3.4 erasure note, `erasure_settle`).
fn settled(sweep: &Sweep<'_, '_>, request: &ErasureRequest, settle_nanos: u64) -> bool {
    (sweep.clock.now)() >= request.requested_at_unix_nanos.saturating_add(settle_nanos)
}

/// Confirming passes after the first rewrite of every partition before the
/// rows phase gives up for this sweep (the tuple phase's bound).
const CONFIRMING_ROUNDS: usize = 8;

/// The generation each partition was at when this rows phase last checked
/// it: the generation its erasure commit won, or the one it observed when
/// there was nothing to rewrite.
type Checked = HashMap<PartitionKey, u64>;

/// Rewrite every partition of the request's tenant with the conversation's
/// rows dropped, then confirm: rewrite again, for at most
/// [`CONFIRMING_ROUNDS`] passes, every partition that has since taken a
/// flush file or moved past its checked generation (a late file, or a
/// second compactor's consolidation of one). Once a pass finds nothing to
/// rewrite, advance the marker to `Tuples` (RFC 0009 §3.4 erasure note).
fn erase_rows(sweep: &mut Sweep<'_, '_>, matches: &ErasureMatch<'_>, outcome: &mut ErasureOutcome) {
    let request = outcome.request.clone();
    let id = request.conversation_id.as_str();
    let drop = |record: &MinedRecord| matches(record, id);
    let Some(partitions) = tenant_partitions(sweep, &request) else {
        return;
    };
    let mut checked = Checked::new();
    let mut erasing = Erasing {
        drop: &drop,
        outcome,
        checked: &mut checked,
    };
    if !erase_each(sweep, &partitions, &mut erasing) {
        return;
    }
    for _ in 0..CONFIRMING_ROUNDS {
        let Some(changed) = changed_partitions(sweep, &request, erasing.checked) else {
            return;
        };
        if changed.is_empty() {
            advance_marker(sweep, erasing.outcome);
            return;
        }
        if !erase_each(sweep, &changed, &mut erasing) {
            return;
        }
    }
    sweep.report.errors.push(format!(
        "erase {:?} {:?}: partitions still changing after {CONFIRMING_ROUNDS} confirming \
         passes — request left in the rows phase",
        request.tenant, request.conversation_id
    ));
}

/// The tenant's hour partitions, or `None` with the listing error recorded.
fn tenant_partitions(
    sweep: &mut Sweep<'_, '_>,
    request: &ErasureRequest,
) -> Option<Vec<PartitionKey>> {
    match hour_partitions(sweep.store, &request.tenant) {
        Ok(partitions) => Some(partitions),
        Err(e) => {
            sweep
                .report
                .errors
                .push(format!("erase {:?}: list partitions: {e}", request.tenant));
            None
        }
    }
}

/// Erase from each of `partitions`, recording its checked generation;
/// whether every one rewrote cleanly.
fn erase_each(
    sweep: &mut Sweep<'_, '_>,
    partitions: &[PartitionKey],
    erasing: &mut Erasing<'_, '_>,
) -> bool {
    let mut clean = true;
    for partition in partitions {
        match erase_partition(sweep, partition, erasing.drop, erasing.outcome) {
            Some(generation) => {
                erasing.checked.insert(partition.clone(), generation);
            }
            None => clean = false,
        }
    }
    clean
}

/// One request's rows phase in progress: its filter, its outcome, and the
/// generation each partition was checked at.
struct Erasing<'e, 'f> {
    drop: &'e RowFilter<'f>,
    outcome: &'e mut ErasureOutcome,
    checked: &'e mut Checked,
}

/// The partitions of the request's tenant a confirming pass must rewrite
/// again: those holding a live flush file, at a generation other than their
/// checked one, or not checked yet. `None` with the error recorded when the
/// store cannot be read.
fn changed_partitions(
    sweep: &mut Sweep<'_, '_>,
    request: &ErasureRequest,
    checked: &Checked,
) -> Option<Vec<PartitionKey>> {
    let partitions = tenant_partitions(sweep, request)?;
    let mut changed = Vec::new();
    for partition in partitions {
        match partition_snapshot(sweep.store, &partition) {
            Ok(now) => {
                let settled =
                    now.live_flush_files == 0 && checked.get(&partition) == Some(&now.generation);
                if !settled {
                    changed.push(partition);
                }
            }
            Err(e) => {
                let hour = hour_label(&partition);
                sweep.report.errors.push(format!(
                    "erase {:?} {:?} {hour}: confirm: {e}",
                    request.tenant, request.conversation_id
                ));
                return None;
            }
        }
    }
    Some(changed)
}

/// Rewrite one partition of the request's tenant without the rows `drop`
/// matches; its checked generation, or `None` if it did not rewrite
/// cleanly. An erasure rewrite is a compaction like any other for the
/// sweep's IO accounting and audit trail (RFC 0009 §3.6): it reads and
/// writes the partition and commits a generation, so `rewrite` records
/// its commit as the consolidation pass does. A partition with nothing to
/// rewrite is checked at the generation read **before** the rewrite: one
/// read after it could already name a peer's commit of rows this rewrite
/// never filtered, which the confirming pass would then wrongly take as
/// checked.
fn erase_partition(
    sweep: &mut Sweep<'_, '_>,
    partition: &PartitionKey,
    drop: &RowFilter<'_>,
    outcome: &mut ErasureOutcome,
) -> Option<u64> {
    let request = &outcome.request;
    let target = Target {
        tenant: &request.tenant,
        partition,
    };
    let context = || {
        let hour = hour_label(partition);
        format!(
            "erase {:?} {:?} {hour}",
            request.tenant, request.conversation_id
        )
    };
    let before = match partition_snapshot(sweep.store, partition) {
        Ok(before) => before,
        Err(e) => {
            sweep.report.errors.push(format!("{}: {e}", context()));
            return None;
        }
    };
    match sweep.rewrite(target, None, Some(drop)) {
        Ok(o) => match o.committed {
            Some(committed) => {
                outcome.partitions_rewritten += 1;
                outcome.rows_dropped += o.rows_dropped;
                sweep.report.gc_failures += o.gc_failures;
                Some(committed.generation)
            }
            None => Some(before.generation),
        },
        Err(e) => {
            e.record(&mut sweep.report, &context());
            None
        }
    }
}

/// Move the request's marker to the `Tuples` phase: its rows are gone.
fn advance_marker(sweep: &mut Sweep<'_, '_>, outcome: &mut ErasureOutcome) {
    let request = &outcome.request;
    match sweep
        .store
        .put_blocking(&request.marker, ERASURE_PHASE_TUPLES.to_vec())
    {
        Ok(()) => outcome.phase = ErasurePhase::Tuples,
        Err(e) => sweep.report.errors.push(format!(
            "erase {:?} {:?}: advance marker: {e}",
            request.tenant, request.conversation_id
        )),
    }
}
