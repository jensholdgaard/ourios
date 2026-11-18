//! What a tail replica keeps for a takeover (RFC 0065 §6.7): the rows it
//! mined above the last checkpoint it applied, and the audit events its
//! miner emitted for them, each with the log index of the entry whose
//! mining emitted it. Applying a checkpoint drops what is at or below the
//! cut, which the proposer published before writing its bundle.
//!
//! Both are bounded. Audit events stay in memory up to a ceiling of
//! estimated bytes; a replica that would pass it drops every event it
//! keeps. Rows that do not fit their bound are dropped as they arrive.
//! Either way the replica records the highest index whose output it
//! dropped, its **gap**, counts what it dropped, and keeps later entries'
//! output; a checkpoint whose cut is at or above the gap clears it.

use std::collections::VecDeque;

use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange};

use super::mining::{Mined, MinedRow};
use crate::record_sink::estimate_bytes;

/// The audit events a group keeps for a takeover, in estimated bytes: a
/// starting value (§6.9).
pub const AUDIT_CEILING: usize = 16 * 1024 * 1024;

/// The most a group keeps, in estimated bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub audit: usize,
    /// Rows, until the local Parquet cache holds them on the tail volume.
    pub rows: usize,
}

/// Rows kept per group until the local Parquet cache holds them: a
/// starting value.
pub const ROW_BOUND: usize = 64 * 1024 * 1024;

impl Default for Bounds {
    fn default() -> Self {
        Self {
            audit: AUDIT_CEILING,
            rows: ROW_BOUND,
        }
    }
}

/// What a replica dropped from what it keeps, since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    pub audit: u64,
    pub rows: u64,
}

/// One group's kept output.
#[derive(Debug)]
pub struct Kept {
    bounds: Bounds,
    audit: VecDeque<(u64, AuditEvent)>,
    rows: VecDeque<MinedRow>,
    audit_bytes: usize,
    row_bytes: usize,
    gap: Option<u64>,
    /// The last cut applied; `0` before any.
    cut: u64,
    dropped: Dropped,
}

impl Default for Kept {
    fn default() -> Self {
        Self::new(Bounds::default())
    }
}

impl Kept {
    #[must_use]
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            audit: VecDeque::new(),
            rows: VecDeque::new(),
            audit_bytes: 0,
            row_bytes: 0,
            gap: None,
            cut: 0,
            dropped: Dropped::default(),
        }
    }

    /// Keep what the entry at `index` mined.
    pub fn keep(&mut self, index: u64, Mined { audit, rows }: Mined) {
        for event in audit {
            self.audit_bytes += audit_bytes(&event);
            self.audit.push_back((index, event));
        }
        if self.audit_bytes > self.bounds.audit {
            self.dropped.audit += self.audit.len() as u64;
            self.audit.clear();
            self.audit_bytes = 0;
            self.gapped(index);
        }
        let bytes: usize = rows.iter().map(|row| estimate_bytes(&row.record)).sum();
        match self.row_bytes.checked_add(bytes) {
            Some(total) if total <= self.bounds.rows => {
                self.row_bytes = total;
                self.rows.extend(rows);
            }
            _ => {
                self.dropped.rows += rows.len() as u64;
                self.gapped(index);
            }
        }
    }

    /// Drop everything at or below `cut`, and clear a gap the cut reaches.
    pub fn checkpoint(&mut self, cut: u64) {
        self.cut = self.cut.max(cut);
        self.drop_through(cut);
    }

    /// Drop everything at or below `through`, which a replay has published,
    /// and clear a gap it reaches. Unlike a checkpoint, it moves no cut.
    pub fn replayed(&mut self, through: u64) {
        self.drop_through(through);
    }

    /// The last cut applied; `0` before any.
    #[must_use]
    pub fn cut(&self) -> u64 {
        self.cut
    }

    fn drop_through(&mut self, through: u64) {
        while let Some((_, event)) = self.audit.front().filter(|(index, _)| *index <= through) {
            self.audit_bytes -= audit_bytes(event);
            self.audit.pop_front();
        }
        while let Some(row) = self.rows.front().filter(|row| row.position.seq <= through) {
            self.row_bytes -= estimate_bytes(&row.record);
            self.rows.pop_front();
        }
        if self.gap.is_some_and(|gap| gap <= through) {
            self.gap = None;
        }
    }

    /// The audit events kept, in emission order.
    pub fn audit(&self) -> impl Iterator<Item = &AuditEvent> {
        self.audit.iter().map(|(_, event)| event)
    }

    /// The rows kept, in log order.
    pub fn rows(&self) -> impl Iterator<Item = &MinedRow> {
        self.rows.iter()
    }

    /// The estimated bytes of the audit events kept.
    #[must_use]
    pub fn audit_bytes(&self) -> usize {
        self.audit_bytes
    }

    /// The highest index whose output was dropped and no checkpoint has
    /// reached.
    #[must_use]
    pub fn gap(&self) -> Option<u64> {
        self.gap
    }

    /// The entries from the last applied cut through the gap; `0` with
    /// no gap.
    #[must_use]
    pub fn gap_reach(&self) -> u64 {
        self.gap.map_or(0, |gap| gap.saturating_sub(self.cut))
    }

    #[must_use]
    pub fn dropped(&self) -> Dropped {
        self.dropped
    }

    /// Record a drop of the output of the entry at `index`.
    #[cfg(test)]
    pub(in crate::replicated::raft) fn gap_at(&mut self, index: u64) {
        self.gapped(index);
    }

    fn gapped(&mut self, index: u64) {
        self.gap = self.gap.max(Some(index));
    }
}

/// A rough footprint of `event`: a fixed overhead plus its variable-length
/// fields, as [`estimate_bytes`] is for a row.
fn audit_bytes(event: &AuditEvent) -> usize {
    let AuditPayload::Template {
        triggering_line_sample,
        change,
        ..
    } = &event.payload
    else {
        // The miner emits only template events; anything else is rare.
        return 256;
    };
    let templates = match change {
        TemplateChange::Created { new_template } | TemplateChange::Adopted { new_template, .. } => {
            new_template.len()
        }
        TemplateChange::Widened {
            old_template,
            new_template,
            positions_widened,
            ..
        } => old_template.len() + new_template.len() + 2 * positions_widened.len(),
        TemplateChange::TypeExpanded {
            old_template,
            new_template,
            slots_expanded,
            ..
        } => old_template.len() + new_template.len() + 16 * slots_expanded.len(),
        TemplateChange::RejectedDegenerate {
            current_template,
            would_be_template,
            would_be_positions,
            ..
        } => current_template.len() + would_be_template.len() + 2 * would_be_positions.len(),
    };
    96 + triggering_line_sample.as_ref().map_or(0, String::len) + templates
}

mod page;

pub use page::{Definitions, RowPage, RowScan, TemplateKey};

#[cfg(test)]
pub(super) mod tests;
