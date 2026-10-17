//! Who sent an HTTP request: the connection peer and the `User-Agent`,
//! as the HTTP server span conventions record them (`client.*`,
//! `network.peer.*`, `user_agent.original`).
//!
//! `client.address` is the socket peer. The conventions prefer the
//! original client behind any proxies, taken from `Forwarded` /
//! `X-Forwarded-For`, but those headers are client-supplied and Ourios
//! has no trusted-proxy configuration, so they are never read here.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, header};

/// The client of one HTTP request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpClient {
    peer: Option<SocketAddr>,
    user_agent: Option<String>,
}

impl HttpClient {
    /// Read the peer from the `ConnectInfo<SocketAddr>` the serve loop
    /// attaches to every request (absent when a router is driven without
    /// a listener, e.g. in-process tests) and the first `User-Agent`
    /// header, kept only when it is valid UTF-8.
    #[must_use]
    pub fn new(headers: &HeaderMap, extensions: &Extensions) -> Self {
        let peer = extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| *peer);
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
            .map(str::to_owned);
        Self { peer, user_agent }
    }

    /// `client.address` and `network.peer.address`: the peer's IP.
    #[must_use]
    pub fn address(&self) -> Option<IpAddr> {
        self.peer.map(|peer| peer.ip())
    }

    /// `client.port` and `network.peer.port`. `i64`, because the tracing
    /// bridge exports a `u64` field as a string where semconv types the
    /// port an int.
    #[must_use]
    pub fn port(&self) -> Option<i64> {
        self.peer.map(|peer| i64::from(peer.port()))
    }

    /// `user_agent.original`.
    #[must_use]
    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }
}

impl<S: Sync> FromRequestParts<S> for HttpClient {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Ok(Self::new(&parts.headers, &parts.extensions)))
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn client(user_agent: Option<HeaderValue>, peer: Option<SocketAddr>) -> HttpClient {
        let mut headers = HeaderMap::new();
        if let Some(value) = user_agent {
            headers.insert(header::USER_AGENT, value);
        }
        let mut extensions = Extensions::new();
        if let Some(peer) = peer {
            extensions.insert(ConnectInfo(peer));
        }
        HttpClient::new(&headers, &extensions)
    }

    #[test]
    fn reads_the_peer_and_the_user_agent() {
        let peer: SocketAddr = "192.0.2.4:65123".parse().expect("addr");
        let client = client(Some(HeaderValue::from_static("probe/1.0")), Some(peer));
        assert_eq!(client.address(), Some(peer.ip()));
        assert_eq!(client.port(), Some(65123));
        assert_eq!(client.user_agent(), Some("probe/1.0"));
    }

    #[test]
    fn a_request_without_either_records_neither() {
        let client = client(None, None);
        assert_eq!(client, HttpClient::default());
        assert_eq!(client.address(), None);
        assert_eq!(client.port(), None);
        assert_eq!(client.user_agent(), None);
    }

    #[test]
    fn a_user_agent_that_is_not_utf8_is_dropped() {
        let value = HeaderValue::from_bytes(b"probe/\xff").expect("obs-text is a valid header");
        assert_eq!(client(Some(value), None).user_agent(), None);
    }

    #[test]
    fn forwarding_headers_are_not_trusted() {
        let peer: SocketAddr = "127.0.0.1:4000".parse().expect("addr");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        headers.insert(
            header::FORWARDED,
            HeaderValue::from_static("for=203.0.113.9"),
        );
        let mut extensions = Extensions::new();
        extensions.insert(ConnectInfo(peer));
        assert_eq!(
            HttpClient::new(&headers, &extensions).address(),
            Some(peer.ip())
        );
    }
}
