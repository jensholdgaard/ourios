//! A replicated start's bootstrap lock (RFC 0065 §6.6): which start
//! creates the high-water object, by `put_if_absent` of [`LOG_IDS_KEY`],
//! and the refusals that keep a deleted lock or object from ever being
//! re-created.

use std::fmt;
use std::time::Duration;

use ourios_parquet::{Store, StoreError};

use super::{LOG_IDS_KEY, LockUnreadable, LogIds};
use crate::template_ids::{
    self, HIGH_WATER_KEY, ScanOptions, Seated, TemplateIdsError, seat_replicated_root,
};

/// Whether the start holds a group's Raft state (a log or §6.1's snapshot
/// record) under its `wal_root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftState {
    Held,
    Empty,
}

/// One replicated start.
#[derive(Debug, Clone, Copy)]
pub struct LockStart<'a> {
    pub store: &'a Store,
    /// The replicated `node_id` this start runs under.
    pub node_id: &'a str,
    pub raft_state: RaftState,
    /// This node's `receiver.template_ids_allow_bootstrap` (RFC 0059 §3.5).
    pub allow_bootstrap: bool,
    pub scan: &'a ScanOptions,
    /// How long the start waits between two reads of the lock.
    pub retry: Duration,
}

/// How a start passed the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// The start holds a group's Raft state, so it came after the
    /// bootstrap and opens its groups from that state.
    FromRaftState,
    /// This start held the lock and set `seeded`, over the object as it
    /// found or created it.
    Bootstrapped(Seated),
    /// Another start had already seeded the object.
    Seeded,
}

/// Why a pass ended without passing the lock; the start reads it again
/// after [`LockStart::retry`].
#[derive(Debug)]
pub enum Waiting {
    /// The store could not be read or written.
    Store(StoreError),
    /// Another start holds the lock and has not seeded the object yet.
    Unseeded { bootstrapper: String },
}

impl Waiting {
    /// The `error.type`-style reason the start waits under.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Store(_) => "log_ids_store",
            Self::Unseeded { .. } => "log_ids_unseeded",
        }
    }
}

impl fmt::Display for Waiting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{LOG_IDS_KEY}: {e}"),
            Self::Unseeded { bootstrapper } => {
                write!(f, "{bootstrapper} has not seeded the high-water object yet")
            }
        }
    }
}

/// What shows a lock found absent was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// A `template_ids.v<k>.json` exists, and one is created only under
    /// the lock.
    HighWater,
    /// The start holds a group's Raft state, and a group exists only under
    /// a log id the lock allocated.
    RaftState,
    /// A group's `formed.json` exists beside an unseeded lock under this
    /// start's `node_id`, so log ids were allocated: the lock is not its
    /// own crash's leftover but a create that landed over a deleted lock.
    FormedGroup,
}

/// Why a start fails closed; nothing it found is re-created or replaced.
#[derive(Debug)]
pub enum StartError {
    /// The lock was deleted: re-creating it at `next` `0` would hand out
    /// log ids already in Parquet.
    LogIdsDeleted(Evidence),
    /// The lock is one no reader takes.
    Unreadable(LockUnreadable),
    /// The store cannot compare-and-swap, which the lock needs.
    NoConditionalUpdate,
    /// The high-water object could not be read or bootstrapped, or was
    /// deleted under a seeded lock (`HighWaterDeleted`).
    HighWater(TemplateIdsError),
}

impl StartError {
    /// The `error.type` the failure reports under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::LogIdsDeleted(_) => "log_ids_deleted",
            Self::Unreadable(_) => "log_ids_unreadable",
            Self::NoConditionalUpdate => "no_conditional_update",
            Self::HighWater(e) => e.error_type(),
        }
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LogIdsDeleted(Evidence::HighWater) => write!(
                f,
                "{LOG_IDS_KEY} is absent beside the high-water object, so it was deleted \
                 (LogIdsDeleted); no node re-creates it"
            ),
            Self::LogIdsDeleted(Evidence::RaftState) => write!(
                f,
                "{LOG_IDS_KEY} is absent while this replica holds a group's Raft state, so it \
                 was deleted (LogIdsDeleted); no node re-creates it"
            ),
            Self::LogIdsDeleted(Evidence::FormedGroup) => write!(
                f,
                "{LOG_IDS_KEY} is unseeded under this node beside the high-water object while a \
                 group has formed, so the lock it held was deleted (LogIdsDeleted)"
            ),
            Self::Unreadable(reason) => write!(f, "{LOG_IDS_KEY} is unreadable: {reason}"),
            Self::NoConditionalUpdate => {
                write!(f, "the store cannot compare-and-swap {LOG_IDS_KEY}")
            }
            Self::HighWater(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable(reason) => Some(reason),
            Self::HighWater(e) => Some(e),
            Self::LogIdsDeleted(_) | Self::NoConditionalUpdate => None,
        }
    }
}

/// Pass the bootstrap lock before a replicated start opens its groups.
///
/// A start holding no Raft state creates the lock while it and every
/// `template_ids.v<k>.json` are absent, and only the start whose create
/// succeeds bootstraps the object and sets `seeded`; every other start
/// waits until `seeded`. A start holding Raft state came after the
/// bootstrap and passes unless it reads the lock absent.
///
/// Each pass that waits reports why to `waiting`, which the caller logs.
///
/// # Errors
///
/// [`StartError`]: the start fails closed, and nothing it found is
/// re-created or replaced.
pub async fn start(
    start: &LockStart<'_>,
    waiting: &mut impl FnMut(&Waiting),
) -> Result<Started, StartError> {
    match start.raft_state {
        RaftState::Held => from_raft_state(start.store).await,
        RaftState::Empty if !start.store.supports_conditional_update() => {
            Err(StartError::NoConditionalUpdate)
        }
        RaftState::Empty => passed(start, waiting).await,
    }
}

/// Pass until the lock is passed or the start fails closed.
async fn passed(
    start: &LockStart<'_>,
    waiting: &mut impl FnMut(&Waiting),
) -> Result<Started, StartError> {
    let mut claim = Claim::Inherited;
    loop {
        match pass(start, &mut claim).await? {
            Pass::Done(started) => return Ok(started),
            Pass::Again => {}
            Pass::Wait(why) => {
                waiting(&why);
                tokio::time::sleep(start.retry).await;
            }
        }
    }
}

/// Only a lock read absent stops a start holding Raft state, since that
/// lock was deleted. One it cannot read or reach is left to the step that
/// allocates a log id, so the start still opens its groups and keeps
/// taking the log.
async fn from_raft_state(store: &Store) -> Result<Started, StartError> {
    match store.get(LOG_IDS_KEY).await {
        Err(e) if e.is_not_found() => Err(StartError::LogIdsDeleted(Evidence::RaftState)),
        Ok(_) | Err(_) => Ok(Started::FromRaftState),
    }
}

/// What one pass left to do.
enum Pass {
    Done(Started),
    /// The lock changed under this pass: read it again now.
    Again,
    Wait(Waiting),
}

/// How far this process has come with a lock unseeded under its own
/// `node_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// This process has not tried to create the lock, so one under its
    /// `node_id` was created by a run before it that stopped before
    /// `seeded`, and an object it finds is that run's.
    Inherited,
    /// This process tried to create the lock, whatever the create
    /// returned, so an object it finds before it has seen none is not its
    /// own: the lock had been deleted after another start bootstrapped.
    Created,
    /// This process saw no object under the lock it created, so an object
    /// it finds now is its own.
    Owned,
    /// The object stands under the lock; only `seeded` is left to set.
    Seated(Seated),
}

async fn pass(start: &LockStart<'_>, claim: &mut Claim) -> Result<Pass, StartError> {
    match start.store.get_with_etag(LOG_IDS_KEY).await {
        Ok((bytes, e_tag)) => {
            let lock = LogIds::decode(&bytes).map_err(StartError::Unreadable)?;
            let e_tag = e_tag.ok_or(StartError::NoConditionalUpdate)?;
            held(start, (lock, &e_tag), claim).await
        }
        Err(e) if e.is_not_found() => create_lock(start, claim).await,
        Err(e) => Ok(Pass::Wait(Waiting::Store(e))),
    }
}

/// Create the lock under this start, unless an object shows it was
/// deleted. Whoever's create landed, the next pass reads the lock.
async fn create_lock(start: &LockStart<'_>, claim: &mut Claim) -> Result<Pass, StartError> {
    match on_blocking(start.store, high_water_exists).await {
        Ok(true) => return created_since(start.store).await,
        Ok(false) => {}
        Err(e) => return waited_or_failed(e),
    }
    let lock = LogIds {
        bootstrapper: start.node_id.to_owned(),
        seeded: false,
        next: 0,
    };
    // A create whose reply is lost may still have landed.
    *claim = Claim::Created;
    let created = match encode(&lock) {
        Ok(bytes) => start.store.put_if_absent(LOG_IDS_KEY, bytes).await,
        Err(e) => Err(e),
    };
    match created {
        Err(e) if !e.is_already_exists() => Ok(Pass::Wait(Waiting::Store(e))),
        Ok(()) | Err(_) => Ok(Pass::Again),
    }
}

/// An object found after the lock was read absent: another start may have
/// bootstrapped in between, so the lock was deleted only if it is still
/// absent.
async fn created_since(store: &Store) -> Result<Pass, StartError> {
    match store.get(LOG_IDS_KEY).await {
        Ok(_) => Ok(Pass::Again),
        Err(e) if e.is_not_found() => Err(StartError::LogIdsDeleted(Evidence::HighWater)),
        Err(e) => Ok(Pass::Wait(Waiting::Store(e))),
    }
}

/// Whether any `template_ids.v<k>.json` exists: a later version is
/// refused as one, so it counts.
fn high_water_exists(store: &Store) -> Result<bool, TemplateIdsError> {
    match template_ids::read(store) {
        Ok(found) => Ok(found.is_some()),
        Err(TemplateIdsError::LaterVersion { .. }) => Ok(true),
        Err(e) => Err(e),
    }
}

async fn held(
    start: &LockStart<'_>,
    (lock, e_tag): (LogIds, &str),
    claim: &mut Claim,
) -> Result<Pass, StartError> {
    match lock {
        LogIds { seeded: true, .. } => seeded(start.store).await,
        _ if lock.bootstrapper == start.node_id => bootstrap(start, (lock, e_tag), claim).await,
        LogIds { bootstrapper, .. } => Ok(Pass::Wait(Waiting::Unseeded { bootstrapper })),
    }
}

/// A seeded lock: the object exists under it, or was deleted.
async fn seeded(store: &Store) -> Result<Pass, StartError> {
    match on_blocking(store, template_ids::read).await {
        Ok(Some(_)) => Ok(Pass::Done(Started::Seeded)),
        Ok(None) => Err(StartError::HighWater(TemplateIdsError::HighWaterDeleted)),
        Err(e) => waited_or_failed(e),
    }
}

/// This start holds the lock unseeded, having just created it or crashed
/// before `seeded`: no log id exists yet, so it creates the object and
/// sets `seeded`. A lock this process created goes on only with an object
/// it created itself.
async fn bootstrap(
    start: &LockStart<'_>,
    (lock, e_tag): (LogIds, &str),
    claim: &mut Claim,
) -> Result<Pass, StartError> {
    let seated = match *claim {
        Claim::Seated(seated) => seated,
        Claim::Created => return owned(start.store, claim).await,
        Claim::Owned | Claim::Inherited => match seat(start).await {
            Ok(seated) => seated,
            Err(e) => return waited_or_failed(e),
        },
    };
    *claim = Claim::Seated(seated);
    seed(start.store, (lock, e_tag), seated).await
}

/// Under a lock this process created, an object already there is another
/// start's; once it has seen none, any it finds is its own.
async fn owned(store: &Store, claim: &mut Claim) -> Result<Pass, StartError> {
    match on_blocking(store, high_water_exists).await {
        Ok(true) => withdrawn(store).await,
        Ok(false) => {
            *claim = Claim::Owned;
            Ok(Pass::Again)
        }
        Err(e) => waited_or_failed(e),
    }
}

/// RFC 0059 §3.5 under the lock, going on with an object already there.
async fn seat(start: &LockStart<'_>) -> Result<Seated, TemplateIdsError> {
    let allow_bootstrap = start.allow_bootstrap;
    let scan = start.scan.clone();
    let seat = move |store: &Store| seat_replicated_root(store, allow_bootstrap, &scan);
    on_blocking(start.store, seat).await
}

/// Set `seeded` on the lock read as `e_tag`, over the object `seated`.
async fn seed(
    store: &Store,
    (lock, e_tag): (LogIds, &str),
    seated: Seated,
) -> Result<Pass, StartError> {
    let lock = LogIds {
        seeded: true,
        ..lock
    };
    let swapped = match encode(&lock) {
        Ok(bytes) => store.put_if_match(LOG_IDS_KEY, bytes, e_tag).await,
        Err(e) => Err(e),
    };
    match swapped {
        Ok(()) => Ok(Pass::Done(Started::Bootstrapped(seated))),
        Err(e) if e.is_precondition() || e.is_not_found() => Ok(Pass::Again),
        Err(e) => Ok(Pass::Wait(Waiting::Store(e))),
    }
}

/// Delete the lock this process created over another start's object, so
/// that no restart takes it for its own crash and seeds it at `next` `0`.
/// A failed delete is retried by the next pass, which finds the object
/// again.
async fn withdrawn(store: &Store) -> Result<Pass, StartError> {
    match store.delete(LOG_IDS_KEY).await {
        Ok(()) => Err(StartError::LogIdsDeleted(Evidence::HighWater)),
        Err(e) => Ok(Pass::Wait(Waiting::Store(e))),
    }
}

fn encode(lock: &LogIds) -> Result<Vec<u8>, StoreError> {
    lock.encode()
        .map_err(|e| StoreError::Runtime(std::io::Error::other(e)))
}

/// A store the high-water could not be reached on is waited out; every
/// other failure fails the start closed.
fn waited_or_failed(e: TemplateIdsError) -> Result<Pass, StartError> {
    match e {
        TemplateIdsError::Store { source, .. } => Ok(Pass::Wait(Waiting::Store(*source))),
        e => Err(StartError::HighWater(e)),
    }
}

/// `f` over `store` off the runtime: the high-water's reads and the
/// bootstrap scan block.
async fn on_blocking<T: Send + 'static>(
    store: &Store,
    f: impl FnOnce(&Store) -> Result<T, TemplateIdsError> + Send + 'static,
) -> Result<T, TemplateIdsError> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .unwrap_or_else(|e| {
            Err(TemplateIdsError::Store {
                op: "read",
                key: HIGH_WATER_KEY.to_owned(),
                source: Box::new(StoreError::Runtime(std::io::Error::other(e))),
            })
        })
}
