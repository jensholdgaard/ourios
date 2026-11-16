//! A tail replica's part in its group's publication (RFC 0065 §6.5,
//! §6.7). Every tail replica keeps its mined output for a takeover; the
//! one the latest flusher assignment names also publishes it.
//!
//! Named, it reads the group's log id from `formed.json` (§6.1) and
//! publishes everything it keeps, then each later step's output. Without a
//! readable record it publishes nothing and tries again at the next step.
//! An assignment naming another replica stops it at that entry: what it
//! has not published stays kept, as on every tail replica. A replica with
//! a gap publishes nothing; once no gap remains it publishes everything it
//! keeps again, and a row a former publication already wrote repeats only
//! its position, which compaction and queries count once (§6.6).

use std::sync::{Arc, Mutex};

use ourios_core::sync::lock_recover;
use ourios_parquet::{PromotedAttributes, Store};
use ourios_semconv as semconv;

use super::formation::{Formation, formed_key};
use super::kept::Kept;
use super::mining::Mined;
use super::publication::Publication;
use super::types::NodeId;
use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use crate::record_sink::{FlushConfig, ParquetRecordSink};

/// Where and how a group's flusher publishes.
#[derive(Clone)]
pub struct Source {
    pub store: Store,
    /// The group's voters in placement order, which `formed.json` names.
    pub voters: Vec<String>,
    pub flush: FlushConfig,
    pub promoted: PromotedAttributes,
    /// The audit sink's soft ceiling, in events.
    pub audit_ceiling: usize,
}

/// The publication a group's age loop drives, while there is one.
pub type Current = Arc<Mutex<Option<Arc<Publication>>>>;

/// One group's publication on one tail replica.
pub struct Flushing {
    replica: NodeId,
    group: u32,
    source: Source,
    named: bool,
    publication: Option<Arc<Publication>>,
    /// Everything kept has been handed to `publication`.
    caught_up: bool,
    /// Why the log id was last unreadable, as reported; `None` once read.
    held: Option<&'static str>,
    current: Current,
}

impl Flushing {
    #[must_use]
    pub fn new(replica: NodeId, group: u32, source: Source) -> Self {
        Self {
            replica,
            group,
            source,
            named: false,
            publication: None,
            caught_up: false,
            held: None,
            current: Current::default(),
        }
    }

    /// The publication to drive the age trigger on.
    #[must_use]
    pub fn current(&self) -> Current {
        Arc::clone(&self.current)
    }

    /// Why a named replica is not publishing, as the `error.type` it last
    /// reported; `None` when nothing holds it.
    #[cfg(test)]
    fn held(&self) -> Option<&'static str> {
        self.held
    }

    /// One step: `assigned` is the replica a flusher assignment applied at
    /// this entry named, `out` what the entry mined, already in `kept`.
    pub fn step(&mut self, assigned: Option<NodeId>, out: &Mined, kept: &Kept) {
        if let Some(assigned) = assigned {
            self.named = assigned == self.replica;
        }
        if !self.named || kept.gap().is_some() {
            self.hold();
            return;
        }
        let Some(publication) = self.publication() else {
            return;
        };
        if self.caught_up {
            publication.publish(&out.audit, &out.rows);
        } else {
            publication.resume();
            publication.publish_kept(kept);
            self.caught_up = true;
            *lock_recover(&self.current) = Some(publication);
        }
    }

    /// Stop publishing: what is not yet published stays kept, and the
    /// whole of it is published again once this replica may publish. The
    /// publication is kept, discarded, for that: one per group, so its
    /// sinks and their metrics are built once.
    fn hold(&mut self) {
        if self.caught_up
            && let Some(publication) = &self.publication
        {
            publication.discard();
        }
        self.caught_up = false;
        *lock_recover(&self.current) = None;
    }

    /// This group's publication, built once its log id is readable.
    fn publication(&mut self) -> Option<Arc<Publication>> {
        if self.publication.is_none() {
            let log = match self.log_id() {
                Ok(log) => log,
                Err(Unread::Absent) => return None,
                Err(Unread::Failed { error_type, error }) => {
                    self.report_held(error_type, &error);
                    return None;
                }
            };
            self.held = None;
            let Source {
                store,
                flush,
                promoted,
                audit_ceiling,
                ..
            } = self.source.clone();
            let records =
                ParquetRecordSink::new(store.clone(), flush).with_promoted_attributes(promoted);
            let events = SharedParquetAuditSink::new(BufferingAuditSink::new(store, audit_ceiling));
            self.publication = Some(Arc::new(Publication::new(log, records, events)));
        }
        self.publication.clone()
    }

    /// The log id `formed.json` records for this group.
    fn log_id(&self) -> Result<u32, Unread> {
        let bytes = match self.source.store.get_blocking_opt(&formed_key(self.group)) {
            Ok(Some(bytes)) => bytes,
            // Expected until the group forms.
            Ok(None) => return Err(Unread::Absent),
            Err(error) => {
                return Err(Unread::Failed {
                    error_type: "store_read",
                    error: error.to_string(),
                });
            }
        };
        let voters: Vec<&str> = self.source.voters.iter().map(String::as_str).collect();
        Formation::decode(&bytes, self.group, &voters)
            .map(|formation| formation.log())
            .map_err(|refused| Unread::Failed {
                error_type: refused.error_type(),
                error: refused.to_string(),
            })
    }

    /// Report being held for `error_type`, once until the reason changes.
    fn report_held(&mut self, error_type: &'static str, error: &str) {
        if self.held == Some(error_type) {
            return;
        }
        self.held = Some(error_type);
        tracing::warn!(
            name: semconv::EVENT_OURIOS_WAL_REPLICATION_PUBLICATION_HELD,
            {
                { semconv::OURIOS_WAL_GROUP } = i64::from(self.group),
                { crate::metrics::ERROR_TYPE } = error_type,
            },
            "the flusher of group {} publishes nothing until it can read the group's log id: {error}",
            self.group,
        );
    }
}

/// Why the log id was not read.
enum Unread {
    /// No record yet.
    Absent,
    Failed {
        error_type: &'static str,
        error: String,
    },
}

#[cfg(test)]
mod tests;
