//! `ourios-semconv` — generated OpenTelemetry semantic-convention name
//! constants for Ourios's custom metrics and attributes.
//!
//! GENERATED from `semconv/registry/` by `weaver registry generate`.
//! Do not edit by hand: change the registry or the template at
//! `templates/registry/rust/`, then regenerate (the exact command CI
//! runs — note `--future`, matching `weaver registry check --future`)
//! and commit the result:
//!
//! ```text
//! weaver registry generate rust crates/ourios-semconv/src \
//!     -t templates -r semconv/registry --future
//! cargo fmt -p ourios-semconv
//! ```
//!
//! The CI `semconv` job fails if this file drifts from the registry.

#![deny(unsafe_code)]

// Metric names (RFC 0009 §3.6).

/// `ourios.compaction.backlog` (updowncounter, unit `{partition}`).
pub const OURIOS_COMPACTION_BACKLOG: &str = "ourios.compaction.backlog";

/// `ourios.compaction.duration` (histogram, unit `s`).
pub const OURIOS_COMPACTION_DURATION: &str = "ourios.compaction.duration";

/// `ourios.compaction.files` (counter, unit `{file}`).
pub const OURIOS_COMPACTION_FILES: &str = "ourios.compaction.files";

/// `ourios.compaction.io` (counter, unit `By`).
pub const OURIOS_COMPACTION_IO: &str = "ourios.compaction.io";

/// `ourios.compaction.orphan.files` (counter, unit `{file}`).
pub const OURIOS_COMPACTION_ORPHAN_FILES: &str = "ourios.compaction.orphan.files";

/// `ourios.compaction.partitions` (counter, unit `{partition}`).
pub const OURIOS_COMPACTION_PARTITIONS: &str = "ourios.compaction.partitions";

/// `ourios.compaction.rows` (counter, unit `{row}`).
pub const OURIOS_COMPACTION_ROWS: &str = "ourios.compaction.rows";

/// `ourios.compaction.sweeps` (counter, unit `{sweep}`).
pub const OURIOS_COMPACTION_SWEEPS: &str = "ourios.compaction.sweeps";

/// `ourios.storage.parquet.file.size` (histogram, unit `By`).
pub const OURIOS_STORAGE_PARQUET_FILE_SIZE: &str = "ourios.storage.parquet.file.size";

// Attribute keys.

/// `ourios.compaction.result` attribute key.
pub const OURIOS_COMPACTION_RESULT: &str = "ourios.compaction.result";

/// `ourios.io.direction` attribute key.
pub const OURIOS_IO_DIRECTION: &str = "ourios.io.direction";

/// `ourios.tenant` attribute key.
pub const OURIOS_TENANT: &str = "ourios.tenant";
