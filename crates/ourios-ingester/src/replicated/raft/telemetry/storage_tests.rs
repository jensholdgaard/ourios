//! The storage writers' sync and stop signals.

use std::io;
use std::path::Path;
use std::sync::Arc;

use openraft::EntryPayload;
use openraft::storage::{IOFlushed, RaftLogStorage};
use openraft::type_config::TypeConfigExt;
use ourios_semconv as semconv;
use ourios_wal::TenantBatch;

use super::tests::Reader;
use super::{Component, Meters, error_type};
use crate::replicated::raft::cluster::wal_config;
use crate::replicated::raft::codec::raft_id;
use crate::replicated::raft::gate::SyncGate;
use crate::replicated::raft::log_store::{Disk, GroupLog, group_dir};
use crate::replicated::raft::types::{Entry, NodeId, RaftTypes, TenantBatchBytes, Vote};
use crate::replicated::raft::vote_log::{self, VoteLog};

const SYNCS: &str = semconv::OURIOS_WAL_REPLICATION_SYNC_DURATION;
const STOPS: &str = semconv::OURIOS_WAL_REPLICATION_STOPS;

fn point<const N: usize>(values: [&str; N]) -> (Vec<String>, u64) {
    (values.map(str::to_owned).to_vec(), 1)
}

fn entry(index: u64) -> Entry {
    let batch = TenantBatch::encode("acme", b"record").expect("batch");
    Entry {
        log_id: raft_id(ourios_wal::RaftLogId { term: 1, index }),
        payload: EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid").into()),
    }
}

async fn append(log: &mut GroupLog, index: u64) -> io::Result<()> {
    let (tx, rx) = RaftTypes::oneshot();
    log.append([entry(index)], IOFlushed::<RaftTypes>::signal(tx))
        .await?;
    rx.await.map_err(|_| io::Error::other("callback dropped"))?
}

/// A vote log on `dir` that records to `reader` through `gate`.
fn votes(dir: &Path, rewrite_bytes: u64, reader: &Reader) -> (VoteLog, Arc<SyncGate>) {
    vote_log::create(dir).expect("create");
    let gate = Arc::new(SyncGate::default());
    let storage = Meters::on(&reader.meter()).storage();
    let log = VoteLog::open_metered(dir, rewrite_bytes, Arc::clone(&gate), storage).expect("open");
    (log, gate)
}

fn vote(term: u64) -> Vote {
    Vote::new(term, NodeId::from(7))
}

/// Each event `run` logs on this thread: its name and its fields but the
/// message, as `key=value`, in key order.
fn events(run: impl FnOnce()) -> Vec<(String, Vec<String>)> {
    use std::sync::{Mutex, PoisonError};
    use tracing_subscriber::prelude::*;

    type Captured = Arc<Mutex<Vec<(String, Vec<String>)>>>;
    struct Capture(Captured);

    #[derive(Default)]
    struct Fields(Vec<String>);

    impl tracing::field::Visit for Fields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push(format!("{}={value}", field.name()));
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() != "message" {
                self.0.push(format!("{}={value:?}", field.name()));
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            fields.0.sort();
            let name = event.metadata().name().to_owned();
            let mut events = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            events.push((name, fields.0));
        }
    }

    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&captured)));
    tracing::subscriber::with_default(subscriber, run);
    let events = captured.lock().unwrap_or_else(PoisonError::into_inner);
    events.clone()
}

/// A stop is the registered event, once per stopped writer: with the
/// group for a log store, without one for the node's vote log.
#[test]
fn a_stop_is_one_registered_event_per_writer() {
    let reader = Reader::new();
    let storage = Meters::on(&reader.meter()).storage();
    let logged = events(|| {
        let full = io::Error::new(io::ErrorKind::StorageFull, "disk full");
        storage.stopped(Component::LogStore(3), &[3], &full);
        let other = io::Error::other("fsync failed");
        storage.stopped(Component::VoteLog, &[1, 2], &other);
    });

    let event = |fields: &[&str]| {
        let fields = fields.iter().map(|f| (*f).to_owned()).collect();
        (
            semconv::EVENT_OURIOS_WAL_REPLICATION_STOPPED.to_owned(),
            fields,
        )
    };
    let expected = [
        event(&[
            "error.type=storage_full",
            "ourios.wal.group=3",
            "ourios.wal.replication.component=log_store",
        ]),
        event(&[
            "error.type=other",
            "ourios.wal.replication.component=vote_log",
        ]),
    ];
    assert_eq!(logged, expected);
}

#[test]
fn an_error_kind_is_its_name_in_snake_case() {
    assert_eq!(error_type(io::ErrorKind::StorageFull), "storage_full");
    assert_eq!(
        error_type(io::ErrorKind::PermissionDenied),
        "permission_denied"
    );
    assert_eq!(error_type(io::ErrorKind::Other), "other");
}

/// Every write-and-fsync is timed with its group, the failed one with its
/// `error.type`, and the first failure is one stop: the writer refuses
/// what follows without writing.
#[tokio::test]
async fn a_failed_group_log_fsync_is_timed_once_and_stops_the_group_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    vote_log::create(dir.path()).expect("create");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    let reader = Reader::new();
    let gate = Arc::new(SyncGate::default());
    let config = wal_config(&group_dir(dir.path(), 3));
    let disk = Disk {
        gate: Arc::clone(&gate),
        storage: Meters::on(&reader.meter()).storage(),
    };
    let mut log = GroupLog::open_metered(config, 3, votes, disk).expect("group log");
    append(&mut log, 0).await.expect("fsynced");
    gate.fail();
    assert!(append(&mut log, 1).await.is_err(), "the fsync failed");
    assert!(append(&mut log, 2).await.is_err(), "refused after it");

    let timed = [
        point(["3", "log_store"]),
        point(["other", "3", "log_store"]),
    ];
    assert_eq!(reader.counts(SYNCS), timed);
    let stops = [point(["other", "3", "log_store"])];
    assert_eq!(reader.points(STOPS), stops);
}

/// The vote log is the node's: its failure stops every group it hosts.
#[tokio::test]
async fn a_failed_vote_log_fsync_stops_every_group_it_hosts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = Reader::new();
    let (votes, gate) = votes(dir.path(), u64::MAX, &reader);
    votes.host(1);
    votes.host(2);
    votes.save(1, vote(1)).await.expect("saved");
    gate.fail();
    assert!(votes.save(2, vote(1)).await.is_err(), "the fsync failed");
    assert!(votes.save(1, vote(2)).await.is_err(), "refused after it");

    let timed = [point(["other", "vote_log"]), point(["vote_log"])];
    assert_eq!(reader.counts(SYNCS), timed);
    let mut stops = reader.points(STOPS);
    stops.sort();
    let expected = [
        point(["other", "1", "vote_log"]),
        point(["other", "2", "vote_log"]),
    ];
    assert_eq!(stops, expected);
}

/// A group whose log opens over a vote log that has already failed is
/// stopped too: counted once when it registers, however often it does.
#[tokio::test]
async fn a_group_registered_after_the_vote_log_failed_is_counted_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = Reader::new();
    let (votes, gate) = votes(dir.path(), u64::MAX, &reader);
    votes.host(1);
    gate.fail();
    assert!(votes.save(1, vote(1)).await.is_err(), "the fsync failed");
    votes.host(2);
    votes.host(2);
    votes.host(1);
    assert!(votes.save(2, vote(1)).await.is_err(), "refused after it");

    let mut stops = reader.points(STOPS);
    stops.sort();
    let expected = [
        point(["other", "1", "vote_log"]),
        point(["other", "2", "vote_log"]),
    ];
    assert_eq!(stops, expected);
}

/// A failed rewrite latches the vote log as a failed write does.
#[tokio::test]
async fn a_failed_vote_log_rewrite_is_timed_and_stops_the_groups() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = Reader::new();
    let (votes, gate) = votes(dir.path(), 0, &reader);
    votes.host(4);
    gate.fail_after(1);
    let saved = votes.save(4, vote(1)).await;
    saved.expect("answered before the rewrite");
    assert!(votes.save(4, vote(2)).await.is_err(), "refused after it");

    let timed = [point(["other", "vote_log"]), point(["vote_log"])];
    assert_eq!(reader.counts(SYNCS), timed);
    assert_eq!(reader.points(STOPS), [point(["other", "4", "vote_log"])]);
}

/// A state machine's stop is the registered event with its group.
#[test]
fn a_state_machine_stop_is_the_registered_event_with_its_group() {
    let reader = Reader::new();
    let storage = Meters::on(&reader.meter()).storage();
    let logged = events(|| {
        let refused = io::Error::new(io::ErrorKind::InvalidData, "refused");
        storage.stopped(Component::StateMachine(4), &[4], &refused);
    });

    let fields = [
        "error.type=invalid_data",
        "ourios.wal.group=4",
        "ourios.wal.replication.component=state_machine",
    ];
    let expected = [(
        semconv::EVENT_OURIOS_WAL_REPLICATION_STOPPED.to_owned(),
        fields.map(str::to_owned).to_vec(),
    )];
    assert_eq!(logged, expected);
}
