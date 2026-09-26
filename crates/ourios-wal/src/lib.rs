//! `ourios-wal` — RFC 0008 write-ahead log.
//!
//! **Status: RFC 0008 `accepted`.** All §5 acceptance arms (.1–.10) are
//! green with no `#[ignore]`'d stubs remaining: `open`, `append` (with
//! §6.5 rotation), `sync`, `replay`, `checkpoint`, `housekeeping`, and
//! `metrics` back wal-before-ack, crash recovery (the real-SIGKILL CI
//! gate), recovery O(N), torn-write heal, corruption halt, segment
//! rotation, checkpoint + durable sidecar, batched-fsync group commit,
//! the unflushed-bytes bound, and the startup recovery driver. The one
//! deferral is the §9 corruption *audit event* (`encode_audit_event`
//! stays `unimplemented!()` pending a system-scoped-audit design). See
//! RFC 0008 for the design contract.
//!
//! The shape of the public API follows §6.1 verbatim — the
//! same `(WalOffset, FrameKind, FrameSink, Wal)` surface the
//! RFC pins. Implementation details (segment file layout,
//! frame format, fsync policy, recovery walk, checkpoint
//! sidecar) are spelled out in §§6.2–6.7; the durability
//! (`sync`, §6.3) and crash-recovery (`replay`, §6.6) halves
//! land here, with the remaining slices in follow-up PRs.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, ErrorKind};
use std::path::PathBuf;

use ourios_core::audit::AuditEvent;

pub(crate) mod checkpoint;
// `frame` is crate-internal, but the `fuzzing` feature exposes it so the
// `fuzz/` cargo-fuzz targets can drive `read_frame` directly (RFC 0015).
// Not part of the stable public API.
#[cfg(not(feature = "fuzzing"))]
pub(crate) mod frame;
#[cfg(feature = "fuzzing")]
pub mod frame;
pub(crate) mod housekeeping;
pub(crate) mod ledger;
pub(crate) mod pass;
// The codec's tombstone and union entry points land ahead of their
// callers: tenant removal and RFC 0053's `PUBLISHED` dictionary are
// the slices that reach them.
#[allow(dead_code)]
pub(crate) mod reclaim;
pub(crate) mod reclaim_store;
pub(crate) mod reconcile;
pub(crate) mod retain;
pub(crate) mod rotation;
pub(crate) mod segment;

pub use ledger::LedgerError;
pub use pass::{
    HousekeepingProgress, PassId, PassOutcome, PlannedSegment, ReclaimError, ReclaimOutcome,
    ReclaimPlan, SkipReason, UnlinkPermit, unlink_failure, unlink_planned,
};
pub use reclaim::{
    DEFAULT_MAX_TENANTS, DEFAULT_MAX_UNLINKS_PER_PASS, MAX_TENANTS_CEILING,
    MAX_UNLINKS_PER_PASS_CEILING,
};
pub use retain::{RetainFloor, SnapshotHorizons, TenantHorizon};
use rotation::DirFsync;
pub use rotation::{RotationFault, RotationFaults, RotationKind, RotationSite, RotationState};
use segment::{SEGMENT_HEADER_LEN, SegmentHeader, write_header};

// -----------------------------------------------------------
// Public types (RFC 0008 §6.1 + §6.2.2)
// -----------------------------------------------------------

/// Opaque, totally-ordered position of a frame in the WAL —
/// `(segment, byte)` per RFC 0008 §6.1, where `segment` is a
/// `Uuid` minted as `UUIDv7` (chronological, sortable) and
/// `byte` is the byte offset within that segment. Ordering is
/// lexicographic on the pair, so `UUIDv7`'s chronological
/// sort gives global monotonicity even after housekeeping
/// deletes older segments. A pure
/// `u64` representation would be ambiguous after deletion
/// (no global offset survives reconstruction from the
/// UUID-named files), so the pair is the durable form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalOffset {
    pub segment: uuid::Uuid,
    pub byte: u64,
}

impl Ord for WalOffset {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.segment
            .cmp(&other.segment)
            .then_with(|| self.byte.cmp(&other.byte))
    }
}

impl PartialOrd for WalOffset {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Frame-kind discriminator per RFC 0008 §6.2.2. The reserved
/// range (`0x04..=0xFF`) is rejected on read as RFC0008.5
/// corruption — the format admits future kinds without a
/// version bump but only when they're added here.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `ExportLogsServiceRequest` protobuf bytes the receiver
    /// decoded, verbatim. Written by receivers before RFC 0046; the
    /// frame carries no tenant, so current replay refuses it as
    /// unsupported (not corruption) — see [`TenantBatch`].
    OtlpBatch = 0x01,
    /// One serialised [`AuditEvent`]. The exact encoding is still
    /// deferred per RFC 0008 §9 — `encode_audit_event` is
    /// `unimplemented!()` pending the system-scoped-audit design — but
    /// the frame layout does not depend on it.
    AuditEvent = 0x02,
    /// RFC 0046 §3.3: the tenant the export was acknowledged under,
    /// then the `ExportLogsServiceRequest` protobuf bytes —
    /// [`TenantBatch`] is the payload codec.
    TenantOtlpBatch = 0x03,
}

/// The `TenantOtlpBatch` payload (RFC 0046 §3.3):
/// `u16 LE tenant byte length ‖ tenant bytes (UTF-8) ‖ protobuf`.
///
/// The tenant is validated on decode *before* the protobuf is
/// touched: a zero length, a length above [`TenantBatch::MAX_TENANT_BYTES`],
/// a length running past the payload, or invalid UTF-8 is a
/// [`TenantBatchError`] — the recovery driver classifies it as an
/// invalid payload (like an undecodable protobuf), not as RFC0008.5
/// corruption, because the frame's own CRC passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantBatch<'a> {
    pub tenant: &'a str,
    pub protobuf: &'a [u8],
}

impl<'a> TenantBatch<'a> {
    /// The tenant-id bound, enforced on encode and decode. RFC 0048
    /// §3.1 ("Tenant id grammar (amends RFC 0046 §3.1)") lowers it to
    /// 128, and RFC 0052 §3.2 amends this codec to match: a frame
    /// carrying a 129-to-256-byte tenant is one no request boundary
    /// would have produced and one the `RECLAIM` dictionary record
    /// cannot represent. Aliased to `ourios-core`'s constant rather
    /// than repeated, since that is the spec the boundary validates
    /// against and two numbers could drift apart.
    pub const MAX_TENANT_BYTES: usize = ourios_core::tenant::MAX_TENANT_BYTES;

    /// Encode `tenant` + `protobuf` into a `TenantOtlpBatch` payload.
    ///
    /// # Errors
    ///
    /// [`TenantBatchError`] if `tenant` is empty or longer than
    /// [`Self::MAX_TENANT_BYTES`] — callers validate the selector first
    /// (RFC 0046 §3.1); this is the codec's own guard.
    pub fn encode(tenant: &str, protobuf: &[u8]) -> Result<Vec<u8>, TenantBatchError> {
        let len = tenant.len();
        if len == 0 {
            return Err(TenantBatchError::EmptyTenant);
        }
        if len > Self::MAX_TENANT_BYTES {
            return Err(TenantBatchError::TenantTooLong { found: len });
        }
        // `len <= MAX_TENANT_BYTES` fits u16 by the check above.
        #[allow(clippy::cast_possible_truncation)]
        let prefix = (len as u16).to_le_bytes();
        let mut out = Vec::with_capacity(2 + len + protobuf.len());
        out.extend_from_slice(&prefix);
        out.extend_from_slice(tenant.as_bytes());
        out.extend_from_slice(protobuf);
        Ok(out)
    }

    /// Decode a `TenantOtlpBatch` payload, validating the tenant prefix
    /// before exposing the protobuf bytes.
    ///
    /// # Errors
    ///
    /// [`TenantBatchError`] on a truncated prefix, an empty or oversize
    /// tenant, a length past the payload end, or invalid UTF-8.
    pub fn decode(payload: &'a [u8]) -> Result<Self, TenantBatchError> {
        let Some((prefix, rest)) = payload.split_first_chunk::<2>() else {
            return Err(TenantBatchError::TruncatedPrefix {
                found: payload.len(),
            });
        };
        let len = usize::from(u16::from_le_bytes(*prefix));
        if len == 0 {
            return Err(TenantBatchError::EmptyTenant);
        }
        if len > Self::MAX_TENANT_BYTES {
            return Err(TenantBatchError::TenantTooLong { found: len });
        }
        if len > rest.len() {
            return Err(TenantBatchError::TenantPastEnd {
                declared: len,
                available: rest.len(),
            });
        }
        let (tenant_bytes, protobuf) = rest.split_at(len);
        let tenant = std::str::from_utf8(tenant_bytes).map_err(|_| TenantBatchError::NotUtf8)?;
        Ok(Self { tenant, protobuf })
    }
}

/// A `TenantOtlpBatch` payload whose tenant prefix is not the documented
/// shape (RFC 0046 §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TenantBatchError {
    TruncatedPrefix { found: usize },
    EmptyTenant,
    TenantTooLong { found: usize },
    TenantPastEnd { declared: usize, available: usize },
    NotUtf8,
}

impl std::fmt::Display for TenantBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TruncatedPrefix { found } => {
                write!(
                    f,
                    "TenantOtlpBatch payload is {found} byte(s); the tenant length prefix needs 2"
                )
            }
            Self::EmptyTenant => write!(f, "TenantOtlpBatch tenant length is zero"),
            Self::TenantTooLong { found } => write!(
                f,
                "TenantOtlpBatch tenant length {found} exceeds {}",
                TenantBatch::MAX_TENANT_BYTES
            ),
            Self::TenantPastEnd {
                declared,
                available,
            } => write!(
                f,
                "TenantOtlpBatch tenant length {declared} runs past the payload ({available} byte(s) follow the prefix)"
            ),
            Self::NotUtf8 => write!(f, "TenantOtlpBatch tenant bytes are not valid UTF-8"),
        }
    }
}

impl std::error::Error for TenantBatchError {}

/// Operator-visible WAL configuration. Every field is a §6.9
/// Tunable — `Wal::open` validates each one against the
/// classification table and refuses to open on out-of-range
/// input. Defaults match the §6.9 table.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Local-disk path under which segment files + `CHECKPOINT`
    /// sidecar live.
    pub root: PathBuf,
    /// `wal_batch_window_ms` — bounds time between first
    /// `append` and corresponding `sync`. Default `100` per
    /// CLAUDE.md §3.4 / §6.3.
    pub batch_window_ms: u64,
    /// `wal_segment_size_bytes` — segment rotation cap.
    /// Default `128 MiB`; lower bound
    /// `≥ MAX_FRAME_BYTES + segment_header + frame_header`
    /// per §6.9 so a max-sized frame always fits.
    pub segment_size_bytes: u64,
    /// `wal_segment_age_secs` — segment time-cap (§6.5).
    /// Default `600` (10 min).
    pub segment_age_secs: u64,
    /// `wal_housekeeping_secs` — checkpoint housekeeping
    /// cadence (§6.7). Default `60`.
    pub housekeeping_secs: u64,
    /// `wal_max_unlinks_per_pass` — how much per-file work one
    /// reclamation pass may do (RFC 0052 §3.7, §3.8). Default
    /// [`DEFAULT_MAX_UNLINKS_PER_PASS`]; range
    /// `1..=`[`MAX_UNLINKS_PER_PASS_CEILING`].
    ///
    /// It bounds how long a pass holds the journal mutex, and so the
    /// stall an append can see, and it sizes the `RECLAIM` sidecar's
    /// `planned` array — which is why `Wal::open` both validates it
    /// and rebuilds the file when it rises above the stored capacity.
    ///
    /// §3.8 also validates the lower bound against
    /// `rotation_retry_attempts`, so RFC0052.4's one-pass debris
    /// clearance holds. That knob arrives with the rotation slice; the
    /// cross-check lands with it, and the range below is the format's
    /// own until then.
    pub max_unlinks_per_pass: u32,
    /// `wal_rotation_retry_attempts` — how many consecutive failed
    /// rotation attempts are retried before the WAL gives up (RFC 0052
    /// §3.3, §3.8). Default [`DEFAULT_ROTATION_RETRY_ATTEMPTS`]; range
    /// `MIN_ROTATION_RETRY_ATTEMPTS..=MAX_ROTATION_RETRY_ATTEMPTS`.
    ///
    /// A failed `rotate` and a failed rotation-origin parent-directory
    /// fsync discharge each charge one unit; the count resets only when
    /// that obligation itself succeeds. Exhausting it enters the
    /// terminal state, which refuses appends until an operator
    /// intervenes. No batch is acknowledged in either state.
    pub rotation_retry_attempts: u32,
    /// `wal_macos_full_fsync` — opt into `fcntl(F_FULLFSYNC)`
    /// on macOS for the slower-but-stronger durability per
    /// §6.3 / §9: on macOS `fsync`/`fdatasync` do not flush the
    /// drive's write cache, so true power-loss durability needs
    /// the full-fsync fcntl (via rustix's safe
    /// wrapper — the workspace denies `unsafe_code`). Ignored on
    /// other platforms, where `fdatasync` already carries the
    /// §6.3 contract.
    pub macos_full_fsync: bool,
}

/// Max bytes for a single frame's payload per RFC 0008 §6.2.2.
/// **Invariant**, not a tunable — a per-deployment limit
/// would let a single batch grow past a segment-recoverable
/// size and make file-format compatibility per-deployment.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// `wal_segment_size_bytes` validated lower bound per §6.9 —
/// `MAX_FRAME_BYTES + segment_header + frame_header` rounded
/// up to a round-numbered 17 MiB so a max-sized frame always
/// fits inside one segment (otherwise `wal_unflushed_bytes`
/// could grow past the §6.9 RFC0008.9 bound).
pub const MIN_SEGMENT_SIZE_BYTES: u64 = 17 * 1024 * 1024;

/// `wal_segment_size_bytes` validated upper bound per §6.9.
pub const MAX_SEGMENT_SIZE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// `wal_batch_window_ms` validated upper bound per §6.9; lower
/// bound is `0` (per-`append` `sync`, allowed but discouraged
/// per the §6.9 table).
pub const MAX_BATCH_WINDOW_MS: u64 = 10_000;

/// `wal_segment_age_secs` validated range per §6.9.
pub const MIN_SEGMENT_AGE_SECS: u64 = 1;
pub const MAX_SEGMENT_AGE_SECS: u64 = 86_400;

/// `wal_housekeeping_secs` validated range per §6.9.
pub const MIN_HOUSEKEEPING_SECS: u64 = 1;
pub const MAX_HOUSEKEEPING_SECS: u64 = 3_600;

/// `wal_rotation_retry_attempts` default per RFC 0052 §3.8 — three
/// consecutive failed attempts.
pub const DEFAULT_ROTATION_RETRY_ATTEMPTS: u32 = 3;

/// `wal_rotation_retry_attempts` validated range per RFC 0052 §3.8. A
/// budget of zero would make the first transient failure terminal; a
/// large one delays the terminal state an operator must act on.
pub const MIN_ROTATION_RETRY_ATTEMPTS: u32 = 1;
pub const MAX_ROTATION_RETRY_ATTEMPTS: u32 = 16;

/// Whether a housekeeping pass may plan segments (RFC 0052 §3.2), as
/// one value rather than two flags — "the sidecar's entry is not yet
/// durable" is only meaningful once the witness exists, and a pair of
/// bools can hold that contradiction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReclaimGate {
    /// No version-2 `CHECKPOINT` beside a record yet: a legacy root,
    /// or a post-RFC one before its first checkpoint. A pass sweeps
    /// partials and plans nothing — unlinking under a version-1
    /// checkpoint would leave exactly the shape the open-time matrix
    /// reads as "nothing was ever reclaimed".
    Unwitnessed,
    /// The witness exists, but the sidecar carrying it is only
    /// renamed: its directory entry is not durable until the parent
    /// fsync returns, and a crash that lost the rename would revert
    /// the checkpoint beneath frames whose segments a pass had
    /// already unlinked.
    FsyncPending,
    Open,
}

/// The append-only write-ahead log itself. One per ingester
/// process; the §6.1 API is `open → replay? → append* →
/// sync* → checkpoint*`.
#[derive(Debug)]
pub struct Wal {
    /// Read by `sync` / `replay` (the §6.3 parent-directory
    /// `fsync` and the §6.6 segment walk both need
    /// `config.root`).
    config: WalConfig,
    /// File handle for the segment currently accepting appends
    /// (per §6.2: append-only, opened by exactly one writer).
    /// Opened with `O_APPEND` so each write atomically lands at
    /// end-of-file regardless of the user-space cursor.
    current_segment: File,
    /// Path of the file the `current_segment` handle points
    /// at, swapped on rotation. Kept for diagnostic messages;
    /// housekeeping deliberately does NOT key on it — the
    /// append target is identified by its header UUID so a
    /// rename can't slip it past the guard, and the RFC 0052 §3.2
    /// ledger records it so an unlink targets the real file.
    current_segment_path: PathBuf,
    /// `UUIDv7` of the current segment — same value as the
    /// filename's stem and the segment's in-file header per
    /// §6.2.1. Carried in every `WalOffset` `append` returns.
    current_segment_uuid: uuid::Uuid,
    /// Whether the parent directory still needs an `fsync` to
    /// make the current segment's directory entry durable
    /// (§6.3), and — RFC 0052 §3.3 — which obligation owes it.
    /// `open` always sets it pending and the first
    /// `sync` of the process clears it — the directory fsync
    /// runs once per open regardless of whether `open` minted a
    /// *fresh* segment or reattached to an *existing* one. The
    /// existing-segment case can't be assumed durable: a prior
    /// process may have created the segment and crashed before
    /// its own first `sync`, so its directory entry could still
    /// be page-cache-only. Acking a frame appended into such a
    /// segment after only a data sync (which persists the
    /// file's data + size but not its directory link) would
    /// risk losing that acked frame to an orphaned inode on
    /// power loss — a §3.4 violation. One extra fsync per
    /// process start is the cheap, conservative guard.
    ///
    /// The origin decides the failure path: an `open`-owed discharge
    /// that fails is an ordinary retryable sync failure, while a
    /// rotation-owed one charges the §3.3 retry budget.
    dir_fsync: DirFsync,
    /// The `CHECKPOINT` sidecar's offset, read at `open` and
    /// advanced by `checkpoint` (§6.7). `None` = first-run /
    /// pre-checkpoint. This is the recovery driver's
    /// Parquet-side suppression horizon ([`Self::last_checkpoint`])
    /// and one of housekeeping's two truncation bounds.
    checkpoint: Option<WalOffset>,
    /// The format version of the `CHECKPOINT` on disk (RFC 0052
    /// §3.2). A version-1 sidecar is a pre-RFC root whose next
    /// checkpoint rewrites it at version 2; the version, not the
    /// mark, decides whether that rewrite happens, so an idle node
    /// offering an unchanged mark still reaches version 2.
    checkpoint_version: Option<checkpoint::SidecarVersion>,
    /// The `RECLAIM` sidecar (RFC 0052 §3.2), when this root has one.
    /// A legacy root opens without a record and the first checkpoint
    /// creates it on the upgrade path.
    reclaim: Option<reclaim_store::ReclaimStore>,
    /// Whether a housekeeping pass may plan segments (RFC 0052 §3.2).
    reclaim_gate: ReclaimGate,
    /// Stale `<uuid>.wal.partial` files, seeded by
    /// [`Self::rebuild_ledger`] and popped by the housekeeping sweep.
    /// §3.3: the sweep lists nothing on the pass, so restart debris is
    /// found without a directory scan under the writer position.
    stale_partials: Vec<PathBuf>,
    /// RFC 0052 §3.2's per-segment ledger: tenant membership, each
    /// tenant's first and last offset per segment, the horizon
    /// cursors and the eligible head. Rebuilt once after recovery and
    /// maintained incrementally, so a pass reads no header and lists
    /// no directory.
    ledger: retain::SegmentLedger,
    /// The segments [`Self::housekeeping_prepare`] popped and
    /// [`Self::housekeeping_commit`] has not yet accounted for. One
    /// plan is outstanding at a time by construction (§3.7): a plan
    /// that is never committed strands nothing, since the entries stay
    /// marked reclaiming and the next prepare re-plans them.
    outstanding: Option<Outstanding>,
    /// Passes [`Self::housekeeping_prepare`] has run, which is where
    /// the [`PassId`] on each plan comes from. Monotone, so a plan a
    /// later prepare superseded never matches the outstanding one.
    passes: u64,
    /// This `Wal`'s own number, minted at open and never reused in the
    /// process, so a [`PassId`] cannot be mistaken for one another
    /// instance on the same root — a reopen — handed out.
    instance: u64,
    /// The pass an outstanding [`UnlinkPermit`] would still authorise,
    /// zero when none is. Shared with the permits themselves, because
    /// the unlink half holds no guard and no WAL handle and still has
    /// to know that a later prepare has moved past its plan.
    live_pass: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Bytes of validated frames in surviving segments, seeded after
    /// recovery by [`Self::rebuild_ledger`] (§3.7). Never file size
    /// less header, which would count a torn tail, and never the
    /// best-effort directory walk `metrics` uses.
    unreclaimed_bytes: u64,
    /// RFC 0052 §3.3's rotation state: healthy, retrying within the
    /// `rotation_retry_attempts` budget, or terminal once it is
    /// exhausted. Only the terminal state refuses appends; a retrying
    /// one lets the next append re-enter `rotate` (or, for the
    /// post-rename site, the next `sync` discharge the directory
    /// fsync), which is what makes the quiesce recoverable without a
    /// restart.
    rotation: RotationState,
    /// The RFC 0052 §6 fault-injection seam. Default-constructed and
    /// inert; only [`Self::arm_rotation_faults`] (behind the
    /// `fault-injection` feature) ever arms it.
    faults: RotationFaults,
    /// §6.8 counters. `unflushed_bytes` is the H3 detection
    /// metric: grows on `append`, resets on a successful
    /// `sync` (or on a rotation's closing segment sync).
    appends_total: u64,
    syncs_total: u64,
    unflushed_bytes: u64,
    corrupt_frames_total: u64,
}

impl Wal {
    /// Open (or create) the WAL rooted at `config.root`.
    /// Validates every §6.9 tunable against its classified
    /// range first — out-of-range fields surface as
    /// [`OpenError::InvalidConfig`] before any filesystem
    /// state is touched.
    ///
    /// On a fresh root (no `*.wal` files present), creates a
    /// new segment with a `UUIDv7` filename and writes the 24 B
    /// §6.2.1 header. On an existing root, opens the
    /// lexicographically-greatest segment (= the newest per
    /// `UUIDv7`'s chronological sort) for further appends; the
    /// caller is responsible for calling [`Self::replay`]
    /// **before** any [`Self::append`] to walk surviving
    /// frames into the recovery sink.
    ///
    /// The `RECLAIM` sidecar is read, reconciled and — on a fresh root
    /// — created and fsynced **before** the initial segment exists
    /// (RFC 0052 §3.2). That ordering is part of the contract: a
    /// record written afterwards would leave a first-ever node one
    /// crash away from a root holding a segment and neither sidecar,
    /// which is the state the open-time matrix fails closed on.
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(config: WalConfig) -> Result<Self, OpenError> {
        validate_config(&config)?;
        prepare_root(&config.root)?;
        // §6.6 step 1: a present-but-invalid sidecar aborts here
        // (before any recovery) rather than being silently
        // treated as None — that would drop the Parquet
        // suppression horizon and duplicate every
        // already-published record on the data side.
        let sidecar = checkpoint::read(&config.root)?;
        let existing_segments = list_segments(&config.root)?;
        // A root with nothing on it has nothing the ledger fails to
        // describe, so its ledger is authoritative from here; one with
        // segments stays unpoppable until `rebuild_ledger` walks them.
        let mut ledger = retain::SegmentLedger::default();
        if existing_segments.is_empty() {
            ledger.describe_root();
        }
        // §3.3 runs the sweep on every pass and the pass itself lists
        // nothing, so the list is seeded from a listing open does.
        // Unlike the segment ledger this is safe here: a
        // `.wal.partial` is debris no reader depends on and has no
        // torn tail for recovery to heal, so nothing it holds can be
        // counted wrongly.
        let stale_partials = ledger::list_partials(&config.root).map_err(|e| match e {
            LedgerError::Io { op, source } => OpenError::Io { op, source },
            other => OpenError::Corrupt {
                detail: other.to_string(),
            },
        })?;
        let witness = reconcile::root(&config, sidecar, &existing_segments)?;
        let (current_segment, current_segment_path, current_segment_uuid) =
            append_target(&config.root, existing_segments)?;
        Ok(Self {
            config,
            current_segment,
            current_segment_path,
            current_segment_uuid,
            // Always pending: the first `sync` fsyncs the parent
            // directory regardless of fresh-vs-existing open, so
            // an acked frame's segment is guaranteed a durable
            // directory entry (see the field doc — §3.4).
            dir_fsync: DirFsync::PendingOpen,
            rotation: RotationState::Healthy,
            faults: RotationFaults::default(),
            checkpoint: sidecar.map(|s| s.offset),
            checkpoint_version: sidecar.map(|s| s.version),
            reclaim: witness.store,
            // `prepare_root` fsynced the root before the sidecars
            // were read, so whatever is on disk there is durable.
            reclaim_gate: witness.gate,
            stale_partials,
            ledger,
            outstanding: None,
            passes: 0,
            instance: NEXT_WAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            live_pass: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            unreclaimed_bytes: 0,
            appends_total: 0,
            syncs_total: 0,
            unflushed_bytes: 0,
            corrupt_frames_total: 0,
        })
    }

    /// Rebuild the in-memory ledger RFC 0052 §3.7 requires from the
    /// surviving segments: the unreclaimed-byte figure and the stale
    /// `*.wal.partial` list the housekeeping sweep pops from. Called
    /// by the recovery driver **after** replay has healed any torn
    /// tail, since a figure taken at `open` would count torn bytes.
    ///
    /// The walk validates every frame prefix exactly as replay does
    /// and decodes each `TenantOtlpBatch`'s tenant, so a frame written
    /// before RFC 0052 §3.2 amended the bound — carrying a tenant
    /// longer than [`TenantBatch::MAX_TENANT_BYTES`] — fails closed
    /// here, naming the frame and the length, rather than having its
    /// key truncated or its frame dropped.
    ///
    /// # Errors
    ///
    /// See [`LedgerError`].
    pub fn rebuild_ledger(&mut self) -> Result<(), LedgerError> {
        let rebuilt = ledger::rebuild(&self.config.root)?;
        self.unreclaimed_bytes = rebuilt.segments.bytes();
        self.ledger = rebuilt.segments;
        self.stale_partials = rebuilt.partials;
        Ok(())
    }

    /// The WAL state RFC 0052 §3.5 exports. The retain floor with its
    /// lag, the rotation-failure state and the age of the oldest
    /// unreclaimed frame arrive with the slices that own them; this is
    /// what the WAL knows once the sidecar is wired.
    #[must_use]
    pub fn reclaim_state(&self) -> ReclaimState {
        let metrics = self.metrics();
        ReclaimState {
            unflushed_bytes: metrics.unflushed_bytes,
            disk_bytes: metrics.disk_bytes,
            segment_count: metrics.segment_count,
            unreclaimed_bytes: self.unreclaimed_bytes,
            checkpoint: self.checkpoint,
            stale_partials: self.stale_partials.len(),
            reclaimable: self.checkpoint_is_settled(),
            floor: self.ledger.floor(),
            rotation: self.rotation.clone(),
        }
    }

    /// Arm RFC 0052 §6's rotation fault-injection seam.
    ///
    /// Behind the `fault-injection` feature, which only this crate's own
    /// test targets enable: §3.3's five sites are `fsync`, `create` and
    /// `rename` calls that no directory permission can single out, so
    /// the matrix RFC0052.4/.5 require has no other way in.
    #[cfg(feature = "fault-injection")]
    pub fn arm_rotation_faults(&mut self, faults: RotationFaults) {
        self.faults = faults;
    }

    /// Append a frame of `kind` carrying `payload` (≤
    /// [`MAX_FRAME_BYTES`]). The frame is **not** durable
    /// yet; the caller batches appends across the §6.3 window
    /// and calls [`Self::sync`] once per batch. Returns the
    /// **post-append** [`WalOffset`] per RFC 0008 §6.1 — the
    /// byte position immediately past the just-written frame.
    /// Checkpoint's "skip every frame at append-offset ≤ X"
    /// (§5 RFC0008.7) and `sync`'s "highest durable offset"
    /// compose naturally on this semantics: a `WalOffset`
    /// returned by `append` then `sync` means "everything
    /// strictly below this byte is on disk".
    ///
    /// Rotation (§6.5) happens here, *before* the write: when the
    /// segment would exceed `wal_segment_size_bytes` with this
    /// frame, or its age (from the `UUIDv7` mint time) exceeds
    /// `wal_segment_age_secs`, the segment is closed (its final
    /// data sync), a fresh one is created under a temporary name,
    /// fsynced and renamed into place — so a frame never straddles
    /// segments.
    ///
    /// RFC 0052 §3.3 changed what the parent-directory fsync
    /// guarantees, and the old wording here — "the parent dir is
    /// fsync'd before the frame lands" — no longer holds: the fresh
    /// segment is installed *before* that fsync, so frames may land in
    /// a segment whose directory entry is not yet durable. Nothing in
    /// it is acked, because [`Self::sync`] discharges the pending fsync
    /// before it reports a durable offset, which is what §3.4 gates the
    /// ack on. A failed rotation is retried under §3.3's bounded
    /// budget: [`AppendError::RotationRetrying`] while it holds,
    /// [`AppendError::RotationTerminal`] once it is spent. No batch is
    /// acked in either state.
    ///
    /// If `write_frame` fails after partial bytes have hit the
    /// segment, the file is best-effort truncated back to its
    /// pre-write length so a subsequent `append` doesn't land
    /// past a torn tail (which the recovery walk would surface
    /// as RFC0008.5 corruption, halting replay). The original
    /// I/O error is surfaced regardless of whether the rollback
    /// itself succeeds — the caller MUST NOT ack the failed
    /// batch either way (§3.4).
    ///
    /// # Errors
    ///
    /// See [`AppendError`].
    ///
    /// # Panics
    ///
    /// Panics in the unreachable case that `payload.len()`
    /// doesn't fit a `u64`. Every platform Rust currently
    /// supports has `usize ≤ u64`, so `u64::try_from(usize)`
    /// always succeeds; the `expect` documents the invariant
    /// rather than guarding a real failure mode.
    pub fn append(&mut self, kind: FrameKind, payload: &[u8]) -> Result<WalOffset, AppendError> {
        if let Some(fault) = self.rotation.terminal() {
            return Err(AppendError::RotationTerminal(fault.clone()));
        }
        if payload.len() > MAX_FRAME_BYTES {
            return Err(AppendError::TooLarge {
                len: payload.len(),
                limit: MAX_FRAME_BYTES,
            });
        }
        // Record the segment's pre-write byte length so we can
        // (a) roll back on a partial write and (b) derive the
        // post-append offset by adding the known frame size.
        // `metadata().len()` rather than `stream_position`:
        // `O_APPEND` guarantees each write lands at EOF
        // atomically but the user-space cursor isn't
        // guaranteed synchronised with the kernel's write
        // offset on every platform (Linux `fcntl(O_APPEND)`
        // notes), so `stream_position` can be 0 or stale on a
        // file we haven't seeked into. File metadata length is
        // truth.
        let mut pre_write_byte = self
            .current_segment
            .metadata()
            .map_err(|source| AppendError::Io {
                op: "stat(current_segment)",
                source,
            })?
            .len();
        let frame_len = frame::FRAME_HEADER_LEN as u64
            + u64::try_from(payload.len()).expect("payload.len() fits u64 (≤ MAX_FRAME_BYTES)");
        // §6.5: rotate *before* the write, so a frame never
        // straddles segments — the size check includes the frame
        // about to land. The §6.9 segment-size lower bound
        // guarantees any legal frame fits a fresh segment.
        if self.rotation_due(pre_write_byte, frame_len) {
            self.rotate_now()?;
            pre_write_byte = SEGMENT_HEADER_LEN as u64;
        }
        if let Err(source) = frame::write_frame(&mut self.current_segment, kind, payload) {
            // Best-effort truncate-back. If the rollback itself
            // fails the segment is left with a partial frame at
            // EOF; the recovery walk catches it as RFC0008.5
            // corruption on the next open and surfaces an
            // operator-actionable audit event. We report the
            // primary I/O error rather than the rollback error
            // because the caller's response is the same either
            // way (refuse to ack the batch per §3.4) and the
            // primary error names the actual write that failed.
            let _ = self.current_segment.set_len(pre_write_byte);
            return Err(AppendError::Io {
                op: "write_frame(current_segment)",
                source,
            });
        }
        // Post-append byte: pre-write length + 12 B header +
        // payload. Computed rather than re-stat'd to avoid a
        // second syscall — the `MAX_FRAME_BYTES` invariant
        // guarantees this sum fits a u64.
        let post_write_byte = pre_write_byte + frame_len;
        self.appends_total += 1;
        self.unflushed_bytes += frame_len;
        // RFC 0052 §3.7 keeps this figure incrementally, as
        // `unflushed_bytes` already is: seeded from the post-recovery
        // walk, raised by every frame that lands, lowered by every
        // verified unlink. A figure only ever seeded would omit
        // everything written since the last restart.
        self.unreclaimed_bytes += frame_len;
        let offset = WalOffset {
            segment: self.current_segment_uuid,
            byte: post_write_byte,
        };
        // §3.2's ledger is rebuilt at recovery and updated on every
        // live append, so a pass never has to read a header to learn
        // which tenants a segment holds.
        // `TenantId::new`, not `try_new`: the ledger must key on the
        // identity **replay** will assign, and `recovery`'s driver
        // wraps the same prefix unvalidated. A stored tenant that the
        // RFC 0048 §3.1 grammar would reject at a request boundary is
        // still a tenant the miner rebuilds state for, and dropping its
        // membership here would leave its segments governed by the
        // checkpoint alone. `TenantBatch::decode` has already bounded
        // the length and checked UTF-8.
        let tenant = match kind {
            FrameKind::TenantOtlpBatch => TenantBatch::decode(payload)
                .ok()
                .map(|batch| ourios_core::tenant::TenantId::new(batch.tenant)),
            FrameKind::OtlpBatch | FrameKind::AuditEvent => None,
        };
        self.ledger.observe(retain::FrameAt {
            offset,
            bytes: frame_len,
            tenant: tenant.as_ref(),
            path: &self.current_segment_path,
        });
        Ok(offset)
    }

    /// §6.5's two triggers, checked before the write lands.
    /// `current_len + frame_len` is the size the segment *would*
    /// reach; the age comes from the `UUIDv7`'s embedded
    /// timestamp — the instant the header was written, which
    /// survives reopen without persisting anything extra. An
    /// empty segment never age-rotates: there is no recovery
    /// window to bound, only file churn.
    fn rotation_due(&self, current_len: u64, frame_len: u64) -> bool {
        if current_len + frame_len > self.config.segment_size_bytes {
            return true;
        }
        if current_len <= SEGMENT_HEADER_LEN as u64 {
            return false;
        }
        segment_age(self.current_segment_uuid)
            .is_some_and(|age| age > std::time::Duration::from_secs(self.config.segment_age_secs))
    }

    /// Whether the current segment has outlived `segment_age_secs` —
    /// the barrier task's idle-rotation predicate (RFC 0052 §3.1). The
    /// age is the `UUIDv7`'s embedded mint time, so this costs no
    /// syscall; whether the segment holds a frame is
    /// [`Self::rotate`]'s own [`RotationKind::Discretionary`] check.
    #[must_use]
    pub fn segment_age_exceeded(&self) -> bool {
        segment_age(self.current_segment_uuid)
            .is_some_and(|age| age > std::time::Duration::from_secs(self.config.segment_age_secs))
    }

    /// Whether a rotation-origin directory fsync is still owed. The
    /// segment a failed post-rename fsync leaves installed is fresh, so
    /// [`Self::segment_age_exceeded`] alone would let an idle node hold
    /// the obligation until traffic returned; the timer asks this too.
    #[must_use]
    pub fn owes_rotation_fsync(&self) -> bool {
        self.dir_fsync == DirFsync::PendingRotation
    }

    /// RFC 0052 §3.3's callable rotation: close the current segment and
    /// install a fresh one, without an append driving it.
    ///
    /// A pending rotation-origin directory fsync is discharged first —
    /// §3.3's one-obligation-at-a-time rule — and the rotation is
    /// refused if that discharge fails. A [`RotationKind::Discretionary`]
    /// rotation of a segment holding no frame is a no-op: there is no
    /// recovery window to bound and nothing to seal. A
    /// [`RotationKind::Owed`] one proceeds regardless.
    ///
    /// # Errors
    ///
    /// See [`AppendError`]. A failure charges the §3.3 retry budget
    /// exactly as an append-driven rotation's does.
    pub fn rotate(&mut self, kind: RotationKind) -> Result<(), AppendError> {
        if let Some(fault) = self.rotation.terminal() {
            return Err(AppendError::RotationTerminal(fault.clone()));
        }
        // §3.3's one-obligation-at-a-time rule outranks the no-op below.
        // A post-rename fsync failure leaves the *fresh* segment
        // installed and empty, so an idle node would take the no-op and
        // leave the obligation owed until traffic returned — the one
        // caller that can discharge it without an append is this timer.
        self.discharge_owed_rotation_fsync()?;
        if kind == RotationKind::Discretionary && !self.current_segment_holds_a_frame()? {
            return Ok(());
        }
        self.rotate_now()
    }

    /// Whether the current segment holds anything past its header.
    fn current_segment_holds_a_frame(&self) -> Result<bool, AppendError> {
        let len = self
            .current_segment
            .metadata()
            .map_err(|source| AppendError::Io {
                op: "stat(current_segment)",
                source,
            })?
            .len();
        Ok(len > SEGMENT_HEADER_LEN as u64)
    }

    /// Close the current segment and open a fresh one (§6.5, as RFC 0052
    /// §3.3 amends it): sync the old segment's data (the last sync it
    /// ever receives — a torn tail on a *closed* segment is RFC0008.5
    /// corruption, so closing without it would convert a benign crash
    /// into a recovery halt), create the new segment under its
    /// `<uuid>.wal.partial` name, fsync its header, rename it into
    /// place, install it, and only then `fsync` the parent directory.
    ///
    /// The ordering is load-bearing in both directions: the header must
    /// be durable *before* the rename, or a surviving `.wal` entry can
    /// have unreadable header bytes; and the parent fsync must come
    /// *after* it, because the rename is what creates the entry. A
    /// failure before the rename therefore leaves nothing
    /// [`Self::open`] would select — `list_segments` returns only
    /// `*.wal` — and the partial is swept by housekeeping.
    ///
    /// The fresh segment is installed *before* the parent fsync, so a
    /// failure there leaves the WAL writing into a complete, valid
    /// segment whose directory entry is not yet durable. That file is
    /// never unlinked; the obligation is recorded and the next
    /// [`Self::sync`] discharges it before acking anything.
    fn rotate_now(&mut self) -> Result<(), AppendError> {
        self.discharge_owed_rotation_fsync()?;
        self.rotation_step(RotationSite::CloseSync, |wal| {
            sync_file_data(&wal.current_segment, wal.config.macos_full_fsync)
        })?;
        // The closing data sync flushed everything appended so far.
        self.unflushed_bytes = 0;
        // RFC 0052 §3.2: no version-2 segment is ever created before
        // the record is durable. A legacy root rotates long before it
        // ever checkpoints, and a rotation that installed a version-2
        // segment beside no sidecar would leave the one shape the
        // open-time matrix fails closed on — a live pre-RFC root
        // bricked by rotating.
        if let Err(e) = reconcile::ensure_record(&mut self.reclaim, &self.config) {
            return Err(rotation_record_failed(e));
        }
        let (file, partial, uuid) = self.create_partial()?;
        self.rotation_step(RotationSite::HeaderSync, |wal| {
            sync_file_data(&file, wal.config.macos_full_fsync)
        })?;
        let path = self.config.root.join(format!("{uuid}.wal"));
        self.rotation_step(RotationSite::Rename, |_| std::fs::rename(&partial, &path))?;
        self.forget_partial(&partial);
        self.current_segment = file;
        self.current_segment_path = path;
        self.current_segment_uuid = uuid;
        self.dir_fsync = DirFsync::PendingRotation;
        self.rotation_step(RotationSite::ParentFsync, |wal| {
            sync_parent_dir(&wal.config.root)
        })?;
        self.dir_fsync = DirFsync::Clean;
        self.rotation.discharged();
        Ok(())
    }

    /// [`Self::step`] rendered on the append surface — every rotation
    /// step but the `sync`-side discharge reports there.
    fn rotation_step<F>(&mut self, site: RotationSite, run: F) -> Result<(), AppendError>
    where
        F: FnOnce(&Self) -> std::io::Result<()>,
    {
        match self.step(site, run) {
            Ok(()) => Ok(()),
            Err(fault) => Err(self.append_error(fault)),
        }
    }

    /// Run one §3.3 rotation step, charging the retry budget when it
    /// fails. The fault seam is consulted first so every site can be
    /// driven from a test; it is inert unless armed.
    ///
    /// The fault is returned raw rather than as one surface's error
    /// type: the same five steps are reached from `append` and from
    /// `sync`, and each renders it on its own enum.
    fn step<F>(&mut self, site: RotationSite, run: F) -> Result<(), RotationFault>
    where
        F: FnOnce(&Self) -> std::io::Result<()>,
    {
        let outcome = match self.faults.take(site) {
            Some(injected) => Err(injected),
            None => run(self),
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(source) => {
                Err(self
                    .rotation
                    .charge(site.op(), &source, self.config.rotation_retry_attempts))
            }
        }
    }

    /// Whether the budget is spent — which of the two rotation variants
    /// a `fault` is reported under.
    fn rotation_is_terminal(&self) -> bool {
        self.rotation.terminal().is_some()
    }

    fn append_error(&self, fault: RotationFault) -> AppendError {
        if self.rotation_is_terminal() {
            AppendError::RotationTerminal(fault)
        } else {
            AppendError::RotationRetrying(fault)
        }
    }

    fn sync_error(&self, fault: RotationFault) -> SyncError {
        if self.rotation_is_terminal() {
            SyncError::RotationTerminal(fault)
        } else {
            SyncError::RotationRetrying(fault)
        }
    }

    /// Create `<uuid>.wal.partial` and write its header. Nothing a
    /// [`Self::open`] can select exists until the rename, since
    /// `list_segments` returns only `*.wal`.
    ///
    /// The path is registered on the housekeeping sweep's list *before*
    /// the attempt, so a rotation that dies at any step leaves its
    /// debris where the sweep already looks — §3.3 requires the pass
    /// itself to list nothing, so a file the sweep never learned about
    /// survives every pass in this process.
    ///
    /// Before the attempt rather than after it because the creation is
    /// two operations: a file that exists and a header written into it.
    /// A failed header write — ENOSPC, the realistic trigger for this
    /// whole retry path — leaves the file behind with nothing returned
    /// to register it. A path the create never reached costs the sweep
    /// one no-op unlink, which `remove_partial` already treats as a
    /// removal to re-verify.
    fn create_partial(&mut self) -> Result<(File, PathBuf, uuid::Uuid), AppendError> {
        let uuid = uuid::Uuid::now_v7();
        let partial = self.config.root.join(format!("{uuid}.wal.partial"));
        let target = partial.clone();
        let mut handle = None;
        self.stale_partials.push(partial.clone());
        self.rotation_step(RotationSite::Create, |_| {
            // The two halves collapse here on purpose: §3.3 charges the
            // budget per *site*, and both are the `Create` site.
            handle = Some(create_segment_at(&target, uuid).map_err(|e| e.source)?);
            Ok(())
        })?;
        match handle {
            Some(file) => Ok((file, partial, uuid)),
            None => unreachable!("a successful Create step always yields the handle"),
        }
    }

    /// Drop one partial from the sweep's list — the rename installed it
    /// under its final name, so there is no debris left to collect.
    fn forget_partial(&mut self, partial: &std::path::Path) {
        self.stale_partials.retain(|queued| queued != partial);
    }

    /// §3.3's one-rotation-obligation-at-a-time rule: a rotation-origin
    /// directory fsync still owed is discharged before a second rotation
    /// begins, and the rotation is refused if that fails. Starting the
    /// second rotation would install a segment whose predecessor's
    /// directory entry is still not durable, losing the ordering the
    /// first obligation exists to restore.
    ///
    /// An `open`-origin obligation does not gate a rotation: the
    /// rotation's own parent fsync discharges it.
    fn discharge_owed_rotation_fsync(&mut self) -> Result<(), AppendError> {
        if self.dir_fsync != DirFsync::PendingRotation {
            return Ok(());
        }
        self.rotation_step(RotationSite::ParentFsync, |wal| {
            sync_parent_dir(&wal.config.root)
        })?;
        self.dir_fsync = DirFsync::Clean;
        self.rotation.discharged();
        Ok(())
    }

    /// Fsync the current segment (and, on the first `sync`
    /// after any `open`, the parent directory — so the segment
    /// holding the just-acked frames is guaranteed a durable
    /// directory entry, the same obligation a rotation will
    /// carry, per §6.3). Returns the highest offset that is now
    /// durable — the receiver gates its acks on this returning
    /// `Ok(_)`.
    ///
    /// Uses `sync_file_data` on the segment per §6.3 —
    /// `fdatasync`, or `fcntl(F_FULLFSYNC)` under the macOS knob:
    /// the payload + size are what must survive, not the inode's
    /// every metadata field. The directory `fsync` is the full
    /// `File::sync_all` — `fdatasync` is undefined on directories
    /// under POSIX.
    ///
    /// With [`WalConfig::macos_full_fsync`] set (macOS only),
    /// the segment sync is `fcntl(F_FULLFSYNC)` instead — see
    /// that field's doc.
    ///
    /// # Errors
    ///
    /// See [`SyncError`].
    pub fn sync(&mut self) -> Result<WalOffset, SyncError> {
        self.sync_segment_data()?;
        self.discharge_dir_fsync()?;
        // Everything written so far is now durable; the highest
        // durable byte is the segment's current length. Re-stat
        // rather than thread a counter so a crash between the
        // fsync and this read still reports truth.
        let byte = self
            .current_segment
            .metadata()
            .map_err(|source| SyncError::Io {
                op: "stat(current_segment)",
                source,
            })?
            .len();
        self.syncs_total += 1;
        self.unflushed_bytes = 0;
        Ok(WalOffset {
            segment: self.current_segment_uuid,
            byte,
        })
    }

    /// Make the current segment's directory entry durable when one of
    /// the two obligations (§6.3's per-open one, or RFC 0052 §3.3's
    /// post-rename one) is outstanding.
    ///
    /// A rotation-origin discharge is the second operation §3.3's retry
    /// budget counts, and a terminal state short-circuits it rather than
    /// hammering a disk that has already failed the same fsync its whole
    /// budget's worth of times. An `open`-origin failure stays an
    /// ordinary retryable sync error, outside the budget — which is what
    /// keeps RFC0052.15's reclassification narrow.
    fn discharge_dir_fsync(&mut self) -> Result<(), SyncError> {
        match self.dir_fsync {
            DirFsync::Clean => Ok(()),
            DirFsync::PendingOpen => self.discharge_open_fsync(),
            DirFsync::PendingRotation => self.discharge_rotation_fsync(),
        }
    }

    fn discharge_open_fsync(&mut self) -> Result<(), SyncError> {
        sync_parent_dir(&self.config.root).map_err(|source| SyncError::Io {
            op: "fsync(wal_root)",
            source,
        })?;
        self.dir_fsync = DirFsync::Clean;
        Ok(())
    }

    fn discharge_rotation_fsync(&mut self) -> Result<(), SyncError> {
        if let Some(fault) = self.rotation.terminal() {
            return Err(SyncError::RotationTerminal(fault.clone()));
        }
        match self.step(RotationSite::ParentFsync, |wal| {
            sync_parent_dir(&wal.config.root)
        }) {
            Ok(()) => {
                self.dir_fsync = DirFsync::Clean;
                self.rotation.discharged();
                Ok(())
            }
            Err(fault) => Err(self.sync_error(fault)),
        }
    }

    /// The live segment's §6.3 data sync — [`sync_file_data`] with
    /// this WAL's knob.
    fn sync_segment_data(&self) -> Result<(), SyncError> {
        sync_file_data(&self.current_segment, self.config.macos_full_fsync).map_err(|source| {
            SyncError::Io {
                op: "sync(current_segment)",
                source,
            }
        })
    }

    /// Record that records ≤ `durable_to` are on object
    /// storage; segments wholly below this offset may be
    /// reclaimed by [`Self::housekeeping`]. Persists the offset
    /// to the `CHECKPOINT` sidecar per §6.7 (atomic write +
    /// fsync + parent-dir fsync) — durability of the checkpoint
    /// itself is what lets the post-restart recovery driver
    /// suppress already-published records on its Parquet path
    /// (replay is at-least-once and delivers every surviving
    /// frame; the driver's suppression is the only dedup).
    ///
    /// Advance is monotonic: a `durable_to` below the current
    /// checkpoint is rejected. Re-asserting the current value never
    /// moves the mark, but it is a no-op only once the checkpoint is
    /// *settled* — sidecar at version 2, its directory entry fsynced,
    /// and `checkpoint_seen` in the RFC 0052 §3.2 record beside it.
    /// Until then an equal mark rewrites all three, which is how a
    /// version-1 root reaches version 2 and how a durability step a
    /// previous call owed and failed is retried; callers that repeat
    /// an equal mark to finish either get that, at the cost of the
    /// writes.
    ///
    /// # Errors
    ///
    /// See [`CheckpointError`]. When the **sidecar write** fails the
    /// in-memory checkpoint is not advanced — the WAL conservatively
    /// keeps all segments rather than risk a post-crash data-side
    /// dup. When the sidecar write succeeded and RFC 0052 §3.2's
    /// `checkpoint_seen` write then failed, the mark *is* durable and
    /// the in-memory checkpoint tracks it: leaving it behind would let
    /// a later, lower mark pass the monotonicity check and rewrite the
    /// sidecar backwards. The error still surfaces, and the next
    /// [`Self::open`] promotes the witness.
    pub fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), CheckpointError> {
        if let Some(current) = self.checkpoint {
            if durable_to < current {
                return Err(CheckpointError::NonMonotonic {
                    current,
                    attempted: durable_to,
                });
            }
            // RFC 0052 §3.2's upgrade is version-aware, not
            // mark-aware: an idle node offers an unchanged mark on
            // every barrier after the first, so a root whose sidecar
            // is still version 1 would never reach version 2 and
            // housekeeping would stay gated forever. Only an equal
            // mark on an already-version-2 sidecar takes the no-write
            // fast path.
            if durable_to == current && self.checkpoint_is_settled() {
                return Ok(());
            }
        }
        reconcile::arm(&mut self.reclaim, &self.config)?;
        // Split at the rename, because the two halves need opposite
        // answers. A failure inside `write` leaves nothing visible
        // under the final name, so nothing advances. A failure of the
        // parent fsync leaves the new mark already visible, so the
        // in-memory mark must follow it: a later call that found a
        // stale mark here would pass the monotonicity check against it
        // and rewrite the sidecar *backwards*, which recovery reads as
        // a lower Parquet suppression horizon and republishes every
        // row above it.
        checkpoint::write(&self.config.root, durable_to)?;
        self.checkpoint = Some(durable_to);
        self.checkpoint_version = Some(checkpoint::SidecarVersion::Current);
        // Advancing the mark is not the same as being allowed to
        // reclaim under it: the rename is visible but its directory
        // entry is not durable until the fsync below returns.
        self.reclaim_gate = ReclaimGate::FsyncPending;
        checkpoint::sync_root(&self.config.root)?;
        self.reclaim_gate = ReclaimGate::Open;
        // The witness is a third durable write and its own failure:
        // reported, never rolled back. `checkpoint_seen` governs only
        // the open-time matrix, and an armed record beside a version-2
        // sidecar is promoted at the next open, so a failure here
        // loses nothing.
        reconcile::witness(&mut self.reclaim)
    }

    /// Every part of the last checkpoint is on disk: the sidecar at
    /// version 2, its directory entry fsynced, and `checkpoint_seen`
    /// in the record beside it. Two callers want exactly this, and for
    /// the same reason — the checkpoint is only as good as the weakest
    /// of the three:
    ///
    /// - the equal-mark no-write path, which must not skip a write a
    ///   previous call owed and failed, or it would skip it for the
    ///   life of the process;
    /// - a housekeeping pass, which must not unlink under a checkpoint
    ///   whose entry a crash could lose, nor under one whose startup
    ///   loss witness is still only `Armed` — RFC 0052 §3.2's matrix
    ///   reads a later missing `CHECKPOINT` as a fresh root without
    ///   `checkpoint_seen`, and by then the frames are gone.
    ///
    /// A legacy root has no witness and is never settled, so a pass
    /// plans nothing until the version-aware upgrade lands.
    fn checkpoint_is_settled(&self) -> bool {
        if self.reclaim_gate != ReclaimGate::Open {
            return false;
        }
        if self.checkpoint_version != Some(checkpoint::SidecarVersion::Current) {
            return false;
        }
        self.reclaim
            .as_ref()
            .is_some_and(|store| store.record().witness.checkpoint == reclaim::Witness::Terminal)
    }

    /// The `CHECKPOINT` sidecar's offset (`None` =
    /// pre-first-checkpoint). The recovery driver reads it once
    /// at startup as its Parquet-side suppression horizon
    /// (§6.6) — `replay` itself delivers every surviving frame.
    #[must_use]
    pub fn last_checkpoint(&self) -> Option<WalOffset> {
        self.checkpoint
    }

    /// Reclaim disk (§6.7): unlink every segment whose
    /// **highest** frame offset is ≤ the checkpoint **and**,
    /// when `retain_floor` is `Some`, ≤ the floor — i.e. wholly
    /// below `min(checkpoint, floor)`. The caller passes the
    /// latest durable miner snapshot's high-water mark as the
    /// floor so truncation never destroys a frame no snapshot
    /// has captured (RFC 0001 §6.9 — the hazard-#5 retain
    /// rule); `None` means no snapshot consumer exists and the
    /// checkpoint alone governs. Whole segments only; the
    /// current append segment is never unlinked. A no-op before
    /// the first checkpoint.
    ///
    /// The timer lives in the caller (`wal_housekeeping_secs`);
    /// this is one pass.
    ///
    /// **Superseded, and unreachable from production (issue #827).**
    /// RFC 0052 §3.2's per-segment rule replaces this global bound
    /// rather than standing beside it: this entry point writes no
    /// `planned` witness, drops the segment from the tenant-aware
    /// ledger and honours no per-pass cap, so a pass run through it
    /// after the ledger has recorded a consumer mode can remove frames
    /// a pinned tenant still holds. It survives only because it is
    /// RFC 0008 §6.7's asserted contract, and the `legacy-housekeeping`
    /// feature is what keeps the two surfaces from ever meeting on a
    /// live root: the feature is enabled by this crate's own
    /// dev-dependency and nothing else, so no product binary can reach
    /// it. Every other caller uses [`Self::housekeeping_pass`] or the
    /// [`Self::housekeeping_prepare`] / [`Self::housekeeping_commit`]
    /// pair.
    ///
    /// # Errors
    ///
    /// See [`HousekeepingError`].
    #[cfg(feature = "legacy-housekeeping")]
    pub fn housekeeping(
        &mut self,
        retain_floor: Option<WalOffset>,
    ) -> Result<(), HousekeepingError> {
        // RFC 0052 §3.3: the sweep runs on **every** pass, witness or
        // not. A `.wal.partial` is debris from a rotation that never
        // installed a segment — no reader can depend on it and no
        // `reclaimed_through` accounts for it — so gating it would let
        // debris created before the first checkpoint survive forever.
        let cap = self.pass_cap();
        ledger::sweep_partials(&mut self.stale_partials, &self.config.root, cap)?;
        let Some(cp) = self.checkpoint else {
            return Ok(());
        };
        if !self.checkpoint_is_settled() {
            return Ok(());
        }
        self.unlink_at_or_below(match retain_floor {
            Some(floor) => cp.min(floor),
            None => cp,
        })
    }

    /// Unlink every closed segment whose highest frame offset is at or
    /// below `bound`. Whole segments only; the current append segment
    /// is never unlinked.
    #[cfg(feature = "legacy-housekeeping")]
    fn unlink_at_or_below(&mut self, bound: WalOffset) -> Result<(), HousekeepingError> {
        let io = |op: &'static str, source| HousekeepingError::Io { op, source };
        let segments = list_segments(&self.config.root).map_err(|e| match e {
            OpenError::Io { op, source } => io(op, source),
            OpenError::InvalidConfig { .. } | OpenError::Corrupt { .. } => {
                unreachable!("list_segments only surfaces OpenError::Io")
            }
        })?;
        let mut unlinked_any = false;
        for path in segments {
            // Segment identity is the in-file header UUID, not the
            // filename, mirroring `open_existing_segment` — a
            // renamed file is still judged by its true identity.
            // That includes the *current* segment: skipping it by
            // path would let a rename slip the live append target
            // past the guard, and unlinking it leaves the writer
            // appending into an unlinked inode no later `open`
            // would ever see.
            let mut handle = File::open(&path).map_err(|e| io("open(segment)", e))?;
            let header = segment::read_header(&mut handle).map_err(|e| {
                io(
                    "read_header(segment)",
                    std::io::Error::new(ErrorKind::InvalidData, format!("{}: {e}", path.display())),
                )
            })?;
            if header.segment_uuid == self.current_segment_uuid {
                continue;
            }
            // A closed segment's highest frame offset is its file
            // length (append offsets are post-frame bytes).
            let len = handle.metadata().map_err(|e| io("stat(segment)", e))?.len();
            let highest = WalOffset {
                segment: header.segment_uuid,
                byte: len,
            };
            if highest <= bound {
                std::fs::remove_file(&path).map_err(|e| io("unlink(segment)", e))?;
                // The frame bytes this segment held leave the
                // unreclaimed figure with it. `len - header` is exact
                // for a *closed* segment: a torn tail on one is
                // RFC0008.5 corruption that halts replay, so a segment
                // that reached this point has none.
                self.unreclaimed_bytes = self
                    .unreclaimed_bytes
                    .saturating_sub(len.saturating_sub(SEGMENT_HEADER_LEN as u64));
                // The RFC 0052 §3.2 ledger is the other view of this
                // directory; leaving a reclaimed segment in it would
                // let a later pass plan a file that is already gone.
                self.ledger.remove(header.segment_uuid);
                unlinked_any = true;
            }
        }
        if unlinked_any {
            sync_parent_dir(&self.config.root)
                .map_err(|e| io("fsync(wal_root after housekeeping)", e))?;
        }
        Ok(())
    }
}

impl Wal {
    /// Walk every surviving segment in chronological order,
    /// handing each well-formed frame to `sink` (§6.6). Used by
    /// the ingester at startup before opening network
    /// listeners; `&mut self` because step 4 *heals* the newest
    /// segment in place (see below).
    ///
    /// Segments are listed and sorted lexicographically; `UUIDv7`
    /// naming makes that chronological, so the last entry is the
    /// segment that was open for appends at crash time (the
    /// **newest**) and every earlier one is closed. For each
    /// frame the decoder (`frame::read_frame`) validates CRC,
    /// `kind`, `_pad`, and `len`; a corrupt frame on **any**
    /// segment halts the whole walk ([`RecoveryError::Corrupt`])
    /// because the high-water-mark logic needs a contiguous log.
    /// A torn (partial) *tail* frame is the one exception: on the
    /// newest segment it is RFC0008.4 clean truncation, so the
    /// scan stops cleanly and the segment is healed —
    /// `ftruncate` to the last valid boundary, a data sync, and
    /// `fsync` the parent dir — so the next `append` resumes on a
    /// frame boundary. On any *closed* segment a torn tail is
    /// instead RFC0008.5 corruption (its rotation fsync should
    /// have completed), so it halts the walk.
    ///
    /// Every well-formed surviving frame is delivered — including
    /// frames at or below the checkpoint that a straddling or
    /// floor-retained segment holds (§6.6, 2026-06-12 amendment).
    /// Suppression is per consumer, in the recovery driver: the
    /// Parquet path consumes only frames above
    /// [`Self::last_checkpoint`], the miner only frames above its
    /// restored snapshot's high-water mark. An in-`replay` skip
    /// would make a lagging snapshot's retained frames
    /// undeliverable, which is exactly the gap the §6.7 retain
    /// floor exists to close.
    ///
    /// # Errors
    ///
    /// See [`RecoveryError`].
    pub fn replay<S: FrameSink>(&mut self, sink: &mut S) -> Result<(), RecoveryError> {
        let segments = list_segments(&self.config.root).map_err(|e| match e {
            OpenError::Io { op, source } => RecoveryError::Io { op, source },
            // `list_segments` only ever surfaces `Io` (it does no
            // config validation or header reads); the other arms
            // are structurally unreachable.
            OpenError::InvalidConfig { .. } | OpenError::Corrupt { .. } => {
                unreachable!("list_segments only surfaces OpenError::Io")
            }
        })?;
        let newest_idx = segments.len().checked_sub(1);
        for (idx, path) in segments.iter().enumerate() {
            let is_newest = Some(idx) == newest_idx;
            match replay_segment(path, is_newest, sink) {
                Ok(SegmentScan::CleanTail) => {}
                Ok(SegmentScan::TornTail { valid_to }) => self.heal_newest_segment(valid_to)?,
                Err(e) => {
                    if matches!(e, RecoveryError::Corrupt { .. }) {
                        self.corrupt_frames_total += 1;
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// RFC0008.4 / §6.6 step 4: truncate a torn tail off the
    /// newest segment so the next `append` starts on the last
    /// valid frame boundary, then make the truncation durable.
    /// The newest segment is the one `open` is holding as
    /// `current_segment`, so the truncation targets that handle
    /// directly — its `O_APPEND` writes re-evaluate end-of-file
    /// per write, so subsequent appends land at `valid_to`.
    fn heal_newest_segment(&mut self, valid_to: u64) -> Result<(), RecoveryError> {
        self.current_segment
            .set_len(valid_to)
            .map_err(|source| RecoveryError::Io {
                op: "ftruncate(heal newest segment)",
                source,
            })?;
        sync_file_data(&self.current_segment, self.config.macos_full_fsync).map_err(|source| {
            RecoveryError::Io {
                op: "sync(heal newest segment)",
                source,
            }
        })?;
        sync_parent_dir(&self.config.root).map_err(|source| RecoveryError::Io {
            op: "fsync(wal_root after heal)",
            source,
        })?;
        self.dir_fsync = DirFsync::Clean;
        Ok(())
    }

    /// Snapshot of the §6.8 metrics. `disk_bytes` and
    /// `segment_count` are computed from a best-effort directory
    /// walk (an unreadable entry is skipped rather than failing
    /// the whole snapshot — this is a dashboard read, not a
    /// correctness path); the counters are exact.
    #[must_use]
    pub fn metrics(&self) -> WalMetrics {
        let mut disk_bytes = 0u64;
        let mut segment_count = 0u32;
        if let Ok(entries) = std::fs::read_dir(&self.config.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    disk_bytes += meta.len();
                    if path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("wal"))
                    {
                        segment_count += 1;
                    }
                }
            }
        }
        WalMetrics {
            appends_total: self.appends_total,
            syncs_total: self.syncs_total,
            unflushed_bytes: self.unflushed_bytes,
            disk_bytes,
            segment_count,
            checkpoint_segment: self.checkpoint.map(|o| o.segment),
            checkpoint_byte: self.checkpoint.map_or(0, |o| o.byte),
            corrupt_frames_total: self.corrupt_frames_total,
        }
    }
}

/// Per-tunable §6.9 validation. Fails fast on the *first*
/// out-of-range field — the error names that field so the
/// operator sees one structured failure rather than a list,
/// and so a sweep through the config doesn't depend on every
/// later field's invariants being independently checkable.
///
/// Grouped by what each knob governs rather than written as one
/// chain: the list grows with every RFC that adds a tunable, and a
/// single function accumulating them is how it stops being readable.
fn validate_config(c: &WalConfig) -> Result<(), OpenError> {
    validate_durability_window(c)?;
    validate_segment_bounds(c)?;
    validate_cadences(c)?;
    validate_reclamation(c)
}

/// `wal_batch_window_ms` — §3.4's latency/durability knob.
fn validate_durability_window(c: &WalConfig) -> Result<(), OpenError> {
    if c.batch_window_ms > MAX_BATCH_WINDOW_MS {
        return Err(outside(
            "batch_window_ms",
            format!(
                "{} exceeds §6.9 upper bound {MAX_BATCH_WINDOW_MS}",
                c.batch_window_ms
            ),
        ));
    }
    Ok(())
}

/// `wal_segment_size_bytes` — its lower bound is the one that carries
/// a real invariant, so the two edges report differently.
fn validate_segment_bounds(c: &WalConfig) -> Result<(), OpenError> {
    if c.segment_size_bytes < MIN_SEGMENT_SIZE_BYTES {
        return Err(outside(
            "segment_size_bytes",
            format!(
                "{} below §6.9 lower bound {MIN_SEGMENT_SIZE_BYTES} (MAX_FRAME_BYTES + headers; a smaller segment couldn't fit a max-sized frame)",
                c.segment_size_bytes
            ),
        ));
    }
    if c.segment_size_bytes > MAX_SEGMENT_SIZE_BYTES {
        return Err(outside(
            "segment_size_bytes",
            format!(
                "{} exceeds §6.9 upper bound {MAX_SEGMENT_SIZE_BYTES}",
                c.segment_size_bytes
            ),
        ));
    }
    Ok(())
}

/// The two second-granularity timers: `wal_segment_age_secs` and
/// `wal_housekeeping_secs`.
fn validate_cadences(c: &WalConfig) -> Result<(), OpenError> {
    in_range(
        "segment_age_secs",
        c.segment_age_secs,
        MIN_SEGMENT_AGE_SECS..=MAX_SEGMENT_AGE_SECS,
        "§6.9",
    )?;
    in_range(
        "housekeeping_secs",
        c.housekeeping_secs,
        MIN_HOUSEKEEPING_SECS..=MAX_HOUSEKEEPING_SECS,
        "§6.9",
    )
}

/// RFC 0052 §3.8's two knobs, and the one rule that reads both.
fn validate_reclamation(c: &WalConfig) -> Result<(), OpenError> {
    in_range(
        "max_unlinks_per_pass",
        c.max_unlinks_per_pass,
        1..=MAX_UNLINKS_PER_PASS_CEILING,
        "RFC 0052 §3.8",
    )?;
    in_range(
        "rotation_retry_attempts",
        c.rotation_retry_attempts,
        MIN_ROTATION_RETRY_ATTEMPTS..=MAX_ROTATION_RETRY_ATTEMPTS,
        "RFC 0052 §3.8",
    )?;
    // RFC 0052 §3.8's one cross-knob rule, and the reason it lives in
    // the WAL: only `Wal::open` sees both numbers. RFC0052.4's one-pass
    // debris clearance is what it protects — a rotation retrying its
    // full budget leaves one `.wal.partial` per attempt, and a pass
    // that cannot pop them all would leave rotation debris on disk
    // indefinitely.
    if c.max_unlinks_per_pass < c.rotation_retry_attempts {
        return Err(outside(
            "max_unlinks_per_pass",
            format!(
                "{} below rotation_retry_attempts {} — one rotation's debris must clear in one pass (RFC 0052 §3.8)",
                c.max_unlinks_per_pass, c.rotation_retry_attempts
            ),
        ));
    }
    Ok(())
}

fn outside(field: &'static str, detail: String) -> OpenError {
    OpenError::InvalidConfig { field, detail }
}

/// The common shape: a closed range whose violation names the field,
/// the value and the section the range comes from.
fn in_range<T>(
    field: &'static str,
    value: T,
    range: std::ops::RangeInclusive<T>,
    section: &str,
) -> Result<(), OpenError>
where
    T: PartialOrd + std::fmt::Display + Copy,
{
    if range.contains(&value) {
        return Ok(());
    }
    Err(outside(
        field,
        format!(
            "{value} outside {section} range {}..={}",
            range.start(),
            range.end()
        ),
    ))
}

/// Map a sidecar failure onto the rotation's error surface. A
/// rotation that cannot make the record durable has created nothing,
/// so it is reported exactly as a failed `create_fresh_segment` is.
///
/// Deliberately **outside** RFC 0052 §3.3's retry budget, and so never
/// terminal. The budget is charged per §3.3 *site*, and the five sites
/// are the segment's own steps; the `RECLAIM` write is §3.2's, and
/// widening the budget to cover it would widen the terminal state past
/// what RFC0052.15 pins as narrow. Nothing is left behind either way —
/// `ensure_record` is a no-op once the store exists — so the next
/// append simply re-enters the rotation.
///
/// This is also where the old permanent quiesce used to be set, which
/// §3.3 removes: a record write that fails is now retried like any
/// other transient I/O failure rather than wedging the node.
fn rotation_record_failed(e: reclaim_store::StoreError) -> AppendError {
    match e {
        reclaim_store::StoreError::Io { op, source } => AppendError::Io { op, source },
        reclaim_store::StoreError::Corrupt { detail } => AppendError::Io {
            op: "write(RECLAIM before rotation)",
            source: std::io::Error::new(ErrorKind::InvalidData, detail),
        },
    }
}

/// Create the WAL root and make its directory entries durable before
/// anything reads them (RFC 0052 §3.2). A rename's parent fsync can
/// fail after the entry is already visible to this process, so a
/// listing alone does not prove a sidecar survives the next crash;
/// one fsync here makes every entry the listing saw durable, and
/// failing it is a fault to surface rather than to continue past.
fn prepare_root(root: &std::path::Path) -> Result<(), OpenError> {
    std::fs::create_dir_all(root).map_err(|source| OpenError::Io {
        op: "create_dir_all(wal_root)",
        source,
    })?;
    sync_parent_dir(root).map_err(|source| OpenError::Io {
        op: "fsync(wal_root before reading the sidecars)",
        source,
    })
}

/// The segment appends land in: the newest existing one (§6.1's
/// lexicographically-greatest), or a fresh one on a root that holds
/// none.
fn append_target(
    root: &std::path::Path,
    existing: Vec<PathBuf>,
) -> Result<(File, PathBuf, uuid::Uuid), OpenError> {
    match existing.into_iter().next_back() {
        Some(newest) => open_existing_segment(&newest),
        None => create_fresh_segment(root),
    }
}

/// Sorted (= chronological per `UUIDv7`) list of `*.wal`
/// segment paths under `root`. Other files in the directory
/// (`CHECKPOINT`, `*.lock`, operator-placed) are deliberately
/// ignored — the segment-header magic check would reject them
/// later anyway, but filtering by extension avoids the cost.
fn list_segments(root: &std::path::Path) -> Result<Vec<PathBuf>, OpenError> {
    // Per-entry errors surface as `OpenError::Io`. A
    // `filter_map(|e| e.ok())` would silently drop entries —
    // a permission-denied stat on the newest segment would
    // become "no segments exist, mint a fresh one alongside
    // the unreadable existing one," which violates §6.1's
    // "open the lexicographically-greatest segment" contract.
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|source| OpenError::Io {
        op: "read_dir(wal_root)",
        source,
    })? {
        let path = entry
            .map_err(|source| OpenError::Io {
                op: "read_dir_entry(wal_root)",
                source,
            })?
            .path();
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wal"))
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Open the segment at `path` for further appends. The header
/// is validated (RFC0008.5: bad magic / unknown version are
/// hard errors that surface as [`OpenError::Corrupt`]); the
/// segment's `UUIDv7` comes from the in-file header so a
/// renamed file still decodes correctly.
///
/// RFC 0052 §3.3 withdrew the idea of unlinking an unreadable newest
/// segment: the shape carries no evidence of which cause produced it,
/// and a heuristic that removes it can silently discard real data. So
/// this still halts; what the RFC adds is that the halt is actionable,
/// naming the file and the shape without claiming a cause.
fn open_existing_segment(path: &std::path::Path) -> Result<(File, PathBuf, uuid::Uuid), OpenError> {
    let mut handle = OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .map_err(|source| OpenError::Io {
            op: "open(existing segment)",
            source,
        })?;
    let header = segment::read_header(&mut handle).map_err(|e| OpenError::Corrupt {
        detail: format!(
            "segment header at {}: {e}. This is the newest segment in the WAL root and its header does not read. A rotation that failed before RFC 0052 §3.3's temporary-name sequence is one way to produce this shape — a header-only file left under its final name — but an unreadable header is indistinguishable from real corruption, so nothing is unlinked and whether to remove this file is an operator's decision.",
            path.display()
        ),
    })?;
    Ok((handle, path.to_path_buf(), header.segment_uuid))
}

/// Create a brand-new segment under `root`: mint a `UUIDv7`,
/// open `<root>/<uuid>.wal` with `create_new(true)` (the
/// caller's race-safe primitive), write the 24 B §6.2.1
/// header, flush — but **do not** fsync. fsync is the §6.3
/// `sync` call's job; `open` is intentionally cheap so the
/// receiver can start servicing requests promptly.
fn create_fresh_segment(root: &std::path::Path) -> Result<(File, PathBuf, uuid::Uuid), OpenError> {
    let uuid = uuid::Uuid::now_v7();
    let path = root.join(format!("{uuid}.wal"));
    let handle = create_segment_at(&path, uuid).map_err(|e| OpenError::Io {
        op: e.op,
        source: e.source,
    })?;
    Ok((handle, path, uuid))
}

/// Which half of the two-step segment creation failed.
///
/// `Wal::open` reports the halves separately, as it always has: an
/// operator reading `create(fresh segment)` is looking at a different
/// fault from one reading `write(segment header)`. A rotation reports
/// its own §3.3 site instead, since the retry budget is charged per
/// site and the two halves share one.
struct SegmentCreateError {
    op: &'static str,
    source: std::io::Error,
}

/// Create one segment file at `path` and write its §6.2.1 header.
///
/// `path` is the final `*.wal` name on [`Wal::open`]'s fresh-root path
/// and the `<uuid>.wal.partial` temporary name on RFC 0052 §3.3's
/// rotation path; the bytes written are identical either way, which is
/// what lets the rename install the file unchanged.
fn create_segment_at(path: &std::path::Path, uuid: uuid::Uuid) -> Result<File, SegmentCreateError> {
    let mut handle = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(path)
        .map_err(|source| SegmentCreateError {
            op: "create(fresh segment)",
            source,
        })?;
    write_header(&mut handle, &SegmentHeader::new(uuid)).map_err(|source| SegmentCreateError {
        op: "write(segment header)",
        source,
    })?;
    // SEGMENT_HEADER_LEN sanity — if `write_header` ever
    // diverges from the on-disk format constant, the metadata
    // size below disagrees with `SEGMENT_HEADER_LEN` and the
    // assertion fires. We query the file's *metadata*
    // (post-fsync-irrelevant byte length) rather than the
    // handle's `stream_position`: on `O_APPEND` handles each
    // write atomically lands at end-of-file but the
    // user-space file-position cursor isn't guaranteed
    // synchronised with the OS-level write offset on every
    // platform (see Linux `fcntl(O_APPEND)` notes), so
    // `stream_position` can return 0 or a stale value.
    debug_assert_eq!(
        handle.metadata().map(|m| m.len()).unwrap_or_default(),
        SEGMENT_HEADER_LEN as u64,
        "segment header write must produce exactly SEGMENT_HEADER_LEN bytes",
    );
    Ok(handle)
}

/// How one segment's frame scan terminated (the §6.6 step-3
/// per-segment outcome the [`Wal::replay`] driver acts on).
enum SegmentScan {
    /// Frames ended on a clean frame boundary (the scan reached
    /// end-of-file exactly between frames). No healing needed.
    CleanTail,
    /// The newest segment stopped on a torn (partial) tail frame
    /// at byte `valid_to` — RFC0008.4. The caller truncates the
    /// segment to `valid_to`. Only ever returned for the newest
    /// segment; a torn tail on a closed segment is corruption.
    TornTail { valid_to: u64 },
}

/// Scan one segment's frames left-to-right (§6.6 step 3),
/// delivering each well-formed frame to `sink`. `is_newest`
/// selects the torn-tail fork: a short read on the newest
/// segment is clean truncation ([`SegmentScan::TornTail`]); on
/// any closed segment it is [`CorruptionReason::TornOnClosedSegment`].
/// A complete-but-invalid frame (CRC, `kind`, `_pad`, `len`)
/// is corruption on every segment.
fn replay_segment<S: FrameSink>(
    path: &std::path::Path,
    is_newest: bool,
    sink: &mut S,
) -> Result<SegmentScan, RecoveryError> {
    let file = File::open(path).map_err(|source| RecoveryError::Io {
        op: "open(segment for replay)",
        source,
    })?;
    let file_len = file
        .metadata()
        .map_err(|source| RecoveryError::Io {
            op: "stat(segment for replay)",
            source,
        })?
        .len();
    let mut reader = BufReader::new(file);
    let segment_uuid = match segment::read_header(&mut reader) {
        Ok(header) => header.segment_uuid,
        Err(segment::HeaderError::Io(source)) => {
            return Err(RecoveryError::Io {
                op: "read_header(segment for replay)",
                source,
            });
        }
        // Bad magic / unknown version on a `*.wal` file is
        // corruption. Full per-reason classification (a dedicated
        // `CorruptionReason`) is RFC0008.5's remit; here it
        // surfaces as an unreadable segment so recovery halts
        // rather than silently skipping the file's data.
        Err(other) => {
            return Err(RecoveryError::Io {
                op: "validate_header(segment for replay)",
                // Pass the typed `HeaderError` straight through as
                // the `io::Error` source so the structured variant
                // (bad magic vs unknown version, with its bytes)
                // survives in the error chain rather than being
                // flattened to a string.
                source: std::io::Error::new(ErrorKind::InvalidData, other),
            });
        }
    };
    let mut pos = SEGMENT_HEADER_LEN as u64;
    loop {
        if pos >= file_len {
            // Reached end-of-file aligned on a frame boundary —
            // the legitimate clean end of a segment.
            return Ok(SegmentScan::CleanTail);
        }
        let frame_start = pos;
        match frame::read_frame(&mut reader) {
            Ok((kind, payload)) => {
                pos += frame::FRAME_HEADER_LEN as u64
                    + u64::try_from(payload.len())
                        .expect("payload.len() fits u64 (read_frame capped it at MAX_FRAME_BYTES)");
                // Post-frame byte = the append-offset `append`
                // returned for this frame (§6.1).
                sink.consume(
                    WalOffset {
                        segment: segment_uuid,
                        byte: pos,
                    },
                    kind,
                    &payload,
                )?;
            }
            // Short read with bytes still remaining (`pos <
            // file_len`, guaranteed by the guard above) = a torn
            // tail frame.
            Err(frame::FrameError::Io(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                if is_newest {
                    return Ok(SegmentScan::TornTail {
                        valid_to: frame_start,
                    });
                }
                return Err(RecoveryError::Corrupt {
                    segment: segment_uuid,
                    byte: frame_start,
                    reason: CorruptionReason::TornOnClosedSegment,
                });
            }
            Err(frame::FrameError::Io(source)) => {
                return Err(RecoveryError::Io {
                    op: "read_frame(segment for replay)",
                    source,
                });
            }
            Err(frame::FrameError::CrcMismatch { .. }) => {
                return Err(RecoveryError::Corrupt {
                    segment: segment_uuid,
                    byte: frame_start,
                    reason: CorruptionReason::CrcMismatch,
                });
            }
            Err(frame::FrameError::UnknownKind { .. }) => {
                return Err(RecoveryError::Corrupt {
                    segment: segment_uuid,
                    byte: frame_start,
                    reason: CorruptionReason::UnknownKind,
                });
            }
            Err(frame::FrameError::NonZeroPad { .. }) => {
                return Err(RecoveryError::Corrupt {
                    segment: segment_uuid,
                    byte: frame_start,
                    reason: CorruptionReason::NonZeroPad,
                });
            }
            Err(frame::FrameError::OversizeLen { .. }) => {
                return Err(RecoveryError::Corrupt {
                    segment: segment_uuid,
                    byte: frame_start,
                    reason: CorruptionReason::OversizeLen,
                });
            }
        }
    }
}

/// The §6.3 file-data sync primitive every durability-critical path
/// uses — `Wal::sync`, rotation's close-segment and fresh-header
/// syncs, and recovery's post-truncate heal: `fdatasync`, upgraded to
/// `fcntl(F_FULLFSYNC)` on macOS when [`WalConfig::macos_full_fsync`]
/// opts in (macOS's `fsync`/`fdatasync` do not flush the drive cache,
/// and a torn tail on a closed or healed segment is exactly as fatal
/// as one on the live segment).
#[cfg(target_os = "macos")]
fn sync_file_data(file: &File, macos_full_fsync: bool) -> std::io::Result<()> {
    if macos_full_fsync {
        return rustix::fs::fcntl_fullfsync(file).map_err(std::io::Error::from);
    }
    file.sync_data()
}

/// See the macOS variant: everywhere else `fdatasync` is the §6.3
/// contract and the knob is ignored.
#[cfg(not(target_os = "macos"))]
fn sync_file_data(file: &File, _macos_full_fsync: bool) -> std::io::Result<()> {
    file.sync_data()
}

/// `fsync` the WAL root directory so a freshly-created or
/// freshly-truncated segment's directory entry is durable
/// (§6.3 / §6.6 step 4). Opens the directory read-only and
/// calls the full `fsync` (`File::sync_all`) — `fdatasync` is
/// undefined on directories under POSIX.
fn sync_parent_dir(root: &std::path::Path) -> std::io::Result<()> {
    File::open(root)?.sync_all()
}

/// Age of a segment, from the `UUIDv7`'s embedded millisecond
/// timestamp — the instant the header was written (§6.5's
/// "since its header was written"), with no extra persisted
/// state and surviving reopen. A full `Duration` rather than
/// whole seconds: truncation would delay the age cap by up to
/// a second past the configured bound. `None` for a non-v7
/// UUID or a clock reading behind the mint time (skew); the
/// caller treats `None` as "not age-rotatable", the
/// conservative direction.
fn segment_age(segment: uuid::Uuid) -> Option<std::time::Duration> {
    let (secs, nanos) = segment.get_timestamp()?.to_unix();
    let created = std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos);
    std::time::SystemTime::now().duration_since(created).ok()
}

/// Recovery-time consumer the [`Wal::replay`] scan hands
/// frames to. Implemented by the ingester's recovery driver:
/// `OtlpBatch` frames re-run through the decoder + tenant
/// fan-out + miner-ingest pipeline; `AuditEvent` frames
/// deserialise and reinject into the audit-event Parquet
/// writer's queue. The frame's offset is what lets the driver
/// suppress per consumer (Parquet above the checkpoint, miner
/// above its snapshot's high-water mark — §6.6).
pub trait FrameSink {
    /// Consume one recovered frame. `offset` is the frame's
    /// append-offset — the same [`WalOffset`] [`Wal::append`]
    /// returned for it.
    ///
    /// # Errors
    ///
    /// Any error the recovery driver surfaces (decoder
    /// failure, downstream pipeline rejection).
    fn consume(
        &mut self,
        offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError>;
}

/// OTel-meter snapshot per §6.8. Renders as
/// `opentelemetry`-meter readings in the ingester; this
/// struct is the cheap-clone return type the metrics call
/// hands back.
#[derive(Debug, Clone, Default)]
pub struct WalMetrics {
    pub appends_total: u64,
    pub syncs_total: u64,
    pub unflushed_bytes: u64,
    pub disk_bytes: u64,
    pub segment_count: u32,
    pub checkpoint_segment: Option<uuid::Uuid>,
    pub checkpoint_byte: u64,
    pub corrupt_frames_total: u64,
}

// -----------------------------------------------------------
// Errors (RFC 0008 §6.1 — return-type surface)
// -----------------------------------------------------------

/// Errors from [`Wal::open`].
#[derive(Debug)]
pub enum OpenError {
    /// A tunable in [`WalConfig`] was outside its §6.9
    /// validated range. Names the field + the offending value
    /// so an operator can correct the config.
    InvalidConfig { field: &'static str, detail: String },
    /// Filesystem I/O failure (root directory unreadable,
    /// segment listing failed, header read failed).
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// A segment header didn't validate (magic / version), or
    /// the `CHECKPOINT` sidecar is corrupt. Treat as data
    /// corruption — the operator must intervene.
    Corrupt { detail: String },
}

/// Errors from [`Wal::append`].
#[derive(Debug)]
pub enum AppendError {
    /// Payload exceeds [`MAX_FRAME_BYTES`].
    TooLarge { len: usize, limit: usize },
    /// I/O failure on the append. The caller MUST treat this
    /// as a hard error and refuse to ack the batch (§3.4).
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// A rotation step failed and RFC 0052 §3.3's retry budget still
    /// holds: a later `append` re-enters `rotate` and can succeed, so
    /// the failure is genuinely transient (RFC0052.15). No batch is
    /// acked either way — the frame did not land.
    RotationRetrying(RotationFault),
    /// The rotation retry budget is exhausted (RFC 0052 §3.3): every
    /// append is refused until an operator intervenes, and the only
    /// exit today is a restart. The fault carries the *first*
    /// underlying I/O error, not a generic quiesce message, so the
    /// diagnosis survives to the operator.
    ///
    /// Reported **server-terminal, client-retryable** — RFC 0018 §3.2's
    /// third class, which this RFC adds: the status stays
    /// `UNAVAILABLE` / `503` because the batch was never acked and the
    /// client must keep it, but no delay fixes the node.
    RotationTerminal(RotationFault),
}

/// Errors from [`Wal::sync`].
#[derive(Debug)]
pub enum SyncError {
    /// `fdatasync` (or platform equivalent) failed. The
    /// receiver MUST NOT ack any batch whose frames were
    /// covered by the failed sync (§3.4).
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// The rotation-origin parent-directory fsync RFC 0052 §3.3 leaves
    /// owed failed, and the retry budget still holds. Transient: a
    /// later `sync` discharges it. Nothing behind it is acked, because
    /// the installed segment's directory entry is not yet durable.
    RotationRetrying(RotationFault),
    /// That discharge exhausted the budget (RFC 0052 §3.3). Reported
    /// server-terminal, client-retryable, exactly as the append surface
    /// does — an ordinary fsync failure never reaches this variant,
    /// which is what keeps RFC0052.15's reclassification narrow.
    RotationTerminal(RotationFault),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len, limit } => {
                write!(f, "frame payload {len} B exceeds the {limit} B limit")
            }
            Self::Io { op, source } => write!(f, "WAL append failed at {op}: {source}"),
            Self::RotationRetrying(fault) => write!(f, "{}", retrying_message(fault)),
            Self::RotationTerminal(fault) => write!(f, "{}", terminal_message(fault)),
        }
    }
}

/// The wire message for a rotation failure still inside its budget. It
/// says the WAL is retrying, because RFC0052.15 requires a client to be
/// told the difference between a state a later request clears and one
/// that needs an operator.
fn retrying_message(fault: &RotationFault) -> String {
    format!(
        "WAL rotation failed at {} and is retrying (attempt {} of {}, RFC 0052 §3.3): {}",
        fault.op(),
        fault.attempts(),
        fault.budget(),
        fault.detail(),
    )
}

/// The wire message for the terminal state. It names the state and
/// carries the first underlying error, so an operator reading one
/// response learns both.
fn terminal_message(fault: &RotationFault) -> String {
    format!(
        "WAL rotation is terminal after {} failed attempts and needs an operator \
         (RFC 0052 §3.3); first failure at {}: {}",
        fault.attempts(),
        fault.op(),
        fault.detail(),
    )
}

impl std::error::Error for AppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::TooLarge { .. } | Self::RotationRetrying(_) | Self::RotationTerminal(_) => None,
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "WAL sync failed at {op}: {source}"),
            Self::RotationRetrying(fault) => write!(f, "{}", retrying_message(fault)),
            Self::RotationTerminal(fault) => write!(f, "{}", terminal_message(fault)),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::RotationRetrying(_) | Self::RotationTerminal(_) => None,
        }
    }
}

/// Errors from [`Wal::checkpoint`].
#[derive(Debug)]
pub enum CheckpointError {
    /// A step of the checkpoint failed. Whether the in-memory
    /// high-water mark advanced depends on **which** step, and a
    /// caller that treats them alike is reading a contract that does
    /// not hold:
    ///
    /// - up to and including the sidecar's rename, nothing is visible
    ///   under the final name, so the mark is **not** advanced — the
    ///   WAL conservatively keeps all segments rather than risk a
    ///   post-crash replay-induced data-side dup;
    /// - the parent-directory fsync *after* that rename, and RFC 0052
    ///   §3.2's `checkpoint_seen` write after it, both fail with the
    ///   new mark already durable-or-visible, so the mark **has**
    ///   advanced. Leaving it behind would let a later, lower mark
    ///   pass the monotonicity check and rewrite the sidecar
    ///   backwards.
    ///
    /// `op` names the step in every case.
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// `durable_to` is below the current high-water-mark —
    /// checkpoint advance is monotonic.
    NonMonotonic {
        current: WalOffset,
        attempted: WalOffset,
    },
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "WAL checkpoint failed at {op}: {source}"),
            Self::NonMonotonic { current, attempted } => write!(
                f,
                "WAL checkpoint is monotonic: segment {} byte {} is below the current segment {} byte {}",
                attempted.segment, attempted.byte, current.segment, current.byte,
            ),
        }
    }
}

impl std::error::Error for CheckpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NonMonotonic { .. } => None,
        }
    }
}

/// Errors from [`Wal::housekeeping`].
#[derive(Debug)]
pub enum HousekeepingError {
    /// Filesystem I/O failure (segment listing, header read,
    /// unlink, or the post-unlink directory fsync). The pass is
    /// safe to retry on the next cadence tick — unlinking whole
    /// segments is idempotent.
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// The pass's consumer mode disagrees with the mode recorded in
    /// the `RECLAIM` header (RFC 0052 §3.2). Every pass on a root runs
    /// under the recorded mode: a `NoConsumer` pass on a root a miner
    /// reclaimed from would delete frames nothing can re-mine, so the
    /// pass is refused before anything is planned or unlinked.
    ModeDisagreement {
        recorded: &'static str,
        attempted: &'static str,
    },
    /// A tenant's state cannot be told apart from a loss (RFC 0052
    /// §3.2): either the `RECLAIM` record holds a `Known` entry above
    /// the tenant's restorable horizon — the frames it names are gone
    /// and no pin can rebuild what they held — or the root is still on
    /// the legacy branch and the tenant's oldest surviving frame sits
    /// above its last recorded horizon, which is what reclamation
    /// under a version-1 checkpoint leaves behind.
    Unrecoverable { tenant: String, horizon: WalOffset },
}

impl std::fmt::Display for HousekeepingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "WAL housekeeping failed at {op}: {source}"),
            Self::ModeDisagreement {
                recorded,
                attempted,
            } => write!(
                f,
                "WAL housekeeping refused: this root recorded consumer mode {recorded} and the pass offered {attempted} (RFC 0052 §3.2)"
            ),
            Self::Unrecoverable { tenant, horizon } => write!(
                f,
                "WAL housekeeping refused: tenant {tenant} has no restorable snapshot at or above segment {} byte {} (RFC 0052 §3.2)",
                horizon.segment, horizon.byte,
            ),
        }
    }
}

impl std::error::Error for HousekeepingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::ModeDisagreement { .. } | Self::Unrecoverable { .. } => None,
        }
    }
}

/// What one [`Wal::housekeeping_prepare`] handed to the file half and
/// [`Wal::housekeeping_commit`] has still to account for. Segments and
/// partials are kept apart because their failure paths differ: a
/// segment the file half never verified stays reclaiming in the
/// ledger, while a partial goes back on the sweep's list.
#[derive(Debug)]
struct Outstanding {
    /// The pass this state belongs to, so a plan a later prepare
    /// superseded is refused before its record is written (§3.7).
    pass: pass::PassId,
    segments: Vec<retain::Popped>,
    partials: Vec<PathBuf>,
    /// The mode this pass runs under, so the record merge in the file
    /// half adopts the same one the ledger half was checked against.
    mode: reclaim::EntryMode,
    /// What the ledger half decided. The commit reports the same
    /// floor, lag, cap state and skip reason: they are facts about
    /// this pass, and re-deriving them from a ledger the unlinks have
    /// since changed would describe a different one.
    progress: HousekeepingProgress,
}

impl Drop for Wal {
    /// A permit outlives the `Wal` that issued it — the plan and the
    /// permit are owned values the file half holds with no handle —
    /// so dropping the WAL has to revoke it. Otherwise a caller could
    /// drop this instance, reopen the same root, and unlink against a
    /// cell the gone instance still owns, past a new `Wal` that
    /// refuses the stale plan at both of its own checks.
    fn drop(&mut self) {
        self.live_pass
            .store(0, std::sync::atomic::Ordering::Release);
    }
}

/// Numbers each `Wal` this process opens, so [`PassId`] names the
/// instance as well as the pass (RFC 0052 §3.7).
static NEXT_WAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Mark one segment's `planned` entry as RFC 0052 §3.2's uncertain
/// deletion, so the next pass re-verifies its presence.
fn mark_uncertain(record: &mut reclaim::ReclaimRecord, segment: uuid::Uuid) {
    for entry in &mut record.planned {
        if entry.segment == segment {
            entry.uncertain = true;
        }
    }
}

/// The WAL state RFC 0052 §3.5 exports, as [`Wal::reclaim_state`]
/// returns it. `disk_bytes` stays the best-effort diagnostic
/// [`WalMetrics`] documents; `unreclaimed_bytes` is the exact figure,
/// seeded from the post-recovery ledger walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimState {
    pub unflushed_bytes: u64,
    pub disk_bytes: u64,
    pub segment_count: u32,
    pub unreclaimed_bytes: u64,
    pub checkpoint: Option<WalOffset>,
    /// `<uuid>.wal.partial` files awaiting the housekeeping sweep.
    pub stale_partials: usize,
    /// Whether a pass may plan segments: RFC 0052 §3.2's witness, a
    /// version-2 `CHECKPOINT` beside a `RECLAIM` record.
    pub reclaimable: bool,
    /// The floor the WAL derived on its last pass (RFC 0052 §3.7).
    /// Between passes that is by definition the floor governing
    /// retention, so the export is never stale; before the first it is
    /// [`RetainFloor::Unknown`], which is not the same claim as "no
    /// consumer exists".
    pub floor: RetainFloor,
    /// RFC 0052 §3.3's rotation state — healthy, retrying with its
    /// attempt count, or terminal. §3.5 exports the distinction because
    /// "retrying" and "given up" need different operator responses.
    pub rotation: RotationState,
}

/// Errors from [`Wal::replay`].
#[derive(Debug)]
pub enum RecoveryError {
    /// Filesystem I/O failure (segment listing, segment open,
    /// segment read).
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// A frame failed RFC0008.5 corruption checks (CRC
    /// mismatch, unknown `kind`, non-zero `_pad`, oversize
    /// `len`, or torn header/payload on a non-newest segment).
    /// Recovery stops scanning *all* segments; an operator
    /// must inspect the named segment before resuming.
    Corrupt {
        segment: uuid::Uuid,
        byte: u64,
        reason: CorruptionReason,
    },
    /// The sink rejected a recovered frame. Surfaces a
    /// downstream pipeline error during replay.
    SinkRejected { detail: String },
}

/// Discriminated reason for [`RecoveryError::Corrupt`], one
/// per §5 RFC0008.5 sub-case so the audit event + test
/// assertions can match exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionReason {
    CrcMismatch,
    UnknownKind,
    NonZeroPad,
    OversizeLen,
    TornOnClosedSegment,
}

/// Stub helper that lets a test create an
/// [`AuditEvent`]-bearing frame payload once the encoder
/// lands (the §9 open-question encoder choice). Documented
/// here so the trait surface is complete; returns
/// `unimplemented!()` for now.
#[must_use]
pub fn encode_audit_event(_event: &AuditEvent) -> Vec<u8> {
    unimplemented!("RFC 0008 §9 — AuditEvent serde format lands with the encoder PR");
}

#[cfg(test)]
mod tests {
    //! Colocated unit tests for the `Wal::open` helpers per
    //! CLAUDE.md §6.2 (unit tests next to the code for
    //! anything non-trivial). End-to-end coverage of `open`
    //! lives in `tests/open.rs`; this module pins the smaller
    //! helper-level contracts so a regression caught at this
    //! layer surfaces here rather than as a cascading failure
    //! in the integration suite.

    // RFC0046.11 — the TenantOtlpBatch prefix is validated before the
    // protobuf is exposed; each malformed shape is its own error.
    #[test]
    fn tenant_batch_round_trips_and_rejects_malformed_prefixes() {
        use super::{TenantBatch, TenantBatchError};
        let payload = TenantBatch::encode("acme/eu", b"proto").expect("encode");
        let decoded = TenantBatch::decode(&payload).expect("decode");
        assert_eq!(decoded.tenant, "acme/eu");
        assert_eq!(decoded.protobuf, b"proto");
        let max = "x".repeat(TenantBatch::MAX_TENANT_BYTES);
        assert!(TenantBatch::encode(&max, b"").is_ok());

        assert_eq!(
            TenantBatch::encode("", b"p").unwrap_err(),
            TenantBatchError::EmptyTenant
        );
        assert!(matches!(
            TenantBatch::encode(&"x".repeat(257), b"p").unwrap_err(),
            TenantBatchError::TenantTooLong { found: 257 }
        ));
        assert!(matches!(
            TenantBatch::decode(&[7]).unwrap_err(),
            TenantBatchError::TruncatedPrefix { found: 1 }
        ));
        assert_eq!(
            TenantBatch::decode(&[0, 0, b'p']).unwrap_err(),
            TenantBatchError::EmptyTenant
        );
        assert!(matches!(
            TenantBatch::decode(&[1, 1, b'a']).unwrap_err(),
            TenantBatchError::TenantTooLong { found: 257 }
        ));
        assert!(matches!(
            TenantBatch::decode(&[5, 0, b'a', b'b']).unwrap_err(),
            TenantBatchError::TenantPastEnd {
                declared: 5,
                available: 2
            }
        ));
        assert_eq!(
            TenantBatch::decode(&[1, 0, 0xFF, b'p']).unwrap_err(),
            TenantBatchError::NotUtf8
        );
    }

    use super::*;

    /// RFC 0052 §3.2 with §6.3: no segment is reclaimed under a
    /// checkpoint whose directory entry is only renamed, not fsynced.
    /// The mark advances in memory at the rename so a later lower one
    /// cannot rewrite the sidecar backwards, but a crash that lost
    /// that rename would revert the checkpoint beneath frames whose
    /// segments the pass had already unlinked.
    #[test]
    fn housekeeping_waits_for_the_checkpoint_directory_fsync() {
        let dest = tempfile::TempDir::new().expect("temp");
        let offsets = mint_closed_segment(dest.path(), &[b"a1", b"a2"]);
        mint_closed_segment(dest.path(), &[b"b1"]);
        let mut wal = Wal::open(default_config(dest.path())).expect("open");
        wal.checkpoint(*offsets.last().expect("offsets"))
            .expect("checkpoint");
        assert!(
            wal.checkpoint_is_settled(),
            "a completed checkpoint is reclaimable",
        );

        wal.reclaim_gate = ReclaimGate::FsyncPending;
        wal.housekeeping(None).expect("housekeeping");
        assert_eq!(
            list_segments(dest.path()).expect("list").len(),
            2,
            "nothing is reclaimed while the sidecar's entry is not durable",
        );

        wal.reclaim_gate = ReclaimGate::Open;
        wal.housekeeping(None).expect("housekeeping");
        assert_eq!(
            list_segments(dest.path()).expect("list").len(),
            1,
            "and the next pass reclaims once it is",
        );
    }

    /// A checkpoint whose `checkpoint_seen` write failed leaves the
    /// record only `Armed`, and no segment is reclaimed under that
    /// either: §3.2's matrix reads a later missing `CHECKPOINT` beside
    /// an armed record as a root mid migration rather than as a loss,
    /// and by then the frames the pass unlinked would be gone.
    #[test]
    fn housekeeping_waits_for_the_startup_loss_witness() {
        let dest = tempfile::TempDir::new().expect("temp");
        let offsets = mint_closed_segment(dest.path(), &[b"a1", b"a2"]);
        mint_closed_segment(dest.path(), &[b"b1"]);
        let mut wal = Wal::open(default_config(dest.path())).expect("open");
        wal.checkpoint(*offsets.last().expect("offsets"))
            .expect("checkpoint");

        // Wind the witness back to where a failed `checkpoint_seen`
        // write would have left it.
        let store = wal.reclaim.as_mut().expect("a post-RFC root has a record");
        let armed = reclaim::ReclaimRecord {
            witness: reclaim::WitnessFlags {
                checkpoint: reclaim::Witness::Armed,
                ..store.record().witness
            },
            ..store.record().clone()
        };
        store.commit(&armed).expect("commit");

        assert!(!wal.checkpoint_is_settled());
        wal.housekeeping(None).expect("housekeeping");
        assert_eq!(
            list_segments(dest.path()).expect("list").len(),
            2,
            "nothing is reclaimed while the loss witness is only armed",
        );
    }

    /// Build a closed segment in a scratch root and move it into
    /// `dest`, bringing the record the producing root created with it
    /// (RFC 0052 §3.2 fails closed on a version-2 segment beside no
    /// sidecar). Returns the frames' append offsets.
    fn mint_closed_segment(dest: &std::path::Path, payloads: &[&[u8]]) -> Vec<WalOffset> {
        let scratch = tempfile::TempDir::new().expect("scratch");
        let mut wal = Wal::open(default_config(scratch.path())).expect("open scratch");
        let offsets = payloads
            .iter()
            .map(|p| wal.append(FrameKind::OtlpBatch, p).expect("append"))
            .collect();
        wal.sync().expect("sync");
        drop(wal);
        let seg = list_segments(scratch.path())
            .expect("list")
            .into_iter()
            .next()
            .expect("one segment");
        std::fs::rename(&seg, dest.join(seg.file_name().expect("name"))).expect("move");
        let record = dest.join(reclaim::SIDECAR_NAME);
        if !record.exists() {
            std::fs::copy(scratch.path().join(reclaim::SIDECAR_NAME), &record).expect("record");
        }
        offsets
    }

    /// §6.3 macOS strong durability (#125): with the knob set, the
    /// segment sync goes through `fcntl(F_FULLFSYNC)` and the
    /// append→sync contract (durable offset advances) holds exactly as
    /// with `fdatasync`. On non-macOS targets the knob is accepted and
    /// ignored — the same assertions pass through the `fdatasync` arm,
    /// so this test runs everywhere and exercises the fcntl only where
    /// it exists.
    #[test]
    fn macos_full_fsync_knob_keeps_the_sync_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = default_config(dir.path());
        config.macos_full_fsync = true;
        let mut wal = Wal::open(config).expect("open");
        wal.append(FrameKind::OtlpBatch, b"full-fsync me")
            .expect("append");
        let offset = wal.sync().expect("sync with the knob set");
        assert!(offset.byte > 0, "durable offset advances");
    }

    fn default_config(root: &std::path::Path) -> WalConfig {
        WalConfig {
            root: root.to_path_buf(),
            batch_window_ms: 100,
            segment_size_bytes: 128 * 1024 * 1024,
            segment_age_secs: 600,
            housekeeping_secs: 60,
            max_unlinks_per_pass: DEFAULT_MAX_UNLINKS_PER_PASS,
            rotation_retry_attempts: DEFAULT_ROTATION_RETRY_ATTEMPTS,
            macos_full_fsync: false,
        }
    }

    /// `validate_config` accepts every default and every
    /// exact boundary value — both the lower and upper edges
    /// of each Tunable's §6.9 range. Catches an off-by-one
    /// that would reject e.g. `segment_size_bytes ==
    /// MIN_SEGMENT_SIZE_BYTES`.
    #[test]
    fn validate_config_accepts_defaults_and_exact_boundaries() {
        let tmp = tempfile::TempDir::new().expect("temp");
        validate_config(&default_config(tmp.path())).expect("defaults");
        let boundaries = [
            WalConfig {
                batch_window_ms: 0,
                ..default_config(tmp.path())
            },
            WalConfig {
                batch_window_ms: MAX_BATCH_WINDOW_MS,
                ..default_config(tmp.path())
            },
            WalConfig {
                segment_size_bytes: MIN_SEGMENT_SIZE_BYTES,
                ..default_config(tmp.path())
            },
            WalConfig {
                segment_size_bytes: MAX_SEGMENT_SIZE_BYTES,
                ..default_config(tmp.path())
            },
            WalConfig {
                segment_age_secs: MIN_SEGMENT_AGE_SECS,
                ..default_config(tmp.path())
            },
            WalConfig {
                segment_age_secs: MAX_SEGMENT_AGE_SECS,
                ..default_config(tmp.path())
            },
            WalConfig {
                housekeeping_secs: MIN_HOUSEKEEPING_SECS,
                ..default_config(tmp.path())
            },
            WalConfig {
                housekeeping_secs: MAX_HOUSEKEEPING_SECS,
                ..default_config(tmp.path())
            },
            // RFC 0052 §3.8 writes this knob's range as
            // `rotation_retry_attempts..=65_536`, so its lower boundary
            // is a *pair*: the format's own 1 is reachable only with a
            // budget of 1 beside it.
            WalConfig {
                max_unlinks_per_pass: 1,
                rotation_retry_attempts: MIN_ROTATION_RETRY_ATTEMPTS,
                ..default_config(tmp.path())
            },
            WalConfig {
                max_unlinks_per_pass: reclaim::MAX_UNLINKS_PER_PASS_CEILING,
                ..default_config(tmp.path())
            },
            WalConfig {
                rotation_retry_attempts: MIN_ROTATION_RETRY_ATTEMPTS,
                ..default_config(tmp.path())
            },
            WalConfig {
                rotation_retry_attempts: MAX_ROTATION_RETRY_ATTEMPTS,
                max_unlinks_per_pass: MAX_ROTATION_RETRY_ATTEMPTS,
                ..default_config(tmp.path())
            },
        ];
        for cfg in boundaries {
            validate_config(&cfg).expect("boundary value");
        }
    }

    /// Every just-outside-bounds value is rejected and the
    /// error names the violated field. Iterated rather than
    /// one test per arm — the message format is what the
    /// operator sees on a real misconfiguration.
    ///
    /// The cases are grouped the way `validate_config` is, so a new
    /// tunable's rows land beside the ones they belong with rather than
    /// extending one list that grows with every RFC.
    #[test]
    fn validate_config_rejects_each_out_of_range_field() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let mut cases = timing_rejections(tmp.path());
        cases.extend(sizing_rejections(tmp.path()));
        cases.extend(reclamation_rejections(tmp.path()));
        for (expected_field, cfg) in cases {
            match validate_config(&cfg).expect_err("out-of-range must reject") {
                OpenError::InvalidConfig { field, .. } => assert_eq!(
                    field, expected_field,
                    "validation should name the violating field exactly",
                ),
                other => panic!("expected InvalidConfig({expected_field}), got {other:?}"),
            }
        }
    }

    type Rejection = (&'static str, WalConfig);

    fn timing_rejections(root: &std::path::Path) -> Vec<Rejection> {
        vec![
            (
                "batch_window_ms",
                WalConfig {
                    batch_window_ms: MAX_BATCH_WINDOW_MS + 1,
                    ..default_config(root)
                },
            ),
            (
                "segment_age_secs",
                WalConfig {
                    segment_age_secs: MIN_SEGMENT_AGE_SECS - 1,
                    ..default_config(root)
                },
            ),
            (
                "segment_age_secs",
                WalConfig {
                    segment_age_secs: MAX_SEGMENT_AGE_SECS + 1,
                    ..default_config(root)
                },
            ),
            (
                "housekeeping_secs",
                WalConfig {
                    housekeeping_secs: MIN_HOUSEKEEPING_SECS - 1,
                    ..default_config(root)
                },
            ),
            (
                "housekeeping_secs",
                WalConfig {
                    housekeeping_secs: MAX_HOUSEKEEPING_SECS + 1,
                    ..default_config(root)
                },
            ),
        ]
    }

    fn sizing_rejections(root: &std::path::Path) -> Vec<Rejection> {
        vec![
            (
                "segment_size_bytes",
                WalConfig {
                    segment_size_bytes: MIN_SEGMENT_SIZE_BYTES - 1,
                    ..default_config(root)
                },
            ),
            (
                "segment_size_bytes",
                WalConfig {
                    segment_size_bytes: MAX_SEGMENT_SIZE_BYTES + 1,
                    ..default_config(root)
                },
            ),
        ]
    }

    fn reclamation_rejections(root: &std::path::Path) -> Vec<Rejection> {
        vec![
            (
                "rotation_retry_attempts",
                WalConfig {
                    rotation_retry_attempts: MIN_ROTATION_RETRY_ATTEMPTS - 1,
                    ..default_config(root)
                },
            ),
            (
                "rotation_retry_attempts",
                WalConfig {
                    rotation_retry_attempts: MAX_ROTATION_RETRY_ATTEMPTS + 1,
                    ..default_config(root)
                },
            ),
            // RFC 0052 §3.8's one cross-knob rule: both values are
            // individually legal, and the pair is not.
            (
                "max_unlinks_per_pass",
                WalConfig {
                    max_unlinks_per_pass: 2,
                    rotation_retry_attempts: 3,
                    ..default_config(root)
                },
            ),
        ]
    }

    /// `list_segments` filters by `.wal` extension and sorts
    /// the result lex (= chronological per `UUIDv7`). Mixed
    /// non-segment files are ignored without erroring.
    #[test]
    fn list_segments_filters_and_sorts() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let a = tmp.path().join("01890c43-7b3d-7c01-9e00-aaaaaaaaaaaa.wal");
        let b = tmp.path().join("01890c43-7b3d-7c01-9e00-bbbbbbbbbbbb.wal");
        let other = tmp.path().join("CHECKPOINT");
        let readme = tmp.path().join("README.md");
        for p in [&b, &a, &other, &readme] {
            std::fs::File::create(p).expect("create");
        }
        let listed = list_segments(tmp.path()).expect("list");
        assert_eq!(listed, vec![a, b], "lex-sorted .wal entries only");
    }

    /// `create_fresh_segment` lays down a real file on disk
    /// whose name parses as `UUIDv7` (version 7, not just
    /// "parseable") and whose body is exactly the 24 B header
    /// matching `SegmentHeader::new(uuid)`. Pins the §6.1
    /// "lex-sortable filename = chronological order"
    /// contract.
    #[test]
    fn create_fresh_segment_writes_a_v7_named_header_only_file() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let (_handle, path, uuid) = create_fresh_segment(tmp.path()).expect("create");
        assert!(path.is_file(), "file actually exists");
        assert_eq!(
            uuid.get_version_num(),
            7,
            "segment UUID MUST be UUIDv7 (chronological sort)",
        );
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        assert_eq!(
            stem.parse::<uuid::Uuid>().expect("stem parses"),
            uuid,
            "filename stem MUST equal the in-memory UUID",
        );
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes.len(), SEGMENT_HEADER_LEN, "header-only file");
        assert_eq!(&bytes[0..4], b"OWAL");
    }

    /// `open_existing_segment` reads a well-formed segment
    /// without erroring and recovers the header UUID. Built
    /// on top of `create_fresh_segment` so the input is
    /// guaranteed to match the §6.2.1 format.
    #[test]
    fn open_existing_segment_recovers_header_uuid() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let (handle_a, path, expected_uuid) = create_fresh_segment(tmp.path()).expect("create");
        drop(handle_a); // close before reopening read+append
        let (_handle_b, returned_path, returned_uuid) =
            open_existing_segment(&path).expect("reopen");
        assert_eq!(returned_path, path);
        assert_eq!(
            returned_uuid, expected_uuid,
            "in-file UUID must round-trip across open",
        );
    }

    /// The append-error rollback uses [`File::set_len`] to
    /// truncate the segment back to its pre-write length. This
    /// test pins the primitive: a [`File`] opened with
    /// `OpenOptions::append(true)` honours [`File::set_len`],
    /// and a subsequent append-only write lands at the
    /// truncated EOF (not the previous larger EOF, which would
    /// leave a hole of zero bytes between the truncated length
    /// and the new write). Mid-write I/O failures themselves
    /// are hard to inject without a mock filesystem, but the
    /// rollback's correctness reduces to "`set_len` followed
    /// by append writes at the new EOF" — which this test pins
    /// directly.
    #[test]
    fn rollback_set_len_then_append_lands_at_truncated_eof() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().expect("temp");
        let path = tmp.path().join("rollback-test.bin");
        let mut handle = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path)
            .expect("create");
        handle.write_all(b"AAAAAAAAAAAAAAAA").expect("first write"); // 16 B
        handle.set_len(8).expect("truncate to 8 B");
        assert_eq!(handle.metadata().expect("stat").len(), 8);
        handle.write_all(b"BBBB").expect("second write");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"AAAAAAAABBBB",
            "post-truncate append lands at the truncated EOF, not the old EOF",
        );
    }

    /// A foreign file with `.wal` extension but no `OWAL`
    /// magic is rejected as `OpenError::Corrupt`, not silently
    /// reused. Pins the RFC0008.5 "stray-file" rejection.
    #[test]
    fn open_existing_segment_rejects_foreign_magic() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let path = tmp.path().join("not-a-segment.wal");
        std::fs::write(&path, b"NOPEhere--filler-bytes--").expect("write");
        match open_existing_segment(&path).expect_err("must reject") {
            OpenError::Corrupt { detail } => assert!(
                detail.contains("magic mismatch"),
                "Display message should name the magic mismatch; got {detail:?}",
            ),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }
}
