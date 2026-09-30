//! The node under test and the transitions the legs drive through it.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::receiver::{CommitCoordinator, Journal, ReceiveError};
use ourios_wal::{RotationFault, RotationSite, RotationState, WalOffset};

use crate::rfc0052_barrier_support::{BarrierRig, JournalFaults, RigSpec, wal_config};

pub(crate) const CAP: usize = 128;

pub(crate) fn housekeeper_of(rig: &BarrierRig) -> Arc<Housekeeper> {
    Arc::new(Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        CAP,
    ))
}

pub(crate) async fn tick(housekeeper: &Arc<Housekeeper>) -> HousekeepingTick {
    let housekeeper = Arc::clone(housekeeper);
    tokio::task::spawn_blocking(move || housekeeper.tick())
        .await
        .expect("the tick catches its own unwind")
}

pub(crate) async fn cut(rig: &Arc<BarrierRig>, rotate_when_idle: bool) -> CutOutcome {
    let rig = Arc::clone(rig);
    tokio::task::spawn_blocking(move || rig.barrier.tick(&rig.pipeline, rotate_when_idle))
        .await
        .expect("the barrier tick catches its own unwind")
}

pub(crate) fn rig_with(tmp: &Path, faults: &Arc<JournalFaults>) -> Arc<BarrierRig> {
    Arc::new(BarrierRig::build(
        tmp,
        RigSpec {
            journal_faults: Some(Arc::clone(faults)),
            ..RigSpec::new(wal_config(&tmp.join("wal")))
        },
    ))
}

/// A journal whose rotation state follows a script, one step per
/// append: the append path the coordinator emits the rotation edges on,
/// driven through states a real disk cannot be made to fail into on
/// command.
pub(crate) struct ScriptedRotation {
    script: VecDeque<RotationState>,
    state: RotationState,
    byte: u64,
}

impl Journal for ScriptedRotation {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<WalOffset, ReceiveError> {
        if let Some(next) = self.script.pop_front() {
            self.state = next;
        }
        self.byte += 1;
        Ok(self.offset())
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Ok(self.offset())
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }

    fn rotation_state(&self) -> RotationState {
        self.state.clone()
    }
}

impl ScriptedRotation {
    fn offset(&self) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::nil(),
            byte: self.byte,
        }
    }
}

pub(crate) fn fault(site: RotationSite, attempts: u32) -> RotationFault {
    RotationFault::new(site, &std::io::Error::other("injected"), attempts, 3)
}

/// Retrying, a second failed attempt (no edge), recovered, retrying
/// again, terminal, and appends past terminal (no edge, no leave).
pub(crate) async fn drive_rotation_edges() {
    let script = VecDeque::from([
        RotationState::Retrying(fault(RotationSite::Create, 1)),
        RotationState::Retrying(fault(RotationSite::Create, 2)),
        RotationState::Healthy,
        RotationState::Retrying(fault(RotationSite::Rename, 1)),
        RotationState::Terminal(fault(RotationSite::Rename, 3)),
        RotationState::Terminal(fault(RotationSite::Rename, 3)),
        RotationState::Terminal(fault(RotationSite::Rename, 3)),
    ]);
    let commits = CommitCoordinator::new(
        Box::new(ScriptedRotation {
            script,
            state: RotationState::Healthy,
            byte: 0,
        }),
        Duration::from_millis(5),
        u64::MAX,
    );
    for _ in 0..7 {
        let outcome = commits.commit(b"frame").await;
        assert!(outcome.result.is_ok(), "the scripted journal acks");
    }
}

/// Pinned (once a checkpoint lets a pass plan, a tenant with frames and
/// no snapshot), lifted (a cut installs its snapshot), and the latch set
/// — each followed by a tick that must not repeat the edge.
pub(crate) async fn drive_floor_and_latch_edges(
    rig: &Arc<BarrierRig>,
    housekeeper: &Arc<Housekeeper>,
) {
    rig.ingest("checkout", &["user 1 logged in"]).await;
    assert_eq!(cut(rig, false).await, CutOutcome::Stamped);
    rig.ingest("billing", &["invoice 7 sent"]).await;
    tick(housekeeper).await;
    tick(housekeeper).await;
    assert_eq!(cut(rig, false).await, CutOutcome::Stamped);
    tick(housekeeper).await;
    tick(housekeeper).await;
    rig.epochs.report(rig.epochs.current());
    tick(housekeeper).await;
    tick(housekeeper).await;
}
