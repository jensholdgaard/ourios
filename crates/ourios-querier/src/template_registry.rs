//! v1 reader-side template-registry derivation (RFC 0017 §3.2).
//!
//! There is **no persisted per-tenant template-registry artifact** in v1
//! (the cached map is the deferred RFC 0005 §3.7.1 / manifest-fork
//! optimisation): the audit stream *is* the registry, and the querier
//! derives the requesting tenant's `(template_id, template_version) →
//! tokens` map at query time by folding the tenant's `template_created` /
//! `template_widened` / `template_type_expanded` events (RFC 0001 §6.4,
//! RFC 0017 §3.1) off the RFC 0005 §3.7 audit Parquet stream.
//!
//! This mirrors [`crate::alias_store::derive_alias_map`] exactly — the same
//! shared [`crate::audit_scan`] walk and the same total, deterministic fold
//! order `(timestamp, file path lexicographic, within-file row index)` (RFC
//! 0005 §3.7.1). Keying by the `(template_id, version)` pair is what lets a
//! row stamped `template_version = N` render against the N-version tokens
//! rather than the latest (RFC 0017 §3.5): each version is its own key, so a
//! later widening never clobbers an earlier version's tokens.

use std::collections::HashMap;
use std::path::Path;

use ourios_core::audit::{AuditEvent, AuditPayload, TEMPLATE_INITIAL_VERSION, TemplateChange};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::{OwnedToken, parse_template};
use ourios_parquet::AuditReader;

use crate::{QueryError, audit_scan};

/// Read-time map from a leaf's `(template_id, template_version)` to the
/// canonical tokens of that version (RFC 0017 §3.2). The value is parsed
/// from the audit stream's stored template string via
/// [`ourios_miner::tree::parse_template`].
pub type TemplateRegistry = HashMap<(u64, u32), Vec<OwnedToken>>;

/// Fold `tenant`'s template registry from its audit stream under
/// `bucket_root` per RFC 0017 §3.2. A tenant with no audit files (or none
/// carrying template events) derives the empty registry.
///
/// Each `template_created` event keys at [`TEMPLATE_INITIAL_VERSION`] (the
/// variant omits the version — a leaf is always born at v1); each
/// `template_widened` / `template_type_expanded` event keys at its
/// `new_version`. `template_widening_rejected_degenerate` events carry no
/// version bump or new tokens, so they contribute nothing.
///
/// # Errors
///
/// [`QueryError::Storage`] if the audit subtree cannot be walked, an audit
/// file cannot be read, or a row claims a tenant other than the one whose
/// partition root it lives under (the RFC 0005 §3.9 row-vs-path backstop).
pub fn derive_template_registry(
    bucket_root: &Path,
    tenant: &TenantId,
) -> Result<TemplateRegistry, QueryError> {
    // Lexicographic file order from the shared walk + in-file row order from
    // the reader give the §3.7.1 tiebreak components; no window — the
    // registry folds the tenant's whole template history.
    let files = audit_scan::audit_files(bucket_root, tenant, None)?;
    let mut events: Vec<AuditEvent> = Vec::new();
    for path in &files {
        let read = AuditReader::open_file(path)
            .and_then(AuditReader::read_all)
            .map_err(|e| QueryError::Storage {
                detail: format!("audit file {}: {e}", path.display()),
            })?;
        for event in read {
            // Row-level tenant backstop (`CLAUDE.md` §3.7 / RFC 0005 §3.9
            // row-vs-path): the walk is rooted at the tenant's partition, so
            // a row claiming another tenant is a corrupt or foreign file —
            // fail loudly rather than fold (or silently drop) it.
            if event.tenant_id != *tenant {
                return Err(QueryError::Storage {
                    detail: format!(
                        "audit file {} carries a row for tenant {} under tenant {}'s \
                         partition root",
                        path.display(),
                        event.tenant_id.as_str(),
                        tenant.as_str(),
                    ),
                });
            }
            if matches!(event.payload, AuditPayload::Template { .. }) {
                events.push(event);
            }
        }
    }
    // The stable sort by event time completes the total order: same-timestamp
    // events keep their (file path, row index) order.
    events.sort_by_key(|e| e.timestamp);

    let mut registry = TemplateRegistry::new();
    for event in events {
        let AuditPayload::Template {
            template_id,
            change,
            ..
        } = event.payload
        else {
            continue;
        };
        let (version, template) = match change {
            TemplateChange::Created { new_template } => (TEMPLATE_INITIAL_VERSION, new_template),
            TemplateChange::Widened {
                new_version,
                new_template,
                ..
            }
            | TemplateChange::TypeExpanded {
                new_version,
                new_template,
                ..
            } => (new_version, new_template),
            // A rejection bumps no version and changes no tokens.
            TemplateChange::RejectedDegenerate { .. } => continue,
        };
        registry.insert((template_id, version), parse_template(&template));
    }
    Ok(registry)
}
