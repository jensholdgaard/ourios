//! Publication from a group's flusher (RFC 0065 §6.5, §6.7): the rows a
//! tail replica mined, each stamped with its ingest position in the
//! group's log (§6.6), and the audit events its miner emitted for them,
//! handed to the same publish path the local receiver uses after the ack.
//!
//! Audit events are handed over before the rows of the same step, and the
//! [`PublishCoordinator`] writes every drained event durably before any
//! file holding rows drained with it, so a template's events reach the
//! store before the first file whose rows use it. A handoff and a drain's
//! capture exclude each other, as the local path's miner lock makes them.
//! The record sink's size trigger publishes inline, once the audit sink's
//! barrier has made every event emitted so far durable; the age trigger
//! and a full flush are the caller's to drive, and flushes run one at a
//! time.

use std::sync::Mutex;

use ourios_core::audit::{AuditEvent, AuditSink as _};
use ourios_core::record::{IngestPosition, MinedRecord, RecordSink as _};
use ourios_core::sync::lock_recover;

use super::kept::Kept;
use super::mining::MinedRow;
use crate::audit_sink::SharedParquetAuditSink;
use crate::publish::PublishCoordinator;
use crate::record_sink::{ParquetRecordSink, SharedParquetSink};

/// A flusher's publication of one group, whose log id is `log`.
pub struct Publication {
    log: u32,
    coordinator: PublishCoordinator,
    /// Held across a whole handoff and across a drain's capture, so a
    /// drain never takes a step's audit events without its rows; released
    /// before the ordered write's I/O.
    handoff: Mutex<()>,
    /// Held across a whole flush, so a flush starts only once every earlier
    /// one has settled: rows an earlier write failed to publish are back in
    /// the buffers for it to drain, and its success covers them.
    writing: Mutex<()>,
}

impl Publication {
    /// Publish through `records` and `events`. The record sink's
    /// inline triggers are held back until every event emitted so far is
    /// durable, as the local receiver's are.
    #[must_use]
    pub fn new(log: u32, records: ParquetRecordSink, events: SharedParquetAuditSink) -> Self {
        let barrier = events.clone();
        let quarantine = events.clone();
        let records = records
            // Flushing, not `settled`: no encode worker is on this path,
            // and a handoff must not leave rows past the ceiling while its
            // own events wait for a drain the handoff excludes.
            .with_audit_barrier(Box::new(move || barrier.barrier()))
            // RFC 0025 §3.3: a permanently rejected record quarantines to
            // the audit stream rather than wedging its partition.
            .with_audit_sink(Box::new(quarantine));
        let coordinator = PublishCoordinator::new(SharedParquetSink::new(records), events);
        Self {
            log,
            coordinator,
            handoff: Mutex::new(()),
            writing: Mutex::new(()),
        }
    }

    /// Hand `audit`, then `rows`, to the publish path.
    pub fn publish<'a>(
        &self,
        audit: impl IntoIterator<Item = &'a AuditEvent>,
        rows: impl IntoIterator<Item = &'a MinedRow>,
    ) {
        let _handoff = lock_recover(&self.handoff);
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
        let _writing = lock_recover(&self.writing);
        let drained = {
            let _handoff = lock_recover(&self.handoff);
            self.coordinator.drain_aged()
        };
        self.coordinator.write_ordered(drained, "age")
    }

    /// Publish everything buffered; `true` once it is all durable.
    #[must_use]
    pub fn flush_all(&self) -> bool {
        let _writing = lock_recover(&self.writing);
        let drained = {
            let _handoff = lock_recover(&self.handoff);
            self.coordinator.drain_all()
        };
        self.coordinator.write_ordered(drained, "barrier")
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
mod held;
#[cfg(test)]
mod tests;
