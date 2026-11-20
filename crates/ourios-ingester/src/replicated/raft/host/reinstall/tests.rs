use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ourios_parquet::{PromotedAttributes, Store};
use tokio::sync::{mpsc, watch};

use super::super::super::bundle::write::write;
use super::super::super::flushing::Source;
use super::super::super::formation::{Formation, formed_key};
use super::super::super::kept::{Bounds, Kept};
use super::super::super::log_ids::gated::{Point, pausing};
use super::super::super::mining::tests::{
    HeldLog, batch, block, configured, events_of, in_force, logged, miner_of, with_threshold,
};
use super::super::super::mining::{Captured, GroupMiner, MinedRow, SharedLog};
use super::super::super::restoring::Restoring;
use super::super::super::snapshot_record::GroupState;
use super::super::super::state_machine::{Applied, AppliedEvent};
use super::super::super::telemetry::{Meters, Storage};
use super::super::super::types::{GroupEntry, LogId, NodeId};
use super::super::{Follow, GroupMined, Keeping, Mining};
use crate::record_sink::FlushConfig;

const LOG: u32 = 41;
const VOTERS: [&str; 3] = ["n0", "n1", "n2"];

/// A configuration, an id block and seven one-line batches, at indexes 1
/// to 9.
fn entries() -> Vec<GroupEntry> {
    let mut entries = vec![configured(0.7), block(0, 10_000)];
    entries.extend((0..7).map(|n| batch("acme", &[&format!("job {n} finished in {n} ms")])));
    entries
}

/// The snapshot record's state at the cut `captured` names.
fn state(captured: &Captured) -> GroupState {
    GroupState {
        id_block: captured.bundle.ids().latest(),
        miner_config: Some(in_force(with_threshold(0.7))),
        ..GroupState::default()
    }
}

/// A store holding group 0's formation and the bundles of `cuts`.
fn store_of(cuts: &[&Captured]) -> Store {
    let store = Store::in_memory();
    let members: BTreeMap<String, NodeId> = (0..)
        .zip(VOTERS)
        .map(|(n, voter)| (voter.to_owned(), NodeId::from(100 + n)))
        .collect();
    let formation = Formation::new(0, LOG, members, &VOTERS).expect("formation");
    store
        .put_blocking(&formed_key(0), formation.encode().expect("encode"))
        .expect("formed");
    for captured in cuts {
        write(&store, &captured.bundle, &captured.snapshots).expect("written");
    }
    store
}

/// A tail replica's follow loop restoring from `store`, over `log`, whose
/// snapshot now is `installed`; where it sends events, and what it mines.
fn following(
    store: Store,
    log: HeldLog,
    installed: (LogId, GroupState),
) -> (
    mpsc::Sender<Applied>,
    mpsc::Receiver<GroupMined>,
    Mining,
    watch::Receiver<bool>,
) {
    let (events, applied) = mpsc::channel(16);
    let (mined, handed) = mpsc::channel(16);
    let meters = Meters::global();
    let kept = Arc::new(Mutex::new(Kept::new(Bounds {
        audit: 1_000,
        rows: 1_000,
    })));
    let hold = Arc::new(watch::channel(false).0);
    let restores = hold.subscribe();
    let restoring = Restoring {
        group: 0,
        source: Source {
            store,
            voters: VOTERS.map(str::to_owned).to_vec(),
            flush: FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::MAX,
                ceiling_bytes: usize::MAX,
            },
            promoted: PromotedAttributes::default(),
            audit_ceiling: usize::MAX,
            #[cfg(feature = "openfga")]
            graph_emitter: None,
        },
        log: Arc::new(log.clone()) as SharedLog,
        hold: Arc::clone(&hold),
        installed: Arc::new(move || Some(installed)),
    };
    let follow = Follow {
        group: 0,
        mined,
        followed: watch::Sender::new(None),
        keeping: Keeping {
            group: 0,
            kept,
            drops: meters.kept(),
            flushing: None,
        },
        sweep: None,
        storage: Storage::global(),
        checkpointer: None,
        reporting: None,
        restoring: Some(restoring),
        lookups: None,
    };
    let mining = Mining::start(follow, GroupMiner::new(log), applied);
    (events, handed, mining, restores)
}

/// The rows `handed` carries, until `rows` of them; fails after 10 s.
async fn rows_of(handed: &mut mpsc::Receiver<GroupMined>, rows: usize) -> Vec<MinedRow> {
    let mut got = Vec::new();
    while got.len() < rows {
        let next = tokio::time::timeout(Duration::from_secs(10), handed.recv()).await;
        let Ok(Some(GroupMined { mined, .. })) = next else {
            panic!("{} of {rows} rows mined: {next:?}", got.len());
        };
        got.extend(mined.rows);
    }
    got
}

/// What a miner that mined straight through `events` did: its bundles at
/// indexes 4 and 6, the rows of indexes 7 and 8, and the rows of index 9.
struct Mined {
    cuts: [(Captured, LogId); 2],
    expected: Vec<MinedRow>,
    last: Vec<MinedRow>,
}

fn mined_through(entries: &[GroupEntry], events: &[Applied]) -> Mined {
    let mut through = miner_of(entries);
    let mut cuts = Vec::new();
    let (mut expected, mut last) = (Vec::new(), Vec::new());
    for (n, event) in events.iter().cloned().enumerate() {
        let mined = through.apply(event).expect("mined");
        match n {
            6 | 7 => expected.extend(mined.rows),
            8 => last = mined.rows,
            _ => {}
        }
        if n == 3 || n == 5 {
            let captured = through.capture(LOG).expect("a cut").expect("captured");
            cuts.push((captured, events[n].at));
        }
    }
    let cuts = <[_; 2]>::try_from(cuts).unwrap_or_else(|_| panic!("two cuts"));
    Mined {
        cuts,
        expected,
        last,
    }
}

/// RFC 0065 §6.5: a later snapshot that purges the entries a restore
/// still has to mine back starts the restore again at that snapshot's
/// cut; and an install the miner has passed is not restored again.
#[test]
fn a_purge_past_the_cut_restores_again_at_the_later_snapshot() {
    let entries = entries();
    let events = events_of(&entries, 32);
    let Mined {
        cuts: [(first, first_at), (later, later_at)],
        expected,
        last,
    } = mined_through(&entries, &events);
    // The first restore's read waits until the replica has applied on.
    let (store, paused) = pausing(store_of(&[&first, &later]), Point::AfterFormedRead);
    // The later snapshot's purge took every entry through it.
    let log = HeldLog(
        logged(&entries)
            .into_iter()
            .filter(|e| e.log_id.index > later_at.index)
            .map(|e| (e.log_id.index, e))
            .collect(),
    );
    let (first_state, later_state) = (state(&first), state(&later));

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let (events_to, mut handed, mining, _) = following(store, log, (later_at, later_state));
            let install = |at, state| Applied {
                at,
                event: AppliedEvent::Installed(state),
            };
            events_to
                .send(install(first_at, first_state))
                .await
                .expect("send");
            for event in &events[4..8] {
                events_to.send(event.clone()).await.expect("send");
            }
            paused
                .reached
                .await
                .expect("the first restore reads the store");
            let drained = async {
                while events_to.capacity() < events_to.max_capacity() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(10), drained)
                .await
                .expect("the replica's events taken while it restores");
            paused.release.send(()).expect("release");

            let mined = rows_of(&mut handed, expected.len()).await;
            assert_eq!(mined, expected, "the entries past the later cut");
            events_to
                .send(install(later_at, later_state))
                .await
                .expect("send");
            events_to.send(events[8].clone()).await.expect("send");
            let next = rows_of(&mut handed, last.len()).await;
            assert_eq!(next, last, "the entry after a passed install, once");
            let more = tokio::time::timeout(Duration::from_millis(500), handed.recv()).await;
            assert!(more.is_err(), "nothing mined again: {more:?}");
            assert!(mining.is_running(), "the miner restored and runs on");
            mining.stop().await;
        });
}

/// A restore can land past events still queued, as when a purge restores
/// again at a later snapshot: those events are passed over, not mined out
/// of order, and the miner mines on from the entry after its cut.
#[test]
fn events_queued_below_a_restored_cut_are_passed_over() {
    let entries = entries();
    let events = events_of(&entries, 32);
    let Mined {
        cuts: [_, (later, later_at)],
        expected,
        ..
    } = mined_through(&entries, &events);
    // The restore's read waits until the test sees its hold.
    let (store, paused) = pausing(store_of(&[&later]), Point::AfterFormedRead);
    let log = HeldLog(
        logged(&entries)
            .into_iter()
            .filter(|e| e.log_id.index > later_at.index)
            .map(|e| (e.log_id.index, e))
            .collect(),
    );
    let later_state = state(&later);

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let (events_to, mut handed, mining, mut held) =
                following(store, log, (later_at, later_state));
            let installed = Applied {
                at: later_at,
                event: AppliedEvent::Installed(later_state),
            };
            events_to.send(installed).await.expect("send");
            // The restore holds the replica's builds until it has caught up.
            paused.reached.await.expect("the restore reads the store");
            assert!(*held.borrow(), "the restore holds the replica's builds");
            paused.release.send(()).expect("release");
            let restored = held.wait_for(|held| !*held);
            tokio::time::timeout(Duration::from_secs(10), restored)
                .await
                .expect("the miner restores")
                .expect("released");
            // Index 5, below the restored cut at 6, then 7 and 8.
            for event in [&events[4], &events[6], &events[7]] {
                events_to.send(event.clone()).await.expect("send");
            }

            let mined = rows_of(&mut handed, expected.len()).await;
            assert_eq!(mined, expected, "the entries past the cut");
            assert!(mining.is_running(), "the miner mines on");
            mining.stop().await;
        });
}

/// Batches, an id block, the first configuration at index 4 holding the
/// batches at 1 and 3, then three batches.
fn configured_at_4_then_more() -> Vec<GroupEntry> {
    let mut entries = vec![
        batch("acme", &["a b c d e"]),
        block(0, 10_000),
        batch("acme", &["a b c d x"]),
        configured(0.7),
    ];
    entries.extend((5..8).map(|n| batch("acme", &[&format!("job {n} finished in {n} ms")])));
    entries
}

/// #1305: a lagging tail miner whose held batches a checkpoint's purge
/// took restores from the snapshot that purged them, as after an install,
/// instead of stopping, and mines on past its cut.
#[test]
fn held_batches_purged_before_they_are_mined_restore_at_the_snapshot() {
    let entries = configured_at_4_then_more();
    let events = events_of(&entries, 32);
    let mut through = miner_of(&entries);
    let (mut cut, mut expected) = (None, Vec::new());
    for (n, event) in events.iter().cloned().enumerate() {
        let mined = through.apply(event).expect("mined");
        match n {
            4 => cut = Some(through.capture(LOG).expect("a cut").expect("captured")),
            5 | 6 => expected.extend(mined.rows),
            _ => {}
        }
    }
    let cut = cut.expect("cut at index 5");
    let cut_at = events[4].at;
    // The snapshot at index 5 purged the log through it, held batches too.
    let log = HeldLog(
        logged(&entries)
            .into_iter()
            .filter(|e| e.log_id.index > cut_at.index)
            .map(|e| (e.log_id.index, e))
            .collect(),
    );
    let store = store_of(&[&cut]);
    let cut_state = state(&cut);

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let (events_to, mut handed, mining, _) = following(store, log, (cut_at, cut_state));
            for event in &events {
                events_to.send(event.clone()).await.expect("send");
            }
            let mined = rows_of(&mut handed, expected.len()).await;
            assert_eq!(mined, expected, "the entries past the snapshot's cut");
            assert!(mining.is_running(), "the miner restored and runs on");
            mining.stop().await;
        });
}
