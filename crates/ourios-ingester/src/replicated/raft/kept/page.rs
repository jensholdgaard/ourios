//! What a tail replica serves of what it keeps (RFC 0065 §6.7): a page of
//! one tenant's rows as of a read point, and the definitions its kept
//! audit events hold of the templates those rows reference.

use std::collections::{BTreeMap, BTreeSet};

use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange};
use ourios_core::tenant::TenantId;
use ourios_parquet::partition::effective_time_unix_nano;

use super::Kept;
use crate::record_sink::estimate_bytes;
use crate::replicated::raft::mining::MinedRow;

/// A template version: `(template_id, template_version)`.
pub type TemplateKey = (u64, u32);

/// Template tokens by version, as `ourios_miner::tree::format_template`
/// writes them and the audit stream records them.
pub type Definitions = BTreeMap<TemplateKey, String>;

/// Which kept rows a page reads.
#[derive(Debug, Clone, Copy)]
pub struct RowScan<'a> {
    pub tenant: &'a TenantId,
    /// The checkpoint cut the reader holds: rows above it.
    pub from: u64,
    /// The read point: rows at or below it.
    pub read: u64,
    /// `[start, end)` of the rows' effective time in Unix nanoseconds.
    pub range: Option<(u64, u64)>,
    /// The `(seq, index)` the previous page ended at.
    pub after: Option<(u64, u32)>,
}

/// One page of kept rows, and where the next starts after.
#[derive(Debug, Clone, PartialEq)]
pub struct RowPage {
    pub rows: Vec<MinedRow>,
    pub next: Option<(u64, u32)>,
}

impl RowScan<'_> {
    /// Whether `row` is past where the scan starts: above `from` and after
    /// `after`.
    fn starts_after(&self, row: &MinedRow) -> bool {
        let at = (row.position.seq, row.position.index);
        at.0 > self.from && self.after.is_none_or(|after| at > after)
    }

    fn selects(&self, row: &MinedRow) -> bool {
        let at = (row.position.seq, row.position.index);
        &row.record.tenant_id == self.tenant
            && at.0 > self.from
            && at.0 <= self.read
            && self.after.is_none_or(|after| at > after)
            && self.range.is_none_or(|range| in_range(row, range))
    }
}

/// Whether `row`'s effective time is in `[start, end)`; a time the record
/// files cannot hold is in no range.
fn in_range(row: &MinedRow, (start, end): (u64, u64)) -> bool {
    effective_time_unix_nano(&row.record)
        .ok()
        .and_then(|time| u64::try_from(time).ok())
        .is_some_and(|time| start <= time && time < end)
}

impl Kept {
    /// The rows `scan` selects, in log order, from its `after` on, up to
    /// `budget` estimated bytes but at least one.
    #[must_use]
    pub fn page(&self, scan: &RowScan<'_>, budget: usize) -> RowPage {
        let mut rows: Vec<MinedRow> = Vec::new();
        let mut bytes = 0usize;
        // Kept rows are in position order: seek past `from` and `after`,
        // and stop past the read point, so a page costs its own rows.
        let start = self.rows.partition_point(|row| !scan.starts_after(row));
        let selected = self
            .rows
            .range(start..)
            .take_while(|row| row.position.seq <= scan.read)
            .filter(|row| scan.selects(row));
        for row in selected {
            let size = estimate_bytes(&row.record);
            if !rows.is_empty() && bytes.saturating_add(size) > budget {
                let next = rows
                    .last()
                    .map(|last| (last.position.seq, last.position.index));
                return RowPage { rows, next };
            }
            bytes = bytes.saturating_add(size);
            rows.push(row.clone());
        }
        RowPage { rows, next: None }
    }

    /// The definitions of `wanted` that the audit events kept for `tenant`
    /// hold: each version a template change binds, and the version a
    /// widening or type expansion replaced, which rows mined before it
    /// still reference.
    #[must_use]
    pub fn definitions(&self, tenant: &TenantId, wanted: &BTreeSet<TemplateKey>) -> Definitions {
        self.audit()
            .filter(|event| &event.tenant_id == tenant)
            .flat_map(bindings)
            .filter(|(key, _)| wanted.contains(key))
            .map(|(key, template)| (key, template.to_owned()))
            .collect()
    }
}

/// The template versions `event` binds tokens to.
fn bindings(event: &AuditEvent) -> Vec<(TemplateKey, &str)> {
    let AuditPayload::Template {
        template_id: id,
        change,
        ..
    } = &event.payload
    else {
        return Vec::new();
    };
    match change {
        TemplateChange::Created { new_template } => vec![((*id, 1), new_template.as_str())],
        TemplateChange::Widened {
            old_version,
            new_version,
            old_template,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            old_version,
            new_version,
            old_template,
            new_template,
            ..
        } => vec![
            ((*id, *old_version), old_template.as_str()),
            ((*id, *new_version), new_template.as_str()),
        ],
        TemplateChange::Adopted {
            template_version,
            new_template,
        } => vec![((*id, *template_version), new_template.as_str())],
        TemplateChange::RejectedDegenerate { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
