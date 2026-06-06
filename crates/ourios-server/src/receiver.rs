//! The OTLP receiver role (RFC 0003 §6.2 / the §9 process-model
//! resolution): both transports — gRPC (`tonic`) and HTTP (`axum`) —
//! over **one** shared `IngestPipeline` backed by a single `Wal`
//! (RFC 0008 §3.1's single-writer rule). Graceful shutdown is driven by
//! one `watch` channel fanned out to both listeners.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;
use ourios_core::config::MinerConfig;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::{IngestPipeline, SharedPipeline, TenantRule};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{Wal, WalConfig};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

/// Where the receiver role binds, and the WAL it persists to.
pub struct ReceiverConfig {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub wal: WalConfig,
}

/// A running receiver role: the **resolved** bound addresses (so a `:0`
/// request is observable) plus the handles to shut it down.
pub struct ReceiverHandle {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    shutdown: watch::Sender<()>,
    grpc: JoinHandle<Result<(), tonic::transport::Error>>,
    http: JoinHandle<std::io::Result<()>>,
}

impl ReceiverHandle {
    /// Signal both listeners to stop and await their graceful shutdown.
    /// Once both tasks return they have dropped their pipeline handles,
    /// releasing the single `Wal`.
    pub async fn shutdown(self) -> Result<(), String> {
        // A send error just means both listeners already stopped — nothing
        // left to signal.
        let _ = self.shutdown.send(());
        self.grpc
            .await
            .map_err(|e| format!("gRPC listener task: {e}"))?
            .map_err(|e| format!("gRPC listener: {e}"))?;
        self.http
            .await
            .map_err(|e| format!("HTTP listener task: {e}"))?
            .map_err(|e| format!("HTTP listener: {e}"))?;
        Ok(())
    }
}

/// Bind both transports and start serving over one shared
/// `IngestPipeline`. Returns once both sockets are bound — so the caller
/// can observe the addresses (e.g. when binding `:0`) — with serving
/// running on spawned tasks until [`ReceiverHandle::shutdown`].
pub async fn serve(config: ReceiverConfig) -> Result<ReceiverHandle, String> {
    let wal = Wal::open(config.wal).map_err(|e| format!("open WAL: {e:?}"))?;
    let pipeline: SharedPipeline = Arc::new(Mutex::new(IngestPipeline::new(
        Box::new(wal),
        MinerCluster::new(MinerConfig::default()),
        TenantRule::service_name(),
    )));

    // gRPC: bind first so `:0` resolves to a real port before serving.
    let grpc_incoming = TcpIncoming::bind(config.grpc_addr)
        .map_err(|e| format!("bind gRPC {}: {e}", config.grpc_addr))?;
    let grpc_addr = grpc_incoming
        .local_addr()
        .map_err(|e| format!("gRPC local_addr: {e}"))?;

    // HTTP: same — bind the listener, then hand it to `axum::serve`.
    let http_listener = TcpListener::bind(config.http_addr)
        .await
        .map_err(|e| format!("bind HTTP {}: {e}", config.http_addr))?;
    let http_addr = http_listener
        .local_addr()
        .map_err(|e| format!("HTTP local_addr: {e}"))?;

    let (shutdown, shutdown_rx) = watch::channel(());

    let grpc_service = LogsServiceServer::new(LogsReceiver::new(pipeline.clone()));
    let grpc = tokio::spawn({
        let mut rx = shutdown_rx.clone();
        async move {
            Server::builder()
                .add_service(grpc_service)
                .serve_with_incoming_shutdown(grpc_incoming, async move {
                    let _ = rx.changed().await;
                })
                .await
        }
    });

    let http_router = router(pipeline, &HttpConfig::default());
    let http = tokio::spawn({
        let mut rx = shutdown_rx;
        async move {
            axum::serve(http_listener, http_router.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = rx.changed().await;
                })
                .await
        }
    });

    Ok(ReceiverHandle {
        grpc_addr,
        http_addr,
        shutdown,
        grpc,
        http,
    })
}
