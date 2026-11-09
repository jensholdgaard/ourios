//! The leader's id blocks on the in-process cluster (RFC 0065 §6.5):
//! reserved in the store before their entry exists, three kept ahead, held
//! back by the kind gate, and never reused across a leader change.

use std::sync::Arc;
use std::time::Duration;

use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use ourios_parquet::Store;
use tokio::sync::{Semaphore, mpsc};

use ourios_semconv as semconv;
use ourios_wal::Control;

use super::{
    PATIENCE, Pass, admit, counted, counted_as, group, heard_from, log,
    membership_change_in_flight, pass, placement, reads_up_to, ready_leader, strs,
};
use crate::replicated::raft::cluster::{Cluster, seated_store, write};
use crate::replicated::raft::flusher::Steward;
use crate::replicated::raft::id_blocks::{self, AHEAD, Ahead, Refusal, TopUp};
use crate::replicated::raft::snapshot_record::IdBlock;
use crate::replicated::raft::telemetry::tests::Reader;
use crate::template_ids::{self, BLOCK, HIGH_WATER_KEY};

type BoxStream<T> = futures::stream::BoxStream<'static, T>;

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
async fn the_block_is_reserved_before_its_entry_exists() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (store, mut reserved, release) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let watch = async {
        reserved.recv().await.expect("a reservation");
        let reserved_through = high_water(&store).await;
        let held = Ahead::of(group(&cluster, leader));
        release.add_permits(1);
        (reserved_through, held)
    };
    let (topped, (reserved_through, held)) =
        tokio::join!(pass(&mut steward, &cluster, leader, &[]), watch);

    assert_eq!(held.blocks, 0, "no entry before the reservation is durable");
    assert_eq!(topped.id_block, Some(TopUp::Appended(block(0))));
    assert!(reserved_through >= Some(BLOCK), "{reserved_through:?}");
    cluster.shutdown().await;
}

/// A reservation that fails appends nothing and says why; so does a store
/// without the high-water, which a reservation never creates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
    membership.await.expect("join");
    let appended = pass(&mut steward, &cluster, leader, &first).await;
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

/// `store`, holding each high-water write once it lands: the receiver
/// hears of it, and the write returns once a permit is added.
fn pausing(store: Store) -> (Store, mpsc::UnboundedReceiver<()>, Arc<Semaphore>) {
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
    (wrapped, heard, release)
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
