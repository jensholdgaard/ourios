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
// The codec's dictionary-mutation surface (assign, tombstone, union,
// raise_reclaimed) lands ahead of its callers: the housekeeping pass
// that merges horizons into the record is RFC 0052's next slice, which
// removes this allow.
#[allow(dead_code)]
pub(crate) mod reclaim;
pub(crate) mod reclaim_store;
pub(crate) mod segment;

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
    /// rename can't slip it past the guard.
    #[allow(dead_code)]
    current_segment_path: PathBuf,
    /// `UUIDv7` of the current segment — same value as the
    /// filename's stem and the segment's in-file header per
    /// §6.2.1. Carried in every `WalOffset` `append` returns.
    current_segment_uuid: uuid::Uuid,
    /// Whether the parent directory still needs an `fsync` to
    /// make the current segment's directory entry durable
    /// (§6.3). `open` always sets it `true` and the first
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
    dir_fsync_pending: bool,
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
    /// Whether a housekeeping pass may plan segments: RFC 0052 §3.2's
    /// witness, a version-2 `CHECKPOINT` beside a record. Until it
    /// exists a pass sweeps partials and plans nothing, since
    /// unlinking under a version-1 checkpoint would leave exactly the
    /// shape the open-time matrix reads as "nothing was ever
    /// reclaimed".
    reclaimable: bool,
    /// Stale `<uuid>.wal.partial` files, seeded by
    /// [`Self::rebuild_ledger`] and popped by the housekeeping sweep.
    /// §3.3: the sweep lists nothing on the pass, so restart debris is
    /// found without a directory scan under the writer position.
    stale_partials: Vec<PathBuf>,
    /// Bytes of validated frames in surviving segments, seeded after
    /// recovery by [`Self::rebuild_ledger`] (§3.7). Never file size
    /// less header, which would count a torn tail, and never the
    /// best-effort directory walk `metrics` uses.
    unreclaimed_bytes: u64,
    /// A rotation step failed (§6.5): the closing segment's
    /// data sync, the fresh segment's creation, or the
    /// parent-dir `fsync`. Once set, every `append` is refused
    /// until an operator intervenes — continuing would risk
    /// either a torn tail on a *closed* segment (which recovery
    /// treats as RFC0008.5 corruption) or a frame landing in a
    /// segment whose directory entry is not durable.
    quiesced: bool,
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
        std::fs::create_dir_all(&config.root).map_err(|source| OpenError::Io {
            op: "create_dir_all(wal_root)",
            source,
        })?;
        // RFC 0052 §3.2: a rename's parent fsync can fail after the
        // entry is already visible to this process, so a listing alone
        // does not prove a sidecar survives the next crash. One
        // directory fsync before the sidecars are read makes every
        // entry the listing saw durable, and failing it is a fault to
        // surface rather than to continue past.
        sync_parent_dir(&config.root).map_err(|source| OpenError::Io {
            op: "fsync(wal_root before reading the sidecars)",
            source,
        })?;
        // §6.6 step 1: a present-but-invalid sidecar aborts here
        // (before any recovery) rather than being silently
        // treated as None — that would drop the Parquet
        // suppression horizon and duplicate every
        // already-published record on the data side.
        let sidecar = checkpoint::read(&config.root)?;
        let existing_segments = list_segments(&config.root)?;
        let witness = reconcile_root(&config, sidecar, &existing_segments)?;
        let (current_segment, current_segment_path, current_segment_uuid) =
            if let Some(newest) = existing_segments.into_iter().next_back() {
                open_existing_segment(&newest)?
            } else {
                create_fresh_segment(&config.root)?
            };
        Ok(Self {
            config,
            current_segment,
            current_segment_path,
            current_segment_uuid,
            // Always pending: the first `sync` fsyncs the parent
            // directory regardless of fresh-vs-existing open, so
            // an acked frame's segment is guaranteed a durable
            // directory entry (see the field doc — §3.4).
            dir_fsync_pending: true,
            quiesced: false,
            checkpoint: sidecar.map(|s| s.offset),
            checkpoint_version: sidecar.map(|s| s.version),
            reclaim: witness.store,
            reclaimable: witness.reclaimable,
            stale_partials: Vec::new(),
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
        let segments = list_segments(&self.config.root).map_err(|e| match e {
            OpenError::Io { op, source } => LedgerError::Io { op, source },
            OpenError::InvalidConfig { .. } | OpenError::Corrupt { .. } => {
                unreachable!("list_segments only surfaces OpenError::Io")
            }
        })?;
        let mut scan = LedgerScan::default();
        let newest_idx = segments.len().checked_sub(1);
        for (idx, path) in segments.iter().enumerate() {
            match replay_segment(path, Some(idx) == newest_idx, &mut scan) {
                Ok(SegmentScan::CleanTail | SegmentScan::TornTail { .. }) => {}
                Err(e) => return Err(scan.into_error(&e)),
            }
        }
        self.unreclaimed_bytes = scan.bytes;
        self.stale_partials = list_partials(&self.config.root)?;
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
            reclaimable: self.reclaimable,
        }
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
    /// data sync), a fresh one is created, and the parent dir
    /// is fsync'd before the frame lands — so a frame never
    /// straddles segments and never lands in a segment without a
    /// durable directory entry. A failed rotation quiesces the
    /// WAL (see [`AppendError::QuiescedAfterRotationFailure`]).
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
        if self.quiesced {
            return Err(AppendError::QuiescedAfterRotationFailure);
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
            self.rotate()?;
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
        Ok(WalOffset {
            segment: self.current_segment_uuid,
            byte: post_write_byte,
        })
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

    /// Close the current segment and open a fresh one (§6.5):
    /// sync the old segment's data (the last sync it ever
    /// receives — a torn tail on a *closed* segment is RFC0008.5
    /// corruption, so closing without it would convert a benign
    /// crash into a recovery halt), create the new `UUIDv7`
    /// segment with its 24 B header, then `fsync` the parent
    /// directory so the new entry is durable before any frame
    /// lands in it. Any step failing quiesces the WAL: every
    /// subsequent `append` returns
    /// [`AppendError::QuiescedAfterRotationFailure`] until an
    /// operator intervenes. (`sync` stays available — the old
    /// segment is still the append target, and acking frames
    /// already written to it is safe.)
    fn rotate(&mut self) -> Result<(), AppendError> {
        if let Err(source) = sync_file_data(&self.current_segment, self.config.macos_full_fsync) {
            self.quiesced = true;
            return Err(AppendError::Io {
                op: "sync(rotation: close segment)",
                source,
            });
        }
        // The closing data sync flushed everything appended so far.
        self.unflushed_bytes = 0;
        let (file, path, uuid) = match create_fresh_segment(&self.config.root) {
            Ok(fresh) => fresh,
            Err(OpenError::Io { op, source }) => {
                self.quiesced = true;
                return Err(AppendError::Io { op, source });
            }
            Err(OpenError::InvalidConfig { .. } | OpenError::Corrupt { .. }) => {
                unreachable!("create_fresh_segment only surfaces OpenError::Io")
            }
        };
        // The header must be durable BEFORE the directory entry: the
        // parent fsync below makes the new file name survive a power
        // cut, and a surviving entry whose 24 B header bytes were
        // lost would fail the next `Wal::open`'s header read — a
        // benign crash turned into OpenError::Corrupt. (`open`'s own
        // fresh segment doesn't carry this ordering: nothing fsyncs
        // its directory entry until the first `sync`, which
        // syncs the segment data first.)
        if let Err(source) = sync_file_data(&file, self.config.macos_full_fsync) {
            self.quiesced = true;
            return Err(AppendError::Io {
                op: "sync(rotation: fresh segment header)",
                source,
            });
        }
        if let Err(source) = sync_parent_dir(&self.config.root) {
            // The fresh header-only segment is left in place: replay
            // reads it as zero frames, and unlinking it here could
            // itself fail. The quiesce is what protects correctness.
            self.quiesced = true;
            return Err(AppendError::Io {
                op: "fsync(wal_root after rotation)",
                source,
            });
        }
        self.current_segment = file;
        self.current_segment_path = path;
        self.current_segment_uuid = uuid;
        self.dir_fsync_pending = false;
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
        if self.dir_fsync_pending {
            sync_parent_dir(&self.config.root).map_err(|source| SyncError::Io {
                op: "fsync(wal_root)",
                source,
            })?;
            self.dir_fsync_pending = false;
        }
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
    /// checkpoint is rejected; re-asserting the current value
    /// is an idempotent no-op.
    ///
    /// # Errors
    ///
    /// See [`CheckpointError`]. On error the in-memory
    /// checkpoint is **not** advanced — the WAL conservatively
    /// keeps all segments rather than risk a post-crash
    /// data-side dup.
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
            if durable_to == current
                && self.checkpoint_version == Some(checkpoint::SidecarVersion::Current)
            {
                return Ok(());
            }
        }
        self.arm_reclaim_record()?;
        checkpoint::write(&self.config.root, durable_to)?;
        self.checkpoint = Some(durable_to);
        self.checkpoint_version = Some(checkpoint::SidecarVersion::Current);
        self.reclaimable = true;
        self.witness_checkpoint()
    }

    /// Write `checkpoint_armed` durably before the first version-2
    /// `CHECKPOINT` attempt, creating the record when a legacy root
    /// has none (RFC 0052 §3.2). Arming before the attempt is what
    /// keeps a crash in the window between the two an ordinary state
    /// rather than a lost checkpoint.
    fn arm_reclaim_record(&mut self) -> Result<(), CheckpointError> {
        let armed = reclaim::WitnessFlags {
            checkpoint: reclaim::Witness::Armed,
            ..reclaim::WitnessFlags::default()
        };
        match self.reclaim.as_mut() {
            Some(store) if store.record().witness.checkpoint == reclaim::Witness::Unarmed => {
                let record = reclaim::ReclaimRecord {
                    witness: armed,
                    ..store.record().clone()
                };
                store.commit(&record).map_err(record_failed)
            }
            Some(_) => Ok(()),
            None => {
                let record = reclaim::ReclaimRecord {
                    witness: armed,
                    ..reclaim::ReclaimRecord::default()
                };
                let geometry = configured_geometry().map_err(|e| CheckpointError::Io {
                    op: "size(RECLAIM)",
                    source: std::io::Error::new(ErrorKind::InvalidData, e.to_string()),
                })?;
                let store = reclaim_store::ReclaimStore::create(
                    &self.config.root,
                    geometry,
                    &record,
                    self.config.macos_full_fsync,
                )
                .map_err(record_failed)?;
                self.reclaim = Some(store);
                Ok(())
            }
        }
    }

    /// Write `checkpoint_seen` at the next record write after the
    /// checkpoint succeeded — immediately, since no pass is pending
    /// (§3.2). Once seen the flag never clears, which is what lets the
    /// open-time matrix read a later missing `CHECKPOINT` as a loss
    /// rather than as a fresh root.
    fn witness_checkpoint(&mut self) -> Result<(), CheckpointError> {
        match self.reclaim.as_mut() {
            Some(store) if store.record().witness.checkpoint != reclaim::Witness::Terminal => {
                let record = reclaim::ReclaimRecord {
                    witness: reclaim::WitnessFlags {
                        checkpoint: reclaim::Witness::Terminal,
                        ..store.record().witness
                    },
                    ..store.record().clone()
                };
                store.commit(&record).map_err(record_failed)
            }
            Some(_) | None => Ok(()),
        }
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
    /// # Errors
    ///
    /// See [`HousekeepingError`].
    pub fn housekeeping(
        &mut self,
        retain_floor: Option<WalOffset>,
    ) -> Result<(), HousekeepingError> {
        // RFC 0052 §3.3: the sweep runs on **every** pass, witness or
        // not. A `.wal.partial` is debris from a rotation that never
        // installed a segment — no reader can depend on it and no
        // `reclaimed_through` accounts for it — so gating it would let
        // debris created before the first checkpoint survive forever.
        self.sweep_partials()?;
        let Some(cp) = self.checkpoint else {
            return Ok(());
        };
        // §3.2: a pass whose root still has a version-1 `CHECKPOINT`,
        // or no record, plans no segment. Unlinking there would leave
        // exactly the shape the open-time matrix reads as "nothing was
        // ever reclaimed".
        if !self.reclaimable {
            return Ok(());
        }
        let bound = match retain_floor {
            Some(floor) => cp.min(floor),
            None => cp,
        };
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
                unlinked_any = true;
            }
        }
        if unlinked_any {
            sync_parent_dir(&self.config.root)
                .map_err(|e| io("fsync(wal_root after housekeeping)", e))?;
        }
        Ok(())
    }

    /// Unlink stale `<uuid>.wal.partial` files, oldest first, within
    /// the per-pass cap (RFC 0052 §3.3). Each unlink is followed by a
    /// parent-directory fsync, so a crash cannot resurrect debris the
    /// pass already removed — including when a later unlink in the
    /// same pass fails.
    ///
    /// A failed unlink keeps its path in the list, which both retries
    /// it on the next pass and keeps it visible through
    /// [`Self::reclaim_state`]. Housekeeping holds the single-writer
    /// position, so no rotation attempt is in progress and every
    /// partial it sees is debris.
    fn sweep_partials(&mut self) -> Result<(), HousekeepingError> {
        let cap = usize::try_from(reclaim::DEFAULT_MAX_UNLINKS_PER_PASS).unwrap_or(usize::MAX);
        let take = self.stale_partials.len().min(cap);
        let batch: Vec<PathBuf> = self.stale_partials.drain(..take).collect();
        let mut retry = Vec::new();
        for path in batch {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    sync_parent_dir(&self.config.root).map_err(|source| HousekeepingError::Io {
                        op: "fsync(wal_root after unlink(partial))",
                        source,
                    })?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(_) => retry.push(path),
            }
        }
        self.stale_partials.extend(retry);
        Ok(())
    }

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
        self.dir_fsync_pending = false;
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
fn validate_config(c: &WalConfig) -> Result<(), OpenError> {
    let outside = |field, detail: String| OpenError::InvalidConfig { field, detail };
    if c.batch_window_ms > MAX_BATCH_WINDOW_MS {
        return Err(outside(
            "batch_window_ms",
            format!(
                "{} exceeds §6.9 upper bound {MAX_BATCH_WINDOW_MS}",
                c.batch_window_ms
            ),
        ));
    }
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
    if !(MIN_SEGMENT_AGE_SECS..=MAX_SEGMENT_AGE_SECS).contains(&c.segment_age_secs) {
        return Err(outside(
            "segment_age_secs",
            format!(
                "{} outside §6.9 range {MIN_SEGMENT_AGE_SECS}..={MAX_SEGMENT_AGE_SECS}",
                c.segment_age_secs
            ),
        ));
    }
    if !(MIN_HOUSEKEEPING_SECS..=MAX_HOUSEKEEPING_SECS).contains(&c.housekeeping_secs) {
        return Err(outside(
            "housekeeping_secs",
            format!(
                "{} outside §6.9 range {MIN_HOUSEKEEPING_SECS}..={MAX_HOUSEKEEPING_SECS}",
                c.housekeeping_secs
            ),
        ));
    }
    // `macos_full_fsync` is a `bool`; nothing to validate.
    Ok(())
}

/// The geometry every `RECLAIM` file this build creates is sized for.
/// `max_tenants` and `max_unlinks_per_pass` become [`WalConfig`] knobs
/// with the housekeeping pass that reads the cap (RFC 0052 §3.8);
/// until then the RFC's own defaults are the only values, and
/// `Wal::open` still rebuilds a file built at a smaller geometry.
fn configured_geometry() -> Result<reclaim::Geometry, reclaim::GeometryError> {
    reclaim::Geometry::new(
        reclaim::DEFAULT_MAX_TENANTS,
        reclaim::DEFAULT_MAX_UNLINKS_PER_PASS,
    )
}

/// What RFC 0052 §3.2's open-time matrix decided about this root.
struct RootWitness {
    store: Option<reclaim_store::ReclaimStore>,
    /// A version-2 `CHECKPOINT` beside a record — the only shape a
    /// pass may plan segments under.
    reclaimable: bool,
}

impl RootWitness {
    /// A pre-RFC root: no record is created, and the first checkpoint
    /// makes one on the upgrade path.
    fn legacy() -> Self {
        Self {
            store: None,
            reclaimable: false,
        }
    }

    /// A record whose root still owes the version-2 upgrade.
    fn with_record(store: reclaim_store::ReclaimStore) -> Self {
        Self {
            store: Some(store),
            reclaimable: false,
        }
    }

    fn witnessed(store: reclaim_store::ReclaimStore) -> Self {
        Self {
            store: Some(store),
            reclaimable: true,
        }
    }
}

/// RFC 0052 §3.2's open-time matrix over the `CHECKPOINT` version, the
/// `RECLAIM` record and the segment headers. Missing means "never
/// reclaimed" and damaged means "reclaimed, extent unknown", so the
/// two are never collapsed: only the first is safe to proceed from.
fn reconcile_root(
    config: &WalConfig,
    sidecar: Option<checkpoint::Sidecar>,
    segments: &[PathBuf],
) -> Result<RootWitness, OpenError> {
    let version = sidecar.map(|s| s.version);
    if reclaim_store::present(&config.root)? {
        return reconcile_record(config, version, segments);
    }
    match version {
        // Every unlink is gated on a checkpoint, and on a post-RFC
        // root the record is created at open, before the first
        // checkpoint can exist — so a version-2 sidecar beside no
        // record is a loss, not a pre-RFC layout.
        Some(checkpoint::SidecarVersion::Current) => Err(lost_record(&config.root)),
        Some(checkpoint::SidecarVersion::Legacy) => Ok(RootWitness::legacy()),
        None => open_without_sidecars(config, segments),
    }
}

/// The record is there: promote or retain its checkpoint witness by
/// the `CHECKPOINT` beside it, and reconcile its `planned` entries
/// against the directory before anything reads it as proof of loss.
fn reconcile_record(
    config: &WalConfig,
    version: Option<checkpoint::SidecarVersion>,
    segments: &[PathBuf],
) -> Result<RootWitness, OpenError> {
    let geometry = configured_geometry().map_err(|e| OpenError::InvalidConfig {
        field: "max_tenants",
        detail: e.to_string(),
    })?;
    let mut store =
        reclaim_store::ReclaimStore::open(&config.root, geometry, config.macos_full_fsync)?;
    match (version, store.record().witness.checkpoint) {
        (Some(checkpoint::SidecarVersion::Current), witness) => {
            promote_checkpoint_witness(&mut store, witness)?;
            reconcile_planned(&mut store, &config.root)?;
            Ok(RootWitness::witnessed(store))
        }
        // The migration's own retry state: armed, no version-2
        // checkpoint, and nothing reclaimed — housekeeping is gated
        // until the witness exists — so the next checkpoint retries
        // the upgrade against the arming already on disk.
        (Some(checkpoint::SidecarVersion::Legacy), _) => {
            reconcile_planned(&mut store, &config.root)?;
            Ok(RootWitness::with_record(store))
        }
        (None, reclaim::Witness::Terminal) => Err(lost_checkpoint(&config.root)),
        // Not fresh at all: the first rotation on a legacy root writes
        // the record before it creates its version-2 segment, so this
        // is that root mid migration. Emptying the record here would
        // discard a mode the first pass may already have adopted.
        (None, _) if !segments.is_empty() => {
            reconcile_planned(&mut store, &config.root)?;
            Ok(RootWitness::with_record(store))
        }
        (None, _) => {
            reset_record(&mut store)?;
            Ok(RootWitness::with_record(store))
        }
    }
}

/// An armed record beside a present version-2 `CHECKPOINT` is the
/// ordinary post-upgrade state — the crash simply landed before the
/// record's next write — so open promotes it durably before anything
/// reads the matrix, and it is never a fault.
fn promote_checkpoint_witness(
    store: &mut reclaim_store::ReclaimStore,
    witness: reclaim::Witness,
) -> Result<(), OpenError> {
    if witness == reclaim::Witness::Terminal {
        return Ok(());
    }
    let record = reclaim::ReclaimRecord {
        witness: reclaim::WitnessFlags {
            checkpoint: reclaim::Witness::Terminal,
            ..store.record().witness
        },
        ..store.record().clone()
    };
    store.commit(&record).map_err(OpenError::from)
}

/// A record beside a missing `CHECKPOINT` on a root with **no
/// segments** is a genuinely fresh root whose arming preceded a
/// checkpoint that never landed: it is opened empty and re-armed by
/// the next attempt.
fn reset_record(store: &mut reclaim_store::ReclaimStore) -> Result<(), OpenError> {
    if *store.record() == reclaim::ReclaimRecord::default() {
        return Ok(());
    }
    store
        .commit(&reclaim::ReclaimRecord::default())
        .map_err(OpenError::from)
}

/// §3.2's open-time reconciliation of the `planned` list. A planned
/// segment still present is retained — the ledger rebuilds it and a
/// later pass re-plans it — and its entry is dropped; an absent one is
/// treated exactly like a completed reclamation, since a planned
/// segment held only covered frames by construction. The reconciled
/// record is made durable before the first pass.
fn reconcile_planned(
    store: &mut reclaim_store::ReclaimStore,
    root: &std::path::Path,
) -> Result<(), OpenError> {
    let mode = match store.record().consumer_mode {
        reclaim::RecordedMode::Known => reclaim::EntryMode::Known,
        reclaim::RecordedMode::NoConsumer => reclaim::EntryMode::NoConsumer,
        // The codec refuses planned records beside an unrecorded mode,
        // so a decoded record with entries always has one.
        reclaim::RecordedMode::Unrecorded => return Ok(()),
    };
    let mut record = store.record().clone();
    let planned = std::mem::take(&mut record.planned);
    if planned.is_empty() {
        return Ok(());
    }
    for entry in &planned {
        let present = root
            .join(format!("{}.wal", entry.segment))
            .try_exists()
            .map_err(|source| OpenError::Io {
                op: "stat(planned segment)",
                source,
            })?;
        if !present {
            raise_reclaimed(&mut record, entry, mode)?;
        }
    }
    store.commit(&record).map_err(OpenError::from)
}

/// An absent planned segment is a reclamation that finished: each
/// tenant's last offset in it raises that tenant's `reclaimed_through`
/// exactly as the commit would have.
fn raise_reclaimed(
    record: &mut reclaim::ReclaimRecord,
    planned: &reclaim::PlannedUnlink,
    mode: reclaim::EntryMode,
) -> Result<(), OpenError> {
    for (&id, &offset) in &planned.last_offsets {
        record
            .dictionary
            .raise_reclaimed(id, reclaim::Entry { mode, offset })
            .map_err(|e| OpenError::Corrupt {
                detail: format!(
                    "RECLAIM sidecar: planned segment {} names {e}",
                    planned.segment
                ),
            })?;
    }
    Ok(())
}

/// Neither sidecar. RFC 0052 §3.2's witness is then the segment
/// header's format version, which every root with any segment
/// necessarily carries: a version-2 segment means the root has run
/// under this RFC and its missing sidecars are a loss, while
/// all-version-1 segments are the shape every live pre-RFC root has.
fn open_without_sidecars(
    config: &WalConfig,
    segments: &[PathBuf],
) -> Result<RootWitness, OpenError> {
    match post_rfc_segment(segments) {
        Some(path) => Err(lost_sidecars(&config.root, path)),
        None if segments.is_empty() => seed_fresh_record(config),
        None => Ok(RootWitness::legacy()),
    }
}

/// The first segment whose header names RFC 0052's version. A header
/// that cannot be read is not a witness either way — `replay` halts on
/// it as it always has, and deciding here would turn that halt into a
/// different one at `open`.
fn post_rfc_segment(segments: &[PathBuf]) -> Option<&PathBuf> {
    segments.iter().find(|path| {
        File::open(path)
            .ok()
            .and_then(|mut handle| segment::read_header(&mut handle).ok())
            .is_some_and(|header| header.version == segment::SEGMENT_VERSION)
    })
}

/// A fresh root: the empty record is written and its parent fsynced
/// **before** `create_fresh_segment`, so every directory that has ever
/// held a segment has held a record too and the fail-closed row can
/// never fire on a node's own first start.
fn seed_fresh_record(config: &WalConfig) -> Result<RootWitness, OpenError> {
    let geometry = configured_geometry().map_err(|e| OpenError::InvalidConfig {
        field: "max_tenants",
        detail: e.to_string(),
    })?;
    let store = reclaim_store::ReclaimStore::create(
        &config.root,
        geometry,
        &reclaim::ReclaimRecord::default(),
        config.macos_full_fsync,
    )?;
    Ok(RootWitness::with_record(store))
}

fn lost_record(root: &std::path::Path) -> OpenError {
    OpenError::Corrupt {
        detail: format!(
            "{} is missing beside a version-2 {} (RFC 0052 §3.2): every unlink is gated on a checkpoint and the record is created at open, so this root has run under RFC 0052 and its reclaim record is a loss, not an absence",
            root.join(reclaim::SIDECAR_NAME).display(),
            root.join(checkpoint::SIDECAR_NAME).display(),
        ),
    }
}

fn lost_checkpoint(root: &std::path::Path) -> OpenError {
    OpenError::Corrupt {
        detail: format!(
            "{} carries checkpoint_seen but {} is missing (RFC 0052 §3.2): a version-2 checkpoint succeeded on this root, so the sidecar is a loss — without it the Parquet-side suppression horizon cannot be rebuilt and replay would republish",
            root.join(reclaim::SIDECAR_NAME).display(),
            root.join(checkpoint::SIDECAR_NAME).display(),
        ),
    }
}

fn lost_sidecars(root: &std::path::Path, segment: &std::path::Path) -> OpenError {
    OpenError::Corrupt {
        detail: format!(
            "{} and {} are both missing beside the version-2 segment {} (RFC 0052 §3.2): the segment header is the witness that this root has run under RFC 0052, so reading it as a fresh root would replay frames whose Parquet rows may already exist",
            root.join(reclaim::SIDECAR_NAME).display(),
            root.join(checkpoint::SIDECAR_NAME).display(),
            segment.display(),
        ),
    }
}

/// Stale `<uuid>.wal.partial` files in the WAL root, sorted. The name
/// is **reserved and exact** (RFC 0052 §3.3): `*.tmp` is never
/// matched — it stays the checkpoint and snapshot namespace — and a
/// name whose stem does not parse as a UUID is ignored like any other
/// non-segment file.
fn list_partials(root: &std::path::Path) -> Result<Vec<PathBuf>, LedgerError> {
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

fn is_partial(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".wal.partial"))
        .is_some_and(|stem| stem.parse::<uuid::Uuid>().is_ok())
}

/// The [`FrameSink`] [`Wal::rebuild_ledger`] walks surviving segments
/// with: it sums validated frame bytes and stops the walk on the one
/// prefix RFC 0052 §3.2 makes fatal.
#[derive(Default)]
struct LedgerScan {
    bytes: u64,
    overlong_tenant: Option<(WalOffset, usize)>,
}

impl LedgerScan {
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

impl FrameSink for LedgerScan {
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
        detail: format!("segment header at {}: {e}", path.display()),
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
    let mut handle = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| OpenError::Io {
            op: "create(fresh segment)",
            source,
        })?;
    write_header(&mut handle, &SegmentHeader::new(uuid)).map_err(|source| OpenError::Io {
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
    Ok((handle, path, uuid))
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
    /// A prior rotation failed (the closing segment's
    /// data sync, the fresh segment's creation, or the
    /// parent-dir `fsync`) and the WAL is quiesced per §6.5 —
    /// operator intervention is required before further appends
    /// are accepted. The append that triggered the failed
    /// rotation surfaced the underlying [`AppendError::Io`];
    /// every append after it gets this variant.
    QuiescedAfterRotationFailure,
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
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len, limit } => {
                write!(f, "frame payload {len} B exceeds the {limit} B limit")
            }
            Self::Io { op, source } => write!(f, "WAL append failed at {op}: {source}"),
            Self::QuiescedAfterRotationFailure => {
                write!(f, "WAL is quiesced after a rotation failure (§6.5)")
            }
        }
    }
}

impl std::error::Error for AppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::TooLarge { .. } | Self::QuiescedAfterRotationFailure => None,
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "WAL sync failed at {op}: {source}"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Errors from [`Wal::checkpoint`].
#[derive(Debug)]
pub enum CheckpointError {
    /// Sidecar atomic-write / fsync failed. The in-memory
    /// high-water-mark is **not** advanced when this fires —
    /// the WAL conservatively keeps all segments rather than
    /// risk a post-crash replay-induced data-side dup.
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
}

impl std::fmt::Display for HousekeepingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "WAL housekeeping failed at {op}: {source}"),
        }
    }
}

impl std::error::Error for HousekeepingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Map a sidecar failure onto the checkpoint's own error surface.
/// Damaged bytes travel as an `InvalidData` source rather than a new
/// public variant, the way the segment-header read already does.
fn record_failed(e: reclaim_store::StoreError) -> CheckpointError {
    match e {
        reclaim_store::StoreError::Io { op, source } => CheckpointError::Io { op, source },
        reclaim_store::StoreError::Corrupt { detail } => CheckpointError::Io {
            op: "write(RECLAIM)",
            source: std::io::Error::new(ErrorKind::InvalidData, detail),
        },
    }
}

/// Errors from [`Wal::rebuild_ledger`].
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
        ];
        for cfg in boundaries {
            validate_config(&cfg).expect("boundary value");
        }
    }

    /// Every just-outside-bounds value is rejected and the
    /// error names the violated field. Iterated rather than
    /// one test per arm — the message format is what the
    /// operator sees on a real misconfiguration.
    #[test]
    fn validate_config_rejects_each_out_of_range_field() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let cases: &[(&str, WalConfig)] = &[
            (
                "batch_window_ms",
                WalConfig {
                    batch_window_ms: MAX_BATCH_WINDOW_MS + 1,
                    ..default_config(tmp.path())
                },
            ),
            (
                "segment_size_bytes",
                WalConfig {
                    segment_size_bytes: MIN_SEGMENT_SIZE_BYTES - 1,
                    ..default_config(tmp.path())
                },
            ),
            (
                "segment_size_bytes",
                WalConfig {
                    segment_size_bytes: MAX_SEGMENT_SIZE_BYTES + 1,
                    ..default_config(tmp.path())
                },
            ),
            (
                "segment_age_secs",
                WalConfig {
                    segment_age_secs: MIN_SEGMENT_AGE_SECS - 1,
                    ..default_config(tmp.path())
                },
            ),
            (
                "segment_age_secs",
                WalConfig {
                    segment_age_secs: MAX_SEGMENT_AGE_SECS + 1,
                    ..default_config(tmp.path())
                },
            ),
            (
                "housekeeping_secs",
                WalConfig {
                    housekeeping_secs: MIN_HOUSEKEEPING_SECS - 1,
                    ..default_config(tmp.path())
                },
            ),
            (
                "housekeeping_secs",
                WalConfig {
                    housekeeping_secs: MAX_HOUSEKEEPING_SECS + 1,
                    ..default_config(tmp.path())
                },
            ),
        ];
        for (expected_field, cfg) in cases {
            match validate_config(cfg).expect_err("out-of-range must reject") {
                OpenError::InvalidConfig { field, .. } => assert_eq!(
                    &field, expected_field,
                    "validation should name the violating field exactly",
                ),
                other => panic!("expected InvalidConfig({expected_field}), got {other:?}"),
            }
        }
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
