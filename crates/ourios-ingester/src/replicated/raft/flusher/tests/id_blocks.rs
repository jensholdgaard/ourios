//! The leader's id blocks on the in-process cluster (RFC 0065 §6.5):
//! reserved in the store before their entry exists, three kept ahead, held
//! back by the kind gate, and never reused across a leader change.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use openraft::ChangeMembers;
use openraft::async_runtime::WatchReceiver as _;
use ourios_parquet::Store;
use ourios_semconv as semconv;
use ourios_wal::Control;
use tokio::sync::{Semaphore, mpsc, oneshot};

use super::{
    PATIENCE, Pass, admit, counted, counted_as, group, heard_from, log,
    membership_change_in_flight, membership_committed, pass, placement, reads_up_to, ready_leader,
    strs,
};
use crate::replicated::raft::cluster::{Cluster, WAIT, name, seated_store, write};
use crate::replicated::raft::flusher::Steward;
use crate::replicated::raft::id_blocks::{self, AHEAD, Ahead, Refusal, Reported, Reports, TopUp};
use crate::replicated::raft::snapshot_record::IdBlock;
use crate::replicated::raft::telemetry::tests::Reader;
use crate::template_ids::{self, BLOCK, HIGH_WATER_KEY};

type BoxStream<T> = futures::stream::BoxStream<'static, T>;
type BoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;

fn steward(ids: Store) -> Steward {
    Steward::with_patience(PATIENCE).reserving(ids)
}

fn block(after: u64) -> IdBlock {
    IdBlock::new(after, after + BLOCK).expect("a block")
}

async fn high_water(store: &Store) -> Option<u64> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || template_ids::read(&store))
        .await
        .expect("join")
        .expect("read")
        .map(|high_water| high_water.reserved_through)
}

/// A fresh group gets one block a pass until it holds three, each above
/// the last, and a pass once they have committed appends none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn a_fresh_group_gets_three_blocks_and_then_none() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let store = seated_store(5_000);
    let mut steward = steward(store.clone());

    for n in 0..u64::from(AHEAD) {
        let topped = pass(&mut steward, &cluster, leader, &[]).await;
        let expected = block(5_000 + n * BLOCK);
        assert_eq!(topped.id_block, Some(TopUp::Appended(expected)));
    }
    let ahead = Ahead::of(group(&cluster, leader));
    assert_eq!(ahead.blocks, AHEAD);
    let again = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(again.id_block, None, "three are ahead");
    assert_eq!(high_water(&store).await, Some(5_000 + 3 * BLOCK));
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    let index = write(cluster.node(leader).raft(0), 1).await;
    cluster.node(follower).applied_through(0, index).await;
    assert_eq!(Ahead::of(group(&cluster, follower)), ahead);
    cluster.shutdown().await;
}

/// The store's high-water covers the block before its entry exists: the
/// reservation is held once its write lands, and the log holds no block
/// until it is let go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn the_block_is_reserved_before_its_entry_exists() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (store, mut reservation) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let watch = async {
        let reserved_through = high_water(&store).await;
        let held = Ahead::of(group(&cluster, leader));
        (reserved_through, held)
    };
    let (topped, (reserved_through, held)) = while_reserved(
        &mut (&mut steward, &cluster, leader),
        passing,
        &mut reservation,
        watch,
    )
    .await;

    assert_eq!(held.blocks, 0, "no entry before the reservation is durable");
    assert_eq!(topped.id_block, Some(TopUp::Appended(block(0))));
    assert!(reserved_through >= Some(BLOCK), "{reserved_through:?}");
    cluster.shutdown().await;
}

/// A reservation that outlives the membership the kind gate admitted it
/// under appends nothing: while it is held, the leader admits a tail member
/// reading below `0x03`, which the gate never asked. The block's ids are
/// only skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn a_reservation_outliving_its_membership_appends_nothing() {
    let cluster = Cluster::start_with_tail(3, 1, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let below = 3;
    reads_up_to(&cluster, below, id_blocks::KIND - 1);
    let (store, mut reservation) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let admit_below = async {
        let learner = cluster.id(below);
        let add = ChangeMembers::AddNodes(BTreeMap::from([(learner, name(learner))]));
        let raft = cluster.node(leader).raft(0);
        raft.change_membership(add, false).await.expect("admitted");
    };
    let (passed, ()) = while_reserved(
        &mut (&mut steward, &cluster, leader),
        passing,
        &mut reservation,
        admit_below,
    )
    .await;

    assert_eq!(passed.id_block, None, "admitted under another membership");
    held_back_after_reserving(&cluster, leader, &store).await;
    cluster.shutdown().await;
}

/// A reservation that outlives the term the kind gate admitted it under
/// appends nothing, even when the same node leads the next term.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn a_reservation_outliving_its_term_appends_nothing() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (store, mut reservation) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let next_term = async {
        let term = cluster
            .node(leader)
            .raft(0)
            .metrics()
            .borrow_watched()
            .current_term;
        lead_a_later_term(&cluster, leader, term).await;
    };
    let (passed, ()) = while_reserved(
        &mut (&mut steward, &cluster, leader),
        passing,
        &mut reservation,
        next_term,
    )
    .await;

    assert_eq!(passed.id_block, None, "admitted under another term");
    held_back_after_reserving(&cluster, leader, &store).await;
    cluster.shutdown().await;
}

/// Depose `leader` and elect it again in a term after `term`: a second
/// node leads while it is cut off, then is cut off itself once `leader`
/// has caught up. The third node never stands, so each election has one
/// candidate, and the second holds `leader`'s log before the cut, since
/// the third refuses its vote to a candidate behind it.
async fn lead_a_later_term(cluster: &Cluster, leader: usize, term: u64) {
    let mut others = (0..3).filter(|&i| i != leader);
    let (interim, third) = (
        others.next().expect("a node"),
        others.next().expect("a node"),
    );
    let logged = cluster
        .node(leader)
        .raft(0)
        .metrics()
        .borrow_watched()
        .last_log_index;
    cluster
        .node(interim)
        .raft(0)
        .wait(WAIT)
        .metrics(|m| m.last_log_index >= logged, "holds the leader's log")
        .await
        .expect("holds the leader's log");
    let elects = |i: usize, enabled: bool| cluster.node(i).raft(0).runtime_config().elect(enabled);
    elects(third, false);
    elects(leader, false);
    cluster.cut(leader, interim);
    cluster.cut(leader, third);
    let interim_id = cluster.id(interim);
    leads(cluster, interim, term).await;

    elects(interim, false);
    elects(leader, true);
    cluster.heal(leader, interim);
    cluster.heal(leader, third);
    let led = cluster
        .node(interim)
        .raft(0)
        .metrics()
        .borrow_watched()
        .last_log_index;
    cluster
        .node(leader)
        .raft(0)
        .wait(WAIT)
        .metrics(
            |m| m.current_leader == Some(interim_id) && m.last_log_index >= led,
            "follows the interim leader",
        )
        .await
        .expect("follows the interim leader");
    cluster.cut(interim, leader);
    cluster.cut(interim, third);
    leads(cluster, leader, term + 1).await;
}

/// Wait for node `i` to lead group 0 in a term after `term`.
async fn leads(cluster: &Cluster, i: usize, term: u64) {
    cluster
        .node(i)
        .raft(0)
        .wait(WAIT)
        .metrics(
            |m| m.current_leader == Some(m.id) && m.current_term > term,
            "leads a later term",
        )
        .await
        .expect("leads a later term");
}

/// The log holds no block, and the store the one reserved and skipped.
async fn held_back_after_reserving(cluster: &Cluster, leader: usize, store: &Store) {
    assert_eq!(Ahead::of(group(cluster, leader)).blocks, 0);
    assert_eq!(high_water(store).await, Some(BLOCK), "reserved, so skipped");
}

/// A reservation that fails appends nothing and says why; so does a store
/// without the high-water, which a reservation never creates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn a_failed_or_absent_reservation_appends_nothing() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let malformed = Store::in_memory();
    malformed
        .put_blocking(HIGH_WATER_KEY, b"not json".to_vec())
        .expect("put");
    let cases = [
        (malformed, Refusal::Failed("malformed")),
        (Store::in_memory(), Refusal::Absent),
    ];
    for (store, refusal) in cases {
        let mut steward = steward(store.clone());
        let refused = pass(&mut steward, &cluster, leader, &[]).await;
        assert_eq!(refused.id_block, Some(TopUp::Refused(refusal)));
        assert_eq!(Ahead::of(group(&cluster, leader)).blocks, 0);
    }
    assert!(
        log(&cluster, leader).controls().highest_kind > id_blocks::KIND,
        "the miner configuration was appended"
    );
    cluster.shutdown().await;
}

/// While a member reads below `0x03` the leader reserves nothing and
/// appends no block; once every member reads it, a block is appended even
/// though the miner configuration, `0x04`, is still held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn the_kind_gate_holds_the_block_back() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let store = seated_store(0);
    let mut steward = steward(store.clone());

    reads_up_to(&cluster, voter, id_blocks::KIND - 1);
    let held = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(held.id_block, None, "a voter reads below the kind");
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);
    assert_eq!(high_water(&store).await, Some(0), "nothing reserved");

    reads_up_to(&cluster, voter, id_blocks::KIND);
    let appended = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(appended.configured, None, "0x04 is still held");
    assert_eq!(appended.id_block, Some(TopUp::Appended(block(0))));
    assert_eq!(
        log(&cluster, leader).controls().highest_kind,
        id_blocks::KIND
    );
    cluster.shutdown().await;
}

/// A leader cut off after appending a block it reserved never commits it,
/// and the next leader reserves above it: its ids are only skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn a_deposed_leaders_block_is_only_skipped_ids() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let first = ready_leader(&cluster).await;
    let store = seated_store(0);
    let mut deposed = steward(store.clone());
    let committed = pass(&mut deposed, &cluster, first, &[]).await;
    assert_eq!(committed.id_block, Some(TopUp::Appended(block(0))));

    for other in (0..3).filter(|&i| i != first) {
        cluster.cut(first, other);
    }
    let cut_off = deposed.pass(group(&cluster, first), super::tail(&[], &cluster));
    let timed_out = tokio::time::timeout(Duration::from_secs(3), cut_off).await;
    assert!(timed_out.is_err(), "no quorum commits the block");
    let uncommitted = Ahead::of(group(&cluster, first));
    assert_eq!(
        uncommitted.latest,
        Some(block(BLOCK)),
        "appended, uncommitted"
    );
    assert_eq!(high_water(&store).await, Some(2 * BLOCK));

    cluster.kill(first).await;
    let second = cluster.leader(0, Some(first)).await;
    write(cluster.node(second).raft(0), 1).await;
    let mut successor = steward(store.clone());
    let next = pass(&mut successor, &cluster, second, &[]).await;
    assert_eq!(next.id_block, Some(TopUp::Appended(block(2 * BLOCK))));
    let ahead = Ahead::of(group(&cluster, second));
    assert_eq!(ahead.blocks, 2, "the deposed leader's block is not there");
    cluster.shutdown().await;
}

/// RFC0065.29: with a membership change in flight the leader appends no
/// new kind, the assignment, the miner configuration nor an id block, and
/// counts each hold; once it commits, all three are appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn a_membership_change_in_flight_holds_back_every_new_kind() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let first = [placed[0].as_str()];
    admit(&cluster, leader, &first).await;
    heard_from(&cluster, leader, &order[..1]).await;
    let second = (cluster.id(order[1]), placed[1].clone());
    let (membership, followers) = membership_change_in_flight(&cluster, leader, second).await;

    let reader = Reader::new();
    let mut steward = counted(&reader).reserving(seated_store(0));
    let held = pass(&mut steward, &cluster, leader, &first).await;
    assert_eq!(
        held,
        Pass::default(),
        "a new kind beside a membership change"
    );
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);
    let holds = reader.points(semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS);
    assert_eq!(holds, [counted_as("membership_change", 3)], "one per kind");

    for &f in &followers {
        cluster.node(f).gate.open();
    }
    membership_committed(membership, &cluster, leader).await;
    let both = [placed[0].as_str(), placed[1].as_str()];
    let appended = pass(&mut steward, &cluster, leader, &both).await;
    assert_eq!(appended.assigned, Some(cluster.id(order[0])));
    assert!(appended.configured.is_some(), "the configuration held back");
    assert!(
        matches!(appended.id_block, Some(TopUp::Appended(_))),
        "the id block held back: {appended:?}"
    );
    cluster.shutdown().await;
}

/// The leader counts each kind its gate holds back in a pass for a member
/// reading below it, the flusher assignment's, the miner configuration's
/// and the id block's, and the assignment once it commits, each by group
/// and reason; a pass that holds back nothing counts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the leader tops id blocks up only on its flusher's reports (RFC 0065 §6.8); see `reported::` for this test with one"]
async fn every_held_kind_and_committed_assignment_is_counted() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let reader = Reader::new();
    let mut steward = counted(&reader).reserving(seated_store(0));
    let holds = semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS;
    let assignments = semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS;

    admit(&cluster, leader, &placed).await;
    heard_from(&cluster, leader, &order).await;
    reads_up_to(&cluster, voter, 0);
    for n in 1..=2 {
        let held = pass(&mut steward, &cluster, leader, &placed).await;
        assert_eq!(held.assigned, None, "a voter reads below the kind");
        let counted = reader.points(holds);
        assert_eq!(counted, [counted_as("member_unconfirmed", 3 * n)]);
    }
    assert!(reader.points(assignments).is_empty(), "nothing committed");

    reads_up_to(&cluster, voter, 1);
    let first = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(first.assigned, Some(cluster.id(order[0])));
    assert_eq!(first.configured, None, "a voter reads below 0x04");
    assert_eq!(first.id_block, None, "a voter reads below 0x03");
    reads_up_to(&cluster, voter, Control::HIGHEST_KIND);
    let configured = pass(&mut steward, &cluster, leader, &placed).await;
    assert!(configured.configured.is_some());
    assert!(matches!(configured.id_block, Some(TopUp::Appended(_))));
    for _ in 1..AHEAD {
        let topped = pass(&mut steward, &cluster, leader, &placed).await;
        assert!(matches!(topped.id_block, Some(TopUp::Appended(_))));
    }
    let kept = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(kept, Pass::default());
    let counted = reader.points(assignments);
    assert_eq!(counted, [counted_as("unassigned", 1)]);
    let counted = reader.points(holds);
    assert_eq!(counted, [counted_as("member_unconfirmed", 8)]);
    cluster.shutdown().await;
}

/// One pass of the steward at the leader, as [`while_reserved`] runs it.
fn passing<'s>(at: &'s mut (&mut Steward, &Cluster, usize)) -> BoxFuture<'s, Pass> {
    let (steward, cluster, leader) = at;
    Box::pin(pass(steward, cluster, *leader, &[]))
}

/// How many passes [`while_reserved`] runs before a held-back gate fails
/// the test, and the pause between them, as the flusher loop's interval
/// would give a member slow to answer `Identify`.
const HELD_PASSES: usize = 5;
const HELD_PAUSE: Duration = Duration::from_millis(200);

/// Run passes until one's reservation lands in a [`pausing`] store, run
/// `meanwhile` while the reservation is held, then let it go. A pass the
/// kind gate holds back never reserves, and the channel stays open while
/// the steward holds the store, so a pass that ends first is retried as
/// the flusher loop would retry it. One that ends first every time fails
/// the test rather than leaving it waiting for a reservation that cannot
/// come.
async fn while_reserved<S, T>(
    steward: &mut S,
    mut pass: impl for<'s> FnMut(&'s mut S) -> BoxFuture<'s, Pass>,
    reservation: &mut Paused,
    meanwhile: impl Future<Output = T>,
) -> (Pass, T) {
    let mut ended = Vec::new();
    while ended.len() < HELD_PASSES {
        let mut running = pass(steward);
        tokio::select! {
            passed = &mut running => {
                assert_eq!(passed.id_block, None, "a block without a reservation");
                ended.push(passed);
                tokio::time::sleep(HELD_PAUSE).await;
            }
            landed = reservation.reserved.recv() => {
                landed.expect("a reservation");
                let held = async {
                    let _release = Release(&reservation.release);
                    meanwhile.await
                };
                return tokio::join!(running, held);
            }
        }
    }
    panic!("the pass ended before reserving on each of {HELD_PASSES} passes: {ended:?}");
}

/// A pass that ends without reserving every time fails within a bound,
/// though the store still holds the reservation channel open.
#[tokio::test]
#[should_panic(expected = "the pass ended before reserving")]
async fn a_pass_ending_before_its_reservation_fails_at_once() {
    fn ended(_: &mut ()) -> BoxFuture<'_, Pass> {
        Box::pin(async { Pass::default() })
    }
    let (_store_holds, reserved) = mpsc::unbounded_channel();
    let mut reservation = Paused {
        reserved,
        release: Arc::new(Semaphore::new(0)),
    };
    let waited = tokio::time::timeout(
        Duration::from_secs(5),
        while_reserved(&mut (), ended, &mut reservation, async {}),
    )
    .await;
    drop(waited);
}

/// A pass held back once and then admitted reserves on the retry, and
/// waits on the release as a [`pausing`] store's write does.
#[tokio::test]
async fn a_held_pass_is_retried_until_it_reserves() {
    type HeldOnce = (usize, mpsc::UnboundedSender<()>, Arc<Semaphore>);
    fn reserving(at: &mut HeldOnce) -> BoxFuture<'_, Pass> {
        let (passes, landed, release) = at;
        *passes += 1;
        let reserves = *passes > 1;
        let (landed, release) = (landed.clone(), Arc::clone(release));
        Box::pin(async move {
            if reserves {
                landed.send(()).expect("open");
                release.acquire().await.expect("open").forget();
            }
            Pass::default()
        })
    }
    let (landed, reserved) = mpsc::unbounded_channel();
    let mut reservation = Paused {
        reserved,
        release: Arc::new(Semaphore::new(0)),
    };
    let mut held_once = (0, landed, Arc::clone(&reservation.release));
    let waited = tokio::time::timeout(
        Duration::from_secs(5),
        while_reserved(&mut held_once, reserving, &mut reservation, async { 7 }),
    )
    .await;
    assert_eq!(waited.expect("reserved on the retry"), (Pass::default(), 7));
    assert_eq!(held_once.0, 2, "held back once, then reserved");
}

/// A panic while the reservation is held still lets the held write
/// return, so the failing test reports instead of waiting on the write.
#[tokio::test]
async fn a_panic_while_held_still_releases_the_write() {
    type Writing = (
        mpsc::UnboundedSender<()>,
        Arc<Semaphore>,
        Option<oneshot::Sender<()>>,
    );
    fn writing(at: &mut Writing) -> BoxFuture<'_, Pass> {
        let (landed, release, returned) = at;
        landed.send(()).expect("open");
        let release = Arc::clone(release);
        let returned = returned.take().expect("one pass");
        let write = tokio::spawn(async move {
            release.acquire().await.expect("open").forget();
            let _ = returned.send(());
        });
        Box::pin(async move {
            write.await.expect("the write");
            Pass::default()
        })
    }
    let (landed, reserved) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let (returned, write_returned) = oneshot::channel();
    let mut writes = (landed, Arc::clone(&release), Some(returned));
    let mut reservation = Paused { reserved, release };
    let failed = tokio::spawn(async move {
        let meanwhile = async { panic!("an election wait timed out") };
        while_reserved(&mut writes, writing, &mut reservation, meanwhile).await
    });
    assert!(failed.await.is_err(), "the panic reaches the test");
    tokio::time::timeout(Duration::from_secs(5), write_returned)
        .await
        .expect("the held write returns")
        .expect("sent");
}

/// Lets a held write return when dropped: once `meanwhile` ends, or
/// while a panic in it unwinds, so the write never outlives the test.
struct Release<'a>(&'a Semaphore);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

/// A [`pausing`] store's side: `reserved` hears of each high-water write
/// once it lands, and the write returns once `release` gains a permit.
struct Paused {
    reserved: mpsc::UnboundedReceiver<()>,
    release: Arc<Semaphore>,
}

/// `store`, holding each high-water write once it lands until it is let
/// go through the [`Paused`] side.
fn pausing(store: Store) -> (Store, Paused) {
    let (landed, heard) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let held = Arc::clone(&release);
    let wrapped = store.wrap_backend(move |inner| {
        Arc::new(Pausing {
            inner,
            landed,
            release: held,
        })
    });
    (
        wrapped,
        Paused {
            reserved: heard,
            release,
        },
    )
}

#[derive(Debug)]
struct Pausing {
    inner: Arc<dyn ObjectStore>,
    landed: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}

impl std::fmt::Display for Pausing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pausing({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Pausing {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let put = self.inner.put_opts(location, payload, opts).await?;
        if location.as_ref().ends_with(HIGH_WATER_KEY) {
            let _ = self.landed.send(());
            self.release.acquire().await.expect("open").forget();
        }
        Ok(put)
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<object_store::Result<Key>>,
    ) -> BoxStream<object_store::Result<Key>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// A steward over `ids` topping up from `reports`.
fn reporting(ids: Store, reports: &Reports) -> Steward {
    steward(ids).with_reports(reports.clone())
}

/// The log id of `leader`'s last entry, as a flusher that mined through
/// it reports.
async fn last_entry(cluster: &Cluster, leader: usize) -> crate::replicated::raft::types::LogId {
    let entries = cluster.node(leader).log(0).await;
    entries.last().expect("an entry").log_id
}

/// RFC 0065 §6.5, §6.8: the leader tops id blocks up on its flusher's
/// reports, so with none it reserves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_report_the_leader_reserves_no_block() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let store = seated_store(5_000);
    let mut steward = reporting(store.clone(), &Reports::default());

    let passed = pass(&mut steward, &cluster, leader, &[]).await;

    assert_eq!(passed.id_block, None);
    assert_eq!(high_water(&store).await, Some(5_000), "nothing reserved");
    cluster.shutdown().await;
}

/// A report tops the group up until the blocks its flusher has not
/// started, and those the log holds after its `mined`, are three; one with
/// two unstarted gets one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_tops_the_group_up_to_three_blocks_its_flusher_has_not_started() {
    for (unstarted, appended) in [(0, AHEAD), (2, 1)] {
        let cluster = Cluster::start(3, &[0]).await;
        let leader = ready_leader(&cluster).await;
        let reports = Reports::default();
        let mut steward = reporting(seated_store(5_000), &reports);
        let mined = last_entry(&cluster, leader).await;
        let proposer = cluster.id(0);
        reports.record(Reported {
            proposer,
            mined,
            unstarted,
        });

        for n in 0..u64::from(appended) {
            let topped = pass(&mut steward, &cluster, leader, &[]).await;
            let expected = block(5_000 + n * BLOCK);
            assert_eq!(
                topped.id_block,
                Some(TopUp::Appended(expected)),
                "{unstarted}"
            );
        }
        let again = pass(&mut steward, &cluster, leader, &[]).await;
        assert_eq!(again.id_block, None, "{unstarted}: three not started");
        cluster.shutdown().await;
    }
}

/// RFC 0065 §6.8: a report whose `mined` a checkpoint has since passed may
/// count blocks the purge removed short, so it tops up nothing until the
/// flusher reports afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_a_checkpoint_has_passed_tops_up_nothing() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, _) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let reports = Reports::default();
    let mut steward = Steward::with_patience(PATIENCE)
        .reserving(seated_store(0))
        .with_reports(reports.clone());
    let flusher = super::pass_until_assigned(&mut steward, &cluster, leader, &placed).await;
    let before = last_entry(&cluster, leader).await;
    super::checkpoint_purging(&cluster, (leader, flusher), &[leader]).await;
    reports.record(Reported {
        proposer: flusher,
        mined: before,
        unstarted: 0,
    });

    let passed = pass(&mut steward, &cluster, leader, &placed).await;

    assert_eq!(passed.id_block, None, "the report is behind the cut");
    let fresh = last_entry(&cluster, leader).await;
    reports.record(Reported {
        proposer: flusher,
        mined: fresh,
        unstarted: 0,
    });
    let passed = pass(&mut steward, &cluster, leader, &placed).await;
    assert!(
        matches!(passed.id_block, Some(TopUp::Appended(_))),
        "{:?}",
        passed.id_block
    );
    cluster.shutdown().await;
}

/// RFC 0065 §6.8: a report counts while its sender is the flusher in
/// force; once another replica is named it tops up nothing until that one
/// reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_former_flushers_report_tops_up_nothing() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, tail) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let reports = Reports::default();
    let mut steward = Steward::with_patience(PATIENCE)
        .reserving(seated_store(0))
        .with_reports(reports.clone());
    let flusher = super::pass_until_assigned(&mut steward, &cluster, leader, &placed).await;
    let former = tail
        .iter()
        .map(|&i| cluster.id(i))
        .find(|&id| id != flusher)
        .expect("another tail replica");
    let mined = last_entry(&cluster, leader).await;
    reports.record(Reported {
        proposer: former,
        mined,
        unstarted: 0,
    });

    let passed = pass(&mut steward, &cluster, leader, &placed).await;

    assert_eq!(passed.id_block, None, "not the flusher in force");
    reports.record(Reported {
        proposer: flusher,
        mined,
        unstarted: 0,
    });
    let passed = pass(&mut steward, &cluster, leader, &placed).await;
    assert!(
        matches!(passed.id_block, Some(TopUp::Appended(_))),
        "{:?}",
        passed.id_block
    );
    cluster.shutdown().await;
}

/// The superseded tests above, run on a report.
mod reported;
