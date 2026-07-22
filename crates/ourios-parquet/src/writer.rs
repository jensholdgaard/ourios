//! Parquet data-file writer per RFC 0005 §3.4 / §3.5 / §3.6.
//!
//! Opens a file at the Hive-style partition path computed by
//! [`PartitionKey::data_path`], names it `<UUIDv7>.parquet` per
//! §3.4, writes batches via [`mined_records_to_batch_with_promoted`], and
//! rotates row groups when the in-progress buffer crosses the
//! §3.5 threshold (128 MiB uncompressed).
//!
//! Encoding policy per §3.6:
//! - ZSTD level 3 compression on every column.
//! - Dictionary encoding **on** globally, **off** explicitly
//!   per-column for every §3.6 row marked `Dictionary = no`:
//!   `body` (the [`CLAUDE.md`] §3.2 cardinality invariant —
//!   bodies are unbounded by design; dict on `body` is the
//!   failure mode), `attributes` (JSON `BYTE_ARRAY`, high
//!   entropy), `trace_id` / `span_id` (16- and 8-byte
//!   near-random opaque ids), `time_unix_nano` /
//!   `observed_time_unix_nano` (delta-encoded inside ZSTD;
//!   dict would interfere), `confidence` (float, narrow range),
//!   and both leaves of the `params` list element
//!   (`params.list.element.type_tag` and
//!   `params.list.element.value` — §3.6 "(list values)" covers
//!   the entire `LIST<STRUCT<...>>` element). The §3.6
//!   `lossy_flag` row says `Dictionary = n/a` (boolean RLE
//!   handles it natively), so no override is needed for that one.
//! - Per-page statistics **on** globally so the Parquet page
//!   index (`ColumnIndex` + `OffsetIndex`) is emitted for the
//!   `Page index = yes` columns; downgraded to
//!   `EnabledStatistics::Chunk` for the `Page index = no`
//!   columns (`tenant_id`, `attributes`, `resource_attributes`,
//!   `body`, both `params` list-element leaves,
//!   `separators.list.element`).
//! - Bloom filters on `template_id` (B2 predicate-pushdown),
//!   `trace_id` / `span_id` (RFC 0031 L3 exact-id lookup — random
//!   ids defeat min/max statistics), and every promoted column.
//!
//! [`CLAUDE.md`]: ../../../../CLAUDE.md

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use ourios_core::record::MinedRecord;
use parquet::arrow::{ArrowSchemaConverter, ArrowWriter};
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::metadata::SortingColumn;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;
use uuid::Uuid;

use crate::data_schema_with_promoted;
use crate::partition::PartitionKey;
use crate::promoted::PromotedAttributes;
use crate::record_batch::{BatchError, mined_records_to_batch_with_promoted};
use crate::store::Store;

/// RFC 0005 §3.5 — row-group rotation threshold for ingest-side files.
/// The writer rotates a row group when `ArrowWriter::in_progress_size`
/// crosses this. That value is parquet-rs's own estimate of the
/// buffered row-group bytes — dominated by already-encoded (compressed)
/// page data rather than raw uncompressed input — so on-disk row-group
/// size tracks it closely. Ingest-side files only; compacted output
/// rotates at the RFC 0036 §3.3 **adaptive** threshold
/// ([`adaptive_flush_bytes`]).
pub const ROW_GROUP_FLUSH_BYTES: usize = 128 * 1024 * 1024; // 128 MiB

/// RFC 0036 §3.3 — the target number of compacted row groups per
/// partition. The adaptive rotation threshold ([`adaptive_flush_bytes`])
/// divides a partition's estimated output size by this so a partition
/// of any size lands on roughly `K` groups: enough to cluster services
/// and give pruning granularity, not so many the footer/page-index
/// overhead dominates. §9.28/§9.29 proved a *fixed* threshold is inert on
/// the real v8 corpus — small hours (a few MiB) compact to one group and
/// prune nothing — while fragmenting large hours; scaling with size fixes
/// both ends.
pub const TARGET_COMPACTED_ROW_GROUPS: u64 = 8;

/// RFC 0036 §3.3 — the adaptive compacted-threshold **floor**: the
/// computed threshold (estimate / `K`) is clamped up to at least 1 MiB,
/// so compaction never rotates pathologically often on a small partition.
/// This bounds the *rotation threshold*, not the resulting group size — a
/// partition that never reaches the threshold still produces a single row
/// group, and the final remainder group can itself be smaller than 1 MiB.
/// It is the lever that makes small real-v8 hours prunable once they
/// exceed roughly `K ×` the floor (§9.29).
pub const MIN_COMPACTED_RG_BYTES: usize = 1024 * 1024; // 1 MiB

/// RFC 0036 §3.3 — the adaptive compacted-threshold **ceiling** (the old
/// fixed `COMPACTED_ROW_GROUP_FLUSH_BYTES`). A huge partition gets *more*
/// than `K` groups, each capped at this, so a compacted row group never
/// grows back toward the 128 MiB ingest band. Measured against the same
/// `ArrowWriter::in_progress_size` buffered estimate as
/// [`ROW_GROUP_FLUSH_BYTES`].
pub const MAX_COMPACTED_RG_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

/// RFC 0036 §3.3 — the adaptive compacted row-group rotation threshold
/// for a partition whose live input files total `estimated_output_bytes`
/// (the sorted output is re-compressed from the same rows, so the input
/// total is a safe upper estimate of the output size). Targets
/// [`TARGET_COMPACTED_ROW_GROUPS`] groups, clamped to
/// [`MIN_COMPACTED_RG_BYTES`]…[`MAX_COMPACTED_RG_BYTES`]. Deterministic in
/// its input, so the same partition compacts to byte-identical output
/// (RFC0036.4).
#[must_use]
pub fn adaptive_flush_bytes(estimated_output_bytes: u64) -> usize {
    let target = estimated_output_bytes / TARGET_COMPACTED_ROW_GROUPS;
    // Saturate on a target wider than usize (unreachable on 64-bit) so the
    // clamp still pins it to the ceiling rather than wrapping.
    usize::try_from(target)
        .unwrap_or(MAX_COMPACTED_RG_BYTES)
        .clamp(MIN_COMPACTED_RG_BYTES, MAX_COMPACTED_RG_BYTES)
}

/// RFC 0036 §7 interim tunable: the environment variable that overrides
/// the adaptive compacted-writer rotation threshold ([`adaptive_flush_bytes`]).
/// A byte count (e.g. `16777216` for 16 MiB). Read **once** per compacted
/// writer at construction; production leaves it unset and gets the
/// adaptive value. This is the operator
/// escape hatch (an RFC 0004 knob eventually) §7 records — never read on
/// the per-row hot path. The threshold sweep's *test* arm sets the
/// threshold with an explicit argument instead (`Writer::open_in_compacted`'s
/// `flush_override`), because setting a process env var races libc
/// `getenv` under `cargo test`'s in-process parallelism.
///
/// Precedence for the compacted rotation threshold: explicit
/// `flush_override` &gt; this env var &gt; the adaptive value.
pub const COMPACTED_RG_BYTES_ENV: &str = "OURIOS_COMPACTED_RG_BYTES";

/// The [`COMPACTED_RG_BYTES_ENV`] override as a positive byte count, or
/// `None` when unset/blank/unparsable/zero — in which case the adaptive
/// threshold applies. Read once at compacted-writer construction (never on
/// the per-row path).
fn env_compacted_flush_bytes() -> Option<usize> {
    parse_compacted_flush_bytes(std::env::var(COMPACTED_RG_BYTES_ENV).ok().as_deref())
}

/// Pure parse of the [`COMPACTED_RG_BYTES_ENV`] value, factored out so it
/// is unit-testable without touching the process environment (setting env
/// vars is unsound under parallel tests). `None`, empty, non-numeric, or
/// zero all yield `None` (the adaptive threshold then applies) — a
/// misconfigured knob must not silently produce a degenerate (0-byte
/// rotation) layout.
fn parse_compacted_flush_bytes(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// RFC 0005 §3.6 default ZSTD compression level for data files —
/// chosen for write throughput on the ingest hot path. [`Writer::open`]
/// uses it; [`Writer::open_with_zstd_level`] lets the bench sweep
/// other levels to measure the space/CPU tradeoff. Changing this
/// default is an RFC 0005 §3.6 decision.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Rows per internal sub-batch passed to `ArrowWriter::write`.
/// Chosen so that even with multi-KiB per-record payloads, a
/// single sub-batch's contribution after the [`ROW_GROUP_FLUSH_BYTES`]
/// threshold check stays well under RFC 0005 §3.5's hard 1 GiB
/// upper bound: 1024 rows × ≤ 768 KiB per record ≈ 768 MiB,
/// plus a 128 MiB pre-flushed buffer ≈ 896 MiB worst case, with
/// the 1 GiB ceiling still uncrossed. Public: the fixed sub-batching
/// is part of the RFC 0036 §3.5 determinism contract — the compaction
/// merge emits in exactly these chunks so both §3.2 paths drive the
/// Parquet writer with an identical call sequence.
pub const SUB_BATCH_ROWS: usize = 1024;

/// RFC 0036 §3.1 — the clustering key shape for compacted output:
/// the promoted `service.name` then `time_unix_nano`, or time alone
/// when `service.name` is not in the promoted set. Derived from the
/// same [`PromotedAttributes`] the compaction rewrite re-projects
/// under (§3.6), so the sort keys always read the *current* set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClusterKeys {
    /// §3.1 keys 1–2: promoted `service.name` (lexicographic UTF-8
    /// bytes, absent/null first), then `time_unix_nano` ascending.
    ServiceThenTime,
    /// §3.1 key 2 alone — the fallback for a promoted set without
    /// `service.name`. Unreachable through [`ClusterKeys::for_promoted`]
    /// today (RFC 0022 makes `service.name` implicit and
    /// non-removable), so it is exercised via the internal seam until
    /// a config can express such a set.
    TimeOnly,
}

impl ClusterKeys {
    pub(crate) fn for_promoted(promoted: &PromotedAttributes) -> Self {
        if promoted
            .resource_keys()
            .iter()
            .any(|k| k == crate::promoted::SERVICE_NAME_KEY)
        {
            Self::ServiceThenTime
        } else {
            Self::TimeOnly
        }
    }
}

/// Buffer-and-put Parquet writer for one partition's data file
/// (RFC 0013 object-storage seam).
///
/// One [`Writer`] writes one Parquet file. Rows are encoded into an
/// in-memory buffer as they arrive (row groups still flush per the
/// §3.5 sizing rule, into the buffer); [`Writer::close`] writes the
/// finished bytes to the object store under the partition's key.
/// The store [`put`](Store::put_blocking) is the **commit point**:
/// the local backend stages to a private temp object and renames it
/// into place (and S3's put is atomic), so an enumerating reader sees
/// either nothing or a logically complete `<uuid>.parquet` — never a
/// partial file. A writer dropped without [`Writer::close`] (panic,
/// early-return) simply discards its buffer; no object is ever
/// published. This satisfies RFC 0005 §7's "atomic-publish
/// convention" open-question item.
///
/// [`Writer::open`] opens a local-filesystem [`Store`] rooted at a
/// `bucket_root` path; [`Writer::open_in`] takes an already-built [`Store`]
/// so the same writer targets S3 (the compactor's RFC 0019 path).
///
/// **The atomic publish is logical, not crash-durable.** The store
/// put is not `fsync`-ed; a host crash between the put and the OS's
/// next page-cache flush could lose the file. Crash-survival
/// durability is the WAL's domain (`CLAUDE.md` §3.4
/// "WAL-before-ack"); see [`Writer::close`]'s rustdoc for the full
/// reasoning.
pub struct Writer {
    inner: Option<ArrowWriter<Vec<u8>>>,
    partition: PartitionKey,
    flush_uuid: Uuid,
    /// Object store rooted at `bucket_root`; the finished file is
    /// `put` to [`Self::key`] on close.
    store: Store,
    /// `/`-delimited object key the file is published to, relative to
    /// the store root (`data/tenant_id=…/year=…/…/<uuid>.parquet`). The
    /// backend-agnostic address (surfaced in [`WrittenFile`]).
    key: String,
    /// Absolute local landing path ([`Writer::open`]); the object key rendered
    /// as a path for [`Writer::open_in`] (which addresses by key regardless of
    /// the store's backend). Surfaced in [`WrittenFile::path`]; address a
    /// store-backed file by [`Self::key`], not this.
    final_path: PathBuf,
    /// Running count of rows written so far (incremented per
    /// sub-batch as each `write` succeeds); reported by
    /// [`Self::close`]. Tracked directly because `into_inner` returns
    /// the buffer, not file metadata.
    num_rows: i64,
    /// RFC 0022 promoted attribute set this writer projects; fixed at
    /// open time (the declared schema embeds its columns).
    promoted: PromotedAttributes,
    /// In-progress bytes at which a row group seals:
    /// [`ROW_GROUP_FLUSH_BYTES`] on the ingest side, the RFC 0036 §3.3
    /// adaptive threshold ([`adaptive_flush_bytes`]) for compacted output.
    /// Fixed at open time.
    flush_bytes: usize,
    /// Set to `true` once any `ArrowWriter::write` /
    /// `ArrowWriter::flush` call returns `Err`. The underlying
    /// `ArrowWriter`'s buffer state is undefined after such a
    /// failure (the row group may be partially written), so
    /// [`Self::close`] refuses to publish — putting a potentially
    /// corrupted buffer would land a bad data file. The buffer is
    /// discarded (there is no on-disk artifact to inspect). Mirrors
    /// [`crate::audit_writer::AuditWriter`]'s contract.
    poisoned: bool,
}

impl Writer {
    /// Open a writer for `partition` under `bucket_root` using the
    /// RFC 0005 §3.6 default compression level
    /// ([`DEFAULT_ZSTD_LEVEL`]). Creates the local partition directory; the
    /// `<UUIDv7>.parquet` object itself is buffer-and-put — rows accumulate in
    /// memory via [`Writer::append_records`] and nothing is published until
    /// [`Writer::close`].
    ///
    /// This is the **local-filesystem** constructor — it opens a
    /// [`Store::local`] rooted at `bucket_root`. [`Writer::open_in`]
    /// takes an already-built [`Store`] instead, so a writer can target
    /// S3 (the compactor's path under RFC 0019).
    ///
    /// # Errors
    ///
    /// See [`Writer::open_with_zstd_level`].
    pub fn open(bucket_root: &Path, partition: PartitionKey) -> Result<Self, WriterError> {
        Self::open_with_zstd_level(bucket_root, partition, DEFAULT_ZSTD_LEVEL)
    }

    /// Like [`Writer::open`] but with an explicit ZSTD compression
    /// level. The on-disk format is unaffected by the level
    /// (Parquet records the codec per column chunk and readers
    /// decode any level), so this is a physical-encoding knob, not
    /// an RFC 0005 §3.5 schema change. Used by `ourios-bench` to
    /// sweep the space/CPU tradeoff; production writes use
    /// [`Writer::open`]'s default until an RFC 0005 §3.6 amendment
    /// says otherwise.
    ///
    /// # Errors
    ///
    /// - [`WriterError::Io`] when the partition directory can't be
    ///   created or the object store can't be opened at `bucket_root`.
    /// - [`WriterError::Parquet`] when `zstd_level` is outside the
    ///   valid ZSTD range or `ArrowWriter` setup fails.
    pub fn open_with_zstd_level(
        bucket_root: &Path,
        partition: PartitionKey,
        zstd_level: i32,
    ) -> Result<Self, WriterError> {
        // Validate the codec level *before* any filesystem side effect, so an
        // invalid level leaves no partition directory behind (the delegate
        // re-validates — cheap and keeps it self-contained).
        ZstdLevel::try_new(zstd_level).map_err(WriterError::Parquet)?;
        // Ensure the store root (and the partition dir) exist:
        // `Store::local` canonicalises `bucket_root`, which must
        // therefore exist; the object-store `put` on close creates any
        // remaining parents.
        let dir = partition.data_path(bucket_root);
        std::fs::create_dir_all(&dir).map_err(|source| WriterError::Io {
            op: "create_dir_all",
            path: dir.clone(),
            source,
        })?;
        let store = Store::local(bucket_root).map_err(|e| WriterError::Io {
            op: "open store",
            path: bucket_root.to_path_buf(),
            source: io::Error::other(e),
        })?;
        let mut writer = Self::open_in_with_zstd_level(&store, partition, zstd_level)?;
        // Surface the absolute local landing path for the local backend
        // (readers/tests join the store root to find the file); the store
        // constructor leaves `final_path` as the object key rendered as a path.
        writer.final_path = dir.join(format!("{}.parquet", writer.flush_uuid));
        Ok(writer)
    }

    /// Open a writer for `partition` on an already-built [`Store`] (local or
    /// S3-compatible) using the RFC 0005 §3.6 default compression level — the
    /// S3-capable constructor (RFC 0019). The compactor and any non-local
    /// writer build a [`Store`] once and open writers through it. Nothing is
    /// created up front (object stores have no directories, and the local
    /// backend's `put` creates parents); the file is `put` to its object key on
    /// [`Writer::close`] (the buffer-and-put commit point).
    ///
    /// # Errors
    ///
    /// See [`Writer::open_in_with_zstd_level`].
    pub fn open_in(store: &Store, partition: PartitionKey) -> Result<Self, WriterError> {
        Self::open_in_with_zstd_level(store, partition, DEFAULT_ZSTD_LEVEL)
    }

    /// Like [`Writer::open_in`] but with an explicit ZSTD level (see
    /// [`Writer::open_with_zstd_level`] for the level's semantics — it is a
    /// physical-encoding knob, not a schema change).
    ///
    /// # Errors
    ///
    /// [`WriterError::Parquet`] when `zstd_level` is outside the valid ZSTD
    /// range or `ArrowWriter` setup fails.
    pub fn open_in_with_zstd_level(
        store: &Store,
        partition: PartitionKey,
        zstd_level: i32,
    ) -> Result<Self, WriterError> {
        Self::open_in_with_promoted(store, partition, zstd_level, PromotedAttributes::default())
    }

    /// Like [`Writer::open_in_with_zstd_level`] but with an explicit RFC 0022
    /// promoted attribute set. The declared schema embeds the promoted
    /// columns, so the set is fixed for the writer's lifetime; every other
    /// constructor uses [`PromotedAttributes::default`] (the implicit
    /// `service.name` only).
    ///
    /// # Errors
    ///
    /// See [`Writer::open_in_with_zstd_level`].
    pub fn open_in_with_promoted(
        store: &Store,
        partition: PartitionKey,
        zstd_level: i32,
        promoted: PromotedAttributes,
    ) -> Result<Self, WriterError> {
        Self::open_in_inner(store, partition, zstd_level, promoted, None, None, 0)
    }

    /// Open a **compacted-output** writer for `partition` (RFC 0036):
    /// row groups rotate at the §3.3 threshold and the file declares the
    /// §3.4 `sorting_columns` for `keys`. The rotation threshold resolves,
    /// in precedence: explicit `flush_override` (positive) — the RFC 0036
    /// §7 threshold sweep's deterministic seam — then the
    /// [`COMPACTED_RG_BYTES_ENV`] operator override, then the **adaptive**
    /// value [`adaptive_flush_bytes`] derives from `estimated_output_bytes`
    /// (the partition's summed live input file sizes; §3.3). All read once
    /// here. Compaction-only — ingest-side files keep the 128 MiB rotation
    /// and declare no sort (their rows are genuinely unsorted, RFC0035.5).
    /// The caller is responsible for appending rows in the declared §3.1
    /// order.
    ///
    /// # Errors
    ///
    /// See [`Writer::open_in_with_zstd_level`].
    pub(crate) fn open_in_compacted(
        store: &Store,
        partition: PartitionKey,
        zstd_level: i32,
        promoted: PromotedAttributes,
        keys: ClusterKeys,
        flush_override: Option<usize>,
        estimated_output_bytes: u64,
    ) -> Result<Self, WriterError> {
        Self::open_in_inner(
            store,
            partition,
            zstd_level,
            promoted,
            Some(keys),
            flush_override,
            estimated_output_bytes,
        )
    }

    /// Shared body of [`Writer::open_in_with_promoted`] (ingest layout,
    /// `compacted: None`) and [`Writer::open_in_compacted`]
    /// (`Some(keys)` — RFC 0036 §3.3 threshold + §3.4 declaration).
    /// `flush_override` and `estimated_output_bytes` apply to the compacted
    /// branch only (the ingest branch always rotates at
    /// [`ROW_GROUP_FLUSH_BYTES`]).
    fn open_in_inner(
        store: &Store,
        partition: PartitionKey,
        zstd_level: i32,
        promoted: PromotedAttributes,
        compacted: Option<ClusterKeys>,
        flush_override: Option<usize>,
        estimated_output_bytes: u64,
    ) -> Result<Self, WriterError> {
        // Validate the codec level up front so invalid input fails fast. The
        // validated level flows into `writer_properties` so it isn't re-checked.
        let zstd = ZstdLevel::try_new(zstd_level).map_err(WriterError::Parquet)?;
        let flush_uuid = Uuid::now_v7();
        // The object key is the partition's Hive path (relative to the store
        // root) plus the file name, with `/` separators — object keys are
        // `/`-delimited regardless of the host OS.
        let key = format!(
            "{}/{}.parquet",
            partition
                .data_path(Path::new(""))
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/"),
            flush_uuid
        );
        // No local root to join on the store path, so the object key rendered
        // as a path is the `final_path` here; the local constructor overrides
        // it with the absolute landing path. For S3 this is not a filesystem
        // path (readers address the file by `key`, surfaced in `WrittenFile`).
        let final_path = PathBuf::from(&key);

        let (sorting, flush_bytes) = match compacted {
            Some(keys) => (
                Some(sorting_columns(keys, &promoted)?),
                // Precedence (§3.3): explicit override > env override >
                // adaptive. The positivity filter guards a 0 override, which
                // would make `in_progress_size() >= flush_bytes` always true
                // (a degenerate per-sub-batch rotation); `env_compacted_flush_bytes`
                // guards the env path the same way. When neither is set the
                // adaptive value scales with the partition's estimated output.
                flush_override
                    .filter(|&n| n > 0)
                    .or_else(env_compacted_flush_bytes)
                    .unwrap_or_else(|| adaptive_flush_bytes(estimated_output_bytes)),
            ),
            None => (None, ROW_GROUP_FLUSH_BYTES),
        };
        let props = writer_properties(zstd, &promoted, sorting);
        // Buffer-and-put: encode into memory; nothing hits the store
        // until `close`. A construction failure leaves no artifact.
        let inner = ArrowWriter::try_new(
            Vec::new(),
            data_schema_with_promoted(&promoted),
            Some(props),
        )
        .map_err(WriterError::Parquet)?;

        Ok(Self {
            inner: Some(inner),
            partition,
            flush_uuid,
            store: store.clone(),
            key,
            final_path,
            num_rows: 0,
            promoted,
            flush_bytes,
            poisoned: false,
        })
    }

    /// Append `records` to the writer. Validates each record's
    /// partition matches the writer's open partition (§3.4 / §3.9
    /// row-vs-path agreement: a writer scoped to one partition
    /// MUST NOT write rows from another), converts the slice to
    /// a `RecordBatch`, and forwards to `ArrowWriter::write`.
    ///
    /// **Row-group sizing.** Internally chunks `records` into
    /// sub-batches of `SUB_BATCH_ROWS` (1024) rows and runs a
    /// flush-when-over-threshold check before each sub-batch
    /// write. RFC 0005 §3.5 pins the row-group target at 128 MiB
    /// – 1 GiB uncompressed; chunking + per-sub-batch flush
    /// keeps the maximum row-group size bounded to roughly
    /// `ROW_GROUP_FLUSH_BYTES + (per-record bytes × SUB_BATCH_ROWS)`,
    /// which stays comfortably under 1 GiB for the per-record
    /// sizes log ingest produces in practice.
    ///
    /// **Poisoning.** A failed `inner.write()` or `inner.flush()`
    /// leaves the underlying `ArrowWriter`'s buffer in an
    /// undefined state — the partial row group can't be safely
    /// recovered. When that happens, the writer marks itself
    /// poisoned and [`Self::close`] subsequently returns
    /// [`WriterError::Poisoned`] instead of publishing the buffer.
    /// `PartitionMismatch` and `Batch` errors do
    /// **not** poison: the writer remains usable for a follow-up
    /// `append_records` call.
    ///
    /// `append_records` is **not all-or-nothing** across the
    /// sub-batches it issues internally. The slice is chunked
    /// into `SUB_BATCH_ROWS`-sized pieces; if chunk *N* writes
    /// successfully and chunk *N+1*'s `mined_records_to_batch`
    /// then errors with `Batch`, the rows from chunks `0..N` have
    /// already landed in the in-progress row group. Callers that
    /// want atomicity must validate inputs (timestamps, body
    /// shapes, partition match) before the first `append_records`
    /// call. `PartitionMismatch`, by contrast, *is* pre-checked
    /// across the whole slice before any writes happen, so it
    /// fires before chunk 0.
    ///
    /// # Errors
    ///
    /// - [`WriterError::Poisoned`] when a prior `append_records`
    ///   already returned `Parquet`; fails fast without touching
    ///   `inner`.
    /// - [`WriterError::PartitionMismatch`] when a record's derived
    ///   partition (per §3.4's time-fallback algorithm) disagrees
    ///   with the writer's `partition`. Pre-checked across the
    ///   whole slice before any `inner.write`. **Non-poisoning**.
    /// - [`WriterError::Batch`] when `RecordBatch` construction
    ///   fails (timestamp overflow per RFC 0005 §3.2, or Arrow
    ///   internal error). **Non-poisoning**, but earlier chunks
    ///   in the same call may have written successfully — see
    ///   the atomicity note above.
    /// - [`WriterError::Parquet`] when the underlying Parquet
    ///   writer rejects the batch (codec or footer error).
    ///   **Poisons the writer**; subsequent `append_records` /
    ///   `close` calls return `Poisoned`.
    ///
    /// # Panics
    ///
    /// Structurally impossible. The inner `ArrowWriter` is
    /// `Some` from [`Writer::open`] until [`Writer::close`]
    /// takes ownership of `self`; `append_records` borrows
    /// `&mut self` and therefore cannot run after `close`.
    pub fn append_records(&mut self, records: &[MinedRecord]) -> Result<(), WriterError> {
        if self.poisoned {
            // Fail fast — touching `inner` after a prior Parquet
            // error would call into an `ArrowWriter` whose buffer
            // state is undefined. `close()` will refuse to publish
            // either way; surface the same `Poisoned` error here
            // so callers can stop driving the writer immediately
            // instead of accumulating further (potentially
            // doomed) Parquet operations.
            return Err(WriterError::Poisoned);
        }
        if records.is_empty() {
            return Ok(());
        }
        for (idx, r) in records.iter().enumerate() {
            let derived = PartitionKey::derive(r).map_err(|e| WriterError::Batch(e.into()))?;
            if derived != self.partition {
                return Err(WriterError::PartitionMismatch {
                    row_index: idx,
                    expected: self.partition.clone(),
                    actual: derived,
                });
            }
        }
        let inner = self
            .inner
            .as_mut()
            .expect("inner ArrowWriter is Some until Writer::close is called");
        // Run the Parquet-touching loop in a helper that takes a
        // `&mut ArrowWriter<Vec<u8>>` so the outer `self.poisoned =
        // true` assignment can run after the borrow on `self.inner`
        // ends. `num_rows` is a disjoint field, so it can be borrowed
        // alongside `inner`; the helper bumps it per successfully
        // written sub-batch. Poison only on Parquet errors — `Batch`
        // errors come from `mined_records_to_batch`, which runs on a
        // single chunk and doesn't touch `inner` itself; the buffer's
        // state at the moment a `Batch` error fires is whatever earlier
        // chunks left it (clean, or holding already-written rows from
        // this same call). Either way a follow-up `append_records` is
        // safe — the contract is "writer remains usable", not "no rows
        // persisted".
        let result = append_chunks(
            inner,
            records,
            &self.promoted,
            &mut self.num_rows,
            self.flush_bytes,
        );
        if matches!(result, Err(WriterError::Parquet(_))) {
            self.poisoned = true;
        }
        result
    }

    /// Close the writer, finalising the Parquet footer in the
    /// in-memory buffer and publishing the bytes to the object store
    /// under the partition's key. Must be called for the file to be
    /// published; dropping without `close` discards the buffer and
    /// publishes nothing.
    ///
    /// **Atomic publish is logical, not crash-durable.** Once
    /// this method returns, the published object has a complete
    /// Parquet footer and any subsequent reader can open it. The
    /// store `put` is not `fsync`-ed, though — a host crash between
    /// the put and the OS's next page-cache flush could lose the
    /// file. Crash-survival durability is the WAL's domain
    /// (`CLAUDE.md` §3.4 "WAL-before-ack"); the Parquet writer is the
    /// storage tier and assumes its records are recoverable via WAL
    /// replay after a crash.
    ///
    /// **Poisoning check.** If a prior `append_records` returned a
    /// [`WriterError::Parquet`] error, the writer is poisoned and this
    /// method refuses to publish — returns [`WriterError::Poisoned`]
    /// and discards the buffer (there is no on-disk artifact to leave
    /// behind, unlike the former temp-file scheme).
    ///
    /// # Errors
    ///
    /// - [`WriterError::Poisoned`] when a prior `append_records`
    ///   failed with a Parquet error.
    /// - [`WriterError::Parquet`] when the footer write fails.
    /// - [`WriterError::Io`] when the store `put` fails. Nothing is
    ///   published in that case (object-store puts are atomic).
    ///
    /// # Panics
    ///
    /// Structurally impossible. `inner` is populated by
    /// [`Writer::open`] and only consumed here; `close` takes `self`
    /// by value so it can't run twice.
    pub fn close(mut self) -> Result<WrittenFile, WriterError> {
        if self.poisoned {
            // Refuse to publish a possibly-partial buffer.
            return Err(WriterError::Poisoned);
        }
        let inner = self
            .inner
            .take()
            .expect("Writer::close consumes self; inner is Some on entry");
        // `into_inner` writes the footer and returns the finished
        // bytes; the `put` is the atomic commit point.
        let bytes = inner.into_inner().map_err(WriterError::Parquet)?;
        let bytes_written = bytes.len() as u64;
        self.store
            .put_blocking(&self.key, bytes)
            .map_err(|e| WriterError::Io {
                op: "put",
                path: self.final_path.clone(),
                source: io::Error::other(e),
            })?;
        Ok(WrittenFile {
            path: self.final_path.clone(),
            key: self.key.clone(),
            partition: self.partition.clone(),
            flush_uuid: self.flush_uuid,
            num_rows: self.num_rows,
            bytes_written,
        })
    }

    /// Inspector for the path reported through [`WrittenFile::path`]: the
    /// absolute landing path for [`Self::open`], or the object key rendered as a
    /// path for [`Self::open_in`] (no local root). Useful for tests that assert
    /// the local landing site; store-backed callers address the object by
    /// [`WrittenFile::key`]. The bytes only exist there after a successful
    /// `close` — while the writer is open they live in memory.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

// No `Drop`: a writer abandoned without `close` just drops its
// in-memory buffer — nothing was ever written to the store, so there
// is no temp artifact to clean up (unlike the former temp-file scheme).

/// Result of a successful [`Writer::close`].
#[derive(Debug)]
pub struct WrittenFile {
    /// Absolute landing path for the **local** backend; the object key
    /// rendered as a path for a store-backed writer ([`Writer::open_in`]).
    /// Address a store-backed file by [`Self::key`], not this.
    pub path: PathBuf,
    /// `/`-delimited object key the file was `put` to, relative to the store
    /// root (`data/tenant_id=…/year=…/…/<uuid>.parquet`) — the backend-agnostic
    /// address (the compactor reads/deletes by this).
    pub key: String,
    /// Partition key the file was opened under.
    pub partition: PartitionKey,
    /// `UUIDv7` flush identifier embedded in the filename.
    pub flush_uuid: Uuid,
    /// Total number of rows in the file (sum across row groups).
    pub num_rows: i64,
    /// Encoded Parquet byte length (the size `put` to the store) — the
    /// backend-agnostic replacement for stat-ing `path`, which a store-backed
    /// (S3) writer can't do.
    pub bytes_written: u64,
}

/// Errors produced by [`Writer`].
#[derive(Debug)]
pub enum WriterError {
    /// I/O failure preparing or publishing the file. Carries the
    /// operation name and the path so logs pinpoint which step failed.
    Io {
        /// Short operation name (e.g. `"create_dir_all"`,
        /// `"open store"`, `"put"`).
        op: &'static str,
        /// The path the operation was acting on (the partition
        /// directory, the store root, or the published object path).
        path: PathBuf,
        /// Underlying `io::Error` (an object-store error is wrapped
        /// via [`io::Error::other`] for the `put` step).
        source: io::Error,
    },
    /// Parquet writer failure (footer write, codec failure).
    Parquet(ParquetError),
    /// `RecordBatch` construction failed — see [`BatchError`].
    Batch(BatchError),
    /// A record in the batch belongs to a different partition than
    /// the one the writer was opened against. Surfaces the
    /// row-vs-path contract from RFC 0005 §3.9 at write time
    /// rather than letting the reader catch the mismatch later.
    PartitionMismatch {
        /// Zero-based index into the batch slice.
        row_index: usize,
        /// The partition the writer was opened against.
        expected: PartitionKey,
        /// The partition derived from the offending record.
        actual: PartitionKey,
    },
    /// A prior [`Writer::append_records`] returned a `Parquet`
    /// error, leaving the underlying writer's buffer in an
    /// undefined state. [`Writer::close`] refuses to publish to
    /// protect against landing a partial / corrupted data file;
    /// the buffer is discarded. Mirrors
    /// [`crate::audit_writer::AuditWriterError::Poisoned`].
    Poisoned,
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "writer I/O on `{op}` at {}: {source}", path.display())
            }
            Self::Parquet(e) => write!(f, "parquet writer: {e}"),
            Self::Batch(e) => write!(f, "record batch: {e}"),
            Self::PartitionMismatch {
                row_index,
                expected,
                actual,
            } => write!(
                f,
                "record at index {row_index} derives partition (tenant_id={}, \
                 year={:04}, month={:02}, day={:02}, hour={:02}) which does not match the \
                 writer's open partition (tenant_id={}, year={:04}, month={:02}, day={:02}, \
                 hour={:02}) — RFC 0005 §3.9 row-vs-path contract",
                actual.tenant_id,
                actual.year,
                actual.month,
                actual.day,
                actual.hour,
                expected.tenant_id,
                expected.year,
                expected.month,
                expected.day,
                expected.hour,
            ),
            Self::Poisoned => write!(
                f,
                "Writer is poisoned — a prior append_records failed with a Parquet error, \
                 leaving the buffer in an undefined state; close() refuses to publish to \
                 avoid landing a partial / corrupted file (the in-memory buffer is \
                 discarded; nothing is put to the store)",
            ),
        }
    }
}

impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parquet(e) => Some(e),
            Self::Batch(e) => Some(e),
            Self::PartitionMismatch { .. } | Self::Poisoned => None,
        }
    }
}

/// Inner Parquet-touching loop of [`Writer::append_records`].
/// Borrows the `ArrowWriter` directly so the caller can set
/// `self.poisoned = true` after the borrow ends if this returns
/// an `Err(WriterError::Parquet(_))`. Per the §3.5 row-group
/// sizing rule, runs a `flush()` when the in-progress buffer
/// crosses `flush_bytes` ([`ROW_GROUP_FLUSH_BYTES`] on the ingest
/// side, the RFC 0036 §3.3 adaptive threshold for compacted output).
/// Symmetric helper to the audit writer's `append_chunks`.
fn append_chunks(
    inner: &mut ArrowWriter<Vec<u8>>,
    records: &[MinedRecord],
    promoted: &PromotedAttributes,
    num_rows: &mut i64,
    flush_bytes: usize,
) -> Result<(), WriterError> {
    // Chunk into SUB_BATCH_ROWS-sized sub-batches and run a
    // flush-if-over-threshold check before every sub-batch.
    // The bound on row-group size is therefore:
    //   (§3.5 lower threshold) + (one sub-batch's worth) ≈
    //   well under §3.5's 1 GiB upper bound for any reasonable
    //   per-record size. The size check happens *before* every
    //   sub-batch (not after), so a sub-batch that pushes the
    //   buffer past the threshold seals the next time around —
    //   bounded overshoot is intentional; unbounded overshoot
    //   is what the RFC prohibits.
    for chunk in records.chunks(SUB_BATCH_ROWS) {
        if inner.in_progress_size() >= flush_bytes {
            inner.flush().map_err(WriterError::Parquet)?;
        }
        let batch =
            mined_records_to_batch_with_promoted(chunk, promoted).map_err(WriterError::Batch)?;
        inner.write(&batch).map_err(WriterError::Parquet)?;
        // Count rows only once the sub-batch has been accepted, so a
        // mid-slice `Batch`/`Parquet` failure leaves `num_rows`
        // reflecting exactly what landed in the buffer. `chunk.len()`
        // is bounded by `SUB_BATCH_ROWS` (1024), so the cast to `i64`
        // is lossless.
        #[allow(clippy::cast_possible_wrap)]
        let written = chunk.len() as i64;
        *num_rows += written;
    }
    // Final post-write check so the next `append_records` call
    // doesn't inherit an over-threshold buffer.
    if inner.in_progress_size() >= flush_bytes {
        inner.flush().map_err(WriterError::Parquet)?;
    }
    Ok(())
}

/// The RFC 0036 §3.4 `sorting_columns` declaration for `keys`: leaf
/// indices into the Parquet schema derived from the promoted set —
/// `resource.service.name` (ascending, nulls first, matching §3.1's
/// absent-first key) then `time_unix_nano` (ascending, non-nullable),
/// or the time key alone for [`ClusterKeys::TimeOnly`].
fn sorting_columns(
    keys: ClusterKeys,
    promoted: &PromotedAttributes,
) -> Result<Vec<SortingColumn>, WriterError> {
    let schema = data_schema_with_promoted(promoted);
    let descr = ArrowSchemaConverter::new()
        .convert(&schema)
        .map_err(WriterError::Parquet)?;
    let leaf_idx = |name: &str| -> Result<i32, WriterError> {
        descr
            .columns()
            .iter()
            .position(|c| c.path().string() == name)
            .and_then(|i| i32::try_from(i).ok())
            .ok_or_else(|| {
                WriterError::Parquet(ParquetError::General(format!(
                    "sorting column `{name}` has no leaf in the data schema"
                )))
            })
    };
    let time = SortingColumn {
        column_idx: leaf_idx(crate::columns::TIME_UNIX_NANO)?,
        descending: false,
        nulls_first: false,
    };
    match keys {
        ClusterKeys::ServiceThenTime => {
            let service = format!(
                "{}{}",
                crate::promoted::RESOURCE_PREFIX,
                crate::promoted::SERVICE_NAME_KEY
            );
            Ok(vec![
                SortingColumn {
                    column_idx: leaf_idx(&service)?,
                    descending: false,
                    nulls_first: true,
                },
                time,
            ])
        }
        ClusterKeys::TimeOnly => Ok(vec![time]),
    }
}

/// Encode `records` to a complete in-memory Parquet file in one shot — the
/// stateless counterpart to [`Writer`] (which buffers across `append_records`
/// calls then `put`s on close). Same schema, codec, and §3.6 encoding policy;
/// row-group sizing (§3.5) still applies within the buffer via `ArrowWriter`.
///
/// # Errors
/// [`WriterError::Parquet`] if `zstd_level` is out of range or encoding
/// fails; [`WriterError::Batch`] if a record can't be converted to an Arrow
/// batch.
pub fn encode_records_to_parquet(
    records: &[MinedRecord],
    zstd_level: i32,
) -> Result<Vec<u8>, WriterError> {
    encode_records_to_parquet_with_promoted(records, zstd_level, &PromotedAttributes::default())
}

/// Like [`encode_records_to_parquet`] but with an explicit RFC 0022 promoted
/// attribute set (the one-shot counterpart to [`Writer::open_in_with_promoted`]).
///
/// # Errors
/// See [`encode_records_to_parquet`].
pub fn encode_records_to_parquet_with_promoted(
    records: &[MinedRecord],
    zstd_level: i32,
    promoted: &PromotedAttributes,
) -> Result<Vec<u8>, WriterError> {
    let zstd = ZstdLevel::try_new(zstd_level).map_err(WriterError::Parquet)?;
    let props = writer_properties(zstd, promoted, None);
    let mut writer =
        ArrowWriter::try_new(Vec::new(), data_schema_with_promoted(promoted), Some(props))
            .map_err(WriterError::Parquet)?;
    for chunk in records.chunks(SUB_BATCH_ROWS) {
        // §3.5 row-group sizing: seal a row group once the in-progress
        // buffer crosses the threshold (same guard as `Writer::append_chunks`)
        // so large inputs don't produce one oversized row group.
        if writer.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
            writer.flush().map_err(WriterError::Parquet)?;
        }
        let batch =
            mined_records_to_batch_with_promoted(chunk, promoted).map_err(WriterError::Batch)?;
        writer.write(&batch).map_err(WriterError::Parquet)?;
    }
    // `into_inner` flushes the final row group, writes the footer, and
    // returns the buffer — the complete Parquet bytes.
    writer.into_inner().map_err(WriterError::Parquet)
}

/// Build the [`WriterProperties`] that encode RFC 0005 §3.5
/// (compression codec) and §3.6 (per-column encoding policy).
/// `zstd` is the already-validated compression level (the caller
/// validates up front so invalid input fails before any
/// filesystem work); production uses [`DEFAULT_ZSTD_LEVEL`], the
/// bench may sweep it. `sorting` is the RFC 0036 §3.4 declaration
/// for compacted output; ingest-side writers pass `None` (declaring
/// a sort their rows don't have would be a lie).
fn writer_properties(
    zstd: ZstdLevel,
    promoted: &PromotedAttributes,
    sorting: Option<Vec<SortingColumn>>,
) -> WriterProperties {
    let mut builder = WriterProperties::builder()
        .set_sorting_columns(sorting)
        .set_compression(Compression::ZSTD(zstd))
        // Dictionary on globally by default (most columns benefit
        // per §3.6); we opt out per-column below for the high-
        // entropy ones.
        .set_dictionary_enabled(true)
        // Per-page statistics enabled. In parquet-rs `≥ 53`,
        // setting `EnabledStatistics::Page` causes the writer to
        // emit the Parquet "page index" (`ColumnIndex` +
        // `OffsetIndex`) alongside the per-page min/max stats in
        // each `DataPage` header. This satisfies §3.6's "page
        // index = yes" entries (for `template_id`, `time_unix_nano`,
        // `severity_*`, `scope_*`, `trace_id`, `span_id`, etc.) —
        // a writer-side metadata check (`column_index_offset` is
        // `Some(_)`) pins the contract.
        .set_statistics_enabled(EnabledStatistics::Page);

    // §3.6 `Dictionary = no` overrides. The RFC's §3.6 table
    // names every column that opts out; this loop is the
    // exhaustive set. `body` carries `CLAUDE.md` §3.2's
    // cardinality invariant load-bearing rationale (bodies are
    // unbounded by design); the others are either high-entropy
    // (`attributes`, `trace_id`, `span_id`) or non-text numeric
    // columns where dict-encoding adds overhead without payoff
    // (`time_unix_nano`, `observed_time_unix_nano`,
    // `effective_time_unix_nano`, `confidence`).
    for no_dict_col in [
        crate::columns::TIME_UNIX_NANO,
        crate::columns::OBSERVED_TIME_UNIX_NANO,
        crate::columns::EFFECTIVE_TIME_UNIX_NANO,
        crate::columns::ATTRIBUTES,
        crate::columns::TRACE_ID,
        crate::columns::SPAN_ID,
        crate::columns::BODY,
        crate::columns::CONFIDENCE,
    ] {
        builder = builder
            .set_column_dictionary_enabled(ColumnPath::new(vec![no_dict_col.to_string()]), false);
    }

    // §3.6 also marks the `params` "(list values)" row as
    // `Dictionary = no` / `Page index = no`. "List values" here
    // covers every leaf of the LIST<STRUCT<...>> element — both
    // the `type_tag` and `value` leaves — per a literal reading
    // of the RFC table. Parquet's 3-level LIST encoding exposes
    // the leaves at the dotted paths
    // `params.list.element.type_tag` (INT32) and
    // `params.list.element.value` (BINARY). These overrides
    // disable dict + page index on both leaves; the
    // `tests/no_body_dict.rs` metadata walks pin both.
    let params_type_tag_leaf = ColumnPath::new(vec![
        crate::columns::PARAMS.to_string(),
        "list".to_string(),
        "element".to_string(),
        "type_tag".to_string(),
    ]);
    let params_value_leaf = ColumnPath::new(vec![
        crate::columns::PARAMS.to_string(),
        "list".to_string(),
        "element".to_string(),
        "value".to_string(),
    ]);
    builder = builder.set_column_dictionary_enabled(params_type_tag_leaf.clone(), false);
    builder = builder.set_column_dictionary_enabled(params_value_leaf.clone(), false);

    // §3.6 `Page index = no` overrides. The global
    // `EnabledStatistics::Page` writes per-page stats AND the
    // Parquet `ColumnIndex` / `OffsetIndex`; downgrading these
    // columns to `EnabledStatistics::Chunk` keeps the chunk-
    // level min/max (still useful for row-group pruning) but
    // suppresses the per-page surface. The columns named here
    // are the §3.6 table's `Page index = no` rows: `tenant_id`
    // (one value per file — page index is moot), `attributes` /
    // `resource_attributes` / `body` (high-entropy JSON / opaque
    // bytes), and the `params` / `separators` list-value leaves
    // ("Per-row entropy too high" / "Almost always a single
    // space").
    for no_page_idx_col in [
        crate::columns::TENANT_ID,
        crate::columns::ATTRIBUTES,
        crate::columns::RESOURCE_ATTRIBUTES,
        crate::columns::BODY,
    ] {
        builder = builder.set_column_statistics_enabled(
            ColumnPath::new(vec![no_page_idx_col.to_string()]),
            EnabledStatistics::Chunk,
        );
    }
    // The `params` and `separators` list-value leaves at the
    // 3-level LIST encoding path. Both `params.list.element.type_tag`
    // and `params.list.element.value` are covered per the §3.6
    // "(list values)" literal reading.
    builder = builder.set_column_statistics_enabled(params_type_tag_leaf, EnabledStatistics::Chunk);
    builder = builder.set_column_statistics_enabled(params_value_leaf, EnabledStatistics::Chunk);
    builder = builder.set_column_statistics_enabled(
        ColumnPath::new(vec![
            crate::columns::SEPARATORS.to_string(),
            "list".to_string(),
            "element".to_string(),
        ]),
        EnabledStatistics::Chunk,
    );

    // §3.6: bloom filter on `template_id` (B2 predicate-pushdown).
    let template_id = ColumnPath::new(vec![crate::columns::TEMPLATE_ID.to_string()]);
    builder = builder.set_column_bloom_filter_enabled(template_id, true);

    // Bloom filters on the trace-context ids: random 16/8-byte values
    // defeat min/max statistics entirely, so an exact-id lookup (the
    // RFC 0031 L3 class) degenerates to a whole-column scan without
    // them — measured at 72.4 MB for a 9-row trace on the 4.9M-record
    // otel-demo-v8 corpus (comparative run #12). Same §3.6 pattern as
    // `template_id` and the promoted columns.
    for column in [crate::columns::TRACE_ID, crate::columns::SPAN_ID] {
        let path = ColumnPath::new(vec![column.to_string()]);
        builder = builder.set_column_bloom_filter_enabled(path, true);
    }

    // RFC 0022 §3.1: promoted attribute columns are the attribute
    // predicate-pushdown surface — bloom filter each (dictionary and
    // page-level statistics are already the global defaults). A
    // promoted column name is a single schema leaf whose name contains
    // literal dots, so the ColumnPath is one part, not a nested path.
    for name in promoted.column_names() {
        let path = ColumnPath::new(vec![name]);
        builder = builder.set_column_bloom_filter_enabled(path, true);
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;

    use super::*;
    use crate::Reader;

    const TS0: u64 = 1_775_127_480_000_000_000;

    fn rec(template_id: u64, ts_ns: u64) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("a"),
            template_id,
            template_version: 1,
            severity_number: 9,
            severity_text: Some("INFO".to_string()),
            scope_name: Some("lib.cart".to_string()),
            scope_version: Some("1.0.0".to_string()),
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0x01,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            }],
            separators: vec![String::new(), " ".to_string()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    /// Encode → decode through the in-memory buffer path round-trips every
    /// row byte-for-byte (the RFC 0013 buffer-and-put contract at the API
    /// boundary, independent of the integration test's `Store` hop).
    #[test]
    fn encode_round_trips_through_open_bytes() {
        let records: Vec<MinedRecord> = (0..300).map(|i| rec(i % 7, TS0 + i * 1_000)).collect();
        let encoded = encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode");
        let decoded = Reader::open_bytes(bytes::Bytes::from(encoded))
            .expect("open_bytes")
            .read_all()
            .expect("read_all");
        assert_eq!(decoded, records, "every row recovered byte-for-byte");
    }

    /// `Writer::open_in` writes through an already-built `Store` (the RFC 0019
    /// S3-capable path) with no pre-created partition dir: the file lands at the
    /// partition's object key, `WrittenFile` carries that key + the encoded byte
    /// length (the backend-agnostic replacements for stat-ing a local path), and
    /// `Reader::open_partition_bytes` over `store.get` recovers every row.
    #[test]
    fn open_in_writes_through_a_store_and_reports_key_and_size() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let records: Vec<MinedRecord> = (0..50).map(|i| rec(i % 3, TS0 + i * 1_000)).collect();
        let partition = PartitionKey::derive(&records[0]).expect("derive");

        let mut writer = Writer::open_in(&store, partition.clone()).expect("open_in");
        writer.append_records(&records).expect("append");
        let written = writer.close().expect("close");

        // The key is the partition's Hive path + the uuid file name.
        let partition_prefix = format!(
            "{}/",
            partition
                .data_path(Path::new(""))
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/"),
        );
        assert!(
            written.key.starts_with(&partition_prefix),
            "key under the partition prefix: {}",
            written.key,
        );
        assert!(written.key.ends_with(".parquet"));
        assert!(written.bytes_written > 0, "encoded byte length recorded");

        // The object is addressable by `key`; the put size matches.
        let bytes = store.get_blocking(&written.key).expect("get");
        assert_eq!(
            bytes.len() as u64,
            written.bytes_written,
            "WrittenFile.bytes_written equals the put size",
        );
        let decoded =
            Reader::open_partition_bytes(bytes::Bytes::from(bytes), partition, &written.key)
                .expect("open_partition_bytes")
                .read_all()
                .expect("read_all");
        assert_eq!(
            decoded, records,
            "every row recovered through the store seam"
        );
    }

    /// `Reader::open_partition_bytes` applies the §3.9 row-vs-path validation
    /// (RFC0009.5): reading store bytes under the wrong partition is a hard
    /// `PartitionMismatch`, so a mis-partitioned compaction input aborts rather
    /// than being silently merged.
    #[test]
    fn open_partition_bytes_rejects_a_mismatched_partition() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let records = vec![rec(1, TS0)]; // tenant "a"
        let partition = PartitionKey::derive(&records[0]).expect("derive");
        let mut writer = Writer::open_in(&store, partition).expect("open_in");
        writer.append_records(&records).expect("append");
        let written = writer.close().expect("close");
        let bytes = store.get_blocking(&written.key).expect("get");

        // A partition for a different tenant (same time bucket, wrong tenant).
        let mut other = rec(1, TS0);
        other.tenant_id = TenantId::new("b");
        let wrong = PartitionKey::derive(&other).expect("derive other");

        let err = Reader::open_partition_bytes(bytes::Bytes::from(bytes), wrong, &written.key)
            .expect("open_partition_bytes")
            .read_all()
            .expect_err("a mismatched tenant must be a hard read error");
        assert!(
            matches!(err, crate::ReaderError::PartitionMismatch { .. }),
            "got {err:?}",
        );
    }

    /// An empty input is still a valid, complete Parquet file (footer +
    /// schema, zero rows) — the writer must not require at least one batch.
    #[test]
    fn encode_empty_yields_readable_zero_rows() {
        let encoded = encode_records_to_parquet(&[], DEFAULT_ZSTD_LEVEL).expect("encode empty");
        let decoded = Reader::open_bytes(bytes::Bytes::from(encoded))
            .expect("open_bytes")
            .read_all()
            .expect("read_all");
        assert!(decoded.is_empty(), "no rows out for no rows in");
    }

    /// The RFC 0036 §7 compacted-threshold env override parses a positive
    /// byte count and yields `None` (so the adaptive threshold then
    /// applies) on every degenerate input — a misconfigured knob must
    /// never yield a 0-byte (every-sub-batch) rotation. Exercised on the
    /// pure parse helper so no process env var is touched (unsound under
    /// parallel tests).
    #[test]
    fn compacted_flush_env_parses_positive_else_none() {
        assert_eq!(
            parse_compacted_flush_bytes(Some("16777216")),
            Some(16 * 1024 * 1024)
        );
        assert_eq!(
            parse_compacted_flush_bytes(Some("  67108864 ")),
            Some(64 * 1024 * 1024)
        );
        for degenerate in [None, Some(""), Some("nope"), Some("0"), Some("-5")] {
            assert_eq!(
                parse_compacted_flush_bytes(degenerate),
                None,
                "degenerate override {degenerate:?} must fall back to the adaptive threshold",
            );
        }
    }

    /// RFC 0036 §3.3 — the adaptive compacted row-group threshold targets
    /// [`TARGET_COMPACTED_ROW_GROUPS`] groups, clamped to the
    /// floor/ceiling. The three worked examples from the RFC: a mid-size
    /// hour lands between the bounds, a huge hour caps at the ceiling, a
    /// tiny hour floors at the minimum.
    #[test]
    fn adaptive_flush_bytes_targets_k_groups_within_bounds() {
        // 14 MB hour → 14/8 = 1.75 MB → between the 1 MiB floor and 32 MiB
        // ceiling (≈ 8 groups).
        assert_eq!(adaptive_flush_bytes(14_000_000), 14_000_000 / 8);
        // 400 MB hour → 400/8 = 50 MB → capped to the 32 MiB ceiling
        // (so > K groups, each ≤ 32 MiB).
        assert_eq!(adaptive_flush_bytes(400_000_000), MAX_COMPACTED_RG_BYTES);
        // 4 MB hour → 4/8 = 0.5 MB → floored to the 1 MiB minimum (so it
        // still splits into a few groups rather than one).
        assert_eq!(adaptive_flush_bytes(4_000_000), MIN_COMPACTED_RG_BYTES);
        // Degenerate: a zero estimate still floors, never rotating at 0.
        assert_eq!(adaptive_flush_bytes(0), MIN_COMPACTED_RG_BYTES);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// RFC 0036 §3.3 — `adaptive_flush_bytes` invariants over the whole
        /// `u64` estimate domain: the result is always inside the
        /// [`MIN_COMPACTED_RG_BYTES`, `MAX_COMPACTED_RG_BYTES`] clamp, and it
        /// is monotonic non-decreasing in the estimate (a larger partition
        /// never gets a *smaller* row-group threshold). Together these pin
        /// the "target-K, clamped" contract the worked-example test samples.
        #[test]
        fn adaptive_flush_bytes_is_clamped_and_monotonic(
            a in proptest::prelude::any::<u64>(),
            b in proptest::prelude::any::<u64>(),
        ) {
            for est in [a, b] {
                let t = adaptive_flush_bytes(est);
                proptest::prop_assert!(
                    (MIN_COMPACTED_RG_BYTES..=MAX_COMPACTED_RG_BYTES).contains(&t),
                    "adaptive_flush_bytes({est}) = {t} outside [{MIN_COMPACTED_RG_BYTES}, \
                     {MAX_COMPACTED_RG_BYTES}]",
                );
            }
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            proptest::prop_assert!(
                adaptive_flush_bytes(lo) <= adaptive_flush_bytes(hi),
                "non-monotonic: f({lo}) > f({hi})",
            );
        }
    }

    /// An out-of-range zstd level fails up front (mirrors the file writer's
    /// validate-before-work contract) rather than producing a bad file.
    #[test]
    fn encode_rejects_invalid_zstd_level() {
        let err = encode_records_to_parquet(&[rec(1, TS0)], 99).expect_err("level 99 invalid");
        assert!(matches!(err, WriterError::Parquet(_)), "got {err:?}");
    }

    // The §3.5 128 MiB row-group flush guard inside `encode_records_to_parquet`
    // is the same predicate as `Writer::append_chunks`; the file-path sizing
    // assertion (RFC0005.6, `#[ignore]`d — needs a multi-hundred-MiB corpus)
    // covers that threshold. These colocated tests lock the round-trip,
    // empty-input, and validation invariants that are cheap to exercise.

    fn prop_rec(
        template_id: u64,
        ts_ns: u64,
        severity_number: u8,
        param_value: &str,
    ) -> MinedRecord {
        MinedRecord {
            template_id,
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            severity_number,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: param_value.to_string(),
            }],
            ..rec(template_id, ts_ns)
        }
    }

    fn row_key(r: &MinedRecord) -> (u64, u64, u8, &str) {
        (
            r.template_id,
            r.time_unix_nano,
            r.severity_number,
            r.params[0].value.as_str(),
        )
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// Round-trip invariant: for any record set, encoding to in-memory
        /// Parquet then decoding recovers exactly the same multiset of rows
        /// (count + content). Only the fields below vary; the rest stay at
        /// the clean-round-trip shape so equality reflects the codec, not
        /// fixture edge cases.
        #[test]
        fn encode_round_trip_preserves_rows(
            rows in proptest::collection::vec(
                (
                    proptest::prelude::any::<u64>(),
                    0u64..3_600_000_000_000u64,
                    proptest::prelude::any::<u8>(),
                    "[0-9]{1,12}",
                ),
                0..=64usize,
            )
        ) {
            let mut expected: Vec<MinedRecord> = rows
                .iter()
                .map(|(tid, off, sev, val)| prop_rec(*tid, TS0 + off, *sev, val))
                .collect();
            let encoded = encode_records_to_parquet(&expected, DEFAULT_ZSTD_LEVEL)
                .expect("encode");
            let mut got = Reader::open_bytes(bytes::Bytes::from(encoded))
                .expect("open_bytes")
                .read_all()
                .expect("read_all");
            proptest::prop_assert_eq!(got.len(), expected.len(), "row count preserved");
            got.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
            expected.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
            proptest::prop_assert_eq!(got, expected, "every row preserved (value-equal)");
        }
    }
}
