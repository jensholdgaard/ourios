//! `ourios-server` — the Ourios binary (`CLAUDE.md` §1, §7).
//!
//! It boots OpenTelemetry (the OTLP push `MeterProvider`, RFC 0001 §6.8) and,
//! unless `OURIOS_COMPACTION_ENABLED` is set falsey, runs the **background
//! compaction role** (RFC 0009 §3.2) — opening a durable audit sink for the
//! §3.6 compaction events (RFC 0005 §3.7) and sweeping until shutdown. A
//! multi-pod deployment disables it on the receiver/querier pods so a single
//! dedicated compactor sweeps.
//!
//! When `OURIOS_RECEIVER_ENABLED` is set it also runs the **OTLP receiver
//! role** (RFC 0003 §6.2 / the §9 process-model resolution): gRPC + HTTP
//! listeners over one shared pipeline (see [`receiver`]). When
//! `OURIOS_QUERIER_ENABLED` is set it runs the **querier role** (RFC 0016):
//! the HTTP query API over the logs DSL (`ourios_server::querier`), reading
//! the same `OURIOS_BUCKET_ROOT` store. Every role shares the tokio runtime
//! and shuts down gracefully on SIGINT or SIGTERM (the latter is what k8s /
//! `nerdctl stop` send), then telemetry flushes.
//!
//! Configuration comes from `OURIOS_*` environment variables, or — when
//! `--config <path>` is given — from a YAML file with `${env:…}` substitution
//! (RFC 0020). With `--config` the file is the sole source of Ourios's
//! configuration and bare `OURIOS_*` env vars do not override it; both paths
//! resolve the same [`ServerConfig`] through the same `build_*` validators.
//!
//! Logs are dogfooded (`CLAUDE.md` §6.3): everything after the telemetry
//! bootstrap logs through `tracing`, which `ourios-telemetry` bridges to an
//! `OTel` log record pushed over OTLP — Ourios's own logs travel the same
//! protocol its users' logs arrive on — with a human-readable copy on stderr.
//! stdout stays reserved for the machine-parsed start-up lines (the
//! bound-port announcements integration tests read).

#![deny(unsafe_code)]

mod receiver;

use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use ourios_config::{MinerConfig, UpstreamTemplates};
use ourios_ingester::Compactor;
use ourios_ingester::receiver::tls::TlsSettings;

use ourios_parquet::{
    CompactionPolicy, ParquetAuditSink, PromotedAttributes, S3Config, StoreConfig,
};
use ourios_server::config::file::{FileConfig, PromotedEntry, TlsSection};
use ourios_telemetry::TelemetryConfig;
use ourios_wal::WalConfig;
use tracing::Instrument as _;

/// Default compaction sweep cadence when `OURIOS_COMPACTION_INTERVAL_SECS`
/// is unset.
const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 300;

/// Default OTLP/gRPC bind address (port 4317, the OTLP default).
const DEFAULT_GRPC_ADDR: &str = "0.0.0.0:4317";
/// Default OTLP/HTTP bind address (port 4318, the OTLP default).
const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:4318";
/// Default querier HTTP bind address (port 4319, adjacent to the OTLP
/// receiver ports).
const DEFAULT_QUERIER_HTTP_ADDR: &str = "0.0.0.0:4319";
/// Default look-back window for a query with no `range(...)` stage — one
/// hour (RFC 0002 §4 P5; RFC 0016 §7).
const DEFAULT_QUERIER_WINDOW_SECS: u64 = 3600;
/// Nanoseconds per second — the unit the DSL compiler's window is in.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Resolved server configuration. `PartialEq` only — the receiver
/// params carry the miner's `f32` thresholds.
#[derive(Debug, Clone, PartialEq)]
struct ServerConfig {
    /// The data + audit store backend (local or S3, RFC 0019).
    store: StoreConfig,
    /// Whether this process runs the background compaction sweep. Default on;
    /// `OURIOS_COMPACTION_ENABLED=0` disables it so a multi-pod deployment can
    /// run a single dedicated compactor rather than every pod sweeping (RFC 0009
    /// §3.2 — `publish_cas` keeps concurrent sweeps correct, but one sweeper
    /// avoids the redundant per-interval object listing).
    compaction_enabled: bool,
    /// How often the compaction daemon sweeps (when enabled).
    compaction_interval: Duration,
    /// The OTLP receiver role, if enabled (RFC 0003 §9).
    receiver: Option<ReceiverParams>,
    /// The querier role, if enabled (RFC 0016).
    querier: Option<QuerierParams>,
    /// The effective RFC 0022 promoted attribute set
    /// (`storage.promoted_attributes`, §3.2) — applied by every write path
    /// (receiver flushes and compaction rewrites; §3.4).
    promoted: PromotedAttributes,
    /// The resolved `auth` section (RFC 0026 static tokens + RFC 0029 OIDC),
    /// or `None` for open mode. Config-file only (§3.1 — tokens ride the
    /// `${env:…}` indirection); the env-only path always resolves open. The
    /// listeners consume the config's enforcement store: with OIDC configured
    /// and no static tokens that store is empty — enforced, not open — until
    /// the RFC 0029 verifier slice teaches the gates the full config.
    auth: Option<ourios_server::auth::AuthConfig>,
}

/// Resolved querier-role configuration (RFC 0016 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuerierParams {
    http_addr: SocketAddr,
    /// RFC 0030 §3.1 — TLS on the querier listener (config-file only;
    /// `None` = plaintext). Carried here from this slice on; the
    /// acceptor wiring consumes it in the RFC0030.3 slice.
    http_tls: Option<TlsSettings>,
    default_window_nanos: u64,
    /// Serve the RFC 0027 MCP surface at `/mcp` (`querier.mcp.enabled` /
    /// `OURIOS_QUERIER_MCP_ENABLED`; default off).
    mcp_enabled: bool,
}

/// Resolved OTLP-receiver-role configuration (RFC 0003 §6.2).
/// `PartialEq` only: [`MinerConfig`] carries `f32` thresholds, which
/// have no total equality.
#[derive(Debug, Clone, PartialEq)]
struct ReceiverParams {
    grpc_addr: SocketAddr,
    /// RFC 0030 §3.1 — TLS per listener (config-file only; `None` =
    /// plaintext). Carried here from this slice on; the acceptor wiring
    /// consumes them in the RFC0030.1/.2 slices.
    grpc_tls: Option<TlsSettings>,
    http_addr: SocketAddr,
    http_tls: Option<TlsSettings>,
    wal_root: PathBuf,
    /// RFC 0035 §3.1 — worker count for the concurrent encode pool
    /// (`receiver.encode_workers` / `OURIOS_RECEIVER_ENCODE_WORKERS`;
    /// default: the host's available cores, validated ≥ 1).
    encode_workers: usize,
    /// RFC 0050 §3.2 — the upstream-template dial (`miner.*`,
    /// config-file only; the env path always gets the defaults, whose
    /// `ignore` mode is byte-identical pre-RFC behaviour).
    miner: MinerConfig,
}

/// Resolve [`ServerConfig`] from the environment:
/// - `OURIOS_STORAGE_BACKEND` (optional, `local` (default) or `s3`) — the data
///   + audit store backend (RFC 0019).
/// - `OURIOS_BUCKET_ROOT` (required for the `local` backend) — the store root.
/// - `OURIOS_S3_BUCKET` (required for `s3`) + `OURIOS_S3_ENDPOINT` /
///   `OURIOS_S3_REGION` / `OURIOS_S3_PREFIX` (optional) — S3 addressing.
/// - `OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` /
///   `OURIOS_S3_SESSION_TOKEN` (optional, **secret**) — explicit S3 credentials
///   applied over the standard chain (RFC 0019 §3.4); when unset, credentials
///   come from the chain (`AmazonS3Builder::from_env`, incl. IRSA). Never
///   logged (RFC 0019 §3.4).
/// - `OURIOS_COMPACTION_ENABLED` (optional, default on) — set to a falsey value
///   (`0`/`false`/`no`/`off`) to disable this process's compaction sweep, so a
///   deployment can run a single dedicated compactor (RFC 0009 §3.2).
/// - `OURIOS_COMPACTION_INTERVAL_SECS` (optional, default
///   [`DEFAULT_COMPACTION_INTERVAL_SECS`]).
/// - `OURIOS_RECEIVER_ENABLED` (optional) — enable the receiver role.
/// - `OURIOS_RECEIVER_GRPC_ADDR` / `OURIOS_RECEIVER_HTTP_ADDR` (optional,
///   default [`DEFAULT_GRPC_ADDR`] / [`DEFAULT_HTTP_ADDR`]).
/// - `OURIOS_WAL_ROOT` (required when the receiver is enabled) — the
///   write-ahead-log root (always local, RFC 0019 §3.1).
/// - `OURIOS_RECEIVER_ENCODE_WORKERS` (optional, ≥ 1; default: available
///   cores) — the RFC 0035 §3.1 concurrent-encode pool size.
fn config_from_env() -> Result<ServerConfig, String> {
    let store = build_store_config(
        std::env::var("OURIOS_STORAGE_BACKEND").ok().as_deref(),
        std::env::var_os("OURIOS_BUCKET_ROOT").map(PathBuf::from),
        std::env::var("OURIOS_S3_BUCKET").ok().as_deref(),
        std::env::var("OURIOS_S3_ENDPOINT").ok().as_deref(),
        std::env::var("OURIOS_S3_REGION").ok().as_deref(),
        std::env::var("OURIOS_S3_PREFIX").ok().as_deref(),
    )?;
    // Explicit S3 credentials (RFC 0019 §3.4), layered over the standard chain.
    // Bound to locals so the `as_deref` borrows outlive the call.
    let s3_access_key_id = std::env::var("OURIOS_S3_ACCESS_KEY_ID").ok();
    let s3_secret_access_key = std::env::var("OURIOS_S3_SECRET_ACCESS_KEY").ok();
    let s3_session_token = std::env::var("OURIOS_S3_SESSION_TOKEN").ok();
    let store = with_s3_credentials(
        store,
        s3_access_key_id.as_deref(),
        s3_secret_access_key.as_deref(),
        s3_session_token.as_deref(),
    );
    let interval_raw = std::env::var("OURIOS_COMPACTION_INTERVAL_SECS").ok();
    let mut config = build_config(
        store,
        std::env::var("OURIOS_COMPACTION_ENABLED").ok().as_deref(),
        interval_raw.as_deref(),
    )?;
    config.receiver = build_receiver_config(
        std::env::var("OURIOS_RECEIVER_ENABLED").ok().as_deref(),
        std::env::var("OURIOS_RECEIVER_GRPC_ADDR").ok().as_deref(),
        std::env::var("OURIOS_RECEIVER_HTTP_ADDR").ok().as_deref(),
        std::env::var_os("OURIOS_WAL_ROOT").map(PathBuf::from),
        std::env::var("OURIOS_RECEIVER_ENCODE_WORKERS")
            .ok()
            .as_deref(),
    )?;
    config.querier = build_querier_config(
        std::env::var("OURIOS_QUERIER_ENABLED").ok().as_deref(),
        std::env::var("OURIOS_QUERIER_HTTP_ADDR").ok().as_deref(),
        std::env::var("OURIOS_QUERIER_DEFAULT_WINDOW_SECS")
            .ok()
            .as_deref(),
        std::env::var("OURIOS_QUERIER_MCP_ENABLED").ok().as_deref(),
    )?;
    Ok(config)
}

/// Ourios log-storage server (`CLAUDE.md` §1).
///
/// Configuration is read from the `--config` file (RFC 0020) when given,
/// otherwise from `OURIOS_*` environment variables.
// A derived `clap` parser (rather than hand-rolled) for `--help`/`--version`,
// usage, and argument-error handling (missing value, unknown flag, trailing
// arguments) — the RFC 0020 §3.2 CLI contract, for free.
#[derive(Debug, clap::Parser)]
#[command(name = "ourios-server", version, about = "Ourios log-storage server")]
struct Cli {
    /// Path to a YAML configuration file (RFC 0020). When given, the file is the
    /// sole source of configuration and the environment participates only through
    /// `${env:…}` substitution inside it; without it, configuration comes from
    /// `OURIOS_*` environment variables.
    #[arg(long, value_name = "PATH", value_parser = non_empty_path)]
    config: Option<PathBuf>,

    /// An operator verb; without one, the server runs its configured roles.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Operate the authorization graph's operational surfaces (RFC 0048).
    #[command(subcommand)]
    Graph(GraphVerb),
}

#[derive(Debug, clap::Subcommand)]
enum GraphVerb {
    /// Request the erasure of one conversation: writes the durable marker
    /// the next compaction sweep acts on (idempotent, RFC 0048 §3.3).
    Erase {
        /// The tenant (RFC 0048 §3.1 grammar).
        #[arg(long)]
        tenant: String,
        /// The raw conversation id (object-id grammar; `/` is allowed).
        #[arg(long)]
        conversation: String,
    },
    /// List pending erasure requests (and backfill locks) with their phase.
    Erasures {
        /// Restrict the listing to one tenant.
        #[arg(long)]
        tenant: Option<String>,
    },
    /// Feed the graph from stored history: read every partition of the
    /// tenant, derive and write the RFC 0047 tuples (idempotent, never
    /// rewrites Parquet). Refuses while erasures are pending (RFC 0048
    /// §3.4).
    Backfill {
        /// The tenant (RFC 0048 §3.1 grammar).
        #[arg(long)]
        tenant: String,
        /// Only partitions whose UTC hour starts at or after this
        /// RFC 3339 instant.
        #[arg(long)]
        from: Option<String>,
        /// Clear a crashed run's backfill lock instead of running.
        #[arg(long, conflicts_with = "from")]
        unlock: bool,
    },
}

/// Run the CLI's operator verb, if one was given: the grammar gate runs
/// **first** — even `resolve_config` can create the local store root, so
/// an off-grammar invocation performs no filesystem, telemetry or backend
/// work at all — then the verb boots the telemetry stack, resolves the
/// same storage config as the daemon inside its CLI span, acts on the
/// store of record, and the caller exits: no roles, no listeners
/// (RFC 0048 §3.3). `false` when there is no verb and the server should
/// run its roles.
async fn run_operator_verb(cli: &Cli) -> Result<bool, String> {
    match &cli.command {
        None => Ok(false),
        Some(Command::Graph(verb)) => {
            // The grammar gate first: an off-grammar invocation exits here,
            // before any configuration, store or telemetry work.
            validate_graph_verb(verb)?;
            // An operator verb is a short-lived CLI program, which
            // OpenTelemetry's CLI semantic conventions cover explicitly. It
            // boots the **same** stack the daemon does, so the universal
            // OTel env vars stay the only control surface (`OTEL_SDK_DISABLED`,
            // `OTEL_*_EXPORTER=none` to silence it) — a bespoke `--telemetry`
            // flag would duplicate them. The stderr `fmt` mirror is installed
            // either way, so the verb's progress events reach the operator
            // even when nothing is exported.
            let telemetry = ourios_telemetry::init(&TelemetryConfig::new("ourios-server"))
                .map_err(|e| e.to_string())?;
            // Configuration resolves *inside* the span, so a malformed
            // `storage`/TLS section is a recorded non-zero exit rather than
            // an untraced one (the convention requires an exit code, and
            // `error.type` whenever it is non-zero).
            let outcome = run_graph_verb_traced(verb, cli.config.as_deref()).await;
            // `Shutdown` includes the effects of `ForceFlush` (OTel SDK
            // spec): a short-lived process must drain before it exits.
            drop(telemetry);
            outcome.map(|()| true)
        }
    }
}

/// One `graph` verb inside the `OTel` **CLI callee span** (semantic
/// conventions for CLI programs): named after the executable, `INTERNAL`
/// kind, carrying `process.executable.name`, `process.pid` and — once the
/// verb returns — `process.exit.code`, plus `error.type` and an error
/// status when that code is non-zero. `process.command_args` is
/// deliberately **not** recorded: the convention says not to collect it by
/// default without sanitisation, and a verb's arguments carry tenant and
/// conversation ids.
async fn run_graph_verb_traced(verb: &GraphVerb, config_path: Option<&Path>) -> Result<(), String> {
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "ourios-server".to_string());
    // A static span name keeps cardinality flat; the convention's
    // `{process.executable.name}` travels as the attribute.
    let span = tracing::info_span!(
        "ourios-server",
        // The convention names the span after the executable; `otel.name`
        // is how a computed name reaches the exported span while the macro
        // literal stays stable (as `/mcp` does).
        otel.name = %executable,
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        process.executable.name = %executable,
        // `int` per semconv — a bare `u32` records as a string through
        // `tracing`, which weaver's live-check flags as a type mismatch.
        process.pid = i64::from(std::process::id()),
        process.exit.code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    let outcome = async {
        let config = resolve_config(config_path)?;
        let store = config.store.open().map_err(|e| e.to_string())?;
        run_graph_verb(verb, &store, &config).await
    }
    .instrument(span.clone())
    .await;
    if outcome.is_ok() {
        span.record("process.exit.code", 0);
    } else {
        // `main` returns `Err` → exit code 1. The error class stays the
        // `OTel` fallback until the verbs carry typed failures.
        span.record("process.exit.code", 1);
        span.record("error.type", "_OTHER");
        span.record("otel.status_code", "ERROR");
    }
    outcome
}

/// The grammar gate for every `graph` verb's ids, run **before** the
/// store is opened (RFC0048.4): the tenant grammar for `--tenant`, the
/// object-id grammar for `--conversation` (a raw id may contain `/`; one
/// that cannot name a graph object for its tenant is still erasable —
/// its *rows* are the leak, RFC 0048 §3.1).
fn validate_graph_verb(verb: &GraphVerb) -> Result<(), String> {
    match verb {
        GraphVerb::Erase {
            tenant,
            conversation,
        } => {
            ourios_core::tenant::validate_tenant_id(tenant)
                .map_err(|e| format!("--tenant: {e} (RFC 0048 §3.1)"))?;
            if !ourios_core::auth::openfga::is_object_id(conversation) {
                return Err(
                    "--conversation: not an object id (non-empty, no ':', '#' or \
                     whitespace, at most 256 bytes) (RFC 0048 §3.1)"
                        .to_string(),
                );
            }
            Ok(())
        }
        GraphVerb::Erasures { tenant } => match tenant {
            Some(tenant) => ourios_core::tenant::validate_tenant_id(tenant)
                .map_err(|e| format!("--tenant: {e} (RFC 0048 §3.1)")),
            None => Ok(()),
        },
        GraphVerb::Backfill { tenant, .. } => ourios_core::tenant::validate_tenant_id(tenant)
            .map_err(|e| format!("--tenant: {e} (RFC 0048 §3.1)")),
    }
}

/// Run one grammar-checked `graph` verb against the configured store and
/// exit (RFC0048.4).
async fn run_graph_verb(
    verb: &GraphVerb,
    store: &ourios_parquet::Store,
    config: &ServerConfig,
) -> Result<(), String> {
    use ourios_ingester::compactor::{
        backfill_locks, pending_erasures, pending_erasures_for, request_erasure,
    };
    match verb {
        GraphVerb::Erase {
            tenant,
            conversation,
        } => {
            request_erasure(store, tenant, conversation).map_err(|e| e.to_string())?;
            println!(
                "erasure requested: tenant {tenant:?} conversation {conversation:?} \
                 (idempotent; the next compaction sweep acts on it)"
            );
            Ok(())
        }
        GraphVerb::Erasures { tenant } => {
            let requests = match tenant {
                Some(tenant) => pending_erasures_for(store, tenant),
                None => pending_erasures(store),
            }
            .map_err(|e| e.to_string())?;
            let mut locks = backfill_locks(store).map_err(|e| e.to_string())?;
            if let Some(tenant) = tenant {
                locks.retain(|t| t == tenant);
            }
            if requests.is_empty() && locks.is_empty() {
                println!("no pending erasures");
                return Ok(());
            }
            for request in requests {
                println!(
                    "pending erasure: tenant {:?} conversation {:?} phase {}",
                    request.tenant,
                    request.conversation_id,
                    match request.phase {
                        ourios_ingester::compactor::ErasurePhase::Rows => "rows",
                        ourios_ingester::compactor::ErasurePhase::Tuples => "tuples",
                    }
                );
            }
            for tenant in locks {
                println!("backfill lock: tenant {tenant:?}");
            }
            Ok(())
        }
        GraphVerb::Backfill {
            tenant,
            from,
            unlock,
        } => run_backfill_verb(store, config, tenant, from.as_deref(), *unlock).await,
    }
}

/// A `--config` value parser that rejects an empty path (a required argument
/// must name a file), yielding a clear `clap` error rather than a later
/// file-not-found on `""`.
fn non_empty_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        Err("the config path must not be empty".to_owned())
    } else {
        Ok(PathBuf::from(value))
    }
}

/// The `graph backfill` verb (RFC 0048 §3.4): parse `--from`, resolve the
/// emitter from `auth.openfga`, run the fenced backfill (or clear a
/// crashed run's lock with `--unlock`).
async fn run_backfill_verb(
    store: &ourios_parquet::Store,
    config: &ServerConfig,
    tenant: &str,
    from: Option<&str>,
    unlock: bool,
) -> Result<(), String> {
    use ourios_ingester::compactor::{backfill_tenant, release_backfill_lock};
    if unlock {
        release_backfill_lock(store, tenant).map_err(|e| e.to_string())?;
        println!("backfill lock cleared for tenant {tenant:?} (if one existed)");
        return Ok(());
    }
    let from_unix_nanos = from
        .map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .map_err(|e| format!("--from: not RFC 3339: {e}"))
                .and_then(|dt| {
                    u64::try_from(
                        dt.timestamp_nanos_opt()
                            .ok_or_else(|| "--from: out of range".to_string())?,
                    )
                    .map_err(|_| "--from: before the epoch".to_string())
                })
        })
        .transpose()?;
    let openfga = config
        .auth
        .as_ref()
        .and_then(|auth| auth.openfga.as_ref())
        .ok_or_else(|| {
            "graph backfill needs auth.openfga configured (the emitter derives \
             tuples from the visibility bindings)"
                .to_string()
        })?;
    // The same startup gate the daemon applies (RFC 0048 §3.2): a graph
    // column outside the promoted set would silently derive no tuples for
    // the whole history this run is meant to feed.
    validate_graph_columns(openfga, &config.promoted)?;
    let emitter =
        ourios_ingester::graph_emitter::GraphEmitter::from_config(openfga)?.ok_or_else(|| {
            "graph backfill needs a conversation object bound \
             (auth.openfga.visibility.objects)"
                .to_string()
        })?;
    let emitter = std::sync::Arc::new(emitter);
    match backfill_tenant(store, &emitter, tenant, from_unix_nanos)
        .await
        .map_err(|e| e.to_string())?
    {
        Ok(report) => {
            println!(
                "backfill complete: tenant {tenant:?}, {} partitions, {} rows offered, \
                 {} tuples written",
                report.partitions, report.rows, report.tuples
            );
            Ok(())
        }
        Err(refusal) => Err(format!("backfill refused: {refusal}")),
    }
}

/// Resolve [`ServerConfig`] from a YAML configuration file (RFC 0020). The file
/// is the **sole** source of Ourios's configuration; the environment
/// participates only through `${env:…}` substitution inside it (§3.2), so a bare
/// `OURIOS_*` env var never overrides a file value.
fn config_from_file(path: &Path) -> Result<ServerConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config file {}: {e}", path.display()))?;
    let file = ourios_server::config::file::parse(&text, &|name| std::env::var(name).ok())
        .map_err(|e| format!("config file {}: {e}", path.display()))?;
    server_config_from_file(&file)
}

/// Map a parsed [`FileConfig`] onto the resolved [`ServerConfig`] through the
/// **same** `build_*` validators the environment path uses — the single
/// validation path (RFC 0020 §3.1). `FileConfig`'s leaves are already the
/// string-valued inputs those functions expect, so this is a pass-through: the
/// file front-end adds no second set of validation rules.
///
/// The validators name `OURIOS_*` env vars in their error text; a file-sourced
/// value that fails reuses that message rather than duplicating the rule — the
/// §3.1 trade-off of one validation path (localising the error text to YAML keys
/// is a possible follow-up).
fn server_config_from_file(file: &FileConfig) -> Result<ServerConfig, String> {
    let store = build_store_config(
        file.storage.backend.as_deref(),
        file.storage.local.bucket_root.as_deref().map(PathBuf::from),
        file.storage.s3.bucket.as_deref(),
        file.storage.s3.endpoint.as_deref(),
        file.storage.s3.region.as_deref(),
        file.storage.s3.prefix.as_deref(),
    )?;
    let store = with_s3_credentials(
        store,
        file.storage.s3.access_key_id.as_deref(),
        file.storage.s3.secret_access_key.as_deref(),
        file.storage.s3.session_token.as_deref(),
    );
    let mut config = build_config(
        store,
        file.compaction.enabled.as_deref(),
        file.compaction.interval_secs.as_deref(),
    )?;
    config.receiver = build_receiver_config(
        file.receiver.enabled.as_deref(),
        file.receiver.grpc_addr.as_deref(),
        file.receiver.http_addr.as_deref(),
        file.receiver.wal_root.as_deref().map(PathBuf::from),
        file.receiver.encode_workers.as_deref(),
    )?;
    if let Some(receiver) = config.receiver.as_mut() {
        receiver.grpc_tls = tls_settings("receiver.grpc_tls", &file.receiver.grpc_tls)?;
        receiver.http_tls = tls_settings("receiver.http_tls", &file.receiver.http_tls)?;
        receiver.miner = build_miner_config(
            file.miner.upstream_templates.as_deref(),
            file.miner.upstream_template_byte_limit.as_deref(),
            file.miner.upstream_association_limit.as_deref(),
        )?;
    }
    config.querier = build_querier_config(
        file.querier.enabled.as_deref(),
        file.querier.http_addr.as_deref(),
        file.querier.default_window_secs.as_deref(),
        file.querier.mcp.enabled.as_deref(),
    )?;
    if let Some(querier) = config.querier.as_mut() {
        querier.http_tls = tls_settings("querier.http_tls", &file.querier.http_tls)?;
    }
    config.promoted = build_promoted_attributes(
        &file.storage.promoted_attributes.resource,
        &file.storage.promoted_attributes.log,
    )?;
    config.auth = ourios_server::auth::build_auth_config(file.auth.as_ref())?;
    Ok(config)
}

/// Pure storage-backend resolution (env reads live in [`config_from_env`];
/// this is the testable core, RFC 0019 §3.1/§3.2).
///
/// `backend_raw` is `OURIOS_STORAGE_BACKEND` (`local` (default) or `s3`),
/// trimmed and treated as unset when empty. The `local` backend requires a
/// non-empty `bucket_root`; `s3` requires a non-empty `s3_bucket` and accepts
/// optional endpoint/region/prefix. Credentials are never read here — the
/// explicit `OURIOS_S3_*` keys are applied separately by [`with_s3_credentials`]
/// and the chain is the fallback in [`StoreConfig::open`] (RFC 0019 §3.4), so
/// an error for a **missing required** value names only the key, never a secret;
/// other errors (an unknown backend) may echo the offending non-secret value for
/// diagnosability.
fn build_store_config(
    backend_raw: Option<&str>,
    bucket_root: Option<PathBuf>,
    s3_bucket: Option<&str>,
    s3_endpoint: Option<&str>,
    s3_region: Option<&str>,
    s3_prefix: Option<&str>,
) -> Result<StoreConfig, String> {
    // Trim and treat empty as unset, so " s3 " selects S3 and a blank value
    // falls back to the local default rather than reading as an unknown backend.
    match backend_raw.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("local") => {
            let root = bucket_root
                .ok_or("OURIOS_BUCKET_ROOT must be set (the local data + audit store root)")?;
            if root.as_os_str().is_empty() {
                return Err("OURIOS_BUCKET_ROOT must not be empty".to_string());
            }
            Ok(StoreConfig::Local(root))
        }
        Some("s3") => {
            let bucket = s3_bucket
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or("OURIOS_S3_BUCKET must be set when OURIOS_STORAGE_BACKEND=s3")?;
            let mut cfg = S3Config::new(bucket);
            if let Some(endpoint) = s3_endpoint.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_endpoint(endpoint);
            }
            if let Some(region) = s3_region.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_region(region);
            }
            if let Some(prefix) = s3_prefix.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_prefix(prefix);
            }
            Ok(StoreConfig::S3(cfg))
        }
        Some(other) => Err(format!(
            "OURIOS_STORAGE_BACKEND must be 'local' or 's3', got {other:?}"
        )),
    }
}

/// Apply explicit S3 credentials (RFC 0019 §3.4) onto a resolved [`StoreConfig`].
///
/// Each value is trimmed and an empty string is treated as unset (matching the
/// addressing knobs), so a present-but-blank env var does not count as "set"
/// and trip the partial-pair check at store-build time. A `local` backend
/// carries no credentials, so it passes through unchanged. The pairing rule
/// (access key + secret together; a session token only with the pair) and the
/// secret-scrubbing of any resulting error are enforced in
/// `ourios_parquet::Store::s3`, which names only the offending field, never a
/// value (RFC 0019 §3.4).
fn with_s3_credentials(
    store: StoreConfig,
    access_key_id: Option<&str>,
    secret_access_key: Option<&str>,
    session_token: Option<&str>,
) -> StoreConfig {
    let clean = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match store {
        StoreConfig::S3(mut cfg) => {
            cfg.access_key_id = clean(access_key_id);
            cfg.secret_access_key = clean(secret_access_key);
            cfg.session_token = clean(session_token);
            StoreConfig::S3(cfg)
        }
        local @ StoreConfig::Local(_) => local,
    }
}

/// Pure querier-config assembly + validation (env reads live in
/// [`config_from_env`]). `None` when the querier role is disabled.
///
/// - `enabled_raw` — `OURIOS_QUERIER_ENABLED` (`1`/`true`/`yes` enables).
/// - `http_raw` — `OURIOS_QUERIER_HTTP_ADDR` (default
///   [`DEFAULT_QUERIER_HTTP_ADDR`]).
/// - `window_raw` — `OURIOS_QUERIER_DEFAULT_WINDOW_SECS` (default
///   [`DEFAULT_QUERIER_WINDOW_SECS`]); must be a non-zero integer of seconds.
fn build_querier_config(
    enabled_raw: Option<&str>,
    http_raw: Option<&str>,
    window_raw: Option<&str>,
    mcp_enabled_raw: Option<&str>,
) -> Result<Option<QuerierParams>, String> {
    if !matches!(enabled_raw, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
    // Opt-in like the roles themselves (RFC 0027 §3.1; default off).
    let mcp_enabled = matches!(mcp_enabled_raw, Some("1" | "true" | "yes"));
    let http_addr = parse_addr(http_raw, DEFAULT_QUERIER_HTTP_ADDR)?;
    let window_secs = match window_raw {
        None => DEFAULT_QUERIER_WINDOW_SECS,
        Some(raw) => {
            let secs: u64 = raw.parse().map_err(|_| {
                format!(
                    "OURIOS_QUERIER_DEFAULT_WINDOW_SECS must be a positive integer, got {raw:?}"
                )
            })?;
            if secs == 0 {
                return Err("OURIOS_QUERIER_DEFAULT_WINDOW_SECS must be non-zero".to_string());
            }
            secs
        }
    };
    let default_window_nanos = window_secs
        .checked_mul(NANOS_PER_SEC)
        .ok_or("OURIOS_QUERIER_DEFAULT_WINDOW_SECS overflows when converted to nanoseconds")?;
    Ok(Some(QuerierParams {
        http_addr,
        http_tls: None,
        default_window_nanos,
        mcp_enabled,
    }))
}

/// Pure receiver-config assembly + validation (env reads live in
/// [`config_from_env`]). `None` when the receiver role is disabled.
fn build_receiver_config(
    enabled_raw: Option<&str>,
    grpc_raw: Option<&str>,
    http_raw: Option<&str>,
    wal_root: Option<PathBuf>,
    encode_workers_raw: Option<&str>,
) -> Result<Option<ReceiverParams>, String> {
    if !matches!(enabled_raw, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
    let grpc_addr = parse_addr(grpc_raw, DEFAULT_GRPC_ADDR)?;
    let http_addr = parse_addr(http_raw, DEFAULT_HTTP_ADDR)?;
    let wal_root = wal_root
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("OURIOS_WAL_ROOT must be set when the receiver role is enabled")?;
    let encode_workers = parse_encode_workers(encode_workers_raw)?;
    Ok(Some(ReceiverParams {
        grpc_addr,
        grpc_tls: None,
        http_addr,
        http_tls: None,
        wal_root,
        encode_workers,
        miner: MinerConfig::default(),
    }))
}

/// Pure miner-dial assembly + validation (RFC 0050 §3.2; config-file
/// only). Absent values take the [`MinerConfig`] defaults — `ignore`
/// mode, byte limit 8192, association limit 4.
fn build_miner_config(
    upstream_templates_raw: Option<&str>,
    byte_limit_raw: Option<&str>,
    association_limit_raw: Option<&str>,
) -> Result<MinerConfig, String> {
    let mut config = MinerConfig::default();
    if let Some(raw) = upstream_templates_raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mode = match raw {
            "ignore" => UpstreamTemplates::Ignore,
            "observe" => UpstreamTemplates::Observe,
            "adopt" => UpstreamTemplates::Adopt,
            other => {
                return Err(format!(
                    "miner.upstream_templates must be 'ignore', 'observe' or 'adopt', got {other:?}"
                ));
            }
        };
        config = config.with_upstream_templates(mode);
    }
    if let Some(raw) = byte_limit_raw.map(str::trim).filter(|s| !s.is_empty()) {
        let limit: u32 = raw.parse().map_err(|_| {
            format!(
                "miner.upstream_template_byte_limit must be an integer of UTF-8 bytes \
                 (0 disables all upstream-template handling), got {raw:?}"
            )
        })?;
        config = config.with_upstream_template_byte_limit(limit);
    }
    if let Some(raw) = association_limit_raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let limit: u16 = raw.parse().map_err(|_| {
            format!("miner.upstream_association_limit must be an integer ≥ 0, got {raw:?}")
        })?;
        config = config.with_upstream_association_limit(limit);
    }
    Ok(config)
}

/// Parse the RFC 0035 encode-pool worker count: ≥ 1 when set, else the
/// host's available cores (min 1 — `available_parallelism` can fail in
/// constrained environments, and the pool needs at least one worker).
fn parse_encode_workers(raw: Option<&str>) -> Result<usize, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get));
    };
    match raw.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "OURIOS_RECEIVER_ENCODE_WORKERS must be an integer ≥ 1, got {raw:?}"
        )),
    }
}

/// Parse a socket address, falling back to `default` when unset.
fn parse_addr(raw: Option<&str>, default: &str) -> Result<SocketAddr, String> {
    let value = raw.unwrap_or(default);
    value
        .parse()
        .map_err(|e| format!("invalid socket address {value:?}: {e}"))
}

/// The receiver role's WAL config: `root` plus the workspace-standard
/// durability knobs (RFC 0008 §6.3).
fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// Pure config assembly + validation (env reads live in
/// [`config_from_env`]; this is the testable core).
fn build_config(
    store: StoreConfig,
    compaction_enabled_raw: Option<&str>,
    interval_raw: Option<&str>,
) -> Result<ServerConfig, String> {
    // Compaction is opt-*out* (default on), unlike the opt-in receiver/querier
    // roles: an explicit falsey value disables the sweep, anything else (incl.
    // unset) keeps it on.
    let compaction_enabled = !matches!(
        compaction_enabled_raw.map(str::trim),
        Some("0" | "false" | "no" | "off")
    );
    // Only parse/validate the interval when compaction is on — a pod with
    // compaction disabled must not fail to start over an interval it never uses
    // (the default is a placeholder there, never read).
    let compaction_interval = if compaction_enabled {
        match interval_raw {
            None => Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS),
            Some(raw) => {
                let secs: u64 = raw.parse().map_err(|_| {
                    format!(
                        "OURIOS_COMPACTION_INTERVAL_SECS must be a positive integer, got {raw:?}"
                    )
                })?;
                if secs == 0 {
                    return Err("OURIOS_COMPACTION_INTERVAL_SECS must be non-zero".to_string());
                }
                Duration::from_secs(secs)
            }
        }
    } else {
        Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS)
    };
    Ok(ServerConfig {
        store,
        compaction_enabled,
        compaction_interval,
        receiver: None,
        querier: None,
        promoted: PromotedAttributes::default(),
        auth: None,
    })
}

/// Resolve `storage.promoted_attributes` (RFC 0022 §3.2 / RFC 0042 §3.2)
/// into the effective promoted set. Keys are taken literally (no
/// globbing), so a key that is empty or carries surrounding whitespace —
/// e.g. an `${env:…}` reference that resolved to nothing, or a quoted
/// `" key"` — is a config error rather than a silently never-matching
/// promoted column. RFC0042.6 makes the remaining offences loud at
/// startup rather than silently collapsed: an unknown `type` token, a
/// key listed twice within a family (whatever spelling each occurrence
/// uses), and a re-typed `service.name`.
fn build_promoted_attributes(
    resource: &[PromotedEntry],
    log: &[PromotedEntry],
) -> Result<PromotedAttributes, String> {
    fn family(
        which: &str,
        entries: &[PromotedEntry],
    ) -> Result<Vec<ourios_parquet::PromotedKey>, String> {
        let mut seen = std::collections::HashSet::new();
        entries
            .iter()
            .map(|e| {
                if e.key.is_empty() || e.key.trim() != e.key {
                    return Err(format!(
                        "storage.promoted_attributes.{which} keys must be non-empty \
                         attribute names without surrounding whitespace"
                    ));
                }
                if !seen.insert(e.key.as_str()) {
                    return Err(format!(
                        "storage.promoted_attributes.{which} lists {:?} more than once \
                         — one declaration per key",
                        e.key
                    ));
                }
                let class = match e.class.as_deref() {
                    None | Some("string") => ourios_parquet::PromotedClass::String,
                    Some("i64") => ourios_parquet::PromotedClass::I64,
                    Some("f64") => ourios_parquet::PromotedClass::F64,
                    Some(other) => {
                        return Err(format!(
                            "storage.promoted_attributes.{which} key {:?} declares unknown \
                             type {other:?} — expected \"string\", \"i64\", or \"f64\" \
                             (RFC 0042 §3.2)",
                            e.key
                        ));
                    }
                };
                if which == "resource"
                    && e.key == ourios_parquet::SERVICE_NAME_KEY
                    && class != ourios_parquet::PromotedClass::String
                {
                    return Err(format!(
                        "storage.promoted_attributes.resource cannot re-type \
                         {:?}: the implicit promotion is string-class (RFC 0042 §3.2)",
                        ourios_parquet::SERVICE_NAME_KEY
                    ));
                }
                Ok(ourios_parquet::PromotedKey {
                    key: e.key.clone(),
                    class,
                })
            })
            .collect()
    }
    Ok(PromotedAttributes::new_typed(
        family("resource", resource)?,
        family("log", log)?,
    ))
}

/// The installed `SIGTERM` stream (what k8s / `nerdctl stop` send), or
/// `None` on non-Unix targets / install failure — SIGINT (`ctrl_c`) then
/// stays the shutdown path.
///
/// **Install this before any role announces readiness**: registration
/// happens at `signal()` call time, and a SIGTERM arriving before it
/// kills the process by default disposition — a supervisor (or test)
/// that signals on the readiness line would race the handler otherwise
/// (seen twice in CI as `rfc0016_5` exiting on `unix_wait_status(15)`).
#[cfg(unix)]
fn install_terminate_signal() -> Option<tokio::signal::unix::Signal> {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(sigterm) => Some(sigterm),
        Err(e) => {
            tracing::error!(name: ourios_semconv::EVENT_OURIOS_SERVER_SIGNAL_HANDLER_ERROR, "install SIGTERM handler (SIGINT remains the shutdown path): {e}");
            None
        }
    }
}

/// Resolve when `sigterm` fires; pend forever when there is no stream.
async fn terminate_signal(#[cfg(unix)] sigterm: Option<tokio::signal::unix::Signal>) {
    #[cfg(unix)]
    match sigterm {
        Some(mut sigterm) => {
            sigterm.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

/// The pre-readiness startup guards: the RFC 0026 open-mode warning and
/// the SIGTERM registration — both must precede any role announcing
/// readiness (see `install_terminate_signal` for the signal race).
#[cfg(unix)]
fn startup_guards(config: &ServerConfig) -> Option<tokio::signal::unix::Signal> {
    warn_if_open_mode(config);
    warn_if_plaintext_credentials(config);
    install_terminate_signal()
}

#[cfg(not(unix))]
fn startup_guards(config: &ServerConfig) {
    warn_if_open_mode(config);
    warn_if_plaintext_credentials(config);
}

/// One `*_tls` block through the single validation path (RFC 0030
/// §3.1): the raw file leaves into [`TlsSettings::from_parts`], with
/// the block's YAML key as the error prefix.
fn tls_settings(prefix: &str, section: &TlsSection) -> Result<Option<TlsSettings>, String> {
    TlsSettings::from_parts(
        prefix,
        section.cert_file.as_deref(),
        section.key_file.as_deref(),
        section.client_ca_file.as_deref(),
        section.min_version.as_deref(),
        section.reload_interval_secs.as_deref(),
    )
}

/// Fail fast on unusable TLS material (RFC0030.5): every configured
/// `*_tls` block's files are read and built into a `rustls` config at
/// startup, so an unreadable or malformed PEM is a startup error naming
/// the block and the path — not a first-handshake surprise.
fn preflight_tls(config: &ServerConfig) -> Result<(), String> {
    let blocks = [
        (
            "receiver.grpc_tls",
            config.receiver.as_ref().and_then(|r| r.grpc_tls.as_ref()),
        ),
        (
            "receiver.http_tls",
            config.receiver.as_ref().and_then(|r| r.http_tls.as_ref()),
        ),
        (
            "querier.http_tls",
            config.querier.as_ref().and_then(|q| q.http_tls.as_ref()),
        ),
    ];
    for (key, settings) in blocks {
        if let Some(settings) = settings {
            settings.load().map_err(|e| format!("{key}: {e}"))?;
        }
    }
    Ok(())
}

/// RFC 0030 §3.4: credentials over a plaintext listener get one startup
/// warning naming the listener — visible, not fatal (TLS may
/// legitimately terminate at a fronting proxy or mesh).
fn warn_if_plaintext_credentials(config: &ServerConfig) {
    if config.auth.is_none() {
        return;
    }
    let mut plaintext: Vec<&str> = Vec::new();
    if let Some(receiver) = &config.receiver {
        if receiver.grpc_tls.is_none() {
            plaintext.push("receiver.grpc_addr");
        }
        if receiver.http_tls.is_none() {
            plaintext.push("receiver.http_addr");
        }
    }
    if let Some(querier) = &config.querier
        && querier.http_tls.is_none()
    {
        plaintext.push("querier.http_addr");
    }
    for listener in plaintext {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_SERVER_TLS_PLAINTEXT_CREDENTIALS,
            listener,
            "{listener} serves bearer credentials over plaintext (no *_tls \
             block; RFC 0030 §3.4) — acceptable only behind a \
             TLS-terminating proxy or mesh"
        );
    }
}

/// RFC 0026 §3.1 open mode: with no `auth` configured, any client that can
/// reach a listener can write into and read from any tenant. Warn once at
/// startup so the exposure is a visible choice, not a silent default. A
/// compactor-only process binds nothing, so it has nothing to expose.
fn warn_if_open_mode(config: &ServerConfig) {
    if config.auth.is_none() && (config.receiver.is_some() || config.querier.is_some()) {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_SERVER_AUTH_OPEN_MODE,
            "auth is not configured: the network listeners accept unauthenticated \
             requests for any tenant (RFC 0026 open mode)"
        );
    }
}

/// Build a network role's RFC 0026/0029/0047 credential resolver from the
/// resolved auth config. OIDC discovery contacts the issuer once, here at
/// startup — a failure (unreachable issuer, issuer mismatch, unusable
/// JWKS) is a startup error, not a degraded mode (§3.2: with no cached
/// keys nothing could ever verify).
/// `None` when no network role is enabled (nothing to authenticate — and
/// no issuer round-trip on a compactor-only process); otherwise built
/// exactly once and cloned into each role, so OIDC discovery runs once
/// and the roles share the verifier's JWKS cache + refresh throttle.
async fn auth_resolver(
    config: &ServerConfig,
) -> Result<Option<ourios_ingester::receiver::AuthResolver>, String> {
    use ourios_ingester::receiver::AuthResolver;
    if config.receiver.is_none() && config.querier.is_none() {
        return Ok(None);
    }
    let static_store = config
        .auth
        .as_ref()
        .and_then(|auth| auth.static_tokens.clone())
        .map(std::sync::Arc::new);
    let mut resolver = match config.auth.as_ref().and_then(|auth| auth.oidc.clone()) {
        Some(oidc) => {
            let verifier = ourios_core::auth::oidc::OidcVerifier::discover(oidc)
                .await
                .map_err(|e| format!("auth.oidc: {e}"))?;
            AuthResolver::with_oidc(static_store, std::sync::Arc::new(verifier))
        }
        None => AuthResolver::static_only(static_store),
    };
    // RFC 0047 §3.1: the graph resolver binds tenants for whatever the two
    // halves above authenticate. No startup round-trip — OpenFGA is
    // consulted per session, fail-closed, so an unreachable store is a 503
    // at request time rather than a start-up failure of every other role.
    if let Some(openfga) = config.auth.as_ref().and_then(|auth| auth.openfga.as_ref()) {
        // RFC 0048 §3.6: the deadline coupling made visible — the client
        // timeout must stay below OpenFGA's OPENFGA_LIST_OBJECTS_DEADLINE,
        // which this server cannot observe; one line to grep for.
        let visibility = openfga.visibility();
        tracing::info!(
            name: ourios_semconv::EVENT_OURIOS_SERVER_GRAPH_LIST_DEADLINE,
            "graph enumeration timeout: list_timeout_ms {} against a declared \
             server_list_objects_deadline_ms {} — align the latter with the OpenFGA \
             server's OPENFGA_LIST_OBJECTS_DEADLINE (RFC 0048 §3.6)",
            visibility.list_timeout().as_millis(),
            visibility.server_deadline_ms(),
        );
        let openfga = ourios_core::auth::openfga::OpenFgaResolver::new(openfga)?;
        resolver = resolver.with_openfga(std::sync::Arc::new(openfga));
    }
    Ok(Some(resolver))
}

/// RFC 0047 §3.4 / RFC 0048 §3.2: the object column, the self-fast-path
/// column and every **operator-listed** identity column must be in the
/// effective promoted set — the operator hears a typo at startup, not as
/// an empty graph. The defaulted identity lists are exempt: they are the
/// RFC 0047 constants, which never required promotion (the emitter reads
/// record attributes, not the projection).
fn validate_graph_columns(
    openfga: &ourios_core::auth::openfga::OpenFgaConfig,
    promoted: &PromotedAttributes,
) -> Result<(), String> {
    let known: std::collections::BTreeSet<String> = promoted.column_names().collect();
    let visibility = openfga.visibility();
    let check = |what: &str, column: &str| -> Result<(), String> {
        if known.contains(column) {
            Ok(())
        } else {
            Err(format!(
                "auth.openfga.visibility.{what}: `{column}` is not a promoted column — add \
                 it to storage.promoted_attributes (RFC 0048 §3.2)"
            ))
        }
    };
    for object in visibility.objects() {
        check("objects[].column", object.column())?;
    }
    if visibility.user_columns_configured() {
        for column in visibility.user_columns() {
            check("identities.user_columns", column)?;
        }
    }
    if visibility.agent_columns_configured() {
        for column in visibility.agent_columns() {
            check("identities.agent_columns", column)?;
        }
    }
    if let Some(column) = visibility.self_principal_column() {
        check("self_principal_column", column)?;
    }
    Ok(())
}

/// Start the querier role if enabled (RFC 0016), over the same store the
/// receiver writes and the compactor sweeps. Reports the bound address on
/// stdout (an operator — or a test binding `:0` — learns the actual port).
async fn start_querier(
    config: &ServerConfig,
    resolver: Option<ourios_ingester::receiver::AuthResolver>,
) -> Result<Option<ourios_server::querier::QuerierHandle>, String> {
    let Some(params) = &config.querier else {
        return Ok(None);
    };
    let handle = ourios_server::querier::serve(ourios_server::querier::QuerierConfig {
        http_addr: params.http_addr,
        http_tls: params.http_tls.clone(),
        // The querier engine is Store-capable (RFC 0019 slice 2a), so it
        // reads whichever backend config resolved (local or S3).
        store: config.store.clone(),
        auth: resolver.expect("resolver built for enabled roles"),
        default_window_nanos: params.default_window_nanos,
        mcp_enabled: params.mcp_enabled,
        // The same effective set the write paths apply — the MCP
        // query-schema resource publishes it (RFC 0032 §3.1).
        promoted: config.promoted.clone(),
    })
    .await?;
    println!("querier HTTP listening on {}", handle.http_addr);
    std::io::stdout().flush().ok();
    Ok(Some(handle))
}

/// Resolve the configuration (file or env, RFC 0020 §3.2) and pre-create
/// a local store root (`Store::local` canonicalises it and errors on a
/// missing dir; an S3 backend needs no such step — mirrors the querier
/// role's `serve()`).
fn resolve_config(config_path: Option<&Path>) -> Result<ServerConfig, String> {
    let config = match config_path {
        Some(path) => config_from_file(path)?,
        None => config_from_env()?,
    };
    if let StoreConfig::Local(root) = &config.store {
        std::fs::create_dir_all(root)
            .map_err(|e| format!("create store root {}: {e}", root.display()))?;
    }
    preflight_tls(&config)?;
    Ok(config)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // `--config <path>` selects the RFC 0020 file front-end; without it the
    // env-only path runs unchanged (§3.2). Both resolve the same `ServerConfig`.
    let cli = Cli::parse();
    if run_operator_verb(&cli).await? {
        return Ok(());
    }
    let config = resolve_config(cli.config.as_deref())?;

    // Preflight the data store *before* binding any network role, so a
    // store-open failure early-returns here rather than after the
    // receiver/querier handles are live — which would bypass their graceful
    // shutdown. `open` only validates local-root existence / backend config; an
    // S3 backend doesn't contact the endpoint here (credentials and connectivity
    // resolve on first request, surfacing later). This opened handle is cloned
    // into the receiver and moved into `Compactor::new` below (both write/sweep
    // the same store); the querier opens its own handle from the same
    // `StoreConfig` in `querier::serve`.
    let store = config.store.open()?;

    // Boot OpenTelemetry first so the compactor's instruments export
    // (RFC 0001 §6.8). The guard flushes pending metrics on shutdown. All of
    // the operator knobs are the universal OTel env vars, honored by the SDK or
    // (for the exporter selectors + `OTEL_SDK_DISABLED`) by `ourios_telemetry::init`
    // itself — `OTEL_EXPORTER_OTLP_*` (endpoint/transport), `OTEL_TRACES_SAMPLER`,
    // `OTEL_METRIC_EXPORT_INTERVAL`, and `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none`
    // — so there is no bespoke Ourios telemetry config to map here.
    let telemetry = ourios_telemetry::init(&TelemetryConfig::new("ourios-server"))?;

    #[cfg(unix)]
    let sigterm = startup_guards(&config);
    #[cfg(not(unix))]
    startup_guards(&config);

    // Start the OTLP receiver role if enabled (RFC 0003 §9). Report the
    // bound addresses on stdout so an operator — or a test binding `:0` —
    // learns the actual ports.
    // One resolver, built once, shared by every enabled network role.
    let resolver = auth_resolver(&config).await?;
    // RFC 0047 §3.3: the graph emitter — built for every role that stores or
    // rewrites rows (receiver flush cadence, compaction sweep) when the graph
    // binds a conversation object; no startup round-trip.
    let graph_emitter = match config.auth.as_ref().and_then(|auth| auth.openfga.as_ref()) {
        Some(openfga) => {
            validate_graph_columns(openfga, &config.promoted)?;
            ourios_ingester::graph_emitter::GraphEmitter::from_config(openfga)?
                .map(std::sync::Arc::new)
        }
        None => None,
    };

    let receiver = match &config.receiver {
        // The receiver's RFC 0014 data write path runs on the resolved store
        // (local or S3, RFC 0019 slice 2c) — the same store the querier reads
        // and the compactor sweeps. The WAL stays local regardless (§3.6).
        Some(params) => {
            let handle = receiver::serve(receiver::ReceiverConfig {
                grpc_addr: params.grpc_addr,
                grpc_tls: params.grpc_tls.clone(),
                http_addr: params.http_addr,
                http_tls: params.http_tls.clone(),
                wal: wal_config(&params.wal_root),
                // The data store the receiver's RFC 0014 write path lands
                // Parquet in — the same store the compactor sweeps (cloned; the
                // handle is cheap to share, the compactor keeps the original).
                store: store.clone(),
                promoted: config.promoted.clone(),
                auth: resolver.clone().expect("resolver built for enabled roles"),
                encode_workers: params.encode_workers,
                miner: params.miner,
                graph_emitter: graph_emitter.clone(),
            })
            .await?;
            println!("receiver gRPC listening on {}", handle.grpc_addr);
            println!("receiver HTTP listening on {}", handle.http_addr);
            std::io::stdout().flush().ok();
            Some(handle)
        }
        None => None,
    };

    let querier = start_querier(&config, resolver.clone()).await?;

    // The compactor sweeps the resolved store (local or S3, RFC 0019 slice 2b),
    // opened in the preflight above so a store failure never leaks a live role,
    // and writes durable compaction audit events through the same `Store` via
    // the `ParquetAuditSink` (RFC 0009 §3.6 → RFC 0005 §3.7, slice 2d). Built
    // only when compaction is enabled (RFC 0009 §3.2) — a deployment disables it
    // on receiver/querier pods so a single dedicated compactor sweeps. When
    // disabled, neither the store clone nor the audit sink is constructed, and
    // the disabled state is logged so it's visible in a multi-pod rollout.
    let compactor = if config.compaction_enabled {
        let audit_store = store.clone();
        let mut compactor = Compactor::new(
            store,
            CompactionPolicy::default(),
            config.compaction_interval,
        )
        .with_promoted_attributes(config.promoted.clone())
        .with_audit_sink(Box::new(ParquetAuditSink::new(audit_store)));
        if let Some(emitter) = graph_emitter.clone() {
            compactor = compactor.with_graph_emitter(emitter);
        }
        Some(compactor)
    } else {
        tracing::info!(name: ourios_semconv::EVENT_OURIOS_SERVER_COMPACTION_DISABLED, "compaction disabled for this process (OURIOS_COMPACTION_ENABLED)");
        None
    };

    // Run until SIGINT or SIGTERM (k8s / `nerdctl stop` send SIGTERM). The
    // compaction loop never returns on its own (it sweeps forever, or just
    // pends when disabled), so the select resolves on a signal (or a SIGINT
    // setup failure).
    let compaction = async {
        match compactor {
            Some(c) => {
                c.run(|result| match result {
                    Ok(report) => {
                        for err in &report.errors {
                            tracing::error!(name: ourios_semconv::EVENT_OURIOS_COMPACTION_SWEEP_ERROR, "compaction sweep error: {err}");
                        }
                    }
                    Err(e) => tracing::error!(name: ourios_semconv::EVENT_OURIOS_COMPACTION_SWEEP_ERROR, "compaction sweep failed: {e}"),
                })
                .await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let shutdown = tokio::select! {
        () = compaction => Ok(()),
        signal = tokio::signal::ctrl_c() => signal,
        () = terminate_signal(
            #[cfg(unix)]
            sigterm,
        ) => Ok(()),
    };

    // Drain the listeners gracefully (the receiver release frees the single
    // `Wal`) before flushing telemetry and exiting.
    if let Some(handle) = querier
        && let Err(e) = handle.shutdown().await
    {
        tracing::error!(name: ourios_semconv::EVENT_OURIOS_QUERIER_SHUTDOWN_ERROR, "querier shutdown error: {e}");
    }
    if let Some(handle) = receiver
        && let Err(e) = handle.shutdown().await
    {
        tracing::error!(name: ourios_semconv::EVENT_OURIOS_RECEIVER_SHUTDOWN_ERROR, "receiver shutdown error: {e}");
    }

    // Flush pending telemetry on the way out (best-effort: a failed final
    // export — e.g. the metrics collector is unreachable at shutdown —
    // must not turn an otherwise-clean shutdown into a non-zero exit).
    // eprintln!, not tracing: the log pipeline this tears down is the one
    // a tracing event would need, so stderr is the only channel left.
    if let Err(e) = telemetry.shutdown() {
        eprintln!("telemetry shutdown error: {e}");
    }

    // A SIGINT (`ctrl_c`) handler setup failure is fatal: cancelling the
    // compactor and exiting 0 would leave the server silently doing no
    // work. (A SIGTERM-handler failure is non-fatal — see
    // `terminate_signal` — leaving SIGINT in charge.)
    shutdown?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ourios_server::config::file::parse;

    /// RFC 0048 §3.3 — the CLI callee span's contract: named after the
    /// executable, `INTERNAL`, `process.*` present, a zero exit code on
    /// success, and **no** `process.command_args` (the convention says not
    /// to collect it without sanitisation). Current-thread runtime: the
    /// span closes after an `.await`, which a thread-local subscriber only
    /// sees deterministically when the task cannot migrate. (Bare
    /// `#[tokio::test]` is already current-thread — this spells the
    /// requirement out so a future edit cannot silently relax it.)
    #[tokio::test(flavor = "current_thread")]
    async fn cli_span_carries_the_semconv_contract_on_success() {
        let (spans, config_path, _tmp) =
            run_traced_verb(&GraphVerb::Erasures { tenant: None }).await;
        let span = spans.first().expect("one CLI span");
        assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Internal);
        let attrs = span_attrs(span);
        // The convention: the span name **is** the executable name. Under
        // `cargo test` that is the harness binary, which is exactly why the
        // assertion compares the two rather than a literal — a hard-coded
        // macro name would silently drift from the attribute.
        let executable = attrs
            .get("process.executable.name")
            .expect("required attribute");
        assert_eq!(&span.name, executable);
        assert!(
            executable.contains("ourios"),
            "derived from the running binary: {executable}"
        );
        assert_eq!(
            attrs.get("process.exit.code").map(String::as_str),
            Some("0")
        );
        assert!(attrs.contains_key("process.pid"));
        assert!(
            !attrs.contains_key("process.command_args"),
            "arguments carry tenant ids and are never collected: {attrs:?}"
        );
        assert!(!attrs.contains_key("error.type"), "{attrs:?}");
        drop(config_path);
    }

    /// The failure arm: a non-zero exit code **and** `error.type`, which the
    /// convention makes conditionally required exactly then. Current-thread
    /// for the same reason as the success case above.
    #[tokio::test(flavor = "current_thread")]
    async fn cli_span_records_the_failure_arm() {
        // `backfill` without an `auth.openfga` section fails after the
        // config resolves — inside the span, where the contract applies.
        let verb = GraphVerb::Backfill {
            tenant: "acme".to_string(),
            from: None,
            unlock: false,
        };
        let (spans, _config, _tmp) = run_traced_verb(&verb).await;
        let span = spans.first().expect("one CLI span");
        let attrs = span_attrs(span);
        assert_eq!(
            attrs.get("process.exit.code").map(String::as_str),
            Some("1")
        );
        assert_eq!(attrs.get("error.type").map(String::as_str), Some("_OTHER"));
        assert_eq!(
            span.status,
            opentelemetry::trace::Status::error(""),
            "a non-zero exit is an error span"
        );
    }

    /// Run one verb under an in-memory tracer and return the exported spans
    /// (plus the temp dir, which must outlive the run).
    async fn run_traced_verb(
        verb: &GraphVerb,
    ) -> (
        Vec<opentelemetry_sdk::trace::SpanData>,
        PathBuf,
        tempfile::TempDir,
    ) {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::TempDir::new().expect("temp");
        let config_path = tmp.path().join("ourios.yaml");
        std::fs::write(
            &config_path,
            format!(
                "storage:\n  local:\n    bucket_root: {}\n",
                tmp.path().display()
            ),
        )
        .expect("write config");

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));
        let guard = tracing::subscriber::set_default(subscriber);
        let _ = run_graph_verb_traced(verb, Some(config_path.as_path())).await;
        drop(guard);
        provider.force_flush().expect("spans flush");
        let spans = exporter.get_finished_spans().expect("spans exported");
        (spans, config_path, tmp)
    }

    /// A span's attributes as `key -> rendered value`.
    fn span_attrs(
        span: &opentelemetry_sdk::trace::SpanData,
    ) -> std::collections::HashMap<String, String> {
        span.attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect()
    }

    /// RFC0048.3 — the promoted-set check is per identity list: a partial
    /// override checks only the listed side; the other side's defaults are
    /// exempt even when unpromoted.
    #[test]
    fn graph_column_check_is_per_identity_list() {
        use ourios_core::auth::openfga::{
            IdentitiesSpec, OpenFgaSpec, VisibilityObjectSpec, VisibilitySpec, build_openfga_config,
        };
        // Promoted: the conversation column + the custom identity column —
        // neither default user column, nor the default agent column.
        let promoted = PromotedAttributes::new(
            vec![],
            vec!["gen_ai.conversation.id".to_string(), "bot.name".to_string()],
        );
        let openfga = |identities: IdentitiesSpec| {
            build_openfga_config(&OpenFgaSpec {
                api_url: Some("http://openfga.invalid:8080".to_string()),
                store_id: Some("s".to_string()),
                visibility: VisibilitySpec {
                    objects: vec![VisibilityObjectSpec {
                        object_type: Some("conversation".to_string()),
                        column: Some("attr.gen_ai.conversation.id".to_string()),
                    }],
                    identities,
                    ..VisibilitySpec::default()
                },
                ..OpenFgaSpec::default()
            })
            .expect("config")
        };
        let agent_only = openfga(IdentitiesSpec {
            user_columns: None,
            agent_columns: Some(vec!["attr.bot.name".to_string()]),
        });
        validate_graph_columns(&agent_only, &promoted).expect("defaulted user columns are exempt");
        let user_only = openfga(IdentitiesSpec {
            user_columns: Some(vec!["attr.bot.name".to_string()]),
            agent_columns: None,
        });
        validate_graph_columns(&user_only, &promoted).expect("defaulted agent columns are exempt");
        let bad = openfga(IdentitiesSpec {
            user_columns: None,
            agent_columns: Some(vec!["attr.absent.key".to_string()]),
        });
        let err = validate_graph_columns(&bad, &promoted).expect_err("listed must be promoted");
        assert!(err.contains("identities.agent_columns"), "{err}");
        assert!(err.contains("attr.absent.key"), "{err}");
    }

    /// A `local` [`StoreConfig`] for `path`, the common test fixture.
    fn local(path: &str) -> StoreConfig {
        StoreConfig::Local(PathBuf::from(path))
    }

    /// Parse `yaml` with an empty environment, then map it onto a `ServerConfig`
    /// through the shared `build_*` validators (RFC 0020 §3.1).
    fn server_config(yaml: &str) -> Result<ServerConfig, String> {
        let file = parse(yaml, &|_| None).expect("well-formed file");
        server_config_from_file(&file)
    }

    /// Scenario RFC0020.1 — a complete file resolves to the same `ServerConfig`
    /// the equivalent `OURIOS_*` environment would produce, field for field.
    /// See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_1_file_resolves_to_the_same_config_as_the_env() {
        let from_file = server_config(
            "\
storage:
  backend: s3
  s3:
    bucket: my-logs
receiver:
  enabled: true
  wal_root: /var/lib/ourios/wal
querier:
  enabled: true
compaction:
  interval_secs: 120
",
        )
        .expect("valid");

        // The same values expressed through the env-path helpers (the shared
        // validators), as `config_from_env` would assemble them.
        let store = with_s3_credentials(
            build_store_config(Some("s3"), None, Some("my-logs"), None, None, None).expect("s3"),
            None,
            None,
            None,
        );
        let mut expected = build_config(store, None, Some("120")).expect("valid");
        expected.receiver = build_receiver_config(
            Some("true"),
            None,
            None,
            Some(PathBuf::from("/var/lib/ourios/wal")),
            None,
        )
        .expect("receiver");
        expected.querier = build_querier_config(Some("true"), None, None, None).expect("querier");

        assert_eq!(from_file, expected);
    }

    /// Scenario RFC0020.3 — the file is authoritative; a bare `OURIOS_*` env var
    /// does not override a file value (only `${env:…}` refs inside the file
    /// consult the environment). See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_3_file_value_is_authoritative_over_bare_env() {
        let yaml = "\
storage:
  local:
    bucket_root: /store
querier:
  enabled: true
  default_window_secs: 1800
";
        // The lookup "sets" the bare env knob to 3600, but the file has no
        // `${env:…}` reference to it, so it is never consulted.
        let file = parse(yaml, &|name| {
            (name == "OURIOS_QUERIER_DEFAULT_WINDOW_SECS").then(|| "3600".to_owned())
        })
        .expect("valid");
        let config = server_config_from_file(&file).expect("valid");

        assert_eq!(
            config.querier.expect("enabled").default_window_nanos,
            1800 * NANOS_PER_SEC,
            "the file value wins; the bare env var is ignored",
        );
    }

    /// RFC 0050 §3.2 — the `miner.*` section resolves onto the
    /// receiver's `MinerConfig`, leaving every non-dial tunable at
    /// its code default.
    #[test]
    fn rfc0050_miner_section_resolves_the_dial() {
        let config = server_config(
            "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
miner:
  upstream_templates: adopt
  upstream_template_byte_limit: 4096
  upstream_association_limit: 2
",
        )
        .expect("valid");
        let receiver = config.receiver.expect("enabled");
        assert_eq!(receiver.miner.upstream_templates, UpstreamTemplates::Adopt);
        assert_eq!(receiver.miner.upstream_template_byte_limit, 4096);
        assert_eq!(receiver.miner.upstream_association_limit, 2);
        assert_eq!(
            receiver.miner.max_templates,
            MinerConfig::default().max_templates,
            "non-dial tunables stay at code defaults",
        );
    }

    /// RFC 0050 §3.2 — an absent `miner.*` section is the unchanged
    /// default: `ignore` mode, byte-identical pre-RFC behaviour.
    #[test]
    fn rfc0050_miner_section_defaults_to_ignore() {
        let config = server_config(
            "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
",
        )
        .expect("valid");
        assert_eq!(
            config.receiver.expect("enabled").miner,
            MinerConfig::default(),
        );
    }

    /// RFC 0050 §3.2 — an unknown mode fails startup loudly, naming
    /// the YAML key (RFC 0001 §3.2.2: refuse to serve rather than
    /// degrade silently).
    #[test]
    fn rfc0050_miner_mode_rejects_unknown_value() {
        let err = server_config(
            "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
miner:
  upstream_templates: maybe
",
        )
        .expect_err("unknown mode must fail");
        assert!(err.contains("miner.upstream_templates"), "{err}");
    }

    /// RFC 0020 §3.3 — the miner dial rides `${env:…}` substitution
    /// like every other scalar leaf.
    #[test]
    fn rfc0050_miner_mode_rides_env_substitution() {
        let yaml = "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
miner:
  upstream_templates: ${env:OURIOS_TEST_MODE}
";
        let file = parse(yaml, &|name| {
            (name == "OURIOS_TEST_MODE").then(|| "observe".to_owned())
        })
        .expect("valid");
        let config = server_config_from_file(&file).expect("valid");
        assert_eq!(
            config.receiver.expect("enabled").miner.upstream_templates,
            UpstreamTemplates::Observe,
        );
    }

    /// Scenario RFC0020.4 — no `--config` selects the env-only path unchanged;
    /// the `--config` CLI contract is enforced by `clap`.
    /// See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_4_no_config_flag_selects_the_env_path() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|cli| cli.config);

        // No flag → None → `config_from_env` runs (its behaviour is unchanged,
        // guarded by the `build_*`/`config_from_env` suites).
        assert_eq!(parse(&["ourios-server"]).expect("ok"), None);
        // `--config <path>` and `--config=<path>` both select the file.
        assert_eq!(
            parse(&["ourios-server", "--config", "/c.yaml"]).expect("ok"),
            Some(PathBuf::from("/c.yaml")),
        );
        assert_eq!(
            parse(&["ourios-server", "--config=/c.yaml"]).expect("ok"),
            Some(PathBuf::from("/c.yaml")),
        );
        // A dangling `--config`, an empty path, a trailing extra argument, and an
        // unknown argument are all rejected (clap enforces the CLI contract).
        assert!(parse(&["ourios-server", "--config"]).is_err());
        assert!(parse(&["ourios-server", "--config="]).is_err());
        assert!(parse(&["ourios-server", "--config", "/c.yaml", "--extra"]).is_err());
        assert!(parse(&["ourios-server", "--config=/c.yaml", "x"]).is_err());
        assert!(parse(&["ourios-server", "--nope"]).is_err());
    }

    /// Scenario RFC0020.5 (value arm) — a well-formed file whose *value* the
    /// shared validators reject fails fast, through the same rule the env path
    /// enforces; no partial config is produced. (The malformed-reference and
    /// unknown-key arms are covered in `config::file`.)
    /// See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_5_invalid_file_value_fails_fast() {
        // `s3` backend with no bucket — the same validation as the env path.
        let err = server_config("storage:\n  backend: s3\n").expect_err("s3 needs a bucket");
        assert!(
            err.contains("S3_BUCKET"),
            "names the missing bucket: {err:?}"
        );

        // A non-numeric querier window is rejected.
        let err = server_config(
            "\
storage:
  local:
    bucket_root: /store
querier:
  enabled: true
  default_window_secs: soon
",
        )
        .expect_err("bad window");
        assert!(
            err.contains("DEFAULT_WINDOW_SECS"),
            "names the offending field: {err:?}",
        );
    }

    /// Scenario RFC0020.6 — secret hygiene across the file path. A resolved
    /// credential is present in the `FileConfig`, yet a sibling value that the
    /// mapping rejects produces an error naming the offending key only — never
    /// the resolved secret (extends RFC 0019 §3.4 / RFC0019.6 to the file path;
    /// the `${env:…}`-only credential rule and `Debug` redaction are covered in
    /// `config::file`). See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_6_secret_hygiene_across_the_file_path() {
        let secret = "topsecret-access-key";
        // The credentials are `${env:…}` references (§3.5); they resolve to real
        // values, but the backend is `s3` with no bucket — a sibling error.
        let file = parse(
            "\
storage:
  backend: s3
  s3:
    access_key_id: ${env:KEY}
    secret_access_key: ${env:SECRET}
",
            &|name| match name {
                "KEY" => Some("AKIAEXAMPLE".to_owned()),
                "SECRET" => Some(secret.to_owned()),
                _ => None,
            },
        )
        .expect("parses (credentials are references)");

        // The secret is resolved and present in the config...
        assert_eq!(file.storage.s3.secret_access_key.as_deref(), Some(secret));

        // ...but the mapping fails on the missing bucket, and the error names the
        // offending key, never the resolved secret.
        let err = server_config_from_file(&file).expect_err("s3 needs a bucket");
        assert!(err.contains("S3_BUCKET"), "names the missing key: {err}");
        assert!(
            !err.contains(secret),
            "the resolved secret must not leak: {err}"
        );
    }

    /// Scenario RFC0026.1 (mapping) — the file's `auth` section resolves
    /// through the shared validators like every other section: a token that
    /// arrived via `${env:…}` substitution authenticates in the resolved
    /// store, an absent section resolves open (`None`), and an empty token
    /// list fails the mapping. The schema/substitution/redaction arms live in
    /// `config::file`, the store validation matrix in `ourios_server::auth`,
    /// and the startup-observable arms in `tests/rfc0026_auth.rs`.
    /// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
    #[test]
    fn rfc0026_1_auth_section_maps_onto_the_token_store() {
        let yaml = "\
storage:
  backend: local
  local:
    bucket_root: /var/lib/ourios
auth:
  tokens:
    - name: edge-collector
      token: ${env:TOK}
      tenants: [acme]
";
        let file = parse(yaml, &|name| {
            (name == "TOK").then(|| "resolved-token".to_owned())
        })
        .expect("well-formed file");
        let config = server_config_from_file(&file).expect("valid");
        let store = config
            .auth
            .expect("auth resolved")
            .static_tokens
            .expect("static half");
        assert_eq!(
            store.authenticate("resolved-token").expect("match").name(),
            "edge-collector",
        );

        let open = server_config("storage:\n  local:\n    bucket_root: /x\n").expect("valid");
        assert!(open.auth.is_none(), "no auth section resolves open");

        let err = server_config("storage:\n  local:\n    bucket_root: /x\nauth:\n  tokens: []\n")
            .expect_err("empty token list");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// Scenario RFC0029.1 (mapping) — the file's `auth.oidc` section resolves
    /// through the shared validators: an `${env:…}`-substituted issuer lands
    /// in the resolved config, an oidc-only section resolves with an *empty*
    /// enforcement store (enforced, not open), a missing `audience` fails,
    /// and `tokens: []` fails even with `oidc` present. The schema arms live
    /// in `config::file`, the validation matrix in `ourios_core::auth`, and
    /// the startup-observable arms in `tests/it/rfc0029_oidc.rs`.
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[test]
    fn rfc0029_1_oidc_section_maps_onto_the_auth_config() {
        let yaml = "\
storage:
  backend: local
  local:
    bucket_root: /var/lib/ourios
auth:
  oidc:
    issuer: ${env:ISSUER}
    audience: ourios
    tenant_claim: ourios_tenants
";
        let file = parse(yaml, &|name| {
            (name == "ISSUER").then(|| "https://dex.internal.example".to_owned())
        })
        .expect("well-formed file");
        let config = server_config_from_file(&file).expect("valid");
        let auth = config.auth.expect("auth resolved");
        let oidc = auth.oidc.as_ref().expect("oidc half");
        assert_eq!(oidc.issuer(), "https://dex.internal.example");
        assert_eq!(oidc.name_claim(), "sub", "core default applied");
        assert!(auth.static_tokens.is_none(), "no static half");
        // Enforced-not-open for oidc-only is a resolver/serving property
        // now (the `enforcement_store` bridge is retired): the served
        // `rfc0029_oidc` arms assert the wire-level 401.
        assert!(auth.oidc.is_some() && auth.static_tokens.is_none());

        let err = server_config(
            "storage:\n  local:\n    bucket_root: /x\nauth:\n  oidc:\n    issuer: https://x\n    tenant_claim: t\n",
        )
        .expect_err("missing audience");
        assert!(err.contains("auth.oidc.audience"), "names the key: {err}");

        let err = server_config(
            "storage:\n  local:\n    bucket_root: /x\nauth:\n  tokens: []\n  oidc:\n    issuer: https://x\n    audience: a\n    tenant_claim: t\n",
        )
        .expect_err("explicit empty list stays an error with oidc present");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// RFC 0027 §3.1 — the MCP flag is opt-in: absent/falsey values leave
    /// it off, the role-standard truthy values enable it, on both the env
    /// and file paths (the same `build_querier_config`).
    #[test]
    fn querier_mcp_flag_defaults_off_and_accepts_truthy() {
        for off in [None, Some("0"), Some("false"), Some("off"), Some("")] {
            let params = build_querier_config(Some("1"), None, None, off)
                .expect("valid")
                .expect("enabled");
            assert!(!params.mcp_enabled, "{off:?} leaves MCP off");
        }
        for on in ["1", "true", "yes"] {
            let params = build_querier_config(Some("1"), None, None, Some(on))
                .expect("valid")
                .expect("enabled");
            assert!(params.mcp_enabled, "{on:?} enables MCP");
        }
    }

    /// `config_from_file` end-to-end through the real filesystem: a valid file
    /// reads and resolves, and both failure paths name the offending file — a
    /// missing file via the read-error prefix, a parse failure via the
    /// config-file prefix (RFC0020.1 read path / RFC0020.5 error reporting).
    #[test]
    fn config_from_file_reads_maps_and_names_the_path() {
        use std::io::Write as _;

        // Happy path: a self-contained file (no `${env:…}` refs) reads and maps.
        let mut good = tempfile::NamedTempFile::new().expect("temp file");
        write!(
            good,
            "storage:\n  local:\n    bucket_root: /store\nquerier:\n  enabled: true\n"
        )
        .expect("write");
        let config = config_from_file(good.path()).expect("valid file resolves");
        assert_eq!(config.store, local("/store"));
        assert!(config.querier.is_some(), "the querier role is enabled");

        // A missing file is reported with the read-error prefix and the path.
        let missing = Path::new("/no/such/ourios-config.yaml");
        let err = config_from_file(missing).expect_err("missing file");
        assert!(err.contains("read config file"), "read-error prefix: {err}");
        assert!(err.contains("ourios-config.yaml"), "names the path: {err}");

        // A parse failure is reported with the config-file prefix, the path, and
        // the offending reference — never a resolved value.
        let mut bad = tempfile::NamedTempFile::new().expect("temp file");
        write!(bad, "storage:\n  backend: ${{1BAD}}\n").expect("write");
        let err = config_from_file(bad.path()).expect_err("malformed reference");
        assert!(err.contains("config file"), "config-file prefix: {err}");
        assert!(err.contains("${1BAD}"), "names the reference: {err}");
        assert!(
            err.contains(&bad.path().display().to_string()),
            "names the path: {err}",
        );
    }

    #[test]
    fn build_config_defaults_the_interval() {
        // Arrange / Act
        let config = build_config(local("/store"), None, None).expect("valid");

        // Assert
        assert_eq!(
            config.compaction_interval,
            Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS),
        );
        assert_eq!(config.store, local("/store"));
    }

    /// RFC 0022 §3.2 — `storage.promoted_attributes` resolves onto the
    /// `ServerConfig` through the shared validator: configured keys land in
    /// the effective set (the implicit `service.name` and dedup are the
    /// `PromotedAttributes` contract), an omitted section is the default
    /// (`service.name`-only) set, and an empty key is a config error.
    #[test]
    fn promoted_attributes_resolve_onto_the_server_config() {
        let config = server_config(
            "storage:\n  local:\n    bucket_root: /store\n  promoted_attributes:\n    resource: [k8s.namespace.name]\n    log: [http.route]\n",
        )
        .expect("valid");
        assert_eq!(
            config.promoted,
            PromotedAttributes::new(
                ["k8s.namespace.name".to_string()],
                ["http.route".to_string()],
            ),
        );

        let defaulted =
            server_config("storage:\n  local:\n    bucket_root: /store\n").expect("valid");
        assert_eq!(defaulted.promoted, PromotedAttributes::default());

        let bare = |key: &str| PromotedEntry {
            key: key.to_string(),
            class: None,
        };
        let typed = |key: &str, class: &str| PromotedEntry {
            key: key.to_string(),
            class: Some(class.to_string()),
        };

        let err = build_promoted_attributes(&[bare("k8s.namespace.name")], &[bare("")])
            .expect_err("empty key");
        assert!(err.contains("non-empty"), "the error names the rule: {err}");
        // Surrounding whitespace would mint a promoted column whose name can
        // never match the intended attribute key — rejected, not normalised.
        build_promoted_attributes(&[bare(" k8s.namespace.name")], &[])
            .expect_err("whitespace-padded key");
        build_promoted_attributes(&[], &[bare("http.route ")])
            .expect_err("trailing-whitespace key");

        // RFC0042.6 — typed entries build the classed set; `string` is the
        // bare spelling's explicit form.
        let set = build_promoted_attributes(
            &[],
            &[
                bare("model"),
                typed("cost_usd", "f64"),
                typed("input_tokens", "i64"),
                typed("decision", "string"),
            ],
        )
        .expect("typed set");
        assert_eq!(
            set.log_keys()
                .iter()
                .map(|k| (k.key.as_str(), k.class))
                .collect::<Vec<_>>(),
            [
                ("model", ourios_parquet::PromotedClass::String),
                ("cost_usd", ourios_parquet::PromotedClass::F64),
                ("input_tokens", ourios_parquet::PromotedClass::I64),
                ("decision", ourios_parquet::PromotedClass::String),
            ]
        );

        // RFC0042.6 — the three loud startup offences, each error naming
        // its offence.
        let err = build_promoted_attributes(&[], &[typed("cost_usd", "float")])
            .expect_err("unknown type");
        assert!(
            err.contains("unknown") && err.contains("float") && err.contains("cost_usd"),
            "names the token and the key: {err}"
        );
        let err = build_promoted_attributes(&[], &[bare("cost_usd"), typed("cost_usd", "f64")])
            .expect_err("duplicate across spellings");
        assert!(
            err.contains("more than once") && err.contains("cost_usd"),
            "names the duplicated key: {err}"
        );
        let err = build_promoted_attributes(&[typed("service.name", "i64")], &[])
            .expect_err("re-typed service.name");
        assert!(
            err.contains("service.name") && err.contains("string-class"),
            "names the implicit promotion rule: {err}"
        );
        // A string-class service.name declaration is redundant but legal.
        build_promoted_attributes(&[typed("service.name", "string")], &[])
            .expect("explicit string service.name collapses");
        // service.name under `log` is an ordinary key (the implicit
        // promotion is the *resource* family), so an i64 there is legal.
        build_promoted_attributes(&[], &[typed("service.name", "i64")])
            .expect("log-family service.name is not the implicit promotion");
    }

    #[test]
    fn build_config_parses_a_custom_interval() {
        // Arrange / Act
        let config = build_config(local("/store"), None, Some("60")).expect("valid");

        // Assert
        assert_eq!(config.compaction_interval, Duration::from_secs(60));
    }

    #[test]
    fn build_config_rejects_a_zero_or_nonnumeric_interval() {
        // Arrange / Act / Assert
        assert!(
            build_config(local("/store"), None, Some("0")).is_err(),
            "a zero interval would busy-loop the daemon",
        );
        assert!(
            build_config(local("/store"), None, Some("soon")).is_err(),
            "non-numeric interval is rejected",
        );
    }

    #[test]
    fn build_config_compaction_is_opt_out() {
        // Default (unset) and any non-falsey value keep compaction on; only an
        // explicit falsey token (trimmed) turns it off — the inverse of the
        // opt-in receiver/querier roles.
        for raw in [None, Some("1"), Some("true"), Some("yes"), Some("anything")] {
            assert!(
                build_config(local("/store"), raw, None)
                    .expect("valid")
                    .compaction_enabled,
                "compaction stays on for {raw:?}",
            );
        }
        for raw in [
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
            Some("  off  "),
        ] {
            assert!(
                !build_config(local("/store"), raw, None)
                    .expect("valid")
                    .compaction_enabled,
                "compaction is disabled for {raw:?}",
            );
        }
    }

    #[test]
    fn build_config_disabled_compaction_ignores_a_bad_interval() {
        // With compaction off, the interval is never used, so an otherwise-
        // rejected value must not block startup.
        for bad in [Some("0"), Some("soon")] {
            assert!(
                build_config(local("/store"), Some("off"), bad).is_ok(),
                "a disabled pod starts despite interval {bad:?}",
            );
        }
    }

    /// Scenario RFC0019.1 — backend selection from config.
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    #[test]
    fn rfc0019_1_backend_selection_from_config() {
        // Unset backend + a bucket root → local.
        assert_eq!(
            build_store_config(None, Some(PathBuf::from("/store")), None, None, None, None)
                .expect("local default"),
            local("/store"),
        );
        // Explicit `local` behaves the same.
        assert_eq!(
            build_store_config(
                Some("local"),
                Some(PathBuf::from("/store")),
                None,
                None,
                None,
                None
            )
            .expect("explicit local"),
            local("/store"),
        );
        // `s3` + a bucket (and optional addressing) → an S3 backend.
        let s3 = build_store_config(
            Some("s3"),
            None,
            Some("my-bucket"),
            Some("http://localhost:4566"),
            Some("us-east-1"),
            Some("ourios"),
        )
        .expect("s3 selected");
        assert_eq!(
            s3,
            StoreConfig::S3(
                S3Config::new("my-bucket")
                    .with_endpoint("http://localhost:4566")
                    .with_region("us-east-1")
                    .with_prefix("ourios"),
            ),
        );
        // `s3` without a bucket, and an unknown backend, both fail fast.
        assert!(
            build_store_config(Some("s3"), None, None, None, None, None).is_err(),
            "s3 backend requires OURIOS_S3_BUCKET",
        );
        assert!(
            build_store_config(
                Some("gcs"),
                Some(PathBuf::from("/store")),
                None,
                None,
                None,
                None
            )
            .is_err(),
            "an unknown backend is rejected",
        );
        // Local backend with no bucket root is rejected — "must be set" for an
        // unset var, distinct from "must not be empty" for a present-but-empty
        // one (clearer operator diagnostics).
        let unset = build_store_config(None, None, None, None, None, None).expect_err("unset");
        assert!(
            unset.contains("must be set"),
            "unset names the missing key, got {unset:?}",
        );
        let empty = build_store_config(
            Some("local"),
            Some(PathBuf::from("")),
            None,
            None,
            None,
            None,
        )
        .expect_err("empty");
        assert!(
            empty.contains("must not be empty"),
            "an empty bucket root is reported distinctly, got {empty:?}",
        );
        // The backend value is trimmed; a blank value is treated as unset
        // (→ local), not as an unknown backend.
        assert_eq!(
            build_store_config(Some("  s3  "), None, Some("b"), None, None, None)
                .expect("trimmed s3"),
            StoreConfig::S3(S3Config::new("b")),
        );
        assert_eq!(
            build_store_config(
                Some("   "),
                Some(PathBuf::from("/store")),
                None,
                None,
                None,
                None
            )
            .expect("blank backend → local"),
            local("/store"),
        );
    }

    /// Scenario RFC0019.6 — config governed by RFC 0004; no secret leakage.
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    #[test]
    fn rfc0019_6_config_governed_no_secret_leakage() {
        // A missing S3 bucket names only the *key*, never a value, and config
        // resolution never reads credentials (those come from the AWS chain in
        // `StoreConfig::open`), so no secret can appear in an error.
        let err = build_store_config(Some("s3"), None, None, None, None, None)
            .expect_err("missing bucket");
        assert!(
            err.contains("OURIOS_S3_BUCKET"),
            "the error names the missing key, got {err:?}",
        );
        // The credential env vars are never echoed in a config error — neither
        // the AWS-chain names nor the explicit OURIOS_S3_* keys (RFC 0019 §3.4).
        for secret_key in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "OURIOS_S3_SECRET_ACCESS_KEY",
            "OURIOS_S3_ACCESS_KEY_ID",
            "OURIOS_S3_SESSION_TOKEN",
        ] {
            assert!(
                !err.contains(secret_key),
                "a credential key must not appear in a config error, got {err:?}",
            );
        }
    }

    /// RFC0019.8 (config layer) — explicit S3 credentials are applied to an
    /// `s3` `StoreConfig`, a present-but-blank value reads as unset, and a
    /// `local` config carries none. The pairing/validation and the redaction of
    /// any build error live in `ourios_parquet::Store::s3` (covered there).
    /// See `docs/rfcs/0019-storage-backend-selection.md` §3.4 / §5 (RFC0019.8).
    #[test]
    fn rfc0019_8_explicit_s3_credentials_applied() {
        let s3 = with_s3_credentials(
            StoreConfig::S3(S3Config::new("b")),
            Some("AKIAEXAMPLE"),
            Some("s3cr3t"),
            Some("tok"),
        );
        assert_eq!(
            s3,
            StoreConfig::S3(
                S3Config::new("b")
                    .with_access_key_id("AKIAEXAMPLE")
                    .with_secret_access_key("s3cr3t")
                    .with_session_token("tok"),
            ),
        );
        // A present-but-blank credential reads as unset (so it can't trip the
        // partial-pair check at store-build time).
        let blank =
            with_s3_credentials(StoreConfig::S3(S3Config::new("b")), Some("  "), None, None);
        assert_eq!(blank, StoreConfig::S3(S3Config::new("b")));
        // A local backend carries no credentials — passes through untouched.
        let local_cfg = with_s3_credentials(local("/store"), Some("x"), Some("y"), None);
        assert_eq!(local_cfg, local("/store"));
    }

    /// Scenario RFC0019.7 — local backend regression (the default path).
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    #[test]
    fn rfc0019_7_local_backend_regression() {
        // The default (no `OURIOS_STORAGE_BACKEND`, a bucket root set) resolves
        // to exactly the local store used before RFC 0019 — the
        // receiver/querier/compactor behaviour is then guarded by their
        // existing local suites, unchanged.
        let config = build_config(
            build_store_config(None, Some(PathBuf::from("/store")), None, None, None, None)
                .expect("default local"),
            None,
            None,
        )
        .expect("valid");
        assert_eq!(config.store, local("/store"));
        assert!(config.compaction_enabled, "compaction is on by default");
    }

    #[test]
    fn build_receiver_config_disabled_unless_explicitly_enabled() {
        // Arrange / Act / Assert — unset or a falsey value disables the role.
        for raw in [None, Some("0"), Some("false"), Some("nope")] {
            assert_eq!(
                build_receiver_config(raw, None, None, Some(PathBuf::from("/wal")), None)
                    .expect("ok"),
                None,
                "receiver disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_receiver_config_enabled_defaults_the_addresses() {
        // Arrange / Act
        let params =
            build_receiver_config(Some("1"), None, None, Some(PathBuf::from("/wal")), None)
                .expect("ok")
                .expect("enabled");

        // Assert
        assert_eq!(params.grpc_addr, DEFAULT_GRPC_ADDR.parse().unwrap());
        assert_eq!(params.http_addr, DEFAULT_HTTP_ADDR.parse().unwrap());
        assert_eq!(params.wal_root, PathBuf::from("/wal"));
    }

    #[test]
    fn build_receiver_config_parses_custom_addresses() {
        // Arrange / Act
        let params = build_receiver_config(
            Some("yes"),
            Some("127.0.0.1:1"),
            Some("127.0.0.1:2"),
            Some(PathBuf::from("/wal")),
            None,
        )
        .expect("ok")
        .expect("enabled");

        // Assert
        assert_eq!(params.grpc_addr, "127.0.0.1:1".parse().unwrap());
        assert_eq!(params.http_addr, "127.0.0.1:2".parse().unwrap());
    }

    #[test]
    fn build_receiver_config_requires_a_wal_root_when_enabled() {
        // Arrange / Act / Assert — the WAL root is mandatory (and must be
        // non-empty) once the receiver role is on.
        assert!(
            build_receiver_config(Some("1"), None, None, None, None).is_err(),
            "a missing WAL root is rejected",
        );
        assert!(
            build_receiver_config(Some("1"), None, None, Some(PathBuf::from("")), None).is_err(),
            "an empty WAL root is rejected",
        );
    }

    #[test]
    fn build_receiver_config_encode_workers_defaults_and_validates() {
        // RFC 0035: unset → available cores (≥ 1); explicit values parse;
        // zero / junk are startup errors.
        let params =
            build_receiver_config(Some("1"), None, None, Some(PathBuf::from("/wal")), None)
                .expect("ok")
                .expect("enabled");
        assert!(params.encode_workers >= 1, "the default is at least one");

        let params = build_receiver_config(
            Some("1"),
            None,
            None,
            Some(PathBuf::from("/wal")),
            Some("3"),
        )
        .expect("ok")
        .expect("enabled");
        assert_eq!(params.encode_workers, 3);

        for bad in ["0", "-1", "many"] {
            assert!(
                build_receiver_config(
                    Some("1"),
                    None,
                    None,
                    Some(PathBuf::from("/wal")),
                    Some(bad),
                )
                .is_err(),
                "encode_workers = {bad:?} is rejected",
            );
        }
    }

    #[test]
    fn build_receiver_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_receiver_config(
                Some("1"),
                Some("not-an-addr"),
                None,
                Some(PathBuf::from("/wal")),
                None,
            )
            .is_err(),
            "a malformed bind address is rejected",
        );
    }

    #[test]
    fn build_querier_config_disabled_unless_explicitly_enabled() {
        // Arrange / Act / Assert — unset or a falsey value disables the role.
        for raw in [None, Some("0"), Some("false"), Some("nope")] {
            assert_eq!(
                build_querier_config(raw, None, None, None).expect("ok"),
                None,
                "querier disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_querier_config_enabled_defaults_address_and_window() {
        // Arrange / Act
        let params = build_querier_config(Some("1"), None, None, None)
            .expect("ok")
            .expect("enabled");

        // Assert
        assert_eq!(params.http_addr, DEFAULT_QUERIER_HTTP_ADDR.parse().unwrap());
        assert_eq!(
            params.default_window_nanos,
            DEFAULT_QUERIER_WINDOW_SECS * NANOS_PER_SEC,
        );
    }

    #[test]
    fn build_querier_config_parses_custom_address_and_window() {
        // Arrange / Act
        let params = build_querier_config(Some("yes"), Some("127.0.0.1:9"), Some("120"), None)
            .expect("ok")
            .expect("enabled");

        // Assert
        assert_eq!(params.http_addr, "127.0.0.1:9".parse().unwrap());
        assert_eq!(params.default_window_nanos, 120 * NANOS_PER_SEC);
    }

    #[test]
    fn build_querier_config_rejects_a_zero_or_nonnumeric_window() {
        // Arrange / Act / Assert — a zero window would make every no-`range`
        // query empty; a non-numeric value is a config typo.
        assert!(
            build_querier_config(Some("1"), None, Some("0"), None).is_err(),
            "a zero default window is rejected",
        );
        assert!(
            build_querier_config(Some("1"), None, Some("soon"), None).is_err(),
            "a non-numeric default window is rejected",
        );
    }

    #[test]
    fn build_querier_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_querier_config(Some("1"), Some("not-an-addr"), None, None).is_err(),
            "a malformed bind address is rejected",
        );
    }
}
