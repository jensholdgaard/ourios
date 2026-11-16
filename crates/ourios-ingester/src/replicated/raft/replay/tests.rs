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

/// An id block, `held` batches of five corpus lines from index 2, then
/// the first configuration and two more batches.
fn configured_late(held: usize) -> Vec<super::super::types::GroupEntry> {
    use super::super::mining::tests::{batch_at, block, configured, corpus, time};
    let lines = corpus();
    let mut chunks = (1..).zip(lines.chunks(5));
    let mut entries = vec![block(0, 10_000)];
    for (n, chunk) in chunks.by_ref().take(held) {
        entries.push(batch_at(time(n), "acme", chunk));
    }
    entries.push(configured(0.7));
    entries.extend(
        chunks
            .take(2)
            .map(|(n, chunk)| batch_at(time(n), "acme", chunk)),
    );
    entries
}

/// RFC 0065 §6.7: a replay bounds each item to one entry's rows, the
/// batches held for a late first configuration included, and the items
/// for each entry join up to what the live miner mined from it.
#[test]
fn a_replay_mines_the_batches_held_for_the_first_configuration_one_at_a_time() {
    let entries = configured_late(6);
    let live = mined_log(&entries);
    let last = live.last().expect("entries").0;
    let replay = replayed(held(&entries), last).expect("replayed");
    for (index, mined) in &replay {
        let mut seqs: Vec<u64> = mined.rows.iter().map(|r| r.position.seq).collect();
        seqs.dedup();
        assert!(seqs.len() <= 1, "item for {index} mined entries {seqs:?}");
    }
    let mut joined: Vec<(u64, Mined)> = Vec::new();
    for (index, Mined { audit, rows }) in replay {
        match joined.last_mut() {
            Some((last, whole)) if *last == index => {
                whole.audit.extend(audit);
                whole.rows.extend(rows);
            }
            _ => joined.push((index, Mined { audit, rows })),
        }
    }
    assert_eq!(joined, live);
    let configured_at = 8;
    assert_eq!(
        live.iter()
            .find(|(index, _)| *index == configured_at)
            .map(|(_, m)| m.rows.len()),
        Some(30),
        "the configuration mined the six held batches"
    );
}
