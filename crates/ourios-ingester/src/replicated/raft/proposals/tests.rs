use std::collections::BTreeMap;

use openraft::EntryPayload;
use openraft::vote::leader_id_std::CommittedLeaderId;
use ourios_wal::{Control, RaftLogId};
use uuid::Uuid;

use super::{View, stale};
use crate::replicated::raft::codec::wal_id;
use crate::replicated::raft::controls::HeldControls;
use crate::replicated::raft::mining::tests::{HeldLog, with_threshold};
use crate::replicated::raft::snapshot_record::{
    GroupMinerConfig, GroupSnapshot, GroupState, SnapshotMeta,
};
use crate::replicated::raft::transport::propose::{ProposedCheckpoint, Report};
use crate::replicated::raft::types::{Entry, GroupEntry, LogId, NodeId};

fn id(term: u64, index: u64) -> LogId {
    LogId {
        leader_id: CommittedLeaderId::new(term),
        index,
    }
}

fn flusher() -> NodeId {
    NodeId::from(Uuid::from_u128(7))
}

const CONFIG: u8 = 0x04;
const ASSIGNMENT: u8 = 0x01;

/// A leader's log of entries 1 to 10 in term 2, the first miner
/// configuration at `configured`, a flusher assignment at 2, and `more`.
struct Log {
    controls: BTreeMap<u64, Control>,
    entries: HeldLog,
}

impl Log {
    fn configured_at(configured: u64) -> Self {
        let mut controls =
            BTreeMap::from([(configured, Control::MinerConfig(with_threshold(0.7)))]);
        controls.insert(2, Control::FlusherAssignment(*flusher().uuid().as_bytes()));
        let entries = (1..=10)
            .map(|index| {
                let payload = match controls.get(&index) {
                    Some(control) => EntryPayload::Normal(GroupEntry::Control(*control)),
                    None => EntryPayload::Blank,
                };
                (
                    index,
                    Entry {
                        log_id: id(2, index),
                        payload,
                    },
                )
            })
            .collect();
        Self {
            controls,
            entries: HeldLog(entries),
        }
    }

    fn with(mut self, index: u64, control: Control) -> Self {
        self.controls.insert(index, control);
        self
    }

    fn held(&self) -> HeldControls {
        HeldControls::new(self.controls.clone(), 0)
    }
}

fn snapshot_at(cut: LogId) -> GroupSnapshot {
    GroupSnapshot {
        meta: SnapshotMeta {
            last_log_id: Some(cut),
            ..SnapshotMeta::default()
        },
        state: GroupState {
            flusher: Some(flusher()),
            highest_kind: CONFIG,
            id_block: None,
            miner_config: Some(GroupMinerConfig::new(with_threshold(0.7)).expect("valid")),
        },
    }
}

fn report(mined: Option<LogId>, cut: Option<(LogId, u8)>) -> Report {
    Report {
        proposer: flusher(),
        mined,
        unstarted: 0,
        checkpoint: cut.map(|(cut, highest_kind)| ProposedCheckpoint { cut, highest_kind }),
    }
}

/// Check `report` against `log`, its snapshot `snapshot` and its commit
/// index `committed`.
fn checked(
    log: &Log,
    snapshot: Option<&GroupSnapshot>,
    committed: u64,
    report: &Report,
) -> Result<(), String> {
    let held = log.held();
    let view = View {
        snapshot,
        held: &held,
        committed: Some(id(2, committed)),
        log: &log.entries,
    };
    stale(report, &view)
}

#[test]
fn a_checkpoint_at_a_committed_entry_past_the_last_cut_is_fresh() {
    let log = Log::configured_at(1);
    let fresh = report(Some(id(2, 10)), Some((id(2, 8), CONFIG)));
    assert_eq!(checked(&log, None, 10, &fresh), Ok(()));
    let bare = report(Some(id(2, 10)), None);
    assert_eq!(checked(&log, None, 10, &bare), Ok(()), "a report alone");
}

/// RFC 0065 §6.8 check 5's report arm: `mined` present and at or above
/// the last applied cut.
#[test]
fn a_report_without_mined_or_below_the_last_cut_is_stale() {
    let log = Log::configured_at(1);
    let snapshot = snapshot_at(id(2, 5));
    assert!(checked(&log, None, 10, &report(None, None)).is_err());
    assert!(checked(&log, Some(&snapshot), 10, &report(Some(id(2, 4)), None)).is_err());
    assert_eq!(
        checked(&log, Some(&snapshot), 10, &report(Some(id(2, 5)), None)),
        Ok(())
    );
}

/// The cut must be an entry the log holds, under its term, at or below
/// the commit index.
#[test]
fn a_cut_not_a_committed_entry_of_the_log_is_stale() {
    let log = Log::configured_at(1);
    let mined = Some(id(2, 10));
    for cut in [id(2, 11), id(3, 8)] {
        assert!(
            checked(&log, None, 10, &report(mined, Some((cut, CONFIG)))).is_err(),
            "{cut}"
        );
    }
    let above_commit = report(mined, Some((id(2, 8), CONFIG)));
    assert!(checked(&log, None, 7, &above_commit).is_err());
}

/// The cut must be above the last applied cut and above every cut a
/// checkpoint entry after it proposes.
#[test]
fn a_cut_not_above_every_earlier_cut_is_stale() {
    let mined = Some(id(2, 10));
    let snapshot = snapshot_at(id(2, 6));
    let log = Log::configured_at(1);
    assert!(
        checked(
            &log,
            Some(&snapshot),
            10,
            &report(mined, Some((id(2, 6), CONFIG)))
        )
        .is_err()
    );
    let proposed = Control::Checkpoint {
        proposer: *flusher().uuid().as_bytes(),
        cut: wal_id(&id(2, 8)),
        highest_kind: CONFIG,
    };
    let log = Log::configured_at(1).with(9, proposed);
    assert!(checked(&log, None, 10, &report(mined, Some((id(2, 8), CONFIG)))).is_err());
    assert_eq!(
        checked(&log, None, 10, &report(mined, Some((id(2, 9), CONFIG)))),
        Ok(())
    );
}

/// The highest kind must be the log's at the cut, and a miner
/// configuration in force there.
#[test]
fn a_cut_of_another_highest_kind_or_without_a_configuration_is_stale() {
    let mined = Some(id(2, 10));
    let log = Log::configured_at(1);
    assert!(checked(&log, None, 10, &report(mined, Some((id(2, 8), ASSIGNMENT)))).is_err());
    let late = Log::configured_at(5);
    assert!(
        checked(
            &late,
            None,
            10,
            &report(mined, Some((id(2, 4), ASSIGNMENT)))
        )
        .is_err(),
        "no configuration in force at the cut"
    );
    assert_eq!(
        checked(&late, None, 10, &report(mined, Some((id(2, 5), CONFIG)))),
        Ok(())
    );
}

/// The `RaftLogId` ordering the cut comparisons rely on is the log id's.
#[test]
fn raft_log_ids_order_as_log_ids() {
    assert!(wal_id(&id(2, 9)) > wal_id(&id(2, 8)));
    assert!(wal_id(&id(3, 1)) > wal_id(&id(2, 8)));
    assert_eq!(wal_id(&id(2, 8)), RaftLogId { term: 2, index: 8 });
}
