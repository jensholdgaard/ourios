//! Writing a checkpoint's bundle (RFC 0065 §6.5 *Writing*): every `.snap`,
//! then `bundle.bin`, then `checkpoint.json`, each durable before the next,
//! so an applied cut always names a complete bundle its proposer wrote.

use std::fmt;

use ourios_parquet::{Store, StoreError};

use super::pointer::{self, PointerError};
use super::{Bundle, BundleError, CutId, bundle_key, snapshot_key};

/// What became of `checkpoint.json` once the bundle was durable. Either
/// way the bundle is complete, and its checkpoint entry is proposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// `checkpoint.json` now names this cut.
    Advanced,
    /// It already named this cut or a later one, a former flusher's or an
    /// earlier attempt's, and is left as it was.
    Behind(CutId),
}

/// Why a checkpoint's write failed; it proposes nothing and purges nothing.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// A snapshot the bundle would not restore, or one per tenant missing.
    Bundle(BundleError),
    /// The bundle names one `.snap` per tenant, and another number came.
    Snapshots { tenants: usize, snapshots: usize },
    /// An object of the bundle already holds other bytes, and is never
    /// overwritten.
    Differs { key: String },
    /// `checkpoint.json` is unreadable, and is never replaced.
    Unreadable(PointerError),
    /// `checkpoint.json` came without an `ETag`, so nothing can guard its
    /// replacement.
    Untagged,
    /// The store has no compare-and-swap, which RFC 0065 §6.2 requires.
    NoCompareAndSwap,
    /// A store call failed.
    Store { key: String, source: StoreError },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundle(e) => write!(f, "bundle refused: {e}"),
            Self::Snapshots { tenants, snapshots } => {
                write!(f, "{snapshots} snapshots for {tenants} tenants")
            }
            Self::Differs { key } => write!(f, "{key} already holds other bytes"),
            Self::Unreadable(e) => write!(f, "{e}"),
            Self::Untagged => f.write_str("checkpoint.json was read without an ETag"),
            Self::NoCompareAndSwap => f.write_str("the store has no compare-and-swap"),
            Self::Store { key, source } => write!(f, "{key}: {source}"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bundle(e) => Some(e),
            Self::Unreadable(e) => Some(e),
            Self::Store { source, .. } => Some(source),
            Self::Snapshots { .. }
            | Self::Differs { .. }
            | Self::Untagged
            | Self::NoCompareAndSwap => None,
        }
    }
}

/// Write `bundle` with `snapshots`, its tenants' `.snap` bytes in its
/// tenant order, then advance `checkpoint.json` to its cut.
///
/// Blocking: the caller runs it off the async runtime.
///
/// # Errors
///
/// The [`WriteError`] that stopped it. Objects already written stay: each
/// is immutable and a retry with the same bytes passes over it.
pub fn write(
    store: &Store,
    bundle: &Bundle,
    snapshots: &[Vec<u8>],
) -> Result<Published, WriteError> {
    if !store.supports_conditional_update() {
        return Err(WriteError::NoCompareAndSwap);
    }
    let tenants = bundle.tenants();
    if tenants.len() != snapshots.len() {
        return Err(WriteError::Snapshots {
            tenants: tenants.len(),
            snapshots: snapshots.len(),
        });
    }
    for (entry, bytes) in tenants.iter().zip(snapshots) {
        bundle.snapshot(entry, bytes).map_err(WriteError::Bundle)?;
    }
    let CutId { log, seq, .. } = bundle.cut();
    for (entry, bytes) in tenants.iter().zip(snapshots) {
        put_once(store, &snapshot_key(log, seq, &entry.tenant), bytes)?;
    }
    let encoded = bundle.encode().map_err(WriteError::Bundle)?;
    put_once(store, &bundle_key(log, seq), &encoded)?;
    advance(store, bundle.cut())
}

fn failed(key: &str) -> impl FnOnce(StoreError) -> WriteError + '_ {
    move |source| WriteError::Store {
        key: key.to_owned(),
        source,
    }
}

/// Create `key` with `bytes`, or find it already holding them, as a retry
/// of this put does; durable either way.
fn put_once(store: &Store, key: &str, bytes: &[u8]) -> Result<(), WriteError> {
    match store.put_if_absent_blocking(key, bytes.to_vec()) {
        Ok(()) => {}
        Err(e) if e.is_already_exists() => {
            if store.get_blocking(key).map_err(failed(key))? != bytes {
                return Err(WriteError::Differs {
                    key: key.to_owned(),
                });
            }
        }
        Err(e) => return Err(failed(key)(e)),
    }
    store.sync_local_blocking(key).map_err(failed(key))
}

/// Move `checkpoint.json` up to `cut`, never back: a stored cut at or
/// above it stays, and a lost race re-reads and decides again.
fn advance(store: &Store, cut: CutId) -> Result<Published, WriteError> {
    let key = pointer::key(cut.log);
    loop {
        let put = match store
            .get_with_etag_blocking_opt(&key)
            .map_err(failed(&key))?
        {
            None => store.put_if_absent_blocking(&key, pointer::encode(cut)),
            Some((bytes, e_tag)) => {
                let stored = pointer::decode(&bytes, cut.log).map_err(WriteError::Unreadable)?;
                if stored.seq >= cut.seq {
                    // It may be this cut's own earlier put, whose sync failed.
                    store.sync_local_blocking(&key).map_err(failed(&key))?;
                    return Ok(Published::Behind(stored));
                }
                let Some(tag) = e_tag else {
                    return Err(WriteError::Untagged);
                };
                store.put_if_match_blocking(&key, pointer::encode(cut), &tag)
            }
        };
        match put {
            Ok(()) => {
                store.sync_local_blocking(&key).map_err(failed(&key))?;
                return Ok(Published::Advanced);
            }
            Err(e) if e.is_precondition() || e.is_already_exists() => {}
            Err(e) => return Err(failed(&key)(e)),
        }
    }
}

#[cfg(test)]
mod tests;
