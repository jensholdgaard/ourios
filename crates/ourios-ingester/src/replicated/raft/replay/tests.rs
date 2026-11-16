use super::super::kept::tests::{entries, mined_log};
use super::super::mining::tests::{HeldLog, logged};
use super::*;

/// A log held in memory with `entries` from index 1.
fn held(entries: &[super::super::types::GroupEntry]) -> HeldLog {
    HeldLog(
        logged(entries)
            .into_iter()
            .map(|e| (e.log_id.index, e))
            .collect(),
    )
}

fn replayed(log: HeldLog, through: u64) -> Result<Vec<(u64, Mined)>, ReplayError> {
    Replayed::new(log, 1, through).collect()
}

/// RFC 0065 §6.7, RFC0065.20: replaying the log from its first entry
/// yields, entry for entry, what the live miner mined: the same rows at
/// the same positions and the same audit events, timestamps included.
#[test]
fn a_replay_from_the_first_entry_mines_what_the_live_miner_did() {
    let entries = entries(12);
    let live = mined_log(&entries);
    let last = live.last().expect("entries").0;
    let replay = replayed(held(&entries), last).expect("replayed");
    assert_eq!(replay, live);
    assert!(
        replay.iter().any(|(_, m)| !m.audit.is_empty()),
        "audit events replayed"
    );
}

/// A replay stops after the entry it was asked to reach.
#[test]
fn a_replay_ends_at_its_last_entry() {
    let entries = entries(12);
    let live = mined_log(&entries);
    let replay = replayed(held(&entries), 6).expect("replayed");
    let through: Vec<_> = live.into_iter().filter(|(index, _)| *index <= 6).collect();
    assert_eq!(replay, through);
}

/// An entry the log no longer holds ends the replay with an error naming
/// it, rather than skipping it.
#[test]
fn a_purged_entry_ends_the_replay() {
    let entries = entries(12);
    let mut log = held(&entries);
    log.0.remove(&5);
    match replayed(log, 12) {
        Err(ReplayError::Purged { index }) => assert_eq!(index, 5),
        other => panic!("expected the purged entry: {other:?}"),
    }
}
