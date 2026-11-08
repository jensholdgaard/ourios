//! A group's first membership (RFC 0065 §6.1). The group's first-ranked
//! voter records the UUID each voter answers `Identify` with in
//! [`formed_key`], once, with a create-if-absent; every voter whose log
//! for the group is empty initializes the group with exactly that
//! membership. A voter the record names under another UUID, one back on a
//! wiped `wal_root`, never initializes: it joins by the leader's
//! replacement, so an empty log never forms a second membership.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::time::Duration;

use openraft::errors::{Fatal, InitializeError, RaftError};
use ourios_parquet::{Store, StoreError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::host::GroupRaft;
use super::transport::{IdentifyError, Identity, Peers};
use super::types::{NodeId, RaftTypes};

/// The record format this build reads and writes; a reader refuses every
/// other one.
pub const FORMED_FORMAT: u32 = 1;

/// The key, under the store root, of `group`'s formation record.
#[must_use]
pub fn formed_key(group: u32) -> String {
    format!("wal/replicated/groups/{group}/formed.json")
}

/// A group's recorded first membership: each voter's `node_id` with the
/// replica UUID it answered `Identify` with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formation {
    group: u32,
    members: BTreeMap<String, NodeId>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: u32,
    group: u32,
    members: Vec<RecordMember>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordMember {
    node_id: String,
    replica: String,
}

impl Formation {
    /// `group`'s membership of `members`, if it is one a voter would
    /// initialize from: exactly `voters`, one UUID each.
    ///
    /// # Errors
    ///
    /// [`Unreadable`] naming why a reader would refuse it.
    pub fn new(
        group: u32,
        members: BTreeMap<String, NodeId>,
        voters: &[&str],
    ) -> Result<Self, Unreadable> {
        let formation = Self { group, members };
        formation.check_voters(formation.members.keys().cloned().collect(), voters)?;
        formation.check_replicas()?;
        Ok(formation)
    }

    #[must_use]
    pub fn group(&self) -> u32 {
        self.group
    }

    /// Each voter's `node_id` with its UUID, by `node_id` bytewise.
    #[must_use]
    pub fn members(&self) -> &BTreeMap<String, NodeId> {
        &self.members
    }

    /// The membership `openraft` initializes with: each UUID with the
    /// `node_id` it runs under.
    #[must_use]
    pub fn membership(&self) -> BTreeMap<NodeId, String> {
        self.members
            .iter()
            .map(|(node_id, replica)| (*replica, node_id.clone()))
            .collect()
    }

    /// The bytes [`formed_key`] holds.
    ///
    /// # Errors
    ///
    /// A serialisation failure, not expected for this shape.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        let members = self
            .members
            .iter()
            .map(|(node_id, replica)| RecordMember {
                node_id: node_id.clone(),
                replica: replica.uuid().hyphenated().to_string(),
            })
            .collect();
        serde_json::to_vec(&Record {
            format: FORMED_FORMAT,
            group: self.group,
            members,
        })
    }

    /// Decode `bytes`, read for `group`, whose voters are `voters`.
    ///
    /// # Errors
    ///
    /// [`Unreadable`] naming why no voter may initialize from it.
    pub fn decode(bytes: &[u8], group: u32, voters: &[&str]) -> Result<Self, Unreadable> {
        // The format is read on its own first: another format may add or
        // drop fields, and that is a version mismatch, not a corrupt record.
        #[derive(Deserialize)]
        struct Header {
            format: u32,
        }
        let malformed = |e: serde_json::Error| Unreadable::Malformed(e.to_string());
        let header: Header = serde_json::from_slice(bytes).map_err(malformed)?;
        if header.format != FORMED_FORMAT {
            return Err(Unreadable::Format(header.format));
        }
        let record: Record = serde_json::from_slice(bytes).map_err(malformed)?;
        if record.group != group {
            return Err(Unreadable::Group(record.group));
        }
        let recorded: Vec<String> = record.members.iter().map(|m| m.node_id.clone()).collect();
        let members = record
            .members
            .into_iter()
            .map(|m| Ok((m.node_id, replica(&m.replica)?)))
            .collect::<Result<BTreeMap<_, _>, Unreadable>>()?;
        let formation = Self { group, members };
        formation.check_voters(recorded, voters)?;
        formation.check_replicas()?;
        Ok(formation)
    }

    /// Where `node_id`, running as `replica`, stands in the record.
    #[must_use]
    pub fn standing(&self, node_id: &str, replica: NodeId) -> Option<Standing> {
        match self.members.get(node_id) {
            Some(&recorded) if recorded == replica => Some(Standing::Member),
            Some(&recorded) => Some(Standing::Replaced { recorded }),
            None => None,
        }
    }

    fn check_voters(&self, mut recorded: Vec<String>, voters: &[&str]) -> Result<(), Unreadable> {
        let mut expected: Vec<String> = voters.iter().map(|&v| v.to_owned()).collect();
        recorded.sort();
        expected.sort();
        if recorded.len() == self.members.len() && recorded == expected {
            Ok(())
        } else {
            Err(Unreadable::Voters { recorded, expected })
        }
    }

    fn check_replicas(&self) -> Result<(), Unreadable> {
        let mut seen = BTreeSet::new();
        match self
            .members
            .values()
            .find(|&&replica| !seen.insert(replica))
        {
            Some(&shared) => Err(Unreadable::SharedReplica(shared)),
            None => Ok(()),
        }
    }
}

/// `text` as a replica UUID, only in the lowercase hyphenated form the
/// record holds.
fn replica(text: &str) -> Result<NodeId, Unreadable> {
    match Uuid::try_parse(text) {
        Ok(uuid) if uuid.hyphenated().to_string() == text => Ok(NodeId::from(uuid)),
        _ => Err(Unreadable::Malformed(format!(
            "replica {text:?} is not a lowercase hyphenated UUID"
        ))),
    }
}

/// Where a voter stands in its group's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The record holds this replica: it initializes with the record.
    Member,
    /// The record holds another replica under this `node_id`.
    Replaced { recorded: NodeId },
}

/// Why a formation record is one no voter initializes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// Not UTF-8 JSON of the record's shape, or a replica that is not a
    /// lowercase hyphenated UUID.
    Malformed(String),
    /// Another record format.
    Format(u32),
    /// The record of another group.
    Group(u32),
    /// `node_id`s other than the group's voters, sorted.
    Voters {
        recorded: Vec<String>,
        expected: Vec<String>,
    },
    /// One UUID under two `node_id`s.
    SharedReplica(NodeId),
}

impl Unreadable {
    /// The `error.type` the refusal reports under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::Format(_) => "unsupported_format",
            Self::Group(_) => "other_group",
            Self::Voters { .. } => "other_voters",
            Self::SharedReplica(_) => "shared_replica",
        }
    }
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "not a formation record: {detail}"),
            Self::Format(found) => write!(
                f,
                "record format {found}, and this build reads only {FORMED_FORMAT}"
            ),
            Self::Group(found) => write!(f, "the record of group {found}"),
            Self::Voters { recorded, expected } => write!(
                f,
                "members {recorded:?} are not the group's voters {expected:?}"
            ),
            Self::SharedReplica(replica) => {
                write!(f, "replica {replica} is recorded for two node_ids")
            }
        }
    }
}

impl std::error::Error for Unreadable {}

/// How a voter's formation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Formed {
    /// The voter's log for the group was not empty: the group had formed.
    Joined,
    /// The voter initialized the group with the recorded membership.
    Initialized,
    /// The record holds another replica under this voter's `node_id`; it
    /// waits to join by the leader's replacement.
    Replaced { recorded: NodeId },
}

/// Why a voter could not form its group.
#[derive(Debug)]
pub enum FormError {
    /// The node is not one of the group's voters.
    NotAVoter { group: u32, node_id: String },
    /// The group's record is one no voter initializes from.
    Unreadable { group: u32, reason: Unreadable },
    /// `openraft` refused the recorded membership.
    Initialize {
        group: u32,
        source: InitializeError<RaftTypes>,
    },
    /// The group's `Raft` has stopped.
    Raft {
        group: u32,
        source: Fatal<RaftTypes>,
    },
}

impl FormError {
    /// The `error.type` the failure reports under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::NotAVoter { .. } => "not_a_voter",
            Self::Unreadable { reason, .. } => reason.error_type(),
            Self::Initialize { .. } => "initialize_refused",
            Self::Raft { .. } => "raft_stopped",
        }
    }
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAVoter { group, node_id } => {
                write!(f, "{node_id} is not a voter of group {group}")
            }
            Self::Unreadable { group, reason } => {
                write!(f, "{} is unreadable: {reason}", formed_key(*group))
            }
            Self::Initialize { group, source } => {
                write!(f, "initialize group {group}: {source}")
            }
            Self::Raft { group, source } => write!(f, "group {group}: {source}"),
        }
    }
}

impl std::error::Error for FormError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotAVoter { .. } => None,
            Self::Unreadable { reason, .. } => Some(reason),
            Self::Initialize { source, .. } => Some(source),
            Self::Raft { source, .. } => Some(source),
        }
    }
}

/// How the recording voter asks a peer who it is (§6.8).
pub trait Identifier: Sync {
    fn identify(
        &self,
        node_id: &str,
        timeout: Duration,
    ) -> impl Future<Output = Result<Identity, IdentifyError>> + Send;
}

impl Identifier for Peers {
    fn identify(
        &self,
        node_id: &str,
        timeout: Duration,
    ) -> impl Future<Output = Result<Identity, IdentifyError>> + Send {
        Peers::identify(self, node_id, timeout)
    }
}

/// One voter's seat in one group.
#[derive(Debug, Clone, Copy)]
pub struct Voter<'a> {
    pub group: u32,
    /// The group's voters in placement order
    /// ([`VotingLayout::voters`](crate::replicated::layout::VotingLayout::voters)),
    /// so the first is the one that records.
    pub voters: &'a [&'a str],
    /// The `node_id` this node runs under.
    pub node_id: &'a str,
}

impl Voter<'_> {
    fn records(&self) -> bool {
        self.voters.first() == Some(&self.node_id)
    }
}

/// How long one `Identify` call may take, and how long a voter waits
/// between passes while the record is missing or a voter has not
/// answered.
#[derive(Debug, Clone, Copy)]
pub struct Patience {
    pub identify: Duration,
    pub retry: Duration,
}

/// Where the record lives and how the recording voter reaches its peers.
pub struct Reach<'a, P> {
    pub store: &'a Store,
    pub peers: &'a P,
    pub patience: Patience,
}

/// What one pass left to do.
enum Pass {
    Done(Formed),
    /// This voter wrote the record, or lost the race to one: read it now.
    Recorded,
    Wait,
}

/// Form `voter`'s group on its `raft` (§6.1), passing until it has formed: a voter
/// whose log for the group is not empty has nothing to form; otherwise it
/// reads the record, the first-ranked voter recording it while it is
/// missing, and initializes with it if the record holds this replica.
///
/// A log is empty while `openraft` would accept `initialize`: no log id,
/// purged or not, and a vote still in term 0. A voter that has granted a
/// peer's vote has been reached by an initialized group and joins it.
///
/// Dropping the future cancels it between store, `Identify` and
/// `openraft` calls, each of which is atomic, so a later call resumes.
///
/// # Errors
///
/// [`FormError`]: an unreadable record is reported once, not retried,
/// since nothing ever replaces it.
pub async fn form<P: Identifier>(
    voter: Voter<'_>,
    raft: &GroupRaft,
    reach: &Reach<'_, P>,
) -> Result<Formed, FormError> {
    if !voter.voters.contains(&voter.node_id) {
        return Err(not_a_voter(voter));
    }
    loop {
        match pass(voter, raft, reach).await? {
            Pass::Done(formed) => return Ok(formed),
            Pass::Recorded => {}
            Pass::Wait => tokio::time::sleep(reach.patience.retry).await,
        }
    }
}

fn not_a_voter(voter: Voter<'_>) -> FormError {
    FormError::NotAVoter {
        group: voter.group,
        node_id: voter.node_id.to_owned(),
    }
}

async fn pass<P: Identifier>(
    voter: Voter<'_>,
    raft: &GroupRaft,
    reach: &Reach<'_, P>,
) -> Result<Pass, FormError> {
    let group = voter.group;
    let raft_err = |source| FormError::Raft { group, source };
    if raft.is_initialized().await.map_err(raft_err)? {
        return Ok(Pass::Done(Formed::Joined));
    }
    let bytes = match read(reach.store, group).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(group, error = %e, "read the group's formation record");
            return Ok(Pass::Wait);
        }
    };
    match bytes {
        Some(bytes) => {
            let formation = Formation::decode(&bytes, group, voter.voters)
                .map_err(|reason| FormError::Unreadable { group, reason })?;
            initialize(voter, raft, &formation).await.map(Pass::Done)
        }
        None if voter.records() => Ok(record(voter, *raft.node_id(), reach).await),
        None => Ok(Pass::Wait),
    }
}

async fn initialize(
    voter: Voter<'_>,
    raft: &GroupRaft,
    formation: &Formation,
) -> Result<Formed, FormError> {
    let group = voter.group;
    match formation.standing(voter.node_id, *raft.node_id()) {
        Some(Standing::Replaced { recorded }) => return Ok(Formed::Replaced { recorded }),
        Some(Standing::Member) => {}
        None => return Err(not_a_voter(voter)),
    }
    match raft.initialize(formation.membership()).await {
        Ok(()) => Ok(Formed::Initialized),
        // A peer's vote or entries reached the log since the pass looked.
        Err(RaftError::APIError(InitializeError::NotAllowed(_))) => Ok(Formed::Joined),
        Err(RaftError::APIError(source)) => Err(FormError::Initialize { group, source }),
        Err(RaftError::Fatal(source)) => Err(FormError::Raft { group, source }),
    }
}

/// Record the group's membership once every other voter has answered
/// `Identify`, `replica` being this voter's own UUID; the first-ranked
/// voter's pass while no record exists.
async fn record<P: Identifier>(voter: Voter<'_>, replica: NodeId, reach: &Reach<'_, P>) -> Pass {
    let group = voter.group;
    let Some(members) = answers(voter, replica, reach).await else {
        return Pass::Wait;
    };
    let encoded = Formation::new(group, members, voter.voters)
        .map_err(|e| e.to_string())
        .and_then(|formation| formation.encode().map_err(|e| e.to_string()));
    let bytes = match encoded {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(group, error = %e, "the voters' answers are not a membership");
            return Pass::Wait;
        }
    };
    match reach.store.put_if_absent(&formed_key(group), bytes).await {
        Ok(()) => Pass::Recorded,
        Err(e) if e.is_already_exists() => Pass::Recorded,
        Err(e) => {
            tracing::warn!(group, error = %e, "write the group's formation record");
            Pass::Wait
        }
    }
}

/// Every voter's `node_id` with the UUID it answered with, this one's
/// being `replica`, once all have answered.
async fn answers<P: Identifier>(
    voter: Voter<'_>,
    replica: NodeId,
    reach: &Reach<'_, P>,
) -> Option<BTreeMap<String, NodeId>> {
    let mut members = BTreeMap::from([(voter.node_id.to_owned(), replica)]);
    for &peer in voter.voters.iter().filter(|&&peer| peer != voter.node_id) {
        let answer = reach.peers.identify(peer, reach.patience.identify).await;
        let identity = answer
            .inspect_err(|e| {
                let group = voter.group;
                tracing::debug!(group, peer, error = ?e, "a voter has not answered Identify");
            })
            .ok()?;
        members.insert(peer.to_owned(), identity.replica);
    }
    Some(members)
}

/// `group`'s record, if one exists. A missing record is expected until
/// the group forms, so it is read as absent rather than failed.
async fn read(store: &Store, group: u32) -> Result<Option<Vec<u8>>, StoreError> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.get_blocking_opt(&formed_key(group)))
        .await
        .unwrap_or_else(|e| Err(StoreError::Runtime(std::io::Error::other(e))))
}

#[cfg(test)]
mod tests;
