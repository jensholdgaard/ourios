//! The bootstrap lock at a replicated start (RFC 0065 §6.6): only the
//! start whose create of the lock succeeds bootstraps the high-water
//! object, and no start re-creates a deleted lock or object.

use std::time::Duration;

use ourios_parquet::Store;
use tokio::sync::mpsc;

use super::bootstrap::{Evidence, LockStart, RaftState, StartError, Started, Waiting, start};
use super::gated::{pausing_create, with_outage};
use super::{AllocateError, LOG_IDS_KEY, LockUnreadable, LogIds, Refused, allocate};
use crate::template_ids::{self, HIGH_WATER_KEY, ScanOptions, Seated, TemplateIdsError};

const A: &str = "ourios-receiver-0";
const B: &str = "ourios-receiver-1";
const RETRY: Duration = Duration::from_millis(5);
const PATIENCE: Duration = Duration::from_secs(30);

fn lock(bootstrapper: &str, seeded: bool, next: u32) -> LogIds {
    LogIds {
        bootstrapper: bootstrapper.to_owned(),
        seeded,
        next,
    }
}

/// The start of one node.
#[derive(Clone, Copy)]
struct Node<'a> {
    id: &'a str,
    raft_state: RaftState,
    allow_bootstrap: bool,
}

impl<'a> Node<'a> {
    fn new(id: &'a str, raft_state: RaftState) -> Self {
        Self {
            id,
            raft_state,
            allow_bootstrap: false,
        }
    }
}

/// Start `node` on `store`, sending each reason it waits under to
/// `reports`.
async fn start_reporting(
    store: Store,
    node: Node<'_>,
    reports: Option<mpsc::UnboundedSender<&'static str>>,
) -> Result<Started, StartError> {
    let scan = ScanOptions::default();
    let lock_start = LockStart {
        store: &store,
        node_id: node.id,
        raft_state: node.raft_state,
        allow_bootstrap: node.allow_bootstrap,
        scan: &scan,
        retry: RETRY,
    };
    let mut waiting = |why: &Waiting| {
        if let Some(reports) = &reports {
            let _ = reports.send(why.as_str());
        }
    };
    tokio::time::timeout(PATIENCE, start(&lock_start, &mut waiting))
        .await
        .expect("the start returned")
}

async fn run(store: &Store, node_id: &str, raft_state: RaftState) -> Result<Started, StartError> {
    start_reporting(store.clone(), Node::new(node_id, raft_state), None).await
}

/// A start in its own task, with the reasons it waits under.
fn spawn_start(
    store: &Store,
    node_id: &'static str,
) -> (
    tokio::task::JoinHandle<Result<Started, StartError>>,
    mpsc::UnboundedReceiver<&'static str>,
) {
    let (reports, reported) = mpsc::unbounded_channel();
    let store = store.clone();
    let task = tokio::spawn(async move {
        start_reporting(store, Node::new(node_id, RaftState::Empty), Some(reports)).await
    });
    (task, reported)
}

async fn next_report(reported: &mut mpsc::UnboundedReceiver<&'static str>) -> &'static str {
    tokio::time::timeout(PATIENCE, reported.recv())
        .await
        .expect("the start reported why it waits")
        .expect("the start is still running")
}

async fn put_lock(store: &Store, lock: &LogIds) {
    store
        .put(LOG_IDS_KEY, lock.encode().expect("encode"))
        .await
        .expect("put the lock");
}

async fn stored_lock(store: &Store) -> Option<Vec<u8>> {
    match store.get(LOG_IDS_KEY).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.is_not_found() => None,
        Err(e) => panic!("read the lock: {e}"),
    }
}

async fn lock_of(store: &Store) -> Option<LogIds> {
    stored_lock(store)
        .await
        .map(|bytes| LogIds::decode(&bytes).expect("a readable lock"))
}

async fn put_high_water(store: &Store, reserved_through: u64) {
    store
        .put(HIGH_WATER_KEY, template_ids::encode(reserved_through))
        .await
        .expect("put the high-water");
}

async fn high_water(store: &Store) -> Option<u64> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || template_ids::read(&store))
        .await
        .expect("joined")
        .expect("a readable high-water")
        .map(|read| read.reserved_through)
}

/// The lock and the high-water as the store holds them.
async fn root_of(store: &Store) -> (Option<LogIds>, Option<u64>) {
    (lock_of(store).await, high_water(store).await)
}

fn bootstrapped(high_water: u64) -> Started {
    Started::Bootstrapped(Seated {
        high_water,
        bootstrapped: true,
    })
}

#[track_caller]
fn assert_high_water_deleted(outcome: &Result<Started, StartError>) {
    assert!(
        matches!(
            outcome,
            Err(StartError::HighWater(TemplateIdsError::HighWaterDeleted))
        ),
        "{outcome:?}"
    );
}

/// On a new store the start creates the lock, bootstraps the object at
/// RFC 0059 §3.5's floor and marks it `seeded` before any log id is
/// allocated.
#[tokio::test]
async fn a_fresh_start_bootstraps_the_object_and_seeds_it_before_any_log_id() {
    let store = Store::in_memory();
    assert_eq!(
        run(&store, A, RaftState::Empty).await.ok(),
        Some(bootstrapped(0))
    );
    assert_eq!(root_of(&store).await, (Some(lock(A, true, 0)), Some(0)));
    assert_eq!(allocate(&store).await.ok(), Some(0));
}

/// Of two starts racing on a new store only the one whose create of the
/// lock succeeds bootstraps; the other passes once it is seeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_racing_starts_only_the_lock_holder_bootstraps() {
    let store = Store::in_memory();
    let (a, _) = spawn_start(&store, A);
    let (b, _) = spawn_start(&store, B);
    let a = a.await.expect("joined").expect("A started");
    let b = b.await.expect("joined").expect("B started");
    let holder = lock_of(&store).await.expect("the lock exists");
    let (won, lost) = match holder.bootstrapper.as_str() {
        A => (a, b),
        B => (b, a),
        other => panic!("the lock names {other}"),
    };
    assert_eq!((won, lost), (bootstrapped(0), Started::Seeded));
    assert_eq!(root_of(&store).await, (Some(holder.clone()), Some(0)));
    assert!(holder.seeded, "{holder:?}");
}

/// Issue #1241: a start paused before its create of the lock, resumed
/// after the other has seeded, allocated log ids and reserved a block and
/// the object was deleted, does not re-create the object.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_paused_before_its_create_never_re_creates_a_deleted_object() {
    let store = Store::in_memory();
    let (paused_store, paused) = pausing_create(store.clone());
    let (resumed, _) = spawn_start(&paused_store, B);
    tokio::time::timeout(PATIENCE, paused.reached)
        .await
        .expect("B reached its create")
        .expect("B's create is held");

    assert_eq!(
        run(&store, A, RaftState::Empty).await.ok(),
        Some(bootstrapped(0))
    );
    assert_eq!(allocate(&store).await.ok(), Some(0));
    assert_eq!(allocate(&store).await.ok(), Some(1));
    let reserving = store.clone();
    let block = tokio::task::spawn_blocking(move || template_ids::reserve(&reserving, 0))
        .await
        .expect("joined")
        .expect("a block");
    assert_eq!(block.through(), template_ids::BLOCK);
    store
        .delete(HIGH_WATER_KEY)
        .await
        .expect("delete the object");

    paused.release.send(()).expect("B is held");
    assert_high_water_deleted(&resumed.await.expect("joined"));
    assert_eq!(high_water(&store).await, None, "the object was re-created");
    assert_eq!(lock_of(&store).await, Some(lock(A, true, 2)));
}

/// A bootstrapper killed between the object and `seeded` finishes on its
/// restart, while every other start waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bootstrapper_killed_after_the_object_finishes_on_its_restart_while_others_wait() {
    let store = Store::in_memory();
    put_lock(&store, &lock(A, false, 0)).await;
    put_high_water(&store, 0).await;

    let (waiter, mut reported) = spawn_start(&store, B);
    for _ in 0..2 {
        assert_eq!(next_report(&mut reported).await, "log_ids_unseeded");
    }
    assert!(!waiter.is_finished(), "B passed an unseeded lock");
    assert_eq!(lock_of(&store).await, Some(lock(A, false, 0)));

    let restarted = run(&store, A, RaftState::Empty).await;
    let finished = Started::Bootstrapped(Seated {
        high_water: 0,
        bootstrapped: false,
    });
    assert_eq!(restarted.ok(), Some(finished));
    assert_eq!(root_of(&store).await, (Some(lock(A, true, 0)), Some(0)));
    let outcome = waiter.await.expect("joined");
    assert_eq!(outcome.ok(), Some(Started::Seeded));
}

/// A bootstrapper killed after its create of the lock and before the
/// object repeats both steps on its restart.
#[tokio::test]
async fn a_bootstrapper_killed_before_the_object_repeats_both_steps() {
    let store = Store::in_memory();
    put_lock(&store, &lock(A, false, 0)).await;
    let refused = allocate(&store).await.expect_err("unseeded");
    assert!(
        matches!(refused, AllocateError::Unseeded { .. }),
        "{refused}"
    );
    assert_eq!(
        run(&store, A, RaftState::Empty).await.ok(),
        Some(bootstrapped(0))
    );
    assert_eq!(root_of(&store).await, (Some(lock(A, true, 0)), Some(0)));
}

/// With the lock deleted after log ids were allocated, a start finding
/// the object present, or holding a group's Raft state, fails with
/// `LogIdsDeleted` and re-creates nothing, and no log id is allocated
/// twice.
#[tokio::test]
async fn a_lock_deleted_after_log_ids_were_allocated_is_never_re_created() {
    let store = Store::in_memory();
    assert_eq!(
        run(&store, A, RaftState::Empty).await.ok(),
        Some(bootstrapped(0))
    );
    assert_eq!(allocate(&store).await.ok(), Some(0));
    assert_eq!(allocate(&store).await.ok(), Some(1));
    store.delete(LOG_IDS_KEY).await.expect("delete the lock");

    for node_id in [A, B] {
        let fresh = run(&store, node_id, RaftState::Empty).await;
        assert!(
            matches!(fresh, Err(StartError::LogIdsDeleted(Evidence::HighWater))),
            "{node_id}: {fresh:?}"
        );
        let formed = run(&store, node_id, RaftState::Held).await;
        assert!(
            matches!(formed, Err(StartError::LogIdsDeleted(Evidence::RaftState))),
            "{node_id}: {formed:?}"
        );
    }
    assert_eq!(stored_lock(&store).await, None, "the lock was re-created");
    assert_eq!(high_water(&store).await, Some(0));
    let refused = allocate(&store).await.expect_err("deleted");
    assert!(
        matches!(refused, AllocateError::Refused(Refused::Deleted)),
        "{refused}"
    );
}

/// A later-version high-water is an object too: a lock absent beside it
/// was deleted.
#[tokio::test]
async fn a_lock_absent_beside_a_later_version_object_was_deleted() {
    let store = Store::in_memory();
    store
        .put("miner/template_ids.v2.json", b"{}".to_vec())
        .await
        .expect("put v2");
    let outcome = run(&store, A, RaftState::Empty).await;
    assert!(
        matches!(outcome, Err(StartError::LogIdsDeleted(Evidence::HighWater))),
        "{outcome:?}"
    );
    assert_eq!(stored_lock(&store).await, None, "the lock was created");
}

/// A seeded lock beside a deleted object fails every start closed with
/// `HighWaterDeleted`, the bootstrapper's included, and nothing is
/// re-created.
#[tokio::test]
async fn a_seeded_lock_beside_a_deleted_object_fails_every_start_closed() {
    let store = Store::in_memory();
    assert_eq!(
        run(&store, A, RaftState::Empty).await.ok(),
        Some(bootstrapped(0))
    );
    assert_eq!(allocate(&store).await.ok(), Some(0));
    store
        .delete(HIGH_WATER_KEY)
        .await
        .expect("delete the object");
    for node_id in [A, B] {
        assert_high_water_deleted(&run(&store, node_id, RaftState::Empty).await);
    }
    assert_eq!(high_water(&store).await, None, "the object was re-created");
    assert_eq!(lock_of(&store).await, Some(lock(A, true, 1)));
}

/// A seeded lock beside the object passes every start without a write.
#[tokio::test]
async fn a_seeded_lock_beside_the_object_passes_without_a_write() {
    let store = Store::in_memory();
    put_lock(&store, &lock(A, true, 3)).await;
    put_high_water(&store, 4_000).await;
    for node_id in [A, B] {
        let outcome = run(&store, node_id, RaftState::Empty).await;
        assert_eq!(outcome.ok(), Some(Started::Seeded), "{node_id}");
    }
    assert_eq!(lock_of(&store).await, Some(lock(A, true, 3)));
    assert_eq!(high_water(&store).await, Some(4_000));
}

const UNREADABLE: &[u8] = br#"{"format":2,"next":0}"#;

/// An unreadable lock fails a start that holds no group's Raft state, and
/// is kept.
#[tokio::test]
async fn an_unreadable_lock_fails_a_start_without_raft_state_and_is_kept() {
    let store = Store::in_memory();
    store
        .put(LOG_IDS_KEY, UNREADABLE.to_vec())
        .await
        .expect("put");
    let outcome = run(&store, A, RaftState::Empty).await;
    assert!(
        matches!(
            outcome,
            Err(StartError::Unreadable(LockUnreadable::Format(2)))
        ),
        "{outcome:?}"
    );
    assert_eq!(stored_lock(&store).await, Some(UNREADABLE.to_vec()));
    assert_eq!(high_water(&store).await, None, "the object was created");
}

/// A replica holding a group's Raft state opens its groups with the lock
/// unreadable or the store unreachable.
#[tokio::test]
async fn a_replica_holding_raft_state_opens_past_an_unreadable_lock_or_store() {
    let store = Store::in_memory();
    store
        .put(LOG_IDS_KEY, UNREADABLE.to_vec())
        .await
        .expect("put");
    let unreadable = run(&store, A, RaftState::Held).await;
    assert_eq!(unreadable.ok(), Some(Started::FromRaftState));
    assert_eq!(stored_lock(&store).await, Some(UNREADABLE.to_vec()));

    let (down, outage) = with_outage(store.clone());
    outage.begin();
    let unreachable = run(&down, A, RaftState::Held).await;
    assert_eq!(unreachable.ok(), Some(Started::FromRaftState));
}

/// A start holding no Raft state waits out an unreachable store, reporting
/// why, and bootstraps once it returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_without_raft_state_waits_out_an_unreachable_store() {
    let store = Store::in_memory();
    let (down, outage) = with_outage(store.clone());
    outage.begin();
    let (started, mut reported) = spawn_start(&down, A);
    assert_eq!(next_report(&mut reported).await, "log_ids_store");
    assert_eq!(stored_lock(&store).await, None);
    outage.end();
    let outcome = started.await.expect("joined");
    assert_eq!(outcome.ok(), Some(bootstrapped(0)));
    assert_eq!(lock_of(&store).await, Some(lock(A, true, 0)));
}

/// RFC 0059 §3.5's table holds under the lock: on a store that holds data
/// the lock holder bootstraps nothing, and seeds nothing, until it is
/// authorised.
#[tokio::test]
async fn an_unauthorised_lock_holder_on_a_store_with_data_seeds_nothing() {
    let store = Store::in_memory();
    store
        .put("data/tenant_id=t/notes.txt", b"not a parquet file".to_vec())
        .await
        .expect("put data");
    let refused = run(&store, A, RaftState::Empty).await;
    assert!(
        matches!(
            refused,
            Err(StartError::HighWater(
                TemplateIdsError::BootstrapNotAuthorized
            ))
        ),
        "{refused:?}"
    );
    assert_eq!(high_water(&store).await, None);
    assert_eq!(lock_of(&store).await, Some(lock(A, false, 0)));

    let node = Node {
        allow_bootstrap: true,
        ..Node::new(A, RaftState::Empty)
    };
    let authorised = start_reporting(store.clone(), node, None).await;
    assert_eq!(authorised.ok(), Some(bootstrapped(0)));
    assert_eq!(lock_of(&store).await, Some(lock(A, true, 0)));
}

/// A store that cannot compare-and-swap takes no lock.
#[tokio::test]
async fn a_store_without_conditional_updates_takes_no_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::local(dir.path()).expect("local store");
    let outcome = run(&store, A, RaftState::Empty).await;
    assert!(
        matches!(outcome, Err(StartError::NoConditionalUpdate)),
        "{outcome:?}"
    );
    assert_eq!(stored_lock(&store).await, None, "the lock was created");
}
