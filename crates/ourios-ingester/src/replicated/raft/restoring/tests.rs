use std::collections::BTreeMap;
use std::time::Duration;

use ourios_parquet::{PromotedAttributes, Store};
use tokio::sync::watch;

use super::super::bundle::write::write;
use super::super::formation::{Formation, formed_key};
use super::super::log_ids::gated::{Outage, Point, pausing, with_outage};
use super::super::mining::tests::{
    HeldLog, batch, block, configured, events_of, in_force, logged, miner_of, with_threshold,
};
use super::super::snapshot_record::GroupState;
use super::super::types::{GroupEntry, LogId, NodeId};
use super::{Restoring, Unrestored};
use crate::record_sink::FlushConfig;
use crate::replicated::raft::mining::Captured;

const GROUP: u32 = 3;
const LOG: u32 = 41;
const VOTERS: [&str; 3] = ["n0", "n1", "n2"];

fn entries() -> Vec<GroupEntry> {
    vec![
        configured(0.7),
        block(0, 100),
        batch("acme", &["user 1 logged in", "user 2 logged in"]),
        batch("beta", &["job 7 started"]),
    ]
}

/// The bundle a flusher captured at the last entry, and that entry.
fn captured() -> (Captured, LogId) {
    let entries = entries();
    let mut miner = miner_of(&entries);
    let events = events_of(&entries, 32);
    let cut = events.last().expect("an entry").at;
    for event in events {
        miner.apply(event).expect("mined");
    }
    (miner.capture(LOG).expect("a cut").expect("captured"), cut)
}

/// The record's state at the cut `captured` names.
fn state(captured: &Captured) -> GroupState {
    GroupState {
        flusher: None,
        highest_kind: 0,
        id_block: captured.bundle.ids().latest(),
        miner_config: Some(in_force(with_threshold(0.7))),
    }
}

fn formed(store: &Store) {
    let members: BTreeMap<String, NodeId> = (0..)
        .zip(VOTERS)
        .map(|(n, voter)| (voter.to_owned(), NodeId::from(100 + n)))
        .collect();
    let formation = Formation::new(GROUP, LOG, members, &VOTERS).expect("formation");
    store
        .put_blocking(&formed_key(GROUP), formation.encode().expect("encode"))
        .expect("formed");
}

/// A replica's restore over `store`, which can go out.
fn restoring(store: Store) -> (Restoring<HeldLog>, Outage) {
    let (store, outage) = with_outage(store);
    let restoring = Restoring {
        group: GROUP,
        source: crate::replicated::raft::flushing::Source {
            store,
            voters: VOTERS.map(str::to_owned).to_vec(),
            flush: FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::MAX,
                ceiling_bytes: usize::MAX,
            },
            promoted: PromotedAttributes::default(),
            audit_ceiling: usize::MAX,
        },
        log: HeldLog(
            logged(&entries())
                .into_iter()
                .map(|e| (e.log_id.index, e))
                .collect(),
        ),
        hold: std::sync::Arc::new(watch::channel(false).0),
        installed: std::sync::Arc::new(|| None),
    };
    (restoring, outage)
}

/// Run `test` on a runtime, its clock paused if `paused`; the fixtures
/// are built before it, since building them blocks on a runtime of its own.
fn run(paused: bool, test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(paused)
        .build()
        .expect("runtime")
        .block_on(test);
}

/// A store holding the group's formation and the bundle at the cut.
fn written(captured: &Captured) -> Store {
    let store = Store::in_memory();
    formed(&store);
    write(&store, &captured.bundle, &captured.snapshots).expect("written");
    store
}

/// RFC 0065 §6.5: a replica that opens from, or installs, a snapshot
/// restores its miner from the bundle at the snapshot's cut.
#[test]
fn a_replica_restores_its_miner_from_the_bundle_at_the_cut() {
    let (captured, cut) = captured();
    let (restoring, _outage) = restoring(written(&captured));
    run(false, async move {
        let (_stop, mut stopped) = watch::channel(false);

        let miner = restoring
            .restore(cut, state(&captured), &mut stopped)
            .await
            .expect("not stopped")
            .expect("restored");

        assert_eq!(miner.applied_through(), Some(cut.index));
        let again = miner.capture(LOG).expect("a cut").expect("captured");
        assert_eq!(again.bundle, captured.bundle);
    });
}

/// A restore started with the store out, `then` called with the outage
/// and the stop once it has retried for 5 s; what it returned, its
/// miner's last entry, and the cut.
fn through_an_outage(
    then: impl FnOnce(&Outage, &watch::Sender<bool>),
) -> (Option<Result<Option<u64>, Unrestored>>, u64) {
    let (captured, cut) = captured();
    let (restoring, outage) = restoring(written(&captured));
    outage.begin();
    let mut restored = None;
    run(true, async {
        let (stop, mut stopped) = watch::channel(false);
        let restoring = tokio::spawn(async move {
            restoring
                .restore(cut, state(&captured), &mut stopped)
                .await
                .map(|restored| restored.map(|miner| miner.applied_through()))
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!restoring.is_finished(), "it waits out the outage");
        then(&outage, &stop);
        restored = restoring.await.expect("ran");
    });
    (restored, cut.index)
}

/// RFC 0065 §6.5: a replica that cannot reach the store retries until
/// it restores.
#[test]
fn a_replica_retries_while_the_store_is_out() {
    let (restored, cut) = through_an_outage(|outage, _| outage.end());

    assert_eq!(restored, Some(Ok(Some(cut))));
}

/// A replica waiting out an outage stops when the host says to.
#[test]
fn a_restore_waiting_on_the_store_stops_when_told() {
    let (restored, _) = through_an_outage(|_, stop| {
        stop.send_replace(true);
    });

    assert_eq!(restored, None, "stopped, not restored");
}

/// A restore whose read of the store hangs still stops when the host says
/// to, rather than holding the host's shutdown until the read returns.
#[test]
fn a_restore_with_a_stalled_read_stops_when_told() {
    let (captured, cut) = captured();
    let (store, paused) = pausing(written(&captured), Point::AfterFormedRead);
    let (restoring, _outage) = restoring(store);
    let mut stopped_restore = false;
    run(false, async {
        let (stop, mut stopped) = watch::channel(false);
        let restoring = tokio::spawn(async move {
            restoring
                .restore(cut, state(&captured), &mut stopped)
                .await
                .is_none()
        });
        paused.reached.await.expect("the read is held");
        stop.send_replace(true);
        let returned = tokio::time::timeout(Duration::from_secs(5), restoring).await;
        stopped_restore = matches!(returned, Ok(Ok(true)));
        // A runtime waits for its blocking tasks as it shuts down.
        let _ = paused.release.send(());
    });

    assert!(
        stopped_restore,
        "the restore stops without its read returning"
    );
}

/// RFC0065.31: with no bundle at the cut, or no formation, the group
/// fails closed rather than mining from an empty miner.
#[test]
fn a_restore_with_nothing_at_the_cut_is_refused() {
    let (captured, cut) = captured();
    let no_bundle = Store::in_memory();
    formed(&no_bundle);
    let unformed = Store::in_memory();
    write(&unformed, &captured.bundle, &captured.snapshots).expect("written");

    for store in [no_bundle, unformed] {
        let (restoring, _outage) = restoring(store);
        let state = state(&captured);
        let mut refused = None;
        run(false, async {
            let (_stop, mut stopped) = watch::channel(false);
            refused = restoring
                .restore(cut, state, &mut stopped)
                .await
                .expect("not stopped")
                .err();
        });

        assert!(
            matches!(&refused, Some(Unrestored::Refused(_))),
            "{refused:?}"
        );
    }
}
