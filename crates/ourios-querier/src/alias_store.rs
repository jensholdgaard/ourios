//! v1 reader-side alias-map derivation (RFC 0005 §3.7.1).
//!
//! There is **no persisted per-tenant alias-map artifact** in v1: the
//! audit stream *is* the alias store, and the querier derives the
//! requesting tenant's [`AliasMap`] at query-compile time by folding
//! the tenant's `alias_asserted` / `alias_retracted` events (RFC 0001
//! §6.7) off the RFC 0005 §3.7 audit Parquet stream. The fold order is
//! total and deterministic — `(timestamp, file path lexicographic,
//! within-file row index)` — pinned by §3.7.1 so same-nanosecond ties
//! fold identically across re-scans. The fold semantics themselves
//! (union-on-overlap, retraction removes the asserted set's ids,
//! canonical = `min(members)`) are owned by RFC 0001 §6.7 and
//! implemented by [`ourios_core::alias::AliasMap`]; this module only
//! feeds it the ordered event stream.
//!
//! The derived map reflects exactly the alias events durably flushed
//! to the audit stream at scan time — the RFC 0001 §6.7
//! eventual-consistency stance, with the staleness window being
//! audit-flush visibility. A future materialized per-tenant cache
//! (the RFC 0009 §3.4 manifest fork) would accelerate, not change,
//! this derivation.

use ourios_core::alias::AliasMap;
use ourios_core::audit::AuditPayload;
use ourios_core::tenant::TenantId;

use crate::{QueryError, StoreRef, audit_scan};

/// Fold `tenant`'s alias map from its audit stream per RFC 0005 §3.7.1. A
/// tenant with no audit files (or none carrying alias events) derives the empty
/// map — every id then resolves to itself.
///
/// `backend` selects the hybrid scan (RFC 0019 §3.3): [`StoreRef::Local`] reads
/// local audit files, [`StoreRef::Remote`] lists keys + reads bytes through the
/// S3 store.
///
/// Alias events are rare operator actions, not ingest-volume data, so
/// the unwindowed scan is small by construction (§3.7.1); no day prune
/// applies because the fold covers the tenant's whole alias history.
///
/// # Errors
///
/// [`QueryError::Storage`] if the audit subtree cannot be listed, an audit
/// file cannot be read, or a row claims a tenant other than the one whose
/// partition root it lives under (the RFC 0005 §3.9 row-vs-path backstop).
pub fn derive_alias_map(backend: StoreRef<'_>, tenant: &TenantId) -> Result<AliasMap, QueryError> {
    // The shared reader gives the §3.7.1 file/row order and the row-level
    // tenant backstop; keep only the alias events. The reader's byte
    // accounting is unused here (RFC 0031 measures the registry derivation,
    // not the alias fold).
    let (all_events, _bytes_read) = audit_scan::read_all_events(backend, tenant)?;
    let mut events: Vec<_> = all_events
        .into_iter()
        .filter(|e| {
            matches!(
                &e.payload,
                AuditPayload::AliasAsserted { .. } | AuditPayload::AliasRetracted { .. }
            )
        })
        .collect();
    // …and the stable sort by event time completes the total order:
    // same-timestamp events keep their (file path, row index) order.
    events.sort_by_key(|e| e.timestamp);
    Ok(AliasMap::from_events(&events))
}
