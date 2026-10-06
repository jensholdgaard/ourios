//! The RFC 0001 §6.2 step-0 short-circuit for `Body::Structured`
//! records, as extended by RFC 0037 §3.1.

use ourios_core::otlp::OtlpLogRecord;

use super::id_alloc::ID_RESERVATION_FAILED;
use super::{MinerCluster, NO_TEMPLATE, StructuredLine, TenantState};

impl MinerCluster {
    /// The tree is not walked; the per-tenant `(severity_number,
    /// scope_name, event_name) → template_id` map is the entire lookup.
    /// First observation of a tuple allocates; subsequent records with
    /// the same tuple reuse. Structured records never widen and never
    /// emit audit events. A first observation with no reservable id is
    /// emitted under [`NO_TEMPLATE`] (the body is verbatim either way)
    /// and counted as a parse failure.
    pub(super) fn ingest_structured(&mut self, line: StructuredLine<'_>) -> u64 {
        let template_id = self.structured_template_id(line.record);
        if template_id == NO_TEMPLATE {
            self.record_parse_failure(line.record, line.service, ID_RESERVATION_FAILED);
        }
        self.emit_structured(line, template_id);
        template_id
    }

    /// The structured-template id for `record`'s `(severity_number,
    /// scope_name, event_name)` tuple, allocated on its first sight, or
    /// [`NO_TEMPLATE`] when that first sight finds no reservable id.
    fn structured_template_id(&mut self, record: &OtlpLogRecord) -> u64 {
        let key = (
            record.severity_number,
            record.scope_name.clone(),
            record.event_name.clone(),
        );
        // Resolve the effective config before the mutable borrow on
        // `self.tenants`.
        let effective_config = self.effective_config(&record.tenant_id);
        if let Some(&existing_id) = self
            .tenants
            .get(&record.tenant_id)
            .and_then(|state| state.structured_templates.get(&key))
        {
            return existing_id;
        }
        let Ok(new_id) = self.ids.take() else {
            return NO_TEMPLATE;
        };
        let state = self
            .tenants
            .entry(record.tenant_id.clone())
            .or_insert_with(|| TenantState::new(effective_config));
        state.structured_templates.insert(key, new_id);
        // Same cache invariant as `create_new_leaf`: one fresh
        // allocation, one cache increment.
        state.template_count += 1;
        new_id
    }
}
