//! Publication from a group's flusher (RFC 0065 §6.5, §6.7): the rows a
//! tail replica mined, each stamped with its ingest position in the
//! group's log (§6.6), and the audit events its miner emitted for them,
//! handed to the same publish path the local receiver uses after the ack.
//!
//! Audit events are handed over before the rows of the same step, and the
//! [`PublishCoordinator`] writes every drained event durably before any
//! file holding rows drained with it, so a template's events reach the
//! store before the first file whose rows use it. The record sink's size
//! trigger publishes inline; the age trigger and a full flush are the
//! caller's to drive.

use ourios_core::audit::{AuditEvent, AuditSink as _};
use ourios_core::record::{IngestPosition, MinedRecord, RecordSink as _};

use super::kept::Kept;
use super::mining::MinedRow;
use crate::publish::PublishCoordinator;

/// A flusher's publication of one group, whose log id is `log`.
pub struct Publication {
    log: u32,
    coordinator: PublishCoordinator,
}

impl Publication {
    #[must_use]
    pub fn new(log: u32, coordinator: PublishCoordinator) -> Self {
        Self { log, coordinator }
    }

    /// Hand `audit`, then `rows`, to the publish path.
    pub fn publish<'a>(
        &self,
        audit: impl IntoIterator<Item = &'a AuditEvent>,
        rows: impl IntoIterator<Item = &'a MinedRow>,
    ) {
        let (mut events, mut records) = (
            self.coordinator.audit().clone(),
            self.coordinator.record().clone(),
        );
        for event in audit {
            events.emit(event.clone());
        }
        for row in rows {
            records.emit(self.stamped(row));
        }
    }

    /// Hand everything `kept` holds to the publish path, audit events
    /// first.
    pub fn publish_kept(&self, kept: &Kept) {
        self.publish(kept.audit(), kept.rows());
    }

    /// Publish what the age trigger has made due; `true` once everything
    /// drained is durable.
    #[must_use]
    pub fn flush_aged(&self) -> bool {
        self.coordinator
            .write_ordered(self.coordinator.drain_aged(), "age")
    }

    /// Publish everything buffered; `true` once it is all durable.
    #[must_use]
    pub fn flush_all(&self) -> bool {
        self.coordinator
            .write_ordered(self.coordinator.drain_all(), "barrier")
    }

    /// `row`'s record, carrying its position in this group's log.
    fn stamped(&self, row: &MinedRow) -> MinedRecord {
        MinedRecord {
            ingest_position: Some(IngestPosition {
                group: self.log,
                seq: row.position.seq,
                index: row.position.index,
            }),
            ..row.record.clone()
        }
    }
}

#[cfg(test)]
mod tests;
