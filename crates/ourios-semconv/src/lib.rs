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

/// The semantic-conventions schema URL these constants were generated from — the
/// registry manifest's `schema_url`, which carries the conventions version
/// (decoupled from this crate's package version). Mirrors upstream
/// `opentelemetry_semantic_conventions::SCHEMA_URL`; attach it to a telemetry
/// resource so consumers can resolve the schema. Sourced from the registry's
/// resolved provenance (uniform across the registry).
pub const SCHEMA_URL: &str = "https://ourios.dev/schemas/ourios-0.1.0.yaml";

// Metric names (RFC 0009 §3.6).

/// `ourios.audit_sink.buffer.usage` (updowncounter, unit `{event}`).
pub const OURIOS_AUDIT_SINK_BUFFER_USAGE: &str = "ourios.audit_sink.buffer.usage";

/// `ourios.audit_sink.derive.errors` (counter, unit `{error}`).
pub const OURIOS_AUDIT_SINK_DERIVE_ERRORS: &str = "ourios.audit_sink.derive.errors";

/// `ourios.audit_sink.flush.errors` (counter, unit `{error}`).
pub const OURIOS_AUDIT_SINK_FLUSH_ERRORS: &str = "ourios.audit_sink.flush.errors";

/// `ourios.audit_sink.flush.events` (counter, unit `{event}`).
pub const OURIOS_AUDIT_SINK_FLUSH_EVENTS: &str = "ourios.audit_sink.flush.events";

/// `ourios.audit_sink.flushes` (counter, unit `{flush}`).
pub const OURIOS_AUDIT_SINK_FLUSHES: &str = "ourios.audit_sink.flushes";

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

/// `ourios.ingest.batches` (counter, unit `{batch}`).
pub const OURIOS_INGEST_BATCHES: &str = "ourios.ingest.batches";

/// `ourios.ingest.records` (counter, unit `{record}`).
pub const OURIOS_INGEST_RECORDS: &str = "ourios.ingest.records";

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

/// `ourios.query.duration` (histogram, unit `s`).
pub const OURIOS_QUERY_DURATION: &str = "ourios.query.duration";

/// `ourios.query.row_groups` (counter, unit `{row_group}`).
pub const OURIOS_QUERY_ROW_GROUPS: &str = "ourios.query.row_groups";

/// `ourios.sink.buffer.usage` (updowncounter, unit `By`).
pub const OURIOS_SINK_BUFFER_USAGE: &str = "ourios.sink.buffer.usage";

/// `ourios.sink.derive.errors` (counter, unit `{error}`).
pub const OURIOS_SINK_DERIVE_ERRORS: &str = "ourios.sink.derive.errors";

/// `ourios.sink.flush.duration` (histogram, unit `s`).
pub const OURIOS_SINK_FLUSH_DURATION: &str = "ourios.sink.flush.duration";

/// `ourios.sink.flush.errors` (counter, unit `{error}`).
pub const OURIOS_SINK_FLUSH_ERRORS: &str = "ourios.sink.flush.errors";

/// `ourios.sink.flush.records` (counter, unit `{record}`).
pub const OURIOS_SINK_FLUSH_RECORDS: &str = "ourios.sink.flush.records";

/// `ourios.storage.parquet.file.size` (histogram, unit `By`).
pub const OURIOS_STORAGE_PARQUET_FILE_SIZE: &str = "ourios.storage.parquet.file.size";

/// `ourios.wal.append.duration` (histogram, unit `s`).
pub const OURIOS_WAL_APPEND_DURATION: &str = "ourios.wal.append.duration";

// Attribute keys.

/// `ourios.audit_sink.flush.outcome` attribute key.
pub const OURIOS_AUDIT_SINK_FLUSH_OUTCOME: &str = "ourios.audit_sink.flush.outcome";

/// `ourios.compaction.result` attribute key.
pub const OURIOS_COMPACTION_RESULT: &str = "ourios.compaction.result";

/// `ourios.io.direction` attribute key.
pub const OURIOS_IO_DIRECTION: &str = "ourios.io.direction";

/// `ourios.miner.template_change` attribute key.
pub const OURIOS_MINER_TEMPLATE_CHANGE: &str = "ourios.miner.template_change";

/// `ourios.query.kind` attribute key.
pub const OURIOS_QUERY_KIND: &str = "ourios.query.kind";

/// `ourios.query.row_group.state` attribute key.
pub const OURIOS_QUERY_ROW_GROUP_STATE: &str = "ourios.query.row_group.state";

/// `ourios.service` attribute key.
pub const OURIOS_SERVICE: &str = "ourios.service";

/// `ourios.sink.flush.trigger` attribute key.
pub const OURIOS_SINK_FLUSH_TRIGGER: &str = "ourios.sink.flush.trigger";

/// `ourios.tenant` attribute key.
pub const OURIOS_TENANT: &str = "ourios.tenant";

// Log event names (the server's own dogfooded logs; every `tracing` call
// site names its event with one of these — `weaver registry live-check`
// enforces it at emission time).

/// `ourios.compaction.sweep.error` log event name.
pub const EVENT_OURIOS_COMPACTION_SWEEP_ERROR: &str = "ourios.compaction.sweep.error";

/// `ourios.querier.shutdown.error` log event name.
pub const EVENT_OURIOS_QUERIER_SHUTDOWN_ERROR: &str = "ourios.querier.shutdown.error";

/// `ourios.receiver.audit_sink.retained` log event name.
pub const EVENT_OURIOS_RECEIVER_AUDIT_SINK_RETAINED: &str = "ourios.receiver.audit_sink.retained";

/// `ourios.receiver.shutdown.error` log event name.
pub const EVENT_OURIOS_RECEIVER_SHUTDOWN_ERROR: &str = "ourios.receiver.shutdown.error";

/// `ourios.receiver.sink.retained` log event name.
pub const EVENT_OURIOS_RECEIVER_SINK_RETAINED: &str = "ourios.receiver.sink.retained";

/// `ourios.receiver.snapshot.error` log event name.
pub const EVENT_OURIOS_RECEIVER_SNAPSHOT_ERROR: &str = "ourios.receiver.snapshot.error";

/// `ourios.receiver.wal.truncated` log event name.
pub const EVENT_OURIOS_RECEIVER_WAL_TRUNCATED: &str = "ourios.receiver.wal.truncated";

/// `ourios.server.compaction.disabled` log event name.
pub const EVENT_OURIOS_SERVER_COMPACTION_DISABLED: &str = "ourios.server.compaction.disabled";

/// `ourios.server.signal_handler.error` log event name.
pub const EVENT_OURIOS_SERVER_SIGNAL_HANDLER_ERROR: &str = "ourios.server.signal_handler.error";
