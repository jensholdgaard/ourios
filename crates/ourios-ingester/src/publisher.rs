//! RFC 0052 §3.1's publisher: the one thread that performs the store I/O
//! for partitions an encode worker detached on the size or ceiling
//! trigger, so the worker is never inside a PUT and a barrier's
//! `quiesce_encodes` waits on encodes alone.
//!
//! A worker hands the publisher a [`Detached`] item and moves on. It
//! never blocks on the queue: a bound the worker waited on would put a
//! stuck PUT back in front of the barrier. So a full queue — or a
//! publisher retired by a panic — hands the item back, and the worker
//! **parks** it into the sink's buffers as a `ready` partition, where the
//! next drain or flush takes it. A park is a settlement with a date on
//! it: it records the `barrier_epoch` current when the records came back,
//! so a cut captured before the park — one whose drain could not have
//! seen them — is refused.
//!
//! One publish guard covers every partition a batch detaches. It is
//! created in `submit`, under the ingest exclusion, before any record of
//! the batch can leave the buffers, and shared through a
//! [`BatchCompletion`]: it settles when the batch's encode phase has ended
//! *and* the last of its detached partitions is durable, requeued, parked
//! or unwound, in whichever order they finish.
//!
//! The thread runs on the crate's lane: a panicking publish latches only its own
//! batch's epoch, every item queued behind it is parked, and the next
//! enqueue starts a new publisher.
//!
//! Audit ordering is not re-established here. A partition is detached
//! only after the sink's inline audit barrier found every emitted event
//! durable — in the receiver `SharedParquetAuditSink::settled`, which
//! answers without store I/O, because a barrier holding the ingest
//! exclusion waits on the worker — so the events its records depend on
//! are durable before the item exists, and stay so. The publisher does
//! no audit I/O either: an audit flush here would race a cut's own take
//! of the audit buffer, refuse, and return records the cut is waiting
//! for.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ourios_core::record::MinedRecord;
use ourios_parquet::PartitionKey;

use crate::cadence::Epoch;
use crate::lane::{Lane, Refused, Serve};
use crate::record_sink::{PublishGuard, SharedParquetSink, TakenPartitions};

/// How many detached items may wait for the publisher. Each is at most a
/// size-target partition and sits outside the sink's byte accounting, so
/// the bound is kept small: past it, partitions park back under that
/// accounting instead.
const QUEUE_ITEMS: usize = 16;

/// The publish guard one encode batch shares across every partition it
/// detaches (RFC 0052 §3.1). Its count is the `Arc`'s: the batch holds one
/// reference for its encode phase and each [`Detached`] item one more, so
/// the guard drops — settling the publish — when the last of them does.
pub struct BatchCompletion {
    guard: PublishGuard,
    returns: Returns,
}

impl BatchCompletion {
    /// The epoch the batch was registered under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.guard.epoch()
    }
}

/// Partitions an encode worker detached on the size or ceiling trigger,
/// on their way to the publisher.
///
/// Its destructor is the backstop: an item dropped with its records still
/// in hand parks them before its share of the completion is released, so
/// even an item lost to a path nobody anticipated is data-safe rather
/// than silently settled.
pub struct Detached {
    taken: Option<TakenPartitions>,
    trigger: &'static str,
    completion: Arc<BatchCompletion>,
}

impl Detached {
    /// `taken`, detached by a batch whose publish is `completion`.
    #[must_use]
    pub fn new(
        taken: TakenPartitions,
        trigger: &'static str,
        completion: &Arc<BatchCompletion>,
    ) -> Self {
        Self {
            taken: Some(taken),
            trigger,
            completion: Arc::clone(completion),
        }
    }

    /// The epoch the owning batch was registered under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.completion.epoch()
    }

    /// Put the records back into the buffers as `ready`, keeping their
    /// `audit_watermark`, and date the return. The share of the
    /// completion is released only afterwards, when `self` drops.
    fn park(mut self) {
        self.park_in_place();
    }

    fn park_in_place(&mut self) {
        if let Some(taken) = self.taken.take() {
            let returns = &self.completion.returns;
            let watermark = taken.audit_watermark();
            let _handoff = returns.lock_handoff();
            returns
                .record
                .park_ready(taken.into_partitions(), watermark, self.completion.epoch());
        }
    }

    fn take_partitions(&mut self) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        self.taken
            .take()
            .map(TakenPartitions::into_partitions)
            .unwrap_or_default()
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        if self.taken.is_some() {
            self.park_in_place();
        } else if std::thread::panicking() {
            // The records left with the unwinding write: they are in
            // neither the buffers nor the store. The completion may not
            // drop here — the batch can still hold other shares — so the
            // unwind is reported now, before this share is released.
            self.completion.guard.report_unwound();
        }
    }
}

/// How records that left the buffers go back into them: through the
/// record sink, under the publish coordinator's handoff.
///
/// The handoff orders every return against every drain (see
/// `PublishCoordinator`'s field). The publisher's returns — a failed
/// PUT's requeue, a park — run on their own threads, so without it one
/// could land between a rotation capture's drain and its epoch, dated
/// at the cut it missed, and refuse nothing.
#[derive(Clone)]
pub(crate) struct Returns {
    record: SharedParquetSink,
    handoff: Arc<Mutex<()>>,
}

impl Returns {
    pub(crate) fn new(record: SharedParquetSink, handoff: Arc<Mutex<()>>) -> Self {
        Self { record, handoff }
    }

    fn lock_handoff(&self) -> MutexGuard<'_, ()> {
        self.handoff.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Encode-and-publish for one batch of partitions, feeding the RFC 0047
/// §3.3 graph once they are durable. The one derivation site both the
/// coordinator's ordered writes and the publisher go through.
#[derive(Clone)]
pub(crate) struct Feed {
    returns: Returns,
    #[cfg(feature = "openfga")]
    graph: Option<Arc<crate::graph_emitter::GraphEmitter>>,
}

impl Feed {
    pub(crate) fn new(returns: Returns) -> Self {
        Self {
            returns,
            #[cfg(feature = "openfga")]
            graph: None,
        }
    }

    /// Put `records` back into the buffers, dated against `registered`,
    /// ordered against every drain.
    fn requeue(&self, records: Vec<(PartitionKey, Vec<MinedRecord>)>, registered: Epoch) {
        let _handoff = self.returns.lock_handoff();
        self.returns.record.requeue(records, registered);
    }

    #[cfg(feature = "openfga")]
    pub(crate) fn with_graph_emitter(
        mut self,
        emitter: Arc<crate::graph_emitter::GraphEmitter>,
    ) -> Self {
        self.graph = Some(emitter);
        self
    }

    /// Publish `records` and, once they are durable, feed the RFC 0047
    /// §3.3 graph from them.
    pub(crate) fn publish(
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
        let failed = self.returns.record.publish_unrequeued(records, trigger);
        let published = failed.is_empty();
        if !published {
            self.requeue(failed, registered);
        }
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
}

/// The lane's work: write an item, or park it when the publisher that
/// accepted it has retired.
struct Write {
    feed: Feed,
    /// The runtime the publisher was built on. A bare thread has no
    /// current runtime, and the graph feed spawns onto one, so each write
    /// enters it — without it every detached partition would skip the
    /// RFC 0047 §3.3 feed on exactly the deployments that configure it.
    runtime: Option<tokio::runtime::Handle>,
}

impl Serve<Detached> for Write {
    fn serve(&self, mut item: Detached) {
        let _entered = self.runtime.as_ref().map(tokio::runtime::Handle::enter);
        let records = item.take_partitions();
        let _published = self.feed.publish(records, item.trigger, item.epoch());
    }

    fn salvage(&self, item: Detached) {
        item.park();
    }
}

/// A handle on the publisher thread. Clones share the thread and its
/// queue; the thread is started by the first enqueue, and joined once the
/// last handle drops, after it has written what was queued.
#[derive(Clone)]
pub struct Publisher {
    lane: Arc<Lane<Detached>>,
    record: SharedParquetSink,
    returns: Returns,
}

impl Publisher {
    pub(crate) fn new(feed: Feed) -> Self {
        let record = feed.returns.record.clone();
        let returns = feed.returns.clone();
        let write = Write {
            feed,
            runtime: tokio::runtime::Handle::try_current().ok(),
        };
        Self {
            lane: Arc::new(Lane::new(1, QUEUE_ITEMS, Arc::new(write))),
            record,
            returns,
        }
    }

    /// A publisher over `sink` alone — no graph feed, and a handoff of
    /// its own rather than a coordinator's.
    #[must_use]
    pub fn over(sink: &SharedParquetSink) -> Self {
        Self::new(Feed::new(Returns::new(
            sink.clone(),
            Arc::new(Mutex::new(())),
        )))
    }

    /// Register one encode batch's publish — the guard every partition
    /// it detaches will share. Call under the ingest exclusion, as
    /// `submit` does, so the epoch is ordered against every cut's
    /// capture.
    #[must_use]
    pub fn begin_batch(&self) -> Arc<BatchCompletion> {
        Arc::new(BatchCompletion {
            guard: self.record.begin_publish(),
            returns: self.returns.clone(),
        })
    }

    /// The record sink this publisher writes to.
    #[must_use]
    pub fn record(&self) -> &SharedParquetSink {
        &self.record
    }

    /// Hand `item` to the publisher, never waiting. A full queue, or a
    /// publisher retired by a panic, parks the item instead; the latter
    /// also starts the next publisher, so the enqueue after this one is
    /// written.
    pub fn publish(&self, item: Detached) {
        match self.lane.try_send(item) {
            Ok(()) => {}
            Err(Refused::Full(item) | Refused::Closed(item)) => item.park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ourios_parquet::Store;

    use super::*;
    use crate::record_sink::{FlushConfig, ParquetRecordSink};

    fn sink(root: &std::path::Path) -> SharedParquetSink {
        SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(root).expect("local store"),
            FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        ))
    }

    /// One holder of a batch's completion.
    #[allow(dead_code)] // the payloads are held only to be dropped
    enum Share {
        Encode(Arc<BatchCompletion>),
        Detached(Detached),
    }

    /// Every order of `items`' indices.
    fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for (i, &first) in items.iter().enumerate() {
            let mut rest = items.to_vec();
            rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, first);
                out.push(tail);
            }
        }
        out
    }

    /// RFC 0052 §3.1: one guard covers every partition a batch detaches,
    /// and it settles only when the batch's encode phase *and* the last
    /// of those partitions are done — in whichever order they finish.
    #[test]
    fn a_batch_completion_settles_on_its_last_share_in_any_order() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sink = sink(dir.path());
        for order in permutations(&[0, 1, 2, 3]) {
            let completion = Publisher::over(&sink).begin_batch();
            // Share 0 is the batch's own encode phase; 1..=3 are detached
            // partitions (empty here — the count is what is under test).
            let mut shares: Vec<Option<Share>> = (0..3)
                .map(|_| {
                    Some(Share::Detached(Detached::new(
                        TakenPartitions::default(),
                        "size",
                        &completion,
                    )))
                })
                .collect();
            shares.insert(0, Some(Share::Encode(completion)));
            for (step, &share) in order.iter().enumerate() {
                assert_eq!(
                    sink.publishes_in_flight(),
                    1,
                    "order {order:?}: the guard is held before step {step}",
                );
                shares[share] = None;
            }
            assert_eq!(
                sink.publishes_in_flight(),
                0,
                "order {order:?}: the last share settled the guard",
            );
        }
    }

    /// A publish that unwinds on the item holding its batch's last share
    /// reaches two reports — the item's own, and the guard's as the share
    /// drops on the same unwinding thread. It is one unwind: one
    /// settlement, one bump of the latch's generation.
    #[test]
    fn an_unwind_on_the_last_share_is_reported_once() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sink = sink(dir.path());
        let completion = Publisher::over(&sink).begin_batch();
        let mut item = Detached::new(TakenPartitions::default(), "size", &completion);
        drop(completion);
        let before = sink.epochs().capture().generation();

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _records = item.take_partitions();
            panic!("injected publish panic");
        }));

        assert!(unwound.is_err(), "the publish panicked");
        assert_eq!(
            sink.epochs().capture().generation() - before,
            1,
            "the latch counted one unwind",
        );
        assert_eq!(
            sink.quiesce_publishes().recorded(),
            1,
            "and one settlement records it",
        );
    }
}
