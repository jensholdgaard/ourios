use ourios_miner::snapshot::SnapshotState;

use super::super::Captured;
use super::super::tests::{
    HeldLog, batch, block, configured, events_of, in_force, logged, miner_of, with_threshold,
};
use super::super::{GroupMiner, MinedRow};
use super::RestoreError;
use crate::replicated::raft::state_machine::Applied;
use crate::replicated::raft::types::{GroupEntry, LogId};

const LOG: u32 = 3;

/// Two tenants mined under a configuration, with a block in use and one
/// pending: the group at the cut.
fn before_the_cut() -> Vec<GroupEntry> {
    vec![
        configured(0.7),
        block(0, 100),
        batch(
            "acme",
            &["user 1 logged in", "user 2 logged in", "disk sda full"],
        ),
        batch("beta", &["job 7 started", "job 8 started"]),
        block(100, 200),
    ]
}

/// What the group takes after the cut: lines that match the templates
/// mined before it and lines that mint new ones.
fn after_the_cut() -> Vec<GroupEntry> {
    vec![
        batch("acme", &["user 3 logged in", "cache miss on key 9"]),
        batch("beta", &["job 9 started", "queue drained after 4 s"]),
        batch("gamma", &["first line of a new tenant"]),
    ]
}

/// The events of the whole log, split at the cut.
fn events() -> (Vec<Applied>, Vec<Applied>, Vec<GroupEntry>) {
    let mut entries = before_the_cut();
    entries.extend(after_the_cut());
    let mut events = events_of(&entries, 32);
    let after = events.split_off(before_the_cut().len());
    (events, after, entries)
}

fn mined(miner: &mut GroupMiner, events: Vec<Applied>) -> Vec<MinedRow> {
    events
        .into_iter()
        .flat_map(|event| miner.apply(event).expect("mined").rows)
        .collect()
}

/// The states `captured`'s bundle admits, in its tenant order.
fn states(captured: &Captured) -> Vec<SnapshotState> {
    let Captured { bundle, snapshots } = captured;
    bundle
        .tenants()
        .iter()
        .zip(snapshots)
        .map(|(entry, bytes)| bundle.snapshot(entry, bytes).expect("admitted"))
        .collect()
}

fn log_of(entries: &[GroupEntry]) -> HeldLog {
    HeldLog(
        logged(entries)
            .into_iter()
            .map(|e| (e.log_id.index, e))
            .collect(),
    )
}

/// RFC0065.31: a replica restored from the bundle at the cut mints the
/// same next template id and the same rows as one that mined through the
/// cut, for tenants it held and a tenant new after the cut.
#[test]
fn a_restored_miner_mines_what_one_that_mined_through_the_cut_does() {
    let (before, after, entries) = events();
    let cut: LogId = before.last().expect("an entry").at;
    let mut through = miner_of(&entries);
    mined(&mut through, before);
    let captured = through
        .capture(LOG)
        .expect("a cut is allowed")
        .expect("captured");

    let config = in_force(with_threshold(0.7));
    let mut restored = GroupMiner::restored(
        log_of(&entries),
        (&captured.bundle, &states(&captured)),
        (cut, config),
    )
    .expect("restored");

    assert_eq!(restored.applied_through(), Some(cut.index));
    let again = restored.capture(LOG).expect("a cut").expect("captured");
    assert_eq!(again.bundle, captured.bundle, "the same group at the cut");
    assert_eq!(
        mined(&mut restored, after.clone()),
        mined(&mut through, after)
    );
}

/// A bundle restores only with one snapshot per tenant it names.
#[test]
fn a_restore_without_a_snapshot_per_tenant_is_refused() {
    let (before, _, entries) = events();
    let cut = before.last().expect("an entry").at;
    let mut through = miner_of(&entries);
    mined(&mut through, before);
    let captured = through.capture(LOG).expect("a cut").expect("captured");
    let mut short = states(&captured);
    short.pop();

    let refused = GroupMiner::restored(
        log_of(&entries),
        (&captured.bundle, &short),
        (cut, in_force(with_threshold(0.7))),
    );

    assert!(
        matches!(
            refused,
            Err(RestoreError::Snapshots {
                tenants: 2,
                snapshots: 1
            })
        ),
        "{:?}",
        refused.err()
    );
}
