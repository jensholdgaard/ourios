//! The in-memory ledger RFC 0052 §3.7 rebuilds from the surviving
//! segments: the unreclaimed-byte figure and the stale
//! `*.wal.partial` list the housekeeping sweep pops from.
//!
//! This is a scan, not a `stat`. The byte figure is the sum of the
//! *validated* frames' lengths — never file size less header, which
//! would count a torn tail — and it is taken after recovery has healed
//! the newest segment, so a figure taken at `Wal::open` would be the
//! wrong one.

use std::path::{Path, PathBuf};

use crate::{
    FrameKind, FrameSink, HousekeepingError, OpenError, RecoveryError, SegmentScan, TenantBatch,
    TenantBatchError, WalOffset, frame, list_segments, reclaim, replay_segment, sync_parent_dir,
};

/// What one walk of the surviving segments found.
pub(crate) struct Ledger {
    pub(crate) bytes: u64,
    pub(crate) partials: Vec<PathBuf>,
}

/// Errors from [`crate::Wal::rebuild_ledger`].
#[derive(Debug)]
pub enum LedgerError {
    /// Filesystem I/O failure (segment listing, segment open, segment
    /// read, or the root listing the partial list is seeded from).
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// A frame carries a tenant longer than
    /// [`TenantBatch::MAX_TENANT_BYTES`] — a frame written before
    /// RFC 0052 §3.2 amended the codec to RFC 0048 §3.1's bound.
    /// Startup fails closed on it: truncating the key would cross two
    /// tenants in a record read as proof of loss, and dropping the
    /// frame would lose acknowledged data.
    TenantTooLong {
        offset: WalOffset,
        found: usize,
        limit: usize,
    },
    /// A segment header or frame failed the walk's validation. Replay
    /// surfaces the same state with its own typed reason; the ledger
    /// scan runs after it and carries the detail through.
    Scan { detail: String },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "WAL ledger rebuild failed at {op}: {source}"),
            Self::TenantTooLong {
                offset,
                found,
                limit,
            } => write!(
                f,
                "frame at segment {} byte {} carries a {found} B tenant, above the {limit} B bound (RFC 0048 §3.1)",
                offset.segment, offset.byte,
            ),
            Self::Scan { detail } => write!(f, "WAL ledger rebuild failed scanning: {detail}"),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::TenantTooLong { .. } | Self::Scan { .. } => None,
        }
    }
}

/// Walk every surviving segment oldest-first, then seed the partial
/// list from the directory — the one listing §3.3 allows, since
/// recovery walks the root anyway and the sweep itself must list
/// nothing under the writer position.
pub(crate) fn rebuild(root: &Path) -> Result<Ledger, LedgerError> {
    let segments = list_segments(root).map_err(|e| match e {
        OpenError::Io { op, source } => LedgerError::Io { op, source },
        OpenError::InvalidConfig { .. } | OpenError::Corrupt { .. } => {
            unreachable!("list_segments only surfaces OpenError::Io")
        }
    })?;
    let mut scan = Scan::default();
    let newest_idx = segments.len().checked_sub(1);
    for (idx, path) in segments.iter().enumerate() {
        match replay_segment(path, Some(idx) == newest_idx, &mut scan) {
            Ok(SegmentScan::CleanTail | SegmentScan::TornTail { .. }) => {}
            Err(e) => return Err(scan.into_error(&e)),
        }
    }
    Ok(Ledger {
        bytes: scan.bytes,
        partials: list_partials(root)?,
    })
}

/// The [`FrameSink`] the walk runs with: it sums validated frame bytes
/// and stops on the one prefix RFC 0052 §3.2 makes fatal.
#[derive(Default)]
struct Scan {
    bytes: u64,
    overlong_tenant: Option<(WalOffset, usize)>,
}

impl Scan {
    fn into_error(self, scan: &RecoveryError) -> LedgerError {
        match self.overlong_tenant {
            Some((offset, found)) => LedgerError::TenantTooLong {
                offset,
                found,
                limit: TenantBatch::MAX_TENANT_BYTES,
            },
            None => LedgerError::Scan {
                detail: format!("{scan:?}"),
            },
        }
    }
}

impl FrameSink for Scan {
    fn consume(
        &mut self,
        offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        let len = u64::try_from(payload.len())
            .expect("payload.len() fits u64 (read_frame capped it at MAX_FRAME_BYTES)");
        self.bytes += frame::FRAME_HEADER_LEN as u64 + len;
        if kind != FrameKind::TenantOtlpBatch {
            return Ok(());
        }
        match TenantBatch::decode(payload) {
            Err(TenantBatchError::TenantTooLong { found }) => {
                self.overlong_tenant = Some((offset, found));
                Err(RecoveryError::SinkRejected {
                    detail: format!("tenant length {found} exceeds the RFC 0048 §3.1 bound"),
                })
            }
            // Every other malformed prefix stays what it is today: an
            // invalid payload the recovery driver classifies, never a
            // reason to refuse to open.
            Ok(_) | Err(_) => Ok(()),
        }
    }
}

/// Stale `<uuid>.wal.partial` files in the WAL root, sorted. The name
/// is **reserved and exact** (RFC 0052 §3.3): `*.tmp` is never
/// matched — it stays the checkpoint and snapshot namespace — and a
/// name whose stem does not parse as a UUID is ignored like any other
/// non-segment file.
fn list_partials(root: &Path) -> Result<Vec<PathBuf>, LedgerError> {
    let io = |op: &'static str| move |source| LedgerError::Io { op, source };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).map_err(io("read_dir(wal_root)"))? {
        let path = entry.map_err(io("read_dir_entry(wal_root)"))?.path();
        if is_partial(&path) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn is_partial(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".wal.partial"))
        .is_some_and(|stem| stem.parse::<uuid::Uuid>().is_ok())
}

/// Unlink stale `<uuid>.wal.partial` files, oldest first, within the
/// per-pass cap (RFC 0052 §3.3). Each unlink is followed by a
/// parent-directory fsync, so a crash cannot resurrect debris the pass
/// already removed — including when a later unlink in the same pass
/// fails.
///
/// A failed unlink keeps its path in `partials`, which both retries it
/// on the next pass and keeps it visible through
/// `Wal::reclaim_state`. Housekeeping holds the single-writer
/// position, so no rotation attempt is in progress and every partial
/// it sees is debris.
pub(crate) fn sweep_partials(
    partials: &mut Vec<PathBuf>,
    root: &Path,
) -> Result<(), HousekeepingError> {
    let cap = usize::try_from(reclaim::DEFAULT_MAX_UNLINKS_PER_PASS).unwrap_or(usize::MAX);
    let take = partials.len().min(cap);
    let batch: Vec<PathBuf> = partials.drain(..take).collect();
    let mut retry = Vec::new();
    for path in batch {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                sync_parent_dir(root).map_err(|source| HousekeepingError::Io {
                    op: "fsync(wal_root after unlink(partial))",
                    source,
                })?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => retry.push(path),
        }
    }
    partials.extend(retry);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_partial;

    /// The selector is the whole safety of the sweep: `*.tmp` is the
    /// checkpoint and snapshot namespace, and a stem that is not a
    /// UUID is an operator's file, not rotation debris.
    #[test]
    fn only_a_uuid_named_wal_partial_matches() {
        let uuid = uuid::Uuid::now_v7();
        for name in [
            format!("{uuid}.wal.partial"),
            format!("{}.wal.partial", uuid::Uuid::nil()),
        ] {
            assert!(is_partial(std::path::Path::new(&name)), "{name}");
        }
        for name in [
            "foo.wal.partial",
            "CHECKPOINT.tmp",
            "RECLAIM",
            "RECLAIM.new",
            "checkout.42.snap.tmp",
            &format!("{uuid}.wal"),
            &format!("{uuid}.wal.partial.bak"),
        ] {
            assert!(!is_partial(std::path::Path::new(name)), "{name}");
        }
    }
}
