use ourios_core::tenant::TenantId;
use ourios_miner::snapshot::{LogMark, WalHighWater, load_snapshot};

use super::super::tests::{
    batch, block, configured, configured_at_4, events_of, framed, in_force, miner_of,
    with_threshold,
};
use super::super::{GroupMiner, GroupMinerError};
use crate::replicated::raft::bundle::{Cut, CutId};
use crate::replicated::raft::snapshot_record::IdBlock;
use crate::replicated::raft::types::GroupEntry;

const LOG: u32 = 3;
/// The term the mining tests' log ids carry.
const TERM: u64 = 2;

fn id_block(after: u64, through: u64) -> IdBlock {
    IdBlock::new(after, through).expect("test block")
}

/// A miner that applied every one of `entries`.
fn applied(entries: &[GroupEntry]) -> GroupMiner {
    let mut miner = miner_of(entries);
    for event in events_of(entries, 16) {
        miner.apply(event).expect("mined");
    }
    miner
}

fn captured(miner: &GroupMiner) -> super::Captured {
    miner
        .capture(LOG)
        .expect("a cut is allowed")
        .expect("captured")
}

/// RFC0065.31: the bundle at the cut holds, for each tenant, a version-3
/// snapshot marked `{log, seq}`, the allocator's range and unused blocks,
/// and the configuration in force, all at the last entry applied.
#[test]
fn a_checkpoint_holds_every_tenant_the_allocator_and_the_configuration_at_its_cut() {
    let entries = vec![
        configured(0.7),
        block(0, 100),
        batch("acme", &["user 1 logged in", "user 2 logged in"]),
        batch("beta", &["disk sda full"]),
        block(100, 200),
    ];
    let miner = applied(&entries);

    let super::Captured { bundle, snapshots } = captured(&miner);

    let at = CutId {
        log: LOG,
        seq: 5,
        term: TERM,
    };
    assert_eq!(bundle.cut(), at);
    assert_eq!(bundle.ids().current(), Some(id_block(0, 100)));
    assert_eq!(bundle.ids().pending(), [id_block(100, 200)]);
    let tenants: Vec<&str> = bundle.tenants().iter().map(|t| t.tenant.as_str()).collect();
    assert_eq!(tenants, ["acme", "beta"]);
    let config = in_force(with_threshold(0.7));
    assert!(bundle.tenants().iter().all(|t| t.config == config));
    let cut = Cut {
        id: at,
        latest_block: Some(id_block(100, 200)),
        config: Some(config),
    };
    assert_eq!(bundle.check(&cut), Ok(()));
    let mut highest = 0;
    for (entry, bytes) in bundle.tenants().iter().zip(&snapshots) {
        let state = bundle
            .snapshot(entry, bytes)
            .expect("a snapshot it restores");
        assert_eq!(
            state.wal_high_water,
            Some(WalHighWater::Replicated(LogMark { log: LOG, seq: 5 }))
        );
        highest = highest.max(state.highest_template_id().expect("a template"));
    }
    assert_eq!(
        bundle.ids().next(),
        highest + 1,
        "the next id after the last minted"
    );
}

/// The configuration a later entry changed to is the one in force at the
/// cut.
#[test]
fn a_reconfigured_group_checkpoints_the_configuration_in_force() {
    let entries = vec![
        configured(0.7),
        block(0, 100),
        batch("acme", &["user 1 logged in"]),
        configured(0.8),
    ];

    let super::Captured { bundle, .. } = captured(&applied(&entries));

    assert_eq!(bundle.tenants()[0].config, in_force(with_threshold(0.8)));
}

/// An allocator that used its block up names it as current with `next`
/// one past its end.
#[test]
fn a_used_up_block_is_current_with_next_past_its_end() {
    let entries = vec![
        configured(0.7),
        block(0, 2),
        batch("acme", &["alpha", "bravo charlie", "delta echo foxtrot"]),
    ];

    let super::Captured { bundle, .. } = captured(&applied(&entries));

    assert_eq!(bundle.ids().current(), Some(id_block(0, 2)));
    assert_eq!(bundle.ids().next(), 3);
    assert!(bundle.ids().pending().is_empty());
}

/// RFC 0065 §6.5: a cut before the group's first configuration is
/// ignored by every replica, so the miner cuts none; nor while the
/// batches held for it are still to be read back, nor before any entry.
#[test]
fn no_cut_before_the_first_configuration_or_while_batches_are_held() {
    let unconfigured = applied(&[block(0, 100), batch("acme", &["a b"])]);
    assert!(unconfigured.capture(LOG).is_none());
    assert!(miner_of(&[]).capture(LOG).is_none(), "nothing applied");

    let entries = configured_at_4();
    let mut miner = miner_of(&entries);
    let events = events_of(&entries, 8);
    let mut cuts = Vec::new();
    for event in events {
        miner.step(event).expect("stepped");
        cuts.push(miner.capture(LOG).is_some());
        while let Some(held) = miner.next_held() {
            held.expect("mined");
            cuts.push(miner.capture(LOG).is_some());
        }
    }
    assert_eq!(cuts, [false, false, false, false, false, false, true]);
}

#[test]
fn a_stopped_miner_cuts_no_checkpoint() {
    let entries = vec![configured(0.7), framed("acme", &[0xff, 0xff])];
    let mut miner = miner_of(&entries);
    let outcomes: Vec<_> = events_of(&entries, 8)
        .into_iter()
        .map(|event| miner.apply(event))
        .collect();
    assert!(matches!(
        outcomes.last(),
        Some(Err(GroupMinerError::Undecodable { .. }))
    ));

    assert!(miner.capture(LOG).is_none());
}

/// Each snapshot is the tenant's own state, as the cluster gives it.
#[test]
fn each_snapshot_is_its_tenants_state_marked_with_the_cut() {
    let entries = vec![
        configured(0.7),
        block(0, 100),
        batch("acme", &["user 1 logged in"]),
    ];
    let miner = applied(&entries);

    let super::Captured { snapshots, .. } = captured(&miner);

    let mut expected = miner
        .cluster()
        .expect("mining")
        .snapshot_state(&TenantId::new("acme"));
    expected.wal_high_water = Some(WalHighWater::Replicated(LogMark { log: LOG, seq: 3 }));
    assert_eq!(load_snapshot(&snapshots[0]).expect("decodes"), expected);
}
