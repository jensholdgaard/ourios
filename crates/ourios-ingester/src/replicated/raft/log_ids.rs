//! The replicated root's log ids (RFC 0065 §6.6): `log_ids.json`, the
//! lock that decides which start bootstraps the high-water object, and
//! the counter a group's first-ranked voter allocates its log id from.

use std::fmt;

use ourios_parquet::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: u32,
    bootstrapper: String,
    seeded: bool,
    next: u32,
}

impl LogIds {
    /// The bytes [`LOG_IDS_KEY`] holds.
    ///
    /// # Errors
    ///
    /// A serialisation failure, not expected for this shape.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&Record {
            format: LOG_IDS_FORMAT,
            bootstrapper: self.bootstrapper.clone(),
            seeded: self.seeded,
            next: self.next,
        })
    }

    /// Decode `bytes`.
    ///
    /// # Errors
    ///
    /// [`LockUnreadable`] naming why no reader takes it.
    pub fn decode(bytes: &[u8]) -> Result<Self, LockUnreadable> {
        // The format is read on its own first, as `formed.json`'s is: another
        // format may hold other fields, which is a version mismatch.
        #[derive(Deserialize)]
        struct Header {
            format: u32,
        }
        let malformed = |e: serde_json::Error| LockUnreadable::Malformed(e.to_string());
        // Derived structs also read a JSON array positionally, which the
        // lock's shape does not allow.
        let value: Value = serde_json::from_slice(bytes).map_err(malformed)?;
        if !value.is_object() {
            return Err(LockUnreadable::Malformed(
                "the lock is not a JSON object".to_owned(),
            ));
        }
        let header: Header = serde_json::from_slice(bytes).map_err(malformed)?;
        if header.format != LOG_IDS_FORMAT {
            return Err(LockUnreadable::Format(header.format));
        }
        let record: Record = serde_json::from_slice(bytes).map_err(malformed)?;
        Ok(Self {
            bootstrapper: record.bootstrapper,
            seeded: record.seeded,
            next: record.next,
        })
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
pub async fn allocate(store: &Store) -> Result<u32, AllocateError> {
    if !store.supports_conditional_update() {
        return Err(AllocateError::Refused(Refused::NoConditionalUpdate));
    }
    loop {
        let (lock, e_tag) = read(store).await?;
        let id = next_of(&lock)?;
        let advanced = LogIds {
            next: id + 1,
            ..lock
        };
        if swapped(store, &advanced, &e_tag).await? {
            return Ok(id);
        }
    }
}

/// The lock and the `ETag` a swap of it must match.
async fn read(store: &Store) -> Result<(LogIds, String), AllocateError> {
    let refused = AllocateError::Refused;
    let (bytes, e_tag) = match store.get_with_etag(LOG_IDS_KEY).await {
        Ok(got) => got,
        Err(e) if e.is_not_found() => return Err(refused(Refused::Deleted)),
        Err(e) => return Err(AllocateError::Store(e)),
    };
    let lock = LogIds::decode(&bytes).map_err(|reason| refused(Refused::Unreadable(reason)))?;
    let e_tag = e_tag.ok_or(refused(Refused::NoConditionalUpdate))?;
    Ok((lock, e_tag))
}

/// Replace the lock with `lock` if it is still the version `e_tag` names;
/// `false` when another allocation won the race.
async fn swapped(store: &Store, lock: &LogIds, e_tag: &str) -> Result<bool, AllocateError> {
    let bytes = lock
        .encode()
        .map_err(|e| AllocateError::Store(StoreError::Runtime(std::io::Error::other(e))))?;
    match store.put_if_match(LOG_IDS_KEY, bytes, e_tag).await {
        Ok(()) => Ok(true),
        Err(e) if e.is_precondition() => Ok(false),
        Err(e) => Err(AllocateError::Store(e)),
    }
}

/// The id `lock` would hand out, if it hands out one.
fn next_of(lock: &LogIds) -> Result<u32, AllocateError> {
    match lock.next {
        _ if !lock.seeded => Err(AllocateError::Unseeded {
            bootstrapper: lock.bootstrapper.clone(),
        }),
        // `next` cannot advance past the last id, so it is never handed out:
        // a wrapped `next` would hand out ids already in Parquet.
        u32::MAX => Err(AllocateError::Refused(Refused::Exhausted)),
        next => Ok(next),
    }
}

#[cfg(test)]
mod tests;
