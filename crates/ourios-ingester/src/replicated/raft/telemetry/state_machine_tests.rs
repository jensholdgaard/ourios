//! A group state machine's stop: its first error `openraft` treats as
//! fatal for the group.

use std::io;
use std::path::Path;
use std::sync::Arc;

use openraft::EntryPayload;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder as _, RaftStateMachine as _};
use ourios_semconv as semconv;
use ourios_wal::{Control, MinerConfigV1, RaftLogId, UpstreamTemplates};

use super::Meters;
use super::tests::Reader;
use crate::replicated::raft::codec::raft_id;
use crate::replicated::raft::gate::SyncGate;
use crate::replicated::raft::snapshot_record::{GroupState, SnapshotMeta};
use crate::replicated::raft::state_machine::GroupSm;
use crate::replicated::raft::types::{Entry, GroupEntry, RaftTypes};

const STOPS: &str = semconv::OURIOS_WAL_REPLICATION_STOPS;

/// A body `MinerConfig` refuses: a threshold above 1.
const REFUSED: MinerConfigV1 = MinerConfigV1 {
    similarity_threshold: 1.5,
    similarity_floor: 0.4,
    param_byte_limit: 256,
    prefix_depth: 2,
    max_node_children: 100,
    max_templates: 20_000,
    max_line_tokens: 512,
    upstream_templates: UpstreamTemplates::Ignore,
    upstream_template_byte_limit: 8192,
    upstream_association_limit: 4,
};

/// The state machine of group 5 on `dir`, its stops counted by `reader`.
fn state_machine(dir: &Path, reader: &Reader) -> (GroupSm, Arc<SyncGate>) {
    let gate = Arc::new(SyncGate::default());
    let mut sm = GroupSm::open(dir, Arc::clone(&gate)).expect("open");
    sm.count_stops(5, Meters::on(&reader.meter()).storage());
    (sm, gate)
}

fn entry(
    index: u64,
    payload: EntryPayload<GroupEntry, crate::replicated::raft::types::Membership>,
) -> EntryResponder<RaftTypes> {
    let log_id = raft_id(RaftLogId { term: 1, index });
    (Entry { log_id, payload }, None)
}

fn refused(index: u64) -> EntryResponder<RaftTypes> {
    let control = GroupEntry::Control(Control::MinerConfig(REFUSED));
    entry(index, EntryPayload::Normal(control))
}

async fn apply(sm: &mut GroupSm, entry: EntryResponder<RaftTypes>) -> io::Result<()> {
    sm.apply(futures_util::stream::iter([Ok(entry)])).await
}

/// A configuration every replica refuses fails the apply, which stops the
/// group: counted once, however many applies fail after it.
#[tokio::test]
async fn a_refused_apply_stops_the_state_machine_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = Reader::new();
    let (mut sm, _gate) = state_machine(dir.path(), &reader);
    assert!(apply(&mut sm, refused(1)).await.is_err(), "refused");
    assert!(apply(&mut sm, refused(2)).await.is_err(), "refused again");

    let stops = [(
        vec!["invalid_data".into(), "5".into(), "state_machine".into()],
        1,
    )];
    assert_eq!(reader.points(STOPS), stops);
}

/// A snapshot the state machine cannot record stops the group, whether a
/// build or an install records it: counted once for both.
#[tokio::test]
async fn a_snapshot_it_cannot_record_stops_the_state_machine_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = Reader::new();
    let (mut sm, gate) = state_machine(dir.path(), &reader);
    let blank = entry(1, EntryPayload::Blank);
    apply(&mut sm, blank).await.expect("applied");
    gate.fail();
    let built = sm.get_snapshot_builder().await.build_snapshot().await;
    assert!(built.is_err(), "the build's record failed");
    let meta = SnapshotMeta {
        last_log_id: Some(raft_id(RaftLogId { term: 1, index: 1 })),
        ..SnapshotMeta::default()
    };
    let installed = sm.install_snapshot(&meta, GroupState::default()).await;
    assert!(installed.is_err(), "the install's record failed");

    let stops = reader.points(STOPS);
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].0[1..], ["5", "state_machine"], "{stops:?}");
}

/// Applies that succeed stop nothing.
#[tokio::test]
async fn a_state_machine_that_applies_stops_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = Reader::new();
    let (mut sm, _gate) = state_machine(dir.path(), &reader);
    apply(&mut sm, entry(1, EntryPayload::Blank))
        .await
        .expect("applied");
    sm.get_snapshot_builder()
        .await
        .build_snapshot()
        .await
        .expect("built");

    assert_eq!(reader.points(STOPS), []);
}
