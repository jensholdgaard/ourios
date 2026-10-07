//! The template bindings a store's audit stream records.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ourios_core::audit::{AuditPayload, TEMPLATE_INITIAL_VERSION, TemplateChange};

use super::audit_events;

/// Every `(template_id, version)` the audit events under `root` bind, with
/// the template texts bound to it.
pub fn audit_bindings(root: &Path) -> BTreeMap<(u64, u32), BTreeSet<String>> {
    let mut bound: BTreeMap<(u64, u32), BTreeSet<String>> = BTreeMap::new();
    for event in audit_events(root) {
        if let AuditPayload::Template {
            template_id,
            change,
            ..
        } = event.payload
            && let Some((version, template)) = binding(change)
        {
            bound
                .entry((template_id, version))
                .or_default()
                .insert(template);
        }
    }
    bound
}

fn binding(change: TemplateChange) -> Option<(u32, String)> {
    match change {
        TemplateChange::Created { new_template } => Some((TEMPLATE_INITIAL_VERSION, new_template)),
        TemplateChange::Widened {
            new_version,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            new_version,
            new_template,
            ..
        } => Some((new_version, new_template)),
        TemplateChange::Adopted {
            template_version,
            new_template,
        } => Some((template_version, new_template)),
        TemplateChange::RejectedDegenerate { .. } => None,
    }
}
