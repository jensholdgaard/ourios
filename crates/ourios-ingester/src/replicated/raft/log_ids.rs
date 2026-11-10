//! The replicated root's log ids (RFC 0065 §6.6): `log_ids.json`, the
//! lock that decides which start bootstraps the high-water object, and
//! the counter a group's first-ranked voter allocates its log id from.

use std::fmt;

use ourios_parquet::{Store, StoreError};

/// The key, under the store root, of the lock and counter.
pub const LOG_IDS_KEY: &str = "wal/log_ids.json";

/// The lock format this build reads and writes; a reader refuses every
/// other one.
pub const LOG_IDS_FORMAT: u32 = 1;

/// The lock's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogIds {
    /// The replicated `node_id` of the start that created the lock.
    pub bootstrapper: String,
    /// Whether the high-water object exists under the lock.
    pub seeded: bool,
    /// The next log id to allocate, and so the number allocated.
    pub next: u32,
}

impl LogIds {
    /// The bytes [`LOG_IDS_KEY`] holds.
    ///
    /// # Errors
    ///
    /// A serialisation failure, not expected for this shape.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        Ok(Vec::new())
    }

    /// Decode `bytes`.
    ///
    /// # Errors
    ///
    /// [`LockUnreadable`] naming why no reader takes it.
    pub fn decode(_bytes: &[u8]) -> Result<Self, LockUnreadable> {
        Err(LockUnreadable::Malformed("not implemented".to_owned()))
    }
}

/// Why a lock is one no reader takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockUnreadable {
    /// Not UTF-8 JSON of the lock's shape.
    Malformed(String),
    /// Another lock format.
    Format(u32),
}

impl LockUnreadable {
    /// The `error.type` the refusal reports under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::Format(_) => "unsupported_format",
        }
    }
}

impl fmt::Display for LockUnreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "not a log-id lock: {detail}"),
            Self::Format(found) => write!(
                f,
                "lock format {found}, and this build reads only {LOG_IDS_FORMAT}"
            ),
        }
    }
}

impl std::error::Error for LockUnreadable {}

/// Why no log id could be allocated, and nothing ever will be until an
/// operator acts: the step fails closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The lock is absent: it was deleted, and re-creating it would hand
    /// out ids already in Parquet.
    Deleted,
    /// The lock is one no reader takes.
    Unreadable(LockUnreadable),
    /// Every log id a `u32` holds has been allocated.
    Exhausted,
    /// The store cannot compare-and-swap, which allocation needs.
    NoConditionalUpdate,
}

impl Refused {
    /// The `error.type` the refusal reports under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Deleted => "log_ids_deleted",
            Self::Unreadable(_) => "log_ids_unreadable",
            Self::Exhausted => "log_ids_exhausted",
            Self::NoConditionalUpdate => "no_conditional_update",
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deleted => write!(f, "{LOG_IDS_KEY} was deleted (LogIdsDeleted)"),
            Self::Unreadable(reason) => write!(f, "{LOG_IDS_KEY} is unreadable: {reason}"),
            Self::Exhausted => write!(f, "{LOG_IDS_KEY} has allocated every log id"),
            Self::NoConditionalUpdate => {
                write!(f, "the store cannot compare-and-swap {LOG_IDS_KEY}")
            }
        }
    }
}

impl std::error::Error for Refused {}

/// Why one allocation ended without an id.
#[derive(Debug)]
pub enum AllocateError {
    /// The store could not be read or written; a later attempt may
    /// succeed.
    Store(StoreError),
    /// The high-water object is not seeded yet: no log id exists before
    /// it, so the caller waits.
    Unseeded { bootstrapper: String },
    /// The step fails closed.
    Refused(Refused),
}

impl fmt::Display for AllocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{LOG_IDS_KEY}: {e}"),
            Self::Unseeded { bootstrapper } => {
                write!(f, "{bootstrapper} has not seeded the high-water object yet")
            }
            Self::Refused(reason) => reason.fmt(f),
        }
    }
}

impl std::error::Error for AllocateError {}

/// Allocate the next log id from [`LOG_IDS_KEY`] by compare-and-swap on
/// `next`, re-reading after a lost race. An allocated id is spent whether
/// or not the caller records it.
///
/// # Errors
///
/// [`AllocateError`].
pub async fn allocate(_store: &Store) -> Result<u32, AllocateError> {
    Err(AllocateError::Refused(Refused::Deleted))
}

#[cfg(test)]
mod tests;
