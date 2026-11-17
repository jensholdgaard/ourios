//! Reading a checkpoint's bundle back (RFC 0065 §6.5 *Reading*): a replica
//! restores only from the bundle at its snapshot record's cut, never a
//! later one `checkpoint.json` names, and only one that decodes, matches
//! the cut, and whose every `.snap` the bundle admits. It never lists the
//! prefix: the tenants are the ones `bundle.bin` names.

use std::fmt;

use ourios_miner::snapshot::SnapshotState;
use ourios_parquet::{Store, StoreError};

use super::{Bundle, BundleError, Cut, CutId, bundle_key, snapshot_key};

/// Why the bundle at a cut was not read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// A store call failed: the replica retries until it can read.
    Store { key: String, source: StoreError },
    /// An object of the bundle is not there.
    Missing { key: String },
    /// The bundle, or a snapshot in it, is refused.
    Refused(BundleError),
}

impl ReadError {
    /// Whether retrying may succeed: the store was not reachable.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Store { .. })
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store { key, source } => write!(f, "{key}: {source}"),
            Self::Missing { key } => write!(f, "{key} is not in the store"),
            Self::Refused(e) => write!(f, "the bundle is refused: {e}"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store { source, .. } => Some(source),
            Self::Refused(e) => Some(e),
            Self::Missing { .. } => None,
        }
    }
}

/// The bundle at `cut`, its snapshot record's cut, and its tenants' states
/// in its tenant order. Blocking: the caller runs it off the runtime.
///
/// # Errors
///
/// The [`ReadError`] that stopped it.
pub fn read(store: &Store, cut: &Cut) -> Result<(Bundle, Vec<SnapshotState>), ReadError> {
    let CutId { log, seq, .. } = cut.id;
    let bytes = get(store, &bundle_key(log, seq))?;
    let bundle = Bundle::decode(&bytes).map_err(ReadError::Refused)?;
    bundle.check(cut).map_err(ReadError::Refused)?;
    let states = bundle
        .tenants()
        .iter()
        .map(|entry| {
            let bytes = get(store, &snapshot_key(log, seq, &entry.tenant))?;
            bundle.snapshot(entry, &bytes).map_err(ReadError::Refused)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((bundle, states))
}

/// The object at `key`.
fn get(store: &Store, key: &str) -> Result<Vec<u8>, ReadError> {
    match store.get_blocking_opt(key) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(ReadError::Missing {
            key: key.to_owned(),
        }),
        Err(source) => Err(ReadError::Store {
            key: key.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests;
