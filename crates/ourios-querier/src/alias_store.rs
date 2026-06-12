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

use std::path::Path;

use ourios_core::alias::AliasMap;
use ourios_core::audit::{AuditEvent, AuditPayload};
use ourios_core::tenant::TenantId;
use ourios_parquet::AuditReader;

use crate::{QueryError, audit_scan};

/// Fold `tenant`'s alias map from its audit stream under `bucket_root`
/// per RFC 0005 §3.7.1. A tenant with no audit files (or none carrying
/// alias events) derives the empty map — every id then resolves to
/// itself.
///
/// Alias events are rare operator actions, not ingest-volume data, so
/// the unwindowed scan is small by construction (§3.7.1); no day prune
/// applies because the fold covers the tenant's whole alias history.
pub(crate) fn derive_alias_map(
    bucket_root: &Path,
    tenant: &TenantId,
) -> Result<AliasMap, QueryError> {
    // Lexicographic file order from the shared walk + in-file row order
    // from the reader give the §3.7.1 tiebreak components…
    let files = audit_scan::audit_files(bucket_root, tenant, None)?;
    let mut events: Vec<AuditEvent> = Vec::new();
    for path in &files {
        let read = AuditReader::open_file(path)
            .and_then(AuditReader::read_all)
            .map_err(|e| QueryError::Storage {
                detail: format!("audit file {}: {e}", path.display()),
            })?;
        for event in read {
            // Row-level tenant backstop (`CLAUDE.md` §3.7 / RFC 0005
            // §3.9 row-vs-path): the walk is already rooted at the
            // tenant's partition, so a row claiming another tenant is
            // a corrupt or foreign file — fail loudly rather than
            // fold (or silently drop) it.
            if event.tenant_id != *tenant {
                return Err(QueryError::Storage {
                    detail: format!(
                        "audit file {} carries a row for tenant {:?} under tenant {:?}'s \
                         partition root",
                        path.display(),
                        event.tenant_id.as_str(),
                        tenant.as_str(),
                    ),
                });
            }
            if matches!(
                event.payload,
                AuditPayload::AliasAsserted { .. } | AuditPayload::AliasRetracted { .. }
            ) {
                events.push(event);
            }
        }
    }
    // …and the stable sort by event time completes the total order:
    // same-timestamp events keep their (file path, row index) order.
    events.sort_by_key(|e| e.timestamp);
    Ok(AliasMap::from_events(&events))
}
