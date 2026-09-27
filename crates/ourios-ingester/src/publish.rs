//! Audit-ordered publication coordinator (issue #302 fix #1/#2).
//!
//! The cross-cutting invariant: **a mined record must never become
//! query-visible (published to the data store) before its template's audit
//! event is durable in the audit stream** (`CLAUDE.md` §3.3, strengthened to
//! hold under concurrency and the inline size trigger).
//!
//! The miner emits a line's template audit event *and* its record within one
//! `MinerCluster::ingest` call (audit event first), under the pipeline's miner
//! lock. This coordinator owns clones of both sinks and enforces the invariant
//! on the cadence path via **snapshot-then-ordered-write**:
//!
//! 1. [`PublishCoordinator::drain_aged`] takes both buffers into owned batches —
//!    a cheap memory move, **no I/O**. The caller runs it under the pipeline's
//!    miner lock (`with_miner`), so the drain is atomic w.r.t. `ingest`: every
//!    record in the drained record batch has its template event in the drained
//!    audit batch (or already durable from a prior cadence) — no record/audit
//!    pair can be split across the drain (closes the TOCTOU race, issue #302 #1).
//! 2. [`PublishCoordinator::write_ordered`] runs **off the lock**: it writes the
//!    audit batch to durability *first*, and publishes the record batch to the
//!    data store **only after** the audit write succeeds. A transient audit
//!    failure holds the records (requeued, retried next cadence); a permanent
//!    audit failure drops the audit batch and still publishes the records — the
//!    documented degraded case (those templates render retained/empty).
//!
//! Between those two steps the drained records exist only in memory — out of
//! the buffers, not yet durable — so the snapshot holds the record sink's
//! in-flight publish guard from drain to settled write (issue #578): a
//! rotation/shutdown `wal_high_water` stamp quiesces these publishes
//! (`SharedParquetSink::quiesce_publishes`) before it can claim their WAL
//! frames durably captured.
//!
//! Rotation / shutdown already drain audit-before-record under the miner lock
//! (the receiver's `flush_then_snapshot`), and the inline size/ceiling publish
//! is gated by the record sink's audit barrier (also under the miner lock), so
//! every publication path is audit-ordered.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ourios_core::audit::AuditEvent;
use ourios_core::record::MinedRecord;
use ourios_parquet::PartitionKey;

use crate::audit_sink::{SharedParquetAuditSink, Ticket};
use crate::cadence::Epoch;
use crate::record_sink::{SharedParquetSink, TakenPartitions};

/// An atomic snapshot of both sinks' buffers, taken under the miner lock and
/// written off-lock by [`PublishCoordinator::write_ordered`].
///
/// Carries the record sink's in-flight publish guard (issue #578): from the
/// drain until this snapshot drops — the write completed, the records
/// requeued, or an unwind — its records count as in flight, and every
/// `wal_high_water` stamping path waits them out
/// (`SharedParquetSink::quiesce_publishes`) before it stamps.
#[derive(Debug)]
pub struct Drained {
    audit: Vec<AuditEvent>,
    records: TakenPartitions,
    guard: crate::record_sink::PublishGuard,
    ticket: Ticket,
}

impl Drained {
    /// Whether the snapshot holds nothing to write (both buffers were empty).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.audit.is_empty() && self.records.is_empty()
    }

    /// The cut epoch this publish was registered under (RFC 0052 §3.1).
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.guard.epoch()
    }

    /// The audit position the records depend on — carried by any
    /// `ready` partition the drain took (§3.1).
    #[must_use]
    pub fn audit_watermark(&self) -> u64 {
        self.records.audit_watermark()
    }

    /// The estimated bytes the snapshot holds — the barrier's
    /// coalescing bound (§3.1).
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.records.estimated_bytes()
    }

    /// The record partitions, borrowed.
    #[must_use]
    pub fn partitions(&self) -> &[(PartitionKey, Vec<MinedRecord>)] {
        self.records.partitions()
    }
}

/// What a drain does about an earlier take whose events are not durable
/// yet.
#[derive(Clone, Copy)]
enum Turn {
    Wait,
    Refuse,
}

/// Coordinates audit-ordered publication across the record + audit sinks
/// (issue #302). Cloneable: every clone drives the same two sinks.
#[derive(Clone)]
pub struct PublishCoordinator {
    record: SharedParquetSink,
    audit: SharedParquetAuditSink,
    /// Orders every take out of the two buffers against every park, and
    /// every audit-failure requeue, back into them (RFC 0052 §3.1). Both
    /// run outside the ingest exclusion, so without it a drain could land
    /// between their two requeues — splitting records from their audit
    /// events — or, in the rotation hook, between the drain and the cut's
    /// epoch, where the return would be dated at that cut and refuse
    /// nothing.
    ///
    /// Taken after the ingest exclusion and the miner lock, before the
    /// sinks' own locks, and never held while waiting on anything else.
    handoff: Arc<Mutex<()>>,
    /// The RFC 0047 §3.3 graph emitter — fed with every published batch
    /// (the flush-cadence bridge), when the graph is configured.
    #[cfg(feature = "openfga")]
    graph: Option<std::sync::Arc<crate::graph_emitter::GraphEmitter>>,
}

impl PublishCoordinator {
    /// Count a cadence sweep step that panicked (#791) — see
    /// [`SharedParquetSink::record_cadence_panic`].
    pub fn record_cadence_panic(&self) {
        self.record.record_cadence_panic();
    }

    /// Build a coordinator over the two shared sinks.
    #[must_use]
    pub fn new(record: SharedParquetSink, audit: SharedParquetAuditSink) -> Self {
        Self {
            record,
            audit,
            handoff: Arc::new(Mutex::new(())),
            #[cfg(feature = "openfga")]
            graph: None,
        }
    }

    /// Feed the RFC 0047 §3.3 graph from every batch this coordinator
    /// publishes: tuples are derived from the records about to be written
    /// and sent — asynchronously, best-effort, idempotent — once the batch
    /// is durable. The compaction sweep re-derives the same tuples later,
    /// so a failed send here only delays visibility.
    #[cfg(feature = "openfga")]
    #[must_use]
    pub fn with_graph_emitter(
        mut self,
        emitter: std::sync::Arc<crate::graph_emitter::GraphEmitter>,
    ) -> Self {
        self.graph = Some(emitter);
        self
    }

    /// Atomically take the audit buffer + the aged record partitions (the
    /// cadence drain). **Must be called under the pipeline's miner lock** so the
    /// drain is atomic w.r.t. `ingest`; it does only cheap memory moves, never
    /// I/O, so the lock is held for microseconds.
    ///
    /// The miner lock is also what makes the returned snapshot's in-flight
    /// guard sound (issue #578): the guard is acquired *before* the take,
    /// under the same lock every `wal_high_water` stamping path holds, so a
    /// concurrent rotation/shutdown either sees these records still buffered
    /// or sees them counted in flight — never neither.
    #[must_use]
    pub fn drain_aged(&self) -> Drained {
        let _handoff = self.lock_handoff();
        let guard = self.record.begin_publish();
        let (audit, ticket) = self.audit.take_ticketed();
        let records = self.record.drain_aged();
        Drained {
            audit,
            records,
            guard,
            ticket,
        }
    }

    /// Atomically take the audit buffer + **all** record partitions (rotation /
    /// shutdown). Same atomicity + in-flight-guard contract as
    /// [`Self::drain_aged`].
    #[must_use]
    pub fn drain_all(&self) -> Drained {
        let _handoff = self.lock_handoff();
        self.take_all()
    }

    /// [`Self::drain_all`], then `then` — with no park able to land
    /// between the two. The rotation hook opens its cut's epoch in
    /// `then`, so a park is dated either before the drain (the cut
    /// carries its records) or after the epoch (its settlement refuses
    /// the cut).
    pub fn drain_all_then<R>(&self, then: impl FnOnce() -> R) -> (Drained, R) {
        let _handoff = self.lock_handoff();
        let drained = self.take_all();
        (drained, then())
    }

    fn take_all(&self) -> Drained {
        let guard = self.record.begin_publish();
        let (audit, ticket) = self.audit.take_ticketed();
        let records = self.record.drain_all();
        Drained {
            audit,
            records,
            guard,
            ticket,
        }
    }

    fn lock_handoff(&self) -> MutexGuard<'_, ()> {
        self.handoff.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// RFC 0052 §3.1's **park**: put a drained snapshot back where the
    /// next drain finds it — the records into the sink's buffers as
    /// `ready`, keeping the `audit_watermark` that makes them
    /// publishable, and the events ahead of whatever the audit sink
    /// buffered meanwhile.
    ///
    /// A park is a settlement with a date on it, exactly like a requeue:
    /// without that, parking would be the one way out of §3.1's
    /// predicate — a pre-cut guard could park its partition after a cut
    /// had already drained the buffers, settle successfully, and the
    /// barrier would checkpoint over records that exist only in buffers
    /// it no longer holds.
    ///
    /// Both requeues and the date are one step against every drain —
    /// see `handoff`.
    pub fn park(&self, drained: Drained) {
        let _handoff = self.lock_handoff();
        let watermark = drained.audit_watermark();
        let Drained {
            audit,
            records,
            guard,
            ticket,
        } = drained;
        let registered = guard.epoch();
        self.audit.requeue(audit);
        self.record
            .park_ready(records.into_partitions(), watermark, registered);
        drop(ticket);
        // Dropped last, and deliberately: a `quiesce_publishes` that saw
        // the count reach zero before the records were back would see
        // them in neither the buffers nor the store, and stamp across
        // them.
        drop(guard);
    }

    /// Write a `drained` snapshot to durability **off the lock**, audit-first:
    /// the audit batch is written before any record partition is published, so
    /// a record never reaches the store before its template event is durable.
    ///
    /// A transient audit failure (events retained for retry) holds the records —
    /// they are requeued, not published — and returns `false`. A permanent audit
    /// failure (malformed content dropped, [`crate::audit_sink`]) does not block:
    /// the records publish (degraded — those templates render retained/empty).
    /// Records whose events may sit in an earlier drain that is not durable
    /// are requeued the same way, after this drain's own events are written
    /// (see `audit_sink::Ledger`). Returns whether everything was published (no transient retention on
    /// either sink) — the caller's snapshot-gating signal (no-loss, §3.4).
    ///
    /// Consuming `drained` settles its in-flight publish guard on return
    /// (issue #578) — on the success path once the records are durable, on
    /// the transient-failure path once they are requeued into the buffers,
    /// and on an unwind — which is what releases a stamping path waiting in
    /// `SharedParquetSink::quiesce_publishes`.
    #[must_use]
    pub fn write_ordered(&self, drained: Drained, trigger: &'static str) -> bool {
        self.write(drained, trigger, Turn::Refuse)
    }

    /// [`Self::write_ordered`] for the barrier's own drains: records behind
    /// an earlier take whose events are still being written wait for that
    /// write instead of requeueing. Only the barrier task may wait — see
    /// `audit_sink::Ledger` for why nothing it waits on waits on it.
    #[must_use]
    pub fn write_ordered_in_turn(&self, drained: Drained, trigger: &'static str) -> bool {
        self.write(drained, trigger, Turn::Wait)
    }

    fn write(&self, drained: Drained, trigger: &'static str, turn: Turn) -> bool {
        let Drained {
            audit,
            records,
            guard,
            ticket,
        } = drained;
        let registered = guard.epoch();
        // The guard settles when this returns, whichever arm took it.
        let _guard = guard;
        let retained = self.audit.write_retaining(audit);
        let records = records.into_partitions();
        if !retained.is_empty() {
            // The audit stream didn't fully reach durability (a transient store
            // error). Do NOT publish the records — their template events aren't
            // durable yet. Requeue both for the next cadence (the WAL is the
            // durability of record), as one step against every drain: see
            // `handoff`. The ticket drops after the events are back, which
            // holds every drain taken before the requeue: see `audit_sink::Ledger`.
            let _handoff = self.lock_handoff();
            self.audit.requeue(retained);
            self.record.requeue(records, registered);
            drop(ticket);
            return false;
        }
        let cleared = match turn {
            Turn::Wait => ticket.clear_in_turn(),
            Turn::Refuse => ticket.clear(),
        };
        if !cleared {
            tracing::debug!(
                trigger,
                "publish held: an earlier drain's template events are not durable yet, so the \
                 records are requeued for the drain that carries both"
            );
            let _handoff = self.lock_handoff();
            self.record.requeue(records, registered);
            return false;
        }
        self.publish_and_feed(records, trigger, registered)
    }

    /// Publish `records` and, once they are durable, feed the RFC 0047
    /// §3.3 graph from them.
    fn publish_and_feed(
        &self,
        records: Vec<(PartitionKey, Vec<MinedRecord>)>,
        trigger: &'static str,
        registered: Epoch,
    ) -> bool {
        #[cfg(feature = "openfga")]
        let tuples = self.graph.as_ref().map(|emitter| {
            let mut tuples = std::collections::BTreeSet::new();
            for (partition, records) in &records {
                tuples.extend(emitter.derive(&partition.tenant_id, records));
                tuples.extend(crate::graph_emitter::GraphEmitter::tool_tuples(
                    &partition.tenant_id,
                ));
            }
            tuples
        });
        let published = self.record.publish_owned(records, trigger, registered);
        #[cfg(feature = "openfga")]
        if published
            && let (Some(emitter), Some(tuples)) = (self.graph.clone(), tuples)
            && !tuples.is_empty()
        {
            // Off the publish path: the graph is fed after the batch is
            // durable, and never delays the next flush.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = emitter.emit(&tuples).await {
                        tracing::warn!(
                            error = %e,
                            "graph emit after flush failed; the sweep re-derives these tuples \
                             (RFC 0047 §3.3)"
                        );
                    }
                });
            } else {
                tracing::warn!(
                    "graph emit after flush skipped: no runtime handle; the sweep re-derives \
                     these tuples (RFC 0047 §3.3)"
                );
            }
        }
        published
    }

    /// The record sink handle (for the receiver's existing flush/snapshot paths).
    #[must_use]
    pub fn record(&self) -> &SharedParquetSink {
        &self.record
    }

    /// The audit sink handle.
    #[must_use]
    pub fn audit(&self) -> &SharedParquetAuditSink {
        &self.audit
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    use ourios_core::audit::{
        AuditEvent, AuditPayload, AuditSink, ParamType, TemplateChange, hash_triggering_line,
    };
    use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::Store;

    use super::PublishCoordinator;
    use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
    use crate::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};

    fn never_age() -> FlushConfig {
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::ZERO, // every partition is "aged" → drained
            ceiling_bytes: usize::MAX,
        }
    }

    fn mined(tenant: &str) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new(tenant),
            template_id: 1,
            template_version: 1,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 1_775_127_480_000_000_000,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "1".to_string(),
            }],
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    fn created_event(tenant: &str) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(tenant),
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Template {
                template_id: 1,
                triggering_line_hash: hash_triggering_line(b"user 1 logged in"),
                triggering_line_sample: Some("user 1 logged in".to_owned()),
                change: TemplateChange::Created {
                    new_template: "user <*> logged in".to_owned(),
                },
            },
        }
    }

    fn data_files(root: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "parquet") {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn write_ordered_holds_records_when_audit_write_fails() {
        // The invariant under a transient audit-store failure: the record
        // partition is NOT published even though the record store is healthy,
        // because its template event could not reach durability.
        let tmp = tempfile::TempDir::new().expect("temp");
        let data_root = tmp.path().join("data");
        let audit_root = tmp.path().join("audit");
        std::fs::create_dir_all(&data_root).expect("data root");
        std::fs::create_dir_all(&audit_root).expect("audit root");

        let records = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(&data_root).expect("data store"),
            never_age(),
        ));
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1024,
        ));
        let coord = PublishCoordinator::new(records.clone(), audit.clone());

        // Buffer one record + its template event.
        records.clone().emit(mined("checkout"));
        audit.clone().emit(created_event("checkout"));

        // Sabotage the AUDIT store so its write fails transiently; the data
        // store stays healthy.
        std::fs::remove_dir_all(&audit_root).expect("remove audit dir");
        std::fs::write(&audit_root, b"not a directory").expect("sabotage audit");

        // Drain atomically, then ordered write.
        let drained = coord.drain_aged();
        assert!(!drained.is_empty(), "the snapshot captured both buffers");
        let published = coord.write_ordered(drained, "age");

        assert!(!published, "a transient audit failure holds the records");
        assert!(
            data_files(&data_root).is_empty(),
            "NO record partition was published while the audit event isn't durable (issue #302)",
        );
        assert_eq!(
            records.buffered_records(),
            1,
            "the record is requeued (the WAL is the durability of record)",
        );
        assert_eq!(
            audit.buffered_events(),
            1,
            "the audit event is retained too"
        );
    }

    #[test]
    fn write_ordered_publishes_audit_then_records_when_healthy() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let data_root = tmp.path().join("data");
        let audit_root = tmp.path().join("audit");
        std::fs::create_dir_all(&data_root).expect("data root");
        std::fs::create_dir_all(&audit_root).expect("audit root");

        let records = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(&data_root).expect("data store"),
            never_age(),
        ));
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1024,
        ));
        let coord = PublishCoordinator::new(records.clone(), audit.clone());

        records.clone().emit(mined("checkout"));
        audit.clone().emit(created_event("checkout"));

        let published = coord.write_ordered(coord.drain_aged(), "age");

        assert!(published, "a healthy store publishes everything");
        assert_eq!(records.buffered_records(), 0, "records drained");
        assert_eq!(audit.buffered_events(), 0, "audit drained");
        assert!(
            !data_files(&data_root).is_empty(),
            "the record partition is published once its audit event is durable",
        );
    }

    struct Stores {
        _tmp: tempfile::TempDir,
        data_root: std::path::PathBuf,
        audit_root: std::path::PathBuf,
        records: SharedParquetSink,
        audit: SharedParquetAuditSink,
        coord: PublishCoordinator,
    }

    impl Stores {
        /// Both sinks as the server wires them: the record sink's inline
        /// audit barrier is the audit sink's own.
        fn new(target_bytes: usize) -> Self {
            let tmp = tempfile::TempDir::new().expect("temp");
            let data_root = tmp.path().join("data");
            let audit_root = tmp.path().join("audit");
            std::fs::create_dir_all(&data_root).expect("data root");
            std::fs::create_dir_all(&audit_root).expect("audit root");
            let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
                Store::local(&audit_root).expect("audit store"),
                1024,
            ));
            let barrier_audit = audit.clone();
            let records = SharedParquetSink::new(
                ParquetRecordSink::new(
                    Store::local(&data_root).expect("data store"),
                    FlushConfig {
                        target_bytes,
                        ..never_age()
                    },
                )
                .with_audit_barrier(Box::new(move || barrier_audit.barrier())),
            );
            let coord = PublishCoordinator::new(records.clone(), audit.clone());
            Self {
                _tmp: tmp,
                data_root,
                audit_root,
                records,
                audit,
                coord,
            }
        }

        /// A line that creates the template, then a drain; a second line
        /// of the same template — no event of its own — then a second
        /// drain. The second drain's record depends on the first's event.
        fn two_drains(&self) -> (super::Drained, super::Drained) {
            self.records.clone().emit(mined("checkout"));
            self.audit.clone().emit(created_event("checkout"));
            let first = self.coord.drain_aged();
            self.records.clone().emit(mined("checkout"));
            let second = self.coord.drain_aged();
            (first, second)
        }

        fn break_audit_store(&self) {
            std::fs::remove_dir_all(&self.audit_root).expect("remove audit dir");
            std::fs::write(&self.audit_root, b"not a directory").expect("sabotage audit");
        }

        fn mend_audit_store(&self) {
            std::fs::remove_file(&self.audit_root).expect("remove the sabotage");
            std::fs::create_dir_all(&self.audit_root).expect("audit root");
        }

        /// No record is in the store, and `buffered` are held for a
        /// later drain.
        fn assert_held(&self, buffered: usize) {
            assert!(
                data_files(&self.data_root).is_empty(),
                "no record is published before its template event is durable",
            );
            assert_eq!(self.records.buffered_records(), buffered);
        }

        /// The next drain carries every held record with its event.
        fn assert_next_drain_publishes_everything(&self) {
            assert!(
                self.coord.write_ordered(self.coord.drain_aged(), "age"),
                "the next drain publishes",
            );
            assert_eq!(self.audit.buffered_events(), 0);
            assert_eq!(self.records.buffered_records(), 0);
        }
    }

    /// The age sweep and the barrier drain concurrently: a drain can hold
    /// a template's created event, unwritten, while a later drain holds a
    /// record of that template and nothing else.
    #[test]
    fn a_later_drain_does_not_publish_ahead_of_an_earlier_drains_unwritten_events() {
        let stores = Stores::new(usize::MAX);
        let (first, second) = stores.two_drains();

        assert!(!stores.coord.write_ordered(second, "age"));
        stores.assert_held(1);

        assert!(stores.coord.write_ordered(first, "barrier"));
        stores.assert_next_drain_publishes_everything();
    }

    /// The earlier drain's audit write fails and its events go back to
    /// the buffer — after the later drain was taken without them.
    #[test]
    fn a_later_drain_does_not_publish_over_an_earlier_drains_requeued_events() {
        let stores = Stores::new(usize::MAX);
        let (first, second) = stores.two_drains();
        stores.break_audit_store();
        assert!(!stores.coord.write_ordered(first, "age"));
        stores.mend_audit_store();

        assert!(!stores.coord.write_ordered(second, "barrier"));
        stores.assert_held(2);
        stores.assert_next_drain_publishes_everything();
    }

    /// The size trigger's inline publish runs beside the drains — on an
    /// encode worker, off the miner lock — so its audit barrier must see
    /// events a drain holds, not only the ones still buffered.
    #[test]
    fn an_inline_publish_does_not_run_ahead_of_events_a_drain_holds() {
        let stores = Stores::new(1);
        stores.audit.clone().emit(created_event("checkout"));
        let held = stores.coord.drain_aged();

        stores.records.clone().emit(mined("checkout"));
        stores.assert_held(1);

        assert!(stores.coord.write_ordered(held, "age"));
        stores.assert_next_drain_publishes_everything();
    }
}
