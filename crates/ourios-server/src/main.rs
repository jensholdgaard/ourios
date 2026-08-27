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
use std::path::{Path, PathBuf};

use clap::Parser;
use ourios_ingester::Compactor;

use ourios_parquet::{CompactionPolicy, ParquetAuditSink, StoreConfig};
use ourios_server::config::resolve::{
    ServerConfig, config_from_env, config_from_file, preflight_tls, validate_graph_columns,
    wal_config, warn_if_open_mode, warn_if_plaintext_credentials,
};

use ourios_telemetry::TelemetryConfig;
use tracing::Instrument as _;

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

    use ourios_parquet::PromotedAttributes;

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
}
