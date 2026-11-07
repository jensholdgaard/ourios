//! How a peer channel connects: over TLS, or plain TCP in tests. A failed
//! attempt is marked [`NotConnected`], so a caller can tell a request that
//! was never written from one whose answer was lost (RFC 0065 §6.8).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tonic::Status;
use tonic::codegen::http::Uri;

use super::client::CONNECT_TIMEOUT;

/// Connects a channel to its peer over TLS, verifying the peer's
/// certificate for the host the channel's address names.
#[derive(Clone)]
pub(super) struct TlsConnector(pub(super) tokio_rustls::TlsConnector);

type Connecting = Pin<Box<dyn Future<Output = io::Result<TokioIo<TlsStream<TcpStream>>>> + Send>>;

impl tower::Service<Uri> for TlsConnector {
    type Response = TokioIo<TlsStream<TcpStream>>;
    type Error = io::Error;
    type Future = Connecting;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Connecting {
        let connector = self.0.clone();
        connecting(async move {
            let (host, port) = host_and_port(&uri)?;
            let name = ServerName::try_from(host.clone())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let tcp = TcpStream::connect((host.as_str(), port)).await?;
            tcp.set_nodelay(true)?;
            Ok(TokioIo::new(connector.connect(name, tcp).await?))
        })
    }
}

/// A connection attempt that failed or did not finish within the connect
/// timeout: no request was written on it (RFC 0065 §6.8).
#[derive(Debug)]
pub(super) struct NotConnected(io::Error);

impl std::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not connected: {}", self.0)
    }
}

impl std::error::Error for NotConnected {}

/// `connect`, bounded by the connect timeout, its failure marked as
/// [`NotConnected`].
fn connecting<T>(
    connect: impl Future<Output = io::Result<T>> + Send + 'static,
) -> Pin<Box<dyn Future<Output = io::Result<T>> + Send>> {
    Box::pin(async move {
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .unwrap_or_else(|_| Err(io::Error::from(io::ErrorKind::TimedOut)));
        connected.map_err(|e| io::Error::new(e.kind(), NotConnected(e)))
    })
}

/// Whether `status` came from a connection attempt that failed, so its
/// request was never sent.
pub(super) fn never_sent(status: &Status) -> bool {
    let mut source = std::error::Error::source(status);
    while let Some(e) = source {
        let inner = e.downcast_ref::<io::Error>().and_then(io::Error::get_ref);
        let marked = inner.is_some_and(<dyn std::error::Error + Send + Sync>::is::<NotConnected>);
        if marked || e.is::<NotConnected>() {
            return true;
        }
        source = e.source();
    }
    false
}

/// Connects a test channel over plain TCP, marking failures as a TLS
/// connection's are.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) struct PlainConnector;

#[cfg(test)]
impl tower::Service<Uri> for PlainConnector {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TokioIo<TcpStream>>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        connecting(async move {
            let (host, port) = host_and_port(&uri)?;
            let tcp = TcpStream::connect((host.as_str(), port)).await?;
            tcp.set_nodelay(true)?;
            Ok(TokioIo::new(tcp))
        })
    }
}

/// The host, without an IPv6 literal's brackets, and the port of a
/// peer's `host:port` address.
fn host_and_port(uri: &Uri) -> io::Result<(String, u16)> {
    let host = uri
        .host()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'));
    match (host, uri.port_u16()) {
        (Some(host), Some(port)) => Ok((host.to_owned(), port)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the peer address {uri} is not host:port"),
        )),
    }
}
