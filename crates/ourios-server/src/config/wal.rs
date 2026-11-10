//! `receiver.wal` — the receiver's WAL backend (RFC 0065 §3.8).
//!
//! [`build_wal_backend`] is the one validation path for both front-ends:
//! the env path passes bare `OURIOS_RECEIVER_WAL_*` values and no TLS
//! block (TLS is config-file only, RFC 0030 §3.1), the file path passes
//! `receiver.wal.*`. Every error names the offending key.

use std::net::{Ipv6Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64, NonZeroUsize};

use ourios_ingester::replicated::raft::{RaftTimings, RaftTimingsError};
use ourios_serving::tls::TlsSettings;

use crate::config::file::TlsSection;
use crate::config::resolve::{parse_addr, tls_settings};

/// Default peer listener address (RFC 0065 §3.8).
pub const DEFAULT_PEER_LISTEN_ADDR: &str = "0.0.0.0:4320";
/// Default group count (RFC 0065 §3.8).
pub const DEFAULT_GROUPS: NonZeroU32 = match NonZeroU32::new(16) {
    Some(groups) => groups,
    None => NonZeroU32::MIN,
};
/// Default tail replicas per group (RFC 0065 §3.8).
pub const DEFAULT_TAIL_REPLICAS: NonZeroUsize = NonZeroUsize::MIN;

const TLS_KEY: &str = "receiver.wal.replicated.tls";

/// The resolved WAL backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalBackend {
    /// One RFC 0008 WAL on local disk (the default).
    Local,
    /// The replicated WAL (RFC 0065).
    Replicated(Box<ReplicatedWal>),
}

/// Resolved `receiver.wal.replicated.*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicatedWal {
    pub node_id: String,
    /// Derived from which of `peers` and `tail_peers` holds `node_id`.
    pub role: NodeRole,
    pub listen_addr: SocketAddr,
    pub peers: Vec<Peer>,
    pub tail_peers: Vec<Peer>,
    pub groups: NonZeroU32,
    pub replication_factor: ReplicationFactor,
    pub tail_replicas: NonZeroUsize,
    /// Querier certificate identities allowed to call `Tail`.
    pub tail_clients: Vec<String>,
    /// Peer mTLS; always carries a client CA.
    pub tls: TlsSettings,
    /// The Raft timings every group on the node runs on.
    pub raft_timings: RaftTimings,
}

/// What this node is in the cluster layout (RFC 0065 §3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    /// Listed in `peers`: votes in the groups placed on it.
    Voter,
    /// Listed in `tail_peers`: a learner serving the queryable tail.
    TailReplica,
}

/// Voters per group. Its majority is the write quorum, so only odd
/// factors above 1 raise the durability floor (RFC 0065 §3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationFactor {
    Three,
    Five,
}

impl ReplicationFactor {
    #[must_use]
    pub fn voters(self) -> usize {
        match self {
            Self::Three => 3,
            Self::Five => 5,
        }
    }
}

/// One `id=host:port` entry of `peers` or `tail_peers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub id: String,
    pub host: String,
    pub port: u16,
}

/// Raw inputs for [`build_wal_backend`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WalInputs<'a> {
    /// `OURIOS_RECEIVER_WAL_BACKEND` / `receiver.wal.backend`.
    pub backend: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_NODE_ID` / `receiver.wal.replicated.node_id`.
    pub node_id: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_LISTEN_ADDR` / `receiver.wal.replicated.listen_addr`.
    pub listen_addr: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_PEERS` / `receiver.wal.replicated.peers`.
    pub peers: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_TAIL_PEERS` / `receiver.wal.replicated.tail_peers`.
    pub tail_peers: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_GROUPS` / `receiver.wal.replicated.groups`.
    pub groups: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_REPLICATION_FACTOR` /
    /// `receiver.wal.replicated.replication_factor`.
    pub replication_factor: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_TAIL_REPLICAS` / `receiver.wal.replicated.tail_replicas`.
    pub tail_replicas: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_TAIL_CLIENTS` / `receiver.wal.replicated.tail_clients`.
    pub tail_clients: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_HEARTBEAT_INTERVAL_MS` /
    /// `receiver.wal.replicated.heartbeat_interval_ms`.
    pub heartbeat_interval_ms: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_ELECTION_TIMEOUT_MIN_MS` /
    /// `receiver.wal.replicated.election_timeout_min_ms`.
    pub election_timeout_min_ms: Option<&'a str>,
    /// `OURIOS_RECEIVER_WAL_ELECTION_TIMEOUT_MAX_MS` /
    /// `receiver.wal.replicated.election_timeout_max_ms`.
    pub election_timeout_max_ms: Option<&'a str>,
    /// `receiver.wal.replicated.tls` (config-file only).
    pub tls: Option<&'a TlsSection>,
}

/// The `OURIOS_RECEIVER_WAL_*` leaves read once, owned so that
/// [`WalEnv::inputs`] can lend them to the shared builder.
#[derive(Debug, Default)]
pub struct WalEnv {
    backend: Option<String>,
    node_id: Option<String>,
    listen_addr: Option<String>,
    peers: Option<String>,
    tail_peers: Option<String>,
    groups: Option<String>,
    replication_factor: Option<String>,
    tail_replicas: Option<String>,
    tail_clients: Option<String>,
    heartbeat_interval_ms: Option<String>,
    election_timeout_min_ms: Option<String>,
    election_timeout_max_ms: Option<String>,
}

impl WalEnv {
    #[must_use]
    pub fn from_env() -> Self {
        let var = |key: &str| std::env::var(format!("OURIOS_RECEIVER_WAL_{key}")).ok();
        Self {
            backend: var("BACKEND"),
            node_id: var("NODE_ID"),
            listen_addr: var("LISTEN_ADDR"),
            peers: var("PEERS"),
            tail_peers: var("TAIL_PEERS"),
            groups: var("GROUPS"),
            replication_factor: var("REPLICATION_FACTOR"),
            tail_replicas: var("TAIL_REPLICAS"),
            tail_clients: var("TAIL_CLIENTS"),
            heartbeat_interval_ms: var("HEARTBEAT_INTERVAL_MS"),
            election_timeout_min_ms: var("ELECTION_TIMEOUT_MIN_MS"),
            election_timeout_max_ms: var("ELECTION_TIMEOUT_MAX_MS"),
        }
    }

    /// The env path has no TLS (RFC 0030 §3.1), so `tls` is `None`.
    #[must_use]
    pub fn inputs(&self) -> WalInputs<'_> {
        WalInputs {
            backend: self.backend.as_deref(),
            node_id: self.node_id.as_deref(),
            listen_addr: self.listen_addr.as_deref(),
            peers: self.peers.as_deref(),
            tail_peers: self.tail_peers.as_deref(),
            groups: self.groups.as_deref(),
            replication_factor: self.replication_factor.as_deref(),
            tail_replicas: self.tail_replicas.as_deref(),
            tail_clients: self.tail_clients.as_deref(),
            heartbeat_interval_ms: self.heartbeat_interval_ms.as_deref(),
            election_timeout_min_ms: self.election_timeout_min_ms.as_deref(),
            election_timeout_max_ms: self.election_timeout_max_ms.as_deref(),
            tls: None,
        }
    }
}

/// Resolve and validate `receiver.wal`. Unset and `local` select the
/// local backend, whose `replicated` leaves are never read.
///
/// # Errors
///
/// An unknown backend, or for `replicated`: a missing or malformed
/// field, a `node_id` in neither or both of `peers` and `tail_peers`, a
/// `replication_factor` other than 3 or 5 or above the voter count, a
/// `tail_replicas` outside `1..=tail_peers`, no `tail_clients`, a Raft
/// timing that is not a positive integer or breaks `heartbeat_interval_ms
/// < election_timeout_min_ms < election_timeout_max_ms`, or a missing
/// `tls` block or client CA. Each names the field.
pub fn build_wal_backend(inputs: WalInputs<'_>) -> Result<WalBackend, String> {
    match present(inputs.backend) {
        None | Some("local") => Ok(WalBackend::Local),
        Some("replicated") => {
            build_replicated(inputs).map(|wal| WalBackend::Replicated(Box::new(wal)))
        }
        Some(other) => Err(format!(
            "receiver.wal.backend (OURIOS_RECEIVER_WAL_BACKEND) must be \"local\" or \
             \"replicated\", got {other:?}"
        )),
    }
}

/// One `receiver.wal.replicated` leaf: its YAML key and the suffix of its
/// `OURIOS_RECEIVER_WAL_*` variable, which every error names together.
#[derive(Clone, Copy)]
struct Leaf {
    field: &'static str,
    env: &'static str,
}

const PEERS: Leaf = Leaf {
    field: "peers",
    env: "PEERS",
};
const TAIL_PEERS: Leaf = Leaf {
    field: "tail_peers",
    env: "TAIL_PEERS",
};
const NODE_ID: Leaf = Leaf {
    field: "node_id",
    env: "NODE_ID",
};
const TAIL_CLIENTS: Leaf = Leaf {
    field: "tail_clients",
    env: "TAIL_CLIENTS",
};
const HEARTBEAT_INTERVAL: Leaf = Leaf {
    field: "heartbeat_interval_ms",
    env: "HEARTBEAT_INTERVAL_MS",
};
const ELECTION_TIMEOUT_MIN: Leaf = Leaf {
    field: "election_timeout_min_ms",
    env: "ELECTION_TIMEOUT_MIN_MS",
};
const ELECTION_TIMEOUT_MAX: Leaf = Leaf {
    field: "election_timeout_max_ms",
    env: "ELECTION_TIMEOUT_MAX_MS",
};

impl Leaf {
    /// A positive whole number of milliseconds, `default` when unset.
    fn millis(self, raw: Option<&str>, default: NonZeroU64) -> Result<NonZeroU64, String> {
        match present(raw) {
            None => Ok(default),
            Some(raw) => raw.parse().map_err(|_| {
                self.error(format_args!(
                    "must be a positive integer number of milliseconds, got {raw:?}"
                ))
            }),
        }
    }

    fn error(self, detail: impl std::fmt::Display) -> String {
        format!(
            "receiver.wal.replicated.{} (OURIOS_RECEIVER_WAL_{}) {detail}",
            self.field, self.env
        )
    }

    fn required(self, raw: Option<&str>) -> Result<&str, String> {
        present(raw)
            .ok_or_else(|| self.error("is required when receiver.wal.backend is \"replicated\""))
    }

    /// Split a comma-separated list, rejecting an empty entry (a stray
    /// comma is more likely a typo than an intent).
    fn list(self, raw: &str) -> Result<Vec<&str>, String> {
        raw.split(',')
            .map(str::trim)
            .map(|entry| match entry {
                "" => Err(self.error(format_args!("has an empty entry in {raw:?}"))),
                entry => Ok(entry),
            })
            .collect()
    }

    fn peers(self, raw: Option<&str>) -> Result<Vec<Peer>, String> {
        let mut peers: Vec<Peer> = Vec::new();
        for entry in self.list(self.required(raw)?)? {
            let peer = parse_peer(entry).ok_or_else(|| {
                self.error(format_args!("entries must be id=host:port, got {entry:?}"))
            })?;
            if peers.iter().any(|seen| seen.id == peer.id) {
                return Err(self.error(format_args!("lists the id {:?} more than once", peer.id)));
            }
            peers.push(peer);
        }
        Ok(peers)
    }
}

fn build_replicated(inputs: WalInputs<'_>) -> Result<ReplicatedWal, String> {
    let peers = PEERS.peers(inputs.peers)?;
    let tail_peers = TAIL_PEERS.peers(inputs.tail_peers)?;
    let node_id = NODE_ID.required(inputs.node_id)?.to_owned();
    let role = node_role(&node_id, &peers, &tail_peers)?;
    if let Some(shared) = peers
        .iter()
        .find(|peer| tail_peers.iter().any(|tail| tail.id == peer.id))
    {
        return Err(format!(
            "receiver.wal.replicated.tail_peers (OURIOS_RECEIVER_WAL_TAIL_PEERS) must not \
             list {:?}, which peers already lists: a node is a voter or a tail replica, \
             never both",
            shared.id
        ));
    }
    let listen_addr =
        parse_addr(present(inputs.listen_addr), DEFAULT_PEER_LISTEN_ADDR).map_err(|e| {
            format!(
                "receiver.wal.replicated.listen_addr (OURIOS_RECEIVER_WAL_LISTEN_ADDR) is not \
                 host:port: {e}"
            )
        })?;
    let groups = parse_groups(present(inputs.groups))?;
    let replication_factor =
        parse_replication_factor(present(inputs.replication_factor), peers.len())?;
    let tail_replicas = parse_tail_replicas(present(inputs.tail_replicas), tail_peers.len())?;
    let tail_clients = TAIL_CLIENTS
        .list(TAIL_CLIENTS.required(inputs.tail_clients)?)?
        .into_iter()
        .map(str::to_owned)
        .collect();
    let raft_timings = parse_raft_timings(inputs)?;
    let tls = peer_tls(inputs.tls)?;
    Ok(ReplicatedWal {
        node_id,
        role,
        listen_addr,
        peers,
        tail_peers,
        groups,
        replication_factor,
        tail_replicas,
        tail_clients,
        tls,
        raft_timings,
    })
}

/// The three timings, each defaulted when unset, then ordered as
/// `heartbeat < election minimum < election maximum`.
fn parse_raft_timings(inputs: WalInputs<'_>) -> Result<RaftTimings, String> {
    let defaults = RaftTimings::default();
    let heartbeat = HEARTBEAT_INTERVAL.millis(
        inputs.heartbeat_interval_ms,
        defaults.heartbeat_interval_ms(),
    )?;
    let min = ELECTION_TIMEOUT_MIN.millis(
        inputs.election_timeout_min_ms,
        defaults.election_timeout_min_ms(),
    )?;
    let max = ELECTION_TIMEOUT_MAX.millis(
        inputs.election_timeout_max_ms,
        defaults.election_timeout_max_ms(),
    )?;
    RaftTimings::new(heartbeat, min, max).map_err(|e| match e {
        RaftTimingsError::HeartbeatNotBelowElectionMin => HEARTBEAT_INTERVAL.error(format_args!(
            "{heartbeat} must be below election_timeout_min_ms {min}"
        )),
        RaftTimingsError::ElectionMinNotBelowMax => ELECTION_TIMEOUT_MIN.error(format_args!(
            "{min} must be below election_timeout_max_ms {max}"
        )),
    })
}

fn present(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

fn parse_peer(entry: &str) -> Option<Peer> {
    let (id, addr) = entry.split_once('=')?;
    let (host, port) = addr.rsplit_once(':')?;
    let (id, host) = (id.trim(), host.trim());
    let port = port.trim().parse::<NonZeroU16>().ok()?;
    let valid_id = !id.is_empty() && !id.contains(|c: char| c == '=' || c.is_whitespace());
    Some(Peer {
        id: valid_id.then(|| id.to_owned())?,
        host: valid_host(host).then(|| host.to_owned())?,
        port: port.get(),
    })
}

/// A DNS name, an IPv4 address or a bracketed IPv6 address: anything
/// else (a scheme, a space, a second `=` from a missing comma) is a typo
/// that would otherwise only surface as a failed dial.
fn valid_host(host: &str) -> bool {
    let label = |label: &str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if let Some(v6) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return v6.parse::<Ipv6Addr>().is_ok();
    }
    // A trailing dot is the DNS root: an absolute name that skips the
    // resolver's search suffixes.
    let name = host.strip_suffix('.').unwrap_or(host);
    name.len() <= 253 && name.split('.').all(label)
}

fn node_role(node_id: &str, peers: &[Peer], tail_peers: &[Peer]) -> Result<NodeRole, String> {
    let listed = |list: &[Peer]| list.iter().any(|peer| peer.id == node_id);
    match (listed(peers), listed(tail_peers)) {
        (true, false) => Ok(NodeRole::Voter),
        (false, true) => Ok(NodeRole::TailReplica),
        (false, false) => Err(format!(
            "receiver.wal.replicated.node_id (OURIOS_RECEIVER_WAL_NODE_ID) {node_id:?} is in \
             neither peers nor tail_peers"
        )),
        (true, true) => Err(format!(
            "receiver.wal.replicated.node_id (OURIOS_RECEIVER_WAL_NODE_ID) {node_id:?} is in \
             both peers and tail_peers: a node is a voter or a tail replica, never both"
        )),
    }
}

fn parse_groups(raw: Option<&str>) -> Result<NonZeroU32, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_GROUPS);
    };
    raw.parse::<NonZeroU32>().map_err(|_| {
        format!(
            "receiver.wal.replicated.groups (OURIOS_RECEIVER_WAL_GROUPS) must be an integer \
             ≥ 1, got {raw:?}"
        )
    })
}

fn parse_replication_factor(raw: Option<&str>, peers: usize) -> Result<ReplicationFactor, String> {
    let factor = match raw {
        None | Some("3") => ReplicationFactor::Three,
        Some("5") => ReplicationFactor::Five,
        Some(other) => {
            return Err(format!(
                "receiver.wal.replicated.replication_factor \
                 (OURIOS_RECEIVER_WAL_REPLICATION_FACTOR) must be 3 or 5, got {other:?}"
            ));
        }
    };
    match factor.voters() {
        voters if voters <= peers => Ok(factor),
        voters => Err(format!(
            "receiver.wal.replicated.replication_factor \
             (OURIOS_RECEIVER_WAL_REPLICATION_FACTOR) {voters} exceeds the {peers} peers"
        )),
    }
}

fn parse_tail_replicas(raw: Option<&str>, tail_peers: usize) -> Result<NonZeroUsize, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_TAIL_REPLICAS);
    };
    match raw.parse::<NonZeroUsize>() {
        Ok(n) if n.get() <= tail_peers => Ok(n),
        _ => Err(format!(
            "receiver.wal.replicated.tail_replicas (OURIOS_RECEIVER_WAL_TAIL_REPLICAS) must be \
             an integer in 1..={tail_peers} (the tail_peers count), got {raw:?}"
        )),
    }
}

fn peer_tls(section: Option<&TlsSection>) -> Result<TlsSettings, String> {
    let settings = match section {
        Some(section) => tls_settings(TLS_KEY, section)?,
        None => None,
    };
    match settings {
        None => Err(format!(
            "{TLS_KEY} is required when receiver.wal.backend is \"replicated\" (peer mTLS; \
             config file only, RFC 0030 §3.1)"
        )),
        Some(tls) if tls.client_ca_file.is_none() => Err(format!(
            "{TLS_KEY}.client_ca_file is required: the peer listener is mutual TLS"
        )),
        Some(tls) => Ok(tls),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::file::parse;
    use crate::config::resolve::{ReceiverInputs, build_receiver_config, server_config_from_file};
    use std::fmt::Write as _;
    use std::path::PathBuf;

    const PEERS: &str = "receiver-0=receiver-0.wal:4320,receiver-1=receiver-1.wal:4320,\
                         receiver-2=receiver-2.wal:4320";
    const FIVE_PEERS: &str = "receiver-0=receiver-0.wal:4320,receiver-1=receiver-1.wal:4320,\
                              receiver-2=receiver-2.wal:4320,receiver-3=receiver-3.wal:4320,\
                              receiver-4=receiver-4.wal:4320";

    /// Every `receiver.wal.replicated` leaf set, `tls` aside.
    const LEAVES: [(&str, &str); 8] = [
        ("node_id", "receiver-0"),
        ("listen_addr", "0.0.0.0:4400"),
        ("peers", PEERS),
        (
            "tail_peers",
            "tail-0=tail-0.wal:4320,tail-1=tail-1.wal:4320",
        ),
        ("groups", "32"),
        ("replication_factor", "3"),
        ("tail_replicas", "2"),
        ("tail_clients", "querier-a, querier-b"),
    ];

    const MTLS: &str = "        cert_file: /tls/peer.crt\n        key_file: /tls/peer.key\n        \
                        client_ca_file: /tls/ca.crt\n";

    /// A change to one leaf: `None` removes it.
    type Edit = (&'static str, Option<&'static str>);

    fn leaves(edits: &[Edit]) -> Vec<(&'static str, &'static str)> {
        let mut leaves: Vec<(&str, Option<&str>)> = LEAVES
            .iter()
            .map(|&(key, value)| (key, Some(value)))
            .collect();
        for &(key, value) in edits {
            match leaves.iter_mut().find(|(k, _)| *k == key) {
                Some(leaf) => leaf.1 = value,
                None => leaves.push((key, value)),
            }
        }
        leaves
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect()
    }

    /// The file front-end: `receiver.wal` with `backend` and the edited
    /// leaves, and `tls` as given (`None` omits the block).
    fn from_file(
        backend: Option<&str>,
        edits: &[Edit],
        tls: Option<&str>,
    ) -> Result<WalBackend, String> {
        let mut yaml = String::from(
            "storage:\n  local:\n    bucket_root: /data\nreceiver:\n  enabled: true\n  \
             wal_root: /wal\n  wal:\n",
        );
        if let Some(backend) = backend {
            writeln!(yaml, "    backend: {backend}").expect("write to a String");
        }
        yaml.push_str("    replicated:\n");
        for (key, value) in leaves(edits) {
            writeln!(yaml, "      {key}: \"{value}\"").expect("write to a String");
        }
        if let Some(tls) = tls {
            yaml.push_str("      tls:\n");
            yaml.push_str(tls);
        }
        let file = parse(&yaml, &|_| None).expect("well-formed file");
        server_config_from_file(&file).map(|config| config.receiver.expect("enabled").wal)
    }

    /// The env front-end: what `config_from_env` passes for the
    /// `OURIOS_RECEIVER_WAL_*` values, which never carry TLS.
    fn from_env(backend: Option<&str>, edits: &[Edit]) -> Result<WalBackend, String> {
        let leaves = leaves(edits);
        let leaf = |key: &str| {
            leaves
                .iter()
                .find(|(k, _)| *k == key)
                .map(|&(_, value)| value)
        };
        build_receiver_config(ReceiverInputs {
            enabled: Some("1"),
            wal_root: Some(PathBuf::from("/wal")),
            wal: WalInputs {
                backend,
                node_id: leaf("node_id"),
                listen_addr: leaf("listen_addr"),
                peers: leaf("peers"),
                tail_peers: leaf("tail_peers"),
                groups: leaf("groups"),
                replication_factor: leaf("replication_factor"),
                tail_replicas: leaf("tail_replicas"),
                tail_clients: leaf("tail_clients"),
                heartbeat_interval_ms: leaf("heartbeat_interval_ms"),
                election_timeout_min_ms: leaf("election_timeout_min_ms"),
                election_timeout_max_ms: leaf("election_timeout_max_ms"),
                tls: None,
            },
            ..ReceiverInputs::default()
        })
        .map(|params| params.expect("enabled").wal)
    }

    fn peer(id: &str) -> Peer {
        Peer {
            id: id.to_owned(),
            host: format!("{id}.wal"),
            port: 4320,
        }
    }

    /// Scenario RFC0065.1 — the backend is configurable and checked at
    /// start: `receiver.wal.backend` unset, `local`, and `replicated` with
    /// every field set resolve to local, local and replicated.
    #[test]
    fn rfc0065_1_valid_backends_resolve() {
        assert_eq!(from_file(None, &[], None), Ok(WalBackend::Local));
        assert_eq!(from_file(Some("local"), &[], None), Ok(WalBackend::Local));
        assert_eq!(from_env(None, &[]), Ok(WalBackend::Local));
        assert_eq!(from_env(Some("local"), &[]), Ok(WalBackend::Local));

        let Ok(WalBackend::Replicated(wal)) = from_file(Some("replicated"), &[], Some(MTLS)) else {
            panic!("a fully set replicated section resolves");
        };
        assert_eq!(
            *wal,
            ReplicatedWal {
                node_id: "receiver-0".to_owned(),
                role: NodeRole::Voter,
                listen_addr: "0.0.0.0:4400".parse().expect("addr"),
                peers: vec![peer("receiver-0"), peer("receiver-1"), peer("receiver-2"),],
                tail_peers: vec![peer("tail-0"), peer("tail-1")],
                groups: NonZeroU32::new(32).expect("nonzero"),
                replication_factor: ReplicationFactor::Three,
                tail_replicas: NonZeroUsize::new(2).expect("nonzero"),
                tail_clients: vec!["querier-a".to_owned(), "querier-b".to_owned()],
                tls: TlsSettings {
                    cert_file: "/tls/peer.crt".into(),
                    key_file: "/tls/peer.key".into(),
                    client_ca_file: Some("/tls/ca.crt".into()),
                    min_version: ourios_serving::tls::TlsMinVersion::V1_2,
                    reload_interval: None,
                },
                raft_timings: RaftTimings::default(),
            }
        );
    }

    #[test]
    fn replicated_defaults_and_the_tail_replica_role() {
        let defaults = [
            ("listen_addr", None),
            ("groups", None),
            ("replication_factor", None),
            ("tail_replicas", None),
        ];
        let Ok(WalBackend::Replicated(wal)) = from_file(Some("replicated"), &defaults, Some(MTLS))
        else {
            panic!("the defaulted leaves resolve");
        };
        assert_eq!(
            wal.listen_addr,
            DEFAULT_PEER_LISTEN_ADDR.parse().expect("addr")
        );
        assert_eq!(wal.groups, DEFAULT_GROUPS);
        assert_eq!(wal.replication_factor, ReplicationFactor::Three);
        assert_eq!(wal.tail_replicas, DEFAULT_TAIL_REPLICAS);

        let edits = [
            ("node_id", Some("tail-1")),
            ("peers", Some(FIVE_PEERS)),
            ("replication_factor", Some("5")),
        ];
        let Ok(WalBackend::Replicated(wal)) = from_file(Some("replicated"), &edits, Some(MTLS))
        else {
            panic!("a tail replica resolves");
        };
        assert_eq!(wal.role, NodeRole::TailReplica);
        assert_eq!(wal.replication_factor.voters(), 5);
    }

    /// Each invalid input, with the field its error must name.
    const INVALID: [(&str, &[Edit], &str); 28] = [
        // The criterion's inputs.
        ("no peers", &[("peers", None)], "peers"),
        (
            "node_id in neither list",
            &[("node_id", Some("receiver-9"))],
            "node_id",
        ),
        (
            "replication_factor 4",
            &[("replication_factor", Some("4"))],
            "replication_factor",
        ),
        (
            "replication_factor above peers",
            &[("replication_factor", Some("5"))],
            "replication_factor",
        ),
        (
            "tail_replicas 0",
            &[("tail_replicas", Some("0"))],
            "tail_replicas",
        ),
        // §3.8's other rules.
        ("no node_id", &[("node_id", None)], "node_id"),
        (
            "node_id in both lists",
            &[("tail_peers", Some("receiver-0=receiver-0.wal:4320"))],
            "node_id",
        ),
        (
            "another id in both lists",
            &[
                ("peers", Some(FIVE_PEERS)),
                ("tail_peers", Some("receiver-4=t:4320")),
            ],
            "tail_peers",
        ),
        (
            "a peer without an address",
            &[("peers", Some("receiver-0"))],
            "peers",
        ),
        (
            "a peer without a port",
            &[("peers", Some("receiver-0=host"))],
            "peers",
        ),
        (
            "a peer with a scheme",
            &[(
                "peers",
                Some("receiver-0=https://receiver-0.wal:4320,receiver-1=b:4320,receiver-2=c:4320"),
            )],
            "peers",
        ),
        (
            "a peer host with a space",
            &[(
                "peers",
                Some("receiver-0=bad host:4320,receiver-1=b:4320,receiver-2=c:4320"),
            )],
            "peers",
        ),
        (
            "peers missing a comma",
            &[(
                "peers",
                Some("receiver-0=a:4320receiver-1=b:4320,receiver-2=c:4320,receiver-3=d:4320"),
            )],
            "peers",
        ),
        (
            "tail_peers missing a comma",
            &[("tail_peers", Some("tail-0=t0:4320tail-1=t1:4320"))],
            "tail_peers",
        ),
        (
            "a tail peer with a scheme",
            &[("tail_peers", Some("tail-0=http://t0:4320"))],
            "tail_peers",
        ),
        (
            "a peer on port 0",
            &[("peers", Some("receiver-0=host:0"))],
            "peers",
        ),
        (
            "a peer without an id",
            &[("peers", Some("=host:4320"))],
            "peers",
        ),
        (
            "an empty peers entry",
            &[("peers", Some("receiver-0=h:1,,receiver-1=h:2"))],
            "peers",
        ),
        (
            "a repeated peer id",
            &[("peers", Some("receiver-0=a:4320,receiver-0=b:4320"))],
            "peers",
        ),
        ("no tail_peers", &[("tail_peers", None)], "tail_peers"),
        (
            "a malformed tail peer",
            &[("tail_peers", Some("tail-0"))],
            "tail_peers",
        ),
        (
            "replication_factor not a number",
            &[("replication_factor", Some("three"))],
            "replication_factor",
        ),
        (
            "tail_replicas above tail_peers",
            &[("tail_replicas", Some("3"))],
            "tail_replicas",
        ),
        ("no tail_clients", &[("tail_clients", None)], "tail_clients"),
        (
            "an empty tail_clients entry",
            &[("tail_clients", Some("a,,b"))],
            "tail_clients",
        ),
        ("groups 0", &[("groups", Some("0"))], "groups"),
        ("groups not a number", &[("groups", Some("many"))], "groups"),
        (
            "a malformed listen_addr",
            &[("listen_addr", Some("0.0.0.0"))],
            "listen_addr",
        ),
    ];

    /// Scenario RFC0065.1, the invalid arm: each of the criterion's
    /// inputs, then the rest of §3.8's rules, fails naming the field,
    /// through both front-ends.
    #[test]
    fn rfc0065_1_invalid_replicated_configs_fail_naming_the_field() {
        for (_, edits, field) in INVALID {
            let key = format!("receiver.wal.replicated.{field} ");
            assert_both_front_ends_fail("replicated", edits, &key);
        }

        let tls_cases = [
            ("no tls", None, "receiver.wal.replicated.tls "),
            (
                "tls without a client CA",
                Some("        cert_file: /tls/peer.crt\n        key_file: /tls/peer.key\n"),
                "receiver.wal.replicated.tls.client_ca_file ",
            ),
            (
                "half a tls pair",
                Some("        cert_file: /tls/peer.crt\n"),
                "receiver.wal.replicated.tls.key_file ",
            ),
        ];
        for (case, tls, key) in tls_cases {
            let error = from_file(Some("replicated"), &[], tls).expect_err(case);
            assert!(error.starts_with(key), "{case}: {error}");
        }
        let error = from_env(Some("replicated"), &[]).expect_err("TLS is config-file only");
        assert!(error.starts_with("receiver.wal.replicated.tls "), "{error}");

        for backend in ["kafka", "Replicated", "raft"] {
            assert_both_front_ends_fail(backend, &[], "receiver.wal.backend ");
        }
    }

    /// Each invalid Raft timing, with the key its error must name.
    const INVALID_TIMINGS: [(&[Edit], &str); 9] = [
        (
            &[("heartbeat_interval_ms", Some("0"))],
            "heartbeat_interval_ms",
        ),
        (
            &[("election_timeout_min_ms", Some("0"))],
            "election_timeout_min_ms",
        ),
        (
            &[("election_timeout_max_ms", Some("0"))],
            "election_timeout_max_ms",
        ),
        (
            &[("heartbeat_interval_ms", Some("-50"))],
            "heartbeat_interval_ms",
        ),
        (
            &[("election_timeout_max_ms", Some("1s"))],
            "election_timeout_max_ms",
        ),
        (
            &[("heartbeat_interval_ms", Some("150"))],
            "heartbeat_interval_ms",
        ),
        (
            &[("heartbeat_interval_ms", Some("200"))],
            "heartbeat_interval_ms",
        ),
        (
            &[("election_timeout_min_ms", Some("300"))],
            "election_timeout_min_ms",
        ),
        (
            &[("election_timeout_max_ms", Some("100"))],
            "election_timeout_min_ms",
        ),
    ];

    /// Scenario RFC0065.1, the Raft timings arm through both front-ends: a
    /// zero, non-numeric or mis-ordered timing fails naming its key, the
    /// rest of the timings at their defaults.
    #[test]
    fn rfc0065_1_invalid_raft_timings_fail_naming_the_key() {
        for (edits, key) in INVALID_TIMINGS {
            let key = format!("receiver.wal.replicated.{key} ");
            assert_both_front_ends_fail("replicated", edits, &key);
        }
    }

    fn assert_both_front_ends_fail(backend: &str, edits: &[Edit], key: &str) {
        for (front_end, result) in [
            ("file", from_file(Some(backend), edits, Some(MTLS))),
            ("env", from_env(Some(backend), edits)),
        ] {
            let case = format!("{backend} {edits:?} ({front_end})");
            let error = result.expect_err(&case);
            assert!(error.starts_with(key), "{case}: {error}");
        }
    }

    #[test]
    fn peer_hosts_may_be_dns_names_ipv4_or_bracketed_ipv6() {
        for host in [
            "receiver-0.wal.svc.cluster.local",
            "receiver-0.wal.svc.cluster.local.",
            "10.0.0.7",
            "[::1]",
            "[fd00::7]",
        ] {
            let peer = parse_peer(&format!("receiver-0={host}:4320")).expect(host);
            assert_eq!((peer.host.as_str(), peer.port), (host, 4320));
        }
        for host in [
            "::1",
            "[::1",
            "-lead.wal",
            "a..b",
            "under_score",
            ".",
            "a.wal..",
        ] {
            assert!(
                parse_peer(&format!("receiver-0={host}:4320")).is_none(),
                "{host}"
            );
        }
    }

    #[test]
    fn the_peer_listener_default_shares_no_port_with_another_listener() {
        use crate::config::resolve::{
            DEFAULT_GRPC_ADDR, DEFAULT_HTTP_ADDR, DEFAULT_QUERIER_HTTP_ADDR,
        };
        let port = |addr: &str| addr.parse::<std::net::SocketAddr>().unwrap().port();
        let peer = port(DEFAULT_PEER_LISTEN_ADDR);
        for other in [
            DEFAULT_GRPC_ADDR,
            DEFAULT_HTTP_ADDR,
            DEFAULT_QUERIER_HTTP_ADDR,
        ] {
            assert_ne!(peer, port(other), "{other}");
        }
    }

    #[test]
    fn a_disabled_receiver_never_reads_its_wal_section() {
        let params = build_receiver_config(ReceiverInputs {
            wal: WalInputs {
                backend: Some("kafka"),
                ..WalInputs::default()
            },
            ..ReceiverInputs::default()
        });
        assert_eq!(params, Ok(None));
    }

    #[test]
    fn the_local_backend_ignores_replicated_leaves() {
        let edits = [("peers", Some("not a peer list")), ("groups", Some("0"))];
        assert_eq!(
            from_file(Some("local"), &edits, None),
            Ok(WalBackend::Local)
        );
        assert_eq!(from_env(None, &edits), Ok(WalBackend::Local));
    }
}
