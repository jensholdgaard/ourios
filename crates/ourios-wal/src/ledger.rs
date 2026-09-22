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

use ourios_core::tenant::TenantId;

use crate::{
    FrameKind, FrameSink, HousekeepingError, OpenError, RecoveryError, SegmentScan, TenantBatch,
    TenantBatchError, WalOffset, frame, list_segments, replay_segment, retain,
    retain::SegmentLedger, sync_parent_dir,
};

/// What one walk of the surviving segments found.
pub(crate) struct Ledger {
    pub(crate) segments: SegmentLedger,
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
        // The ledger records the file the frames are in, not the name
        // their uuid would give: a segment an operator renamed must
        // still be unlinkable by a later pass.
        scan.path.clone_from(path);
        match replay_segment(path, Some(idx) == newest_idx, &mut scan) {
            Ok(SegmentScan::CleanTail | SegmentScan::TornTail { .. }) => {}
            Err(e) => return Err(scan.into_error(&e)),
        }
    }
    // The walk has now seen every segment on the root, which is what
    // lets a pass pop from this ledger at all.
    scan.segments.describe_root();
    Ok(Ledger {
        segments: scan.segments,
        partials: list_partials(root)?,
    })
}

/// The [`FrameSink`] the walk runs with: it records every validated
/// frame into the segment ledger and stops on the one prefix
/// RFC 0052 §3.2 makes fatal.
#[derive(Default)]
struct Scan {
    segments: SegmentLedger,
    path: PathBuf,
    overlong_tenant: Option<(WalOffset, usize)>,
}

impl Scan {
    fn record(&mut self, offset: WalOffset, bytes: u64, tenant: Option<&TenantId>) {
        self.segments.observe(retain::FrameAt {
            offset,
            bytes,
            tenant,
            path: &self.path,
        });
    }
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
        let bytes = frame::FRAME_HEADER_LEN as u64 + len;
        if kind != FrameKind::TenantOtlpBatch {
            // §3.2: only `TenantOtlpBatch` frames carry tenant
            // membership. Every other kind is governed by the
            // checkpoint alone, so it contributes bytes and a highest
            // offset and pins nothing.
            self.record(offset, bytes, None);
            return Ok(());
        }
        match TenantBatch::decode(payload) {
            Err(TenantBatchError::TenantTooLong { found }) => {
                self.overlong_tenant = Some((offset, found));
                Err(RecoveryError::SinkRejected {
                    detail: format!("tenant length {found} exceeds the RFC 0048 §3.1 bound"),
                })
            }
            // `TenantId::new`, not `try_new`: the ledger keys on the
            // identity **replay** assigns, and `recovery`'s driver
            // wraps the same prefix unvalidated. A stored tenant the
            // RFC 0048 §3.1 grammar would reject at a request boundary
            // is still one the miner rebuilds state for, so dropping
            // its membership here would leave its segments governed by
            // the checkpoint alone.
            Ok(batch) => {
                self.record(offset, bytes, Some(&TenantId::new(batch.tenant)));
                Ok(())
            }
            // Every other malformed prefix stays what it is today: an
            // invalid payload the recovery driver classifies, never a
            // reason to refuse to open. It still holds bytes, so it
            // joins the ledger under the checkpoint rule alone.
            Err(_) => {
                self.record(offset, bytes, None);
                Ok(())
            }
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

/// Whether `path` is §3.3's reserved partial **directly in `root`**.
///
/// The sweep's own seeding walks `root`, so its paths satisfy this by
/// construction; the check exists for the unlocked file half, which is
/// handed a plan whose fields a caller can reach.
pub(crate) fn is_reserved_partial(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root) && is_partial(path)
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
/// **A path leaves the list only when its removal is verified.** The
/// list is the sweep's only record — §3.3 forbids a listing on the
/// pass — so a failed unlink stays queued, an unlink whose parent
/// fsync failed stays queued as §3.2's *uncertain* deletion for the
/// next pass to re-verify (an unlink that then finds the file gone
/// completes the reclamation), and the candidates a failed pass never
/// reached stay queued untouched. Anything else would make debris
/// invisible to every later pass.
///
/// Housekeeping holds the single-writer position, so no rotation
/// attempt is in progress and every partial it sees is debris.
pub(crate) fn sweep_partials(
    partials: &mut Vec<PathBuf>,
    root: &Path,
    cap: usize,
) -> Result<(), HousekeepingError> {
    let take = partials.len().min(cap);
    let batch: Vec<PathBuf> = partials.drain(..take).collect();
    let mut queued = Vec::new();
    for (index, path) in batch.iter().enumerate() {
        match remove_partial(path, root) {
            Swept::Removed => {}
            Swept::Retry => queued.push(path.clone()),
            // One fsync covers every unlink the pass has done, so a
            // failure ends it: this path and every candidate after it
            // go back on the list before the error surfaces.
            Swept::Uncertain(source) => {
                queued.extend_from_slice(&batch[index..]);
                partials.extend(queued);
                return Err(HousekeepingError::Io {
                    op: "fsync(wal_root after unlink(partial))",
                    source,
                });
            }
        }
    }
    partials.extend(queued);
    Ok(())
}

/// What one partial's removal did. Only [`Swept::Removed`] lets the
/// path leave the list.
enum Swept {
    Removed,
    /// The unlink failed. Retried next pass, and visible through
    /// `Wal::reclaim_state` until it succeeds.
    Retry,
    /// The unlink succeeded but the parent fsync did not, so whether
    /// the entry survives a restart is unknown until re-verified.
    Uncertain(std::io::Error),
}

fn remove_partial(path: &Path, root: &Path) -> Swept {
    match std::fs::remove_file(path) {
        Ok(()) => durable_removal(root),
        // Already gone — which is *not* the same as durably gone.
        // This is the re-verification of a previous pass's uncertain
        // deletion, and what was uncertain about it was exactly the
        // fsync. Letting the path leave the list on the strength of a
        // directory change a crash can still undo would forget the
        // file entirely, so the fsync is repeated rather than assumed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => durable_removal(root),
        Err(_) => Swept::Retry,
    }
}

/// An unlink counts only once the directory entry that carried it is
/// durable.
fn durable_removal(root: &Path) -> Swept {
    match sync_parent_dir(root) {
        Ok(()) => Swept::Removed,
        Err(source) => Swept::Uncertain(source),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{is_partial, sweep_partials};

    /// RFC 0052 §3.8's proposed default cap.
    const CAP: usize = 128;

    /// The list is the sweep's only record, so a pass that fails
    /// part-way must not consume it. With the parent fsync failing,
    /// the path it just unlinked is an uncertain deletion and the
    /// candidates it never reached were never even attempted: all of
    /// them stay queued, or they become invisible to every later pass.
    #[test]
    fn a_failed_parent_fsync_leaves_the_whole_batch_queued() {
        let tmp = tempfile::TempDir::new().expect("temp");
        // Fixed names, ordered as the seeded list is: the assertion is
        // about *which* paths survive a failed pass and in what order,
        // so minting them would make the expectation unreadable.
        let debris: Vec<PathBuf> = [
            "01890c43-7b3d-7c01-9e00-00000000000a.wal.partial",
            "01890c43-7b3d-7c01-9e00-00000000000b.wal.partial",
            "01890c43-7b3d-7c01-9e00-00000000000c.wal.partial",
        ]
        .iter()
        .map(|name| {
            let path = tmp.path().join(name);
            std::fs::write(&path, b"rotation debris").expect("write");
            path
        })
        .collect();
        let mut partials = debris.clone();

        // A root the fsync cannot open: each unlink succeeds, the
        // fsync that would make it durable does not.
        let unopenable = tmp.path().join("gone");
        sweep_partials(&mut partials, &unopenable, CAP).expect_err("the parent fsync must fail");

        assert_eq!(
            partials, debris,
            "every path stays queued, in order: the uncertain one and the untried ones alike",
        );
        assert!(
            !debris[0].exists() && debris[1].exists(),
            "the pass stops at the first failure rather than unlinking the rest",
        );

        // The next pass reconciles it: an unlink that finds the file
        // gone completes the reclamation.
        sweep_partials(&mut partials, tmp.path(), CAP).expect("a pass against a real root");
        assert!(
            partials.is_empty(),
            "and the list drains once the fsync works"
        );
        assert!(debris.iter().all(|p| !p.exists()));

        // Re-verifying an already-gone path is not complete until its
        // own fsync lands: what was uncertain about the previous
        // pass's unlink was exactly the fsync, so dropping the path
        // here would forget a directory entry a crash can bring back.
        let mut requeued = vec![debris[0].clone()];
        sweep_partials(&mut requeued, &unopenable, CAP).expect_err("the parent fsync must fail");
        assert_eq!(
            requeued,
            vec![debris[0].clone()],
            "an unverified removal stays queued even when the file is already gone",
        );
    }

    /// The selector is the whole safety of the sweep: `*.tmp` is the
    /// checkpoint and snapshot namespace, and a stem that is not a
    /// UUID is an operator's file, not rotation debris.
    #[test]
    fn only_a_uuid_named_wal_partial_matches() {
        // Fixed names rather than minted ones: the selector is a
        // textual shape, so the cases belong in the test as text.
        for name in [
            "01890c43-7b3d-7c01-9e00-0123456789ab.wal.partial",
            "00000000-0000-0000-0000-000000000000.wal.partial",
        ] {
            assert!(is_partial(Path::new(name)), "{name}");
        }
        for name in [
            "foo.wal.partial",
            "CHECKPOINT.tmp",
            "RECLAIM",
            "RECLAIM.new",
            "checkout.42.snap.tmp",
            "01890c43-7b3d-7c01-9e00-0123456789ab.wal",
            "01890c43-7b3d-7c01-9e00-0123456789ab.wal.partial.bak",
            "01890c43-7b3d-7c01-9e00-0123456789ab",
        ] {
            assert!(!is_partial(Path::new(name)), "{name}");
        }
    }
}
