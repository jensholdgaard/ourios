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

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use ourios_core::audit::AuditEvent;

pub(crate) mod frame;
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
    /// deferred to the encoder implementation PR per RFC
    /// 0008 §9 (this PR is the red-gate scaffold — the
    /// encoder lands together with the matching `#[ignore]`
    /// removal in the next PR).
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
    /// Held for `append` / `sync` / `checkpoint` to read in
    /// future slices. `#[allow(dead_code)]` matches the other
    /// red-gate fields below.
    #[allow(dead_code)]
    config: WalConfig,
    /// File handle for the segment currently accepting appends
    /// (per §6.2: append-only, opened by exactly one writer).
    /// Opened with `O_APPEND` so each write atomically lands at
    /// end-of-file regardless of the user-space cursor.
    current_segment: File,
    /// Path of the file the `current_segment` handle points
    /// at. Kept alongside the handle for diagnostic messages
    /// and the post-rotation parent-dir `fsync` (§6.3) that
    /// lands with `sync` in the next slice.
    #[allow(dead_code)]
    current_segment_path: PathBuf,
    /// `UUIDv7` of the current segment — same value as the
    /// filename's stem and the segment's in-file header per
    /// §6.2.1. Carried in every `WalOffset` `append` returns.
    current_segment_uuid: uuid::Uuid,
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
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(config: WalConfig) -> Result<Self, OpenError> {
        validate_config(&config)?;
        std::fs::create_dir_all(&config.root).map_err(|source| OpenError::Io {
            op: "create_dir_all(wal_root)",
            source,
        })?;
        let existing_segments = list_segments(&config.root)?;
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
        })
    }

    /// Append a frame of `kind` carrying `payload` (≤
    /// [`MAX_FRAME_BYTES`]). The frame is **not** durable
    /// yet; the caller batches appends across the §6.3 window
    /// and calls [`Self::sync`] once per batch. Returns a
    /// [`WalOffset`] pointing at the **start** of the new
    /// frame (the byte the 12 B header begins at), so the
    /// offset uniquely identifies the appended record even
    /// after later appends extend the segment.
    ///
    /// # Errors
    ///
    /// See [`AppendError`].
    pub fn append(&mut self, kind: FrameKind, payload: &[u8]) -> Result<WalOffset, AppendError> {
        if payload.len() > MAX_FRAME_BYTES {
            return Err(AppendError::TooLarge {
                len: payload.len(),
                limit: MAX_FRAME_BYTES,
            });
        }
        // Record the byte offset where the new frame *starts*
        // — i.e. the segment's pre-write length. We use
        // `metadata().len()` rather than `stream_position`:
        // `O_APPEND` guarantees each write lands at end-of-file
        // atomically but the user-space cursor isn't
        // guaranteed synchronised with the kernel's write
        // offset on every platform (Linux `fcntl(O_APPEND)`
        // notes), so `stream_position` can be 0 or stale on a
        // file we haven't seeked into. File metadata length is
        // truth.
        let byte = self
            .current_segment
            .metadata()
            .map_err(|source| AppendError::Io {
                op: "stat(current_segment)",
                source,
            })?
            .len();
        frame::write_frame(&mut self.current_segment, kind, payload).map_err(|source| {
            AppendError::Io {
                op: "write_frame(current_segment)",
                source,
            }
        })?;
        Ok(WalOffset {
            segment: self.current_segment_uuid,
            byte,
        })
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

#[cfg(test)]
mod tests {
    //! Colocated unit tests for the `Wal::open` helpers per
    //! CLAUDE.md §6.2 (unit tests next to the code for
    //! anything non-trivial). End-to-end coverage of `open`
    //! lives in `tests/open.rs`; this module pins the smaller
    //! helper-level contracts so a regression caught at this
    //! layer surfaces here rather than as a cascading failure
    //! in the integration suite.
    use super::*;

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
