//! An in-process cluster over loopback gRPC: nodes that each host their
//! groups in a [`GroupHost`], links the test can cut, and nodes it can
//! kill, crash under §8's crash model, and restart from their
//! directories.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openraft::RaftMetrics;
use openraft::async_runtime::WatchReceiver as _;
use openraft::storage::RaftLogReader as _;
use ourios_parquet::Store;
use ourios_wal::{TenantBatch, WalConfig, WalOffset};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::server::TcpIncoming;

use super::gate::SyncGate;
use super::host::{GroupHost, GroupRaft, HostSpec, Wiring};
use super::log_store::group_dir;
use super::state_machine::GroupSm;
use super::telemetry::Meters;
use super::transport::{Identity, Peers, ReplicationServer};
use super::types::{Entry, GroupEntry, NodeId, RaftTypes, TenantBatchBytes};
use super::vote_log;
use crate::template_ids::{self, HIGH_WATER_KEY};

mod tests;

pub(crate) const WAIT: Option<Duration> = Some(Duration::from_secs(10));

pub(crate) fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 2,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

/// The longest election timeout [`raft_config`] allows.
pub(crate) const ELECTION_TIMEOUT_MAX: Duration = Duration::from_secs(2);

/// [`group_config`](super::group_config) with a longer election timeout
/// and heartbeat interval. The default 150–300 ms timeout is also the
/// leader's quorum lease, which a loaded CI runner can miss, unseating a
/// leader the test still writes to. `openraft` gives each heartbeat and
/// append the heartbeat interval as its deadline, which the transport cuts
/// to three quarters, so at the default 50 ms a loaded runner times many of
/// them out and the leader's view of when it last heard from a replica
/// goes stale.
pub(crate) fn raft_config() -> openraft::Config {
    openraft::Config {
        election_timeout_min: 1_000,
        election_timeout_max: ELECTION_TIMEOUT_MAX.as_millis().try_into().expect("fits"),
        heartbeat_interval: 200,
        ..super::group_config().expect("group config")
    }
    .validate()
    .expect("test config")
}

/// §8's crash model: drop every byte past the boundary `synced` names.
/// Returns the bytes dropped.
pub(crate) fn cut_to(dir: &Path, synced: WalOffset) -> u64 {
    let mut lost = 0;
    for dirent in std::fs::read_dir(dir).expect("read_dir") {
        let path = dirent.expect("dirent").path();
        let Some(uuid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".wal"))
            .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        let len = std::fs::metadata(&path).expect("stat").len();
        if uuid > synced.segment {
            lost += len;
            std::fs::remove_file(&path).expect("remove");
        } else if uuid == synced.segment {
            lost += len - synced.byte;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            file.set_len(synced.byte).expect("cut");
        }
    }
    lost
}

/// An in-memory store whose template-id high-water is seated at
/// `reserved_through`, for a leader to reserve id blocks in.
pub(crate) fn seated_store(reserved_through: u64) -> Store {
    let store = Store::in_memory();
    store
        .put_blocking(HIGH_WATER_KEY, template_ids::encode(reserved_through))
        .expect("seat the high-water");
    store
}

pub(crate) fn bind() -> (TcpIncoming, SocketAddr) {
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr = incoming.local_addr().expect("local addr");
    (incoming, addr)
}

pub(crate) type Serving = (oneshot::Sender<()>, JoinHandle<()>);

/// Serve `server` on `incoming` until the returned sender fires or drops.
pub(crate) fn serve(server: ReplicationServer<GroupSm>, incoming: TcpIncoming) -> Serving {
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        server
            .serve_plaintext(incoming, shutdown)
            .await
            .expect("serve");
    });
    (stop, serving)
}

/// The `node_id` a test replica runs under: its node data in every
/// membership, and its key in the address map.
pub(crate) fn name(id: NodeId) -> String {
    id.to_string()
}

/// A replica's identity, the `node_id` it runs under, the directory its
/// logs and votes live in, the groups it votes in, and where its host
/// reports.
pub(crate) struct Replica {
    pub(crate) id: NodeId,
    pub(crate) node_id: String,
    pub(crate) dir: TempDir,
    pub(crate) groups: Vec<u32>,
    pub(crate) meters: Meters,
}

impl Replica {
    pub(crate) fn new(n: u64, groups: &[u32]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        vote_log::create(dir.path()).expect("vote log");
        let id = NodeId::from(n);
        Self {
            id,
            node_id: name(id),
            dir,
            groups: groups.to_vec(),
            meters: Meters::global(),
        }
    }
}

pub(crate) struct Node {
    pub(crate) id: NodeId,
    pub(crate) host: GroupHost,
    /// Every fsync of every group log on the node passes it.
    pub(crate) gate: Arc<SyncGate>,
    pub(crate) stop: Option<oneshot::Sender<()>>,
    pub(crate) serving: JoinHandle<()>,
    replica: Replica,
}

impl Node {
    /// `replica`, from the state in its directory, served on `incoming`.
    pub(crate) async fn open(
        replica: Replica,
        incoming: TcpIncoming,
        peers: &Peers,
        server: ReplicationServer<GroupSm>,
    ) -> Self {
        Self::open_on(replica, peers, server, |server| serve(server, incoming)).await
    }

    /// `replica`, from the state in its directory, served by `serve`.
    pub(crate) async fn open_on(
        replica: Replica,
        peers: &Peers,
        server: ReplicationServer<GroupSm>,
        serve: impl FnOnce(ReplicationServer<GroupSm>) -> Serving,
    ) -> Self {
        let gate = Arc::new(SyncGate::default());
        let spec = HostSpec {
            id: replica.id,
            wal: wal_config(replica.dir.path()),
            groups: replica.groups.clone(),
            peers: peers.clone(),
        };
        server.identify_as(Identity::new(replica.node_id.clone(), replica.id));
        let wiring = Wiring {
            config: raft_config(),
            server,
            gate: Arc::clone(&gate),
        };
        let host = GroupHost::open_with(spec, wiring, &replica.meters)
            .await
            .expect("host");
        let (stop, serving) = serve(host.server());
        Self {
            id: replica.id,
            host,
            gate,
            stop: Some(stop),
            serving,
            replica,
        }
    }

    pub(crate) fn raft(&self, group: u32) -> &GroupRaft {
        self.host.raft(group).expect("a hosted group")
    }

    pub(crate) fn node_id(&self) -> &str {
        &self.replica.node_id
    }

    pub(crate) async fn kill(&mut self) {
        self.host.shutdown().await.expect("shutdown");
        self.stop_serving().await;
    }

    async fn stop_serving(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(10), &mut self.serving)
            .await
            .expect("server stops")
            .expect("server task");
    }

    /// Stop the node as a crash would: every group writer stops before
    /// its next fsync, and each group's WAL is cut back to its last
    /// sync. Returns the bytes the cut dropped.
    pub(crate) async fn crash(mut self) -> (Replica, u64) {
        let hooks: Vec<_> = self
            .host
            .groups()
            .map(|g| (g, self.host.log_hooks(g).expect("hosted")))
            .collect();
        self.gate.crash();
        self.host.shutdown().await.expect("shutdown");
        self.stop_serving().await;
        let replica = self.into_replica();
        let mut lost = 0;
        for (group, hooks) in hooks {
            let synced = hooks.crash().expect("the open synced the log");
            lost += cut_to(&group_dir(replica.dir.path(), group), synced);
        }
        (replica, lost)
    }

    /// The replica, once killed. Dropping the host releases the vote log
    /// the next open of the directory takes.
    pub(crate) fn into_replica(self) -> Replica {
        self.replica
    }

    pub(crate) async fn applied_through(&self, group: u32, index: u64) {
        self.raft(group)
            .wait(WAIT)
            .applied_index_at_least(Some(index), "applied")
            .await
            .expect("applied");
        let sm = self.host.state_machine(group).expect("hosted");
        let applied = sm.applied().map(|id| id.index);
        assert!(applied >= Some(index), "{} applied {applied:?}", self.id);
    }

    /// Every entry `group`'s log holds.
    pub(crate) async fn log(&self, group: u32) -> Vec<Entry> {
        let mut reader = self.host.log_reader(group).expect("hosted");
        reader.try_get_log_entries(..).await.expect("read")
    }
}

/// The leader of `group` as `nodes[0]` sees it, once it sees one other
/// than `not`.
pub(crate) async fn leader(group: u32, nodes: &[&Node], not: Option<NodeId>) -> NodeId {
    let metrics = nodes[0]
        .raft(group)
        .wait(WAIT)
        .metrics(
            |m: &RaftMetrics<RaftTypes>| m.current_leader.is_some() && m.current_leader != not,
            "a leader",
        )
        .await
        .expect("leader elected");
    metrics.current_leader.expect("leader")
}

pub(crate) fn batch(n: u64) -> TenantBatchBytes {
    let payload = TenantBatch::encode("acme", format!("record {n}").as_bytes()).expect("batch");
    TenantBatchBytes::new(payload).expect("valid")
}

/// Commit `batch(n)`, returning its index.
pub(crate) async fn write(raft: &GroupRaft, n: u64) -> u64 {
    propose(raft, batch(n).into()).await
}

/// Commit `entry`, returning its index. A new leader refuses writes until
/// its blank entry commits.
pub(crate) async fn propose(raft: &GroupRaft, entry: GroupEntry) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match raft.client_write(entry.clone()).await {
            Ok(response) => return response.log_id.index,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("propose {entry}: {e}"),
        }
    }
}

/// A node up or down.
enum Slot {
    Up(Node),
    Down(Replica),
}

/// Nodes `0..size`, every one voting in every group, on one shared pool
/// whose links the test cuts.
pub(crate) struct Cluster {
    peers: Peers,
    addrs: Vec<SocketAddr>,
    slots: Vec<Slot>,
}

impl Cluster {
    /// Start `size` nodes and bootstrap each of `groups` from node 0
    /// with every node a voter.
    pub(crate) async fn start(size: u64, groups: &[u32]) -> Self {
        Self::start_metered(size, groups, |_| Meters::global()).await
    }

    /// [`Self::start`], node `i` reporting to `meters(i)`.
    pub(crate) async fn start_metered(
        size: u64,
        groups: &[u32],
        meters: impl Fn(usize) -> Meters,
    ) -> Self {
        Self::open((size, 0), groups, meters).await
    }

    /// `voters` nodes that vote in every group, and `tail` more, nodes
    /// `voters..`, that host every group but are in no membership until a
    /// leader adds them as learners.
    pub(crate) async fn start_with_tail(voters: u64, tail: u64, groups: &[u32]) -> Self {
        Self::open((voters, tail), groups, |_| Meters::global()).await
    }

    async fn open(
        (voters, tail): (u64, u64),
        groups: &[u32],
        meters: impl Fn(usize) -> Meters,
    ) -> Self {
        let (cluster, members) = Self::open_unformed((voters, tail), groups, meters).await;
        for &group in groups {
            let first = cluster.node(0).raft(group);
            first.initialize(members.clone()).await.expect("initialize");
        }
        cluster
    }

    /// `size` nodes that host every group with none initialized, as a
    /// fresh cluster starts before its groups form (§6.1).
    pub(crate) async fn start_unformed(size: u64, groups: &[u32]) -> Self {
        Self::open_unformed((size, 0), groups, |_| Meters::global())
            .await
            .0
    }

    async fn open_unformed(
        (voters, tail): (u64, u64),
        groups: &[u32],
        meters: impl Fn(usize) -> Meters,
    ) -> (Self, BTreeMap<NodeId, String>) {
        let bound: Vec<_> = (1..=voters + tail)
            .map(|n| (NodeId::from(n), bind()))
            .collect();
        let addresses = bound.iter().map(|(id, (_, a))| (name(*id), a.to_string()));
        let peers = Peers::plaintext().with_addresses(addresses);
        let members: BTreeMap<_, _> = bound
            .iter()
            .take(usize::try_from(voters).expect("a small cluster"))
            .map(|(id, _)| (*id, name(*id)))
            .collect();
        let mut cluster = Self {
            peers,
            addrs: bound.iter().map(|(_, (_, a))| *a).collect(),
            slots: Vec::new(),
        };
        for (n, (_, (incoming, _))) in (1..).zip(bound) {
            let replica = Replica {
                meters: meters(cluster.slots.len()),
                ..Replica::new(n, groups)
            };
            let server = ReplicationServer::default();
            let node = Node::open(replica, incoming, &cluster.peers, server).await;
            cluster.slots.push(Slot::Up(node));
        }
        (cluster, members)
    }

    pub(crate) fn node(&self, i: usize) -> &Node {
        match &self.slots[i] {
            Slot::Up(node) => node,
            Slot::Down(_) => panic!("node {i} is down"),
        }
    }

    /// The pool every node dials its peers through.
    pub(crate) fn peers(&self) -> &Peers {
        &self.peers
    }

    pub(crate) fn id(&self, i: usize) -> NodeId {
        match &self.slots[i] {
            Slot::Up(node) => node.id,
            Slot::Down(replica) => replica.id,
        }
    }

    fn up(&self) -> impl Iterator<Item = (usize, &Node)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| match slot {
                Slot::Up(node) => Some((i, node)),
                Slot::Down(_) => None,
            })
    }

    pub(crate) fn cut(&self, a: usize, b: usize) {
        self.peers.cut(self.id(a), self.id(b));
    }

    pub(crate) fn heal(&self, a: usize, b: usize) {
        self.peers.heal(self.id(a), self.id(b));
    }

    fn take(&mut self, i: usize) -> Node {
        match self.slots.remove(i) {
            Slot::Up(node) => node,
            Slot::Down(_) => panic!("node {i} is down"),
        }
    }

    pub(crate) async fn kill(&mut self, i: usize) {
        let mut node = self.take(i);
        node.kill().await;
        self.slots.insert(i, Slot::Down(node.into_replica()));
    }

    /// Crash node `i` (see [`Node::crash`]); returns the bytes lost.
    pub(crate) async fn crash(&mut self, i: usize) -> u64 {
        let (replica, lost) = self.take(i).crash().await;
        self.slots.insert(i, Slot::Down(replica));
        lost
    }

    /// Crash every node still up, leaving each directory as its last
    /// fsync left it. Every gate stops first, so no node fsyncs while
    /// another shuts down.
    pub(crate) async fn crash_all(&mut self) {
        for (_, node) in self.up() {
            node.gate.crash();
        }
        for i in 0..self.slots.len() {
            if matches!(self.slots[i], Slot::Up(_)) {
                self.crash(i).await;
            }
        }
    }

    /// The directory node `i` keeps its logs and votes in.
    pub(crate) fn dir(&self, i: usize) -> &Path {
        match &self.slots[i] {
            Slot::Up(node) => node.replica.dir.path(),
            Slot::Down(replica) => replica.dir.path(),
        }
    }

    /// Replace down node `i`'s directory with an empty one, as a restart on
    /// a wiped `wal_root` finds it: the node keeps its `node_id` and runs
    /// as the new replica `n`.
    pub(crate) fn wipe(&mut self, i: usize, n: u64) {
        let Slot::Down(old) = &self.slots[i] else {
            panic!("node {i} is up");
        };
        let replica = Replica {
            node_id: old.node_id.clone(),
            ..Replica::new(n, &old.groups)
        };
        self.slots[i] = Slot::Down(replica);
    }

    /// Start node `i` again from its directory, at its address.
    pub(crate) async fn restart(&mut self, i: usize) {
        let Slot::Down(replica) = self.slots.remove(i) else {
            panic!("node {i} is up");
        };
        let incoming = TcpIncoming::bind(self.addrs[i]).expect("rebind");
        let server = ReplicationServer::default();
        let node = Node::open(replica, incoming, &self.peers, server).await;
        self.slots.insert(i, Slot::Up(node));
    }

    /// The live node leading `group`, once one other than `not` leads.
    pub(crate) async fn leader(&self, group: u32, not: Option<usize>) -> usize {
        let not = not.map(|i| self.id(i));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let leading = self.up().find(|(_, node)| {
                let leading = node.raft(group).metrics().borrow_watched().current_leader;
                leading == Some(node.id) && leading != not
            });
            match leading {
                Some((i, _)) => return i,
                None if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                None => panic!("no leader of group {group}"),
            }
        }
    }

    pub(crate) async fn shutdown(mut self) {
        for slot in &mut self.slots {
            if let Slot::Up(node) = slot {
                node.kill().await;
            }
        }
    }
}
