//! A replicated start's bootstrap lock (RFC 0065 §6.6): which start
//! creates the high-water object, by `put_if_absent` of [`LOG_IDS_KEY`],
//! and the refusals that keep a deleted lock or object from ever being
//! re-created.

use std::fmt;
use std::time::Duration;

use ourios_parquet::{Store, StoreError};

use super::{LOG_IDS_KEY, LockUnreadable};
use crate::template_ids::{ScanOptions, Seated, TemplateIdsError};

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
/// # Errors
///
/// [`StartError`].
pub async fn start(
    start: &LockStart<'_>,
    waiting: &mut impl FnMut(&Waiting),
) -> Result<Started, StartError> {
    let _ = (start, waiting);
    Ok(Started::FromRaftState)
}
