//! `ourios-wal` — RFC 0008 write-ahead log.
//!
//! **Status: red gate (PR-M2).** The public API surface is in
//! place but every method returns `unimplemented!()`. The
//! `#[ignore]`'d integration tests under
//! `crates/ourios-wal/tests/` enumerate the RFC 0008 §5
//! acceptance criteria (RFC0008.1 through .9) that the
//! implementation work has to satisfy before the RFC moves
//! `red → green`. See RFC 0008 for the design contract.
//!
//! The shape of the public API follows §6.1 verbatim — the
//! same `(WalOffset, FrameKind, FrameSink, Wal)` surface the
//! RFC pins. Implementation details (segment file layout,
//! frame format, fsync policy, recovery walk, checkpoint
//! sidecar) are spelled out in §§6.2–6.7 and land in
//! follow-up PRs together with the matching ignored-test
//! flips to `#[test]`.

use std::path::PathBuf;

use ourios_core::audit::AuditEvent;

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
/// range (`0x03..=0xFF`) is rejected on read as RFC0008.5
/// corruption — the format admits future kinds without a
/// version bump but only when they're added here.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `ExportLogsServiceRequest` protobuf bytes the receiver
    /// decoded, verbatim.
    OtlpBatch = 0x01,
    /// One serialised [`AuditEvent`]. Encoding choice is
    /// deferred to PR-M2's implementation per §9.
    AuditEvent = 0x02,
}

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
    /// §6.3 / §9. Ignored on other platforms.
    pub macos_full_fsync: bool,
}

/// Max bytes for a single frame's payload per RFC 0008 §6.2.2.
/// **Invariant**, not a tunable — a per-deployment limit
/// would let a single batch grow past a segment-recoverable
/// size and make file-format compatibility per-deployment.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The append-only write-ahead log itself. One per ingester
/// process; the §6.1 API is `open → append* → sync* →
/// checkpoint* + replay` (the last only on startup).
pub struct Wal {
    // Placeholder for the segment-management state per §6.2 +
    // the in-memory checkpoint high-water-mark per §6.7. The
    // real fields land with the implementation PRs.
    _config: WalConfig,
}

impl Wal {
    /// Open (or create) the WAL rooted at `config.root` and
    /// validate every §6.9 tunable against its classified
    /// range. Returns [`OpenError::InvalidConfig`] on the
    /// first field that falls outside its validated range.
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(_config: WalConfig) -> Result<Self, OpenError> {
        unimplemented!("RFC 0008 red gate — implementation pending (§6.1)");
    }

    /// Append a frame of `kind` carrying `payload` (≤
    /// [`MAX_FRAME_BYTES`]). The frame is **not** durable
    /// yet; the caller batches appends across the §6.3 window
    /// and calls [`Self::sync`] once per batch.
    ///
    /// # Errors
    ///
    /// See [`AppendError`].
    pub fn append(&mut self, _kind: FrameKind, _payload: &[u8]) -> Result<WalOffset, AppendError> {
        unimplemented!("RFC 0008 red gate — implementation pending (§6.1 / §6.2.2)");
    }

    /// Fsync the current segment (and the parent directory if
    /// a rotation just happened, per §6.3). Returns the
    /// highest offset that is now durable — the receiver
    /// gates its acks on this returning `Ok(_)`.
    ///
    /// # Errors
    ///
    /// See [`SyncError`].
    pub fn sync(&mut self) -> Result<WalOffset, SyncError> {
        unimplemented!("RFC 0008 red gate — implementation pending (§6.1 / §6.3)");
    }

    /// Record that records ≤ `durable_to` are on object
    /// storage; segments wholly below this offset may be
    /// reclaimed. Persists the offset to the `CHECKPOINT`
    /// sidecar per §6.7 (atomic write + fsync + parent-dir
    /// fsync) — durability of the checkpoint itself is what
    /// stops the post-restart at-least-once replay from
    /// duplicating already-published records.
    ///
    /// # Errors
    ///
    /// See [`CheckpointError`].
    pub fn checkpoint(&mut self, _durable_to: WalOffset) -> Result<(), CheckpointError> {
        unimplemented!("RFC 0008 red gate — implementation pending (§6.1 / §6.7)");
    }

    /// Walk every surviving segment in chronological order,
    /// handing each well-formed frame above the checkpoint
    /// (per §6.6 step 1) to `sink`. Used by the ingester at
    /// startup before opening network listeners.
    ///
    /// # Errors
    ///
    /// See [`RecoveryError`].
    pub fn replay<S: FrameSink>(&self, _sink: &mut S) -> Result<(), RecoveryError> {
        unimplemented!("RFC 0008 red gate — implementation pending (§6.1 / §6.6)");
    }

    /// Snapshot of the OTel-meter metrics per §6.8.
    #[must_use]
    pub fn metrics(&self) -> WalMetrics {
        unimplemented!("RFC 0008 red gate — implementation pending (§6.8)");
    }
}

/// Recovery-time consumer the [`Wal::replay`] scan hands
/// frames to. Implemented by the ingester's recovery driver:
/// `OtlpBatch` frames re-run through the decoder + tenant
/// fan-out + miner-ingest pipeline; `AuditEvent` frames
/// deserialise and reinject into the audit-event Parquet
/// writer's queue.
pub trait FrameSink {
    /// Consume one recovered frame.
    ///
    /// # Errors
    ///
    /// Any error the recovery driver surfaces (decoder
    /// failure, downstream pipeline rejection).
    fn consume(&mut self, kind: FrameKind, payload: &[u8]) -> Result<(), RecoveryError>;
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
    /// A prior rotation failed its fsync and the WAL is
    /// quiesced per §6.5 — operator intervention is required
    /// before further appends are accepted.
    QuiescedAfterRotationFsyncFailure,
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
