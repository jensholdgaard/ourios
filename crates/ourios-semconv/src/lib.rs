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

/// `ourios.miner.alias.assertions` (counter, unit `{assertion}`).
pub const OURIOS_MINER_ALIAS_ASSERTIONS: &str = "ourios.miner.alias.assertions";

/// `ourios.miner.alias.retractions` (counter, unit `{retraction}`).
pub const OURIOS_MINER_ALIAS_RETRACTIONS: &str = "ourios.miner.alias.retractions";

/// `ourios.miner.body_retention.utilization` (gauge, unit `1`).
pub const OURIOS_MINER_BODY_RETENTION_UTILIZATION: &str = "ourios.miner.body_retention.utilization";

/// `ourios.miner.confidence` (histogram, unit `1`).
pub const OURIOS_MINER_CONFIDENCE: &str = "ourios.miner.confidence";

/// `ourios.miner.confidence.p01` (gauge, unit `1`).
pub const OURIOS_MINER_CONFIDENCE_P01: &str = "ourios.miner.confidence.p01";

/// `ourios.miner.confidence.p50` (gauge, unit `1`).
pub const OURIOS_MINER_CONFIDENCE_P50: &str = "ourios.miner.confidence.p50";

/// `ourios.miner.duration` (histogram, unit `s`).
pub const OURIOS_MINER_DURATION: &str = "ourios.miner.duration";

/// `ourios.miner.merges` (counter, unit `{merge}`).
pub const OURIOS_MINER_MERGES: &str = "ourios.miner.merges";

/// `ourios.miner.params.overflow` (counter, unit `{overflow}`).
pub const OURIOS_MINER_PARAMS_OVERFLOW: &str = "ourios.miner.params.overflow";

/// `ourios.miner.params.overflow.utilization` (gauge, unit `1`).
pub const OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION: &str =
    "ourios.miner.params.overflow.utilization";

/// `ourios.miner.parse_failures` (counter, unit `{failure}`).
pub const OURIOS_MINER_PARSE_FAILURES: &str = "ourios.miner.parse_failures";

/// `ourios.miner.template.count` (gauge, unit `{template}`).
pub const OURIOS_MINER_TEMPLATE_COUNT: &str = "ourios.miner.template.count";

/// `ourios.miner.template.version_changes` (counter, unit `{change}`).
pub const OURIOS_MINER_TEMPLATE_VERSION_CHANGES: &str = "ourios.miner.template.version_changes";

/// `ourios.storage.parquet.file.size` (histogram, unit `By`).
pub const OURIOS_STORAGE_PARQUET_FILE_SIZE: &str = "ourios.storage.parquet.file.size";

// Attribute keys.

/// `ourios.compaction.result` attribute key.
pub const OURIOS_COMPACTION_RESULT: &str = "ourios.compaction.result";

/// `ourios.io.direction` attribute key.
pub const OURIOS_IO_DIRECTION: &str = "ourios.io.direction";

/// `ourios.miner.template_change` attribute key.
pub const OURIOS_MINER_TEMPLATE_CHANGE: &str = "ourios.miner.template_change";

/// `ourios.service` attribute key.
pub const OURIOS_SERVICE: &str = "ourios.service";

/// `ourios.tenant` attribute key.
pub const OURIOS_TENANT: &str = "ourios.tenant";
