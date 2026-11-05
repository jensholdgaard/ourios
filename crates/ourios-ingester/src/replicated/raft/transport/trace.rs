//! Peer-RPC spans: gRPC client and server spans under the RPC
//! conventions (RFC 0038 §3.5, RFC 0065 §6.9).
//!
//! An append without entries is a heartbeat or a commit-only append, sent
//! every heartbeat interval per group and peer, so a span each would be
//! most of a node's trace volume. It gets a span only when it fails, built
//! once the failure is known: `tracing` cannot backdate a span, so that
//! one goes through the `OpenTelemetry` API with the call's real start.
//!
//! A `StreamAppend` stream gets one span on each side, built the same way
//! when it ends, and only when it carried entries or failed: what a
//! stream carries is not known when it opens, and a heartbeat is a stream
//! of one entry-less append.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use opentelemetry::KeyValue;
use opentelemetry::global::BoxedSpan;
use opentelemetry::trace::{Span as _, SpanBuilder, SpanKind, Status, Tracer as _};
use tonic::Code;

use super::wire;

/// Where a failed untraced call's span is recorded: the process-global
/// tracer provider, or a tracer of its own (tests).
#[derive(Clone, Default)]
pub(super) enum Tracing {
    #[default]
    Global,
    #[cfg(test)]
    Owned(std::sync::Arc<opentelemetry::global::BoxedTracer>),
}

impl Tracing {
    fn start(&self, builder: SpanBuilder) -> BoxedSpan {
        match self {
            Self::Global => ourios_core::telemetry::tracer("ourios.replication").build(builder),
            #[cfg(test)]
            Self::Owned(tracer) => tracer.build(builder),
        }
    }
}

/// Whether a request is traced whether or not it fails.
pub(super) trait Traced {
    fn traced(&self) -> bool {
        true
    }
}

impl Traced for wire::AppendRequest {
    fn traced(&self) -> bool {
        !self.entries.is_empty()
    }
}

impl Traced for wire::WireVoteRequest {}
impl Traced for wire::SnapshotRequest {}

/// The host and port of a `host:port` peer address.
#[derive(Clone, Copy)]
pub(super) struct Peer<'a> {
    host: &'a str,
    port: Option<u16>,
}

impl<'a> Peer<'a> {
    pub(super) fn parse(addr: &'a str) -> Self {
        match addr.rsplit_once(':') {
            Some((host, port)) => Self {
                host,
                port: port.parse().ok(),
            },
            None => Self {
                host: addr,
                port: None,
            },
        }
    }
}

/// Which end of the call a span records, which decides the codes that
/// fail it: every non-`OK` code fails a client span, and only the codes
/// semconv lists fail a server span (the receiver's rule, `grpc.rs`).
#[derive(Clone, Copy)]
enum Side {
    Client,
    Server,
}

impl Side {
    fn fails(self, code: Code) -> bool {
        match self {
            Self::Client => code != Code::Ok,
            Self::Server => matches!(
                code,
                Code::Unknown
                    | Code::DeadlineExceeded
                    | Code::Unimplemented
                    | Code::Internal
                    | Code::Unavailable
                    | Code::DataLoss
            ),
        }
    }

    fn kind(self) -> SpanKind {
        match self {
            Self::Client => SpanKind::Client,
            Self::Server => SpanKind::Server,
        }
    }

    fn otel_kind(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// The gRPC status name semconv's `rpc.response.status_code` carries.
fn code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

/// The `rpc.method` of a request path: the path without its leading `/`.
pub(super) fn rpc_method(path: &'static str) -> &'static str {
    path.strip_prefix('/').unwrap_or(path)
}

/// One peer RPC's span, open from the start for a traced request and
/// built at the end for an untraced one that failed.
pub(super) struct Call<'a> {
    side: Side,
    method: &'static str,
    peer: Option<Peer<'a>>,
    started: SystemTime,
    span: Option<tracing::Span>,
    tracing: Tracing,
}

impl<'a> Call<'a> {
    /// A call to `peer`.
    pub(super) fn client(
        tracing: Tracing,
        path: &'static str,
        peer: Peer<'a>,
        traced: bool,
    ) -> Self {
        Self::open(tracing, Side::Client, path, Some(peer)).traced(traced)
    }

    pub(super) fn server(tracing: Tracing, path: &'static str, traced: bool) -> Self {
        Self::open(tracing, Side::Server, path, None).traced(traced)
    }

    fn open(tracing: Tracing, side: Side, path: &'static str, peer: Option<Peer<'a>>) -> Self {
        Self {
            side,
            method: rpc_method(path),
            peer,
            started: SystemTime::now(),
            span: None,
            tracing,
        }
    }

    fn traced(mut self, traced: bool) -> Self {
        if traced {
            self.span = Some(self.tracing_span());
        }
        self
    }

    fn tracing_span(&self) -> tracing::Span {
        let host = self.peer.map(|p| p.host);
        let port = self.peer.and_then(|p| p.port).map(i64::from);
        tracing::info_span!(
            "peer rpc",
            otel.name = self.method,
            otel.kind = self.side.otel_kind(),
            rpc.system.name = "grpc",
            rpc.method = self.method,
            server.address = host,
            server.port = port,
            rpc.response.status_code = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        )
    }

    /// The span to run the call under; a disabled one when untraced.
    pub(super) fn span(&self) -> tracing::Span {
        self.span.clone().unwrap_or_else(tracing::Span::none)
    }

    pub(super) fn finish(self, code: Code) {
        self.end(code, false);
    }

    /// End the call; an untraced one that `carried` entries is recorded
    /// whatever its code.
    fn end(self, code: Code, carried: bool) {
        let name = code_name(code);
        let failed = self.side.fails(code);
        match &self.span {
            Some(span) => {
                span.record("rpc.response.status_code", name);
                if failed {
                    ourios_core::telemetry::record_span_error(span, name);
                }
            }
            None if failed || carried => self.built_span(name, failed),
            None => {}
        }
    }

    fn built_span(&self, name: &'static str, failed: bool) {
        let mut attributes = vec![
            KeyValue::new("rpc.system.name", "grpc"),
            KeyValue::new("rpc.method", self.method),
            KeyValue::new("rpc.response.status_code", name),
        ];
        if failed {
            attributes.push(KeyValue::new(ourios_core::telemetry::ERROR_TYPE, name));
        }
        if let Some(peer) = self.peer {
            attributes.push(KeyValue::new("server.address", peer.host.to_owned()));
            if let Some(port) = peer.port {
                attributes.push(KeyValue::new("server.port", i64::from(port)));
            }
        }
        let builder = SpanBuilder::from_name(self.method)
            .with_kind(self.side.kind())
            .with_start_time(self.started)
            .with_attributes(attributes);
        let mut span = self.tracing.start(builder);
        if failed {
            span.set_status(Status::error(""));
        }
        span.end();
    }
}

/// Whether a stream carried any entries: noted by the task that moves
/// its requests, read when its span is recorded.
#[derive(Clone, Default)]
pub(super) struct Carried(Arc<AtomicBool>);

impl Carried {
    pub(super) fn note(&self, request: &impl Traced) {
        if request.traced() {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// One `StreamAppend` stream's call, recorded when it drops with the
/// code it ended on: `CANCELLED` unless [`Self::end`] says otherwise, as
/// a stream dropped midway was cancelled by its holder.
pub(super) struct StreamCall<'a> {
    call: Option<Call<'a>>,
    code: Code,
    carried: Carried,
}

impl<'a> StreamCall<'a> {
    pub(super) fn client(tracing: Tracing, path: &'static str, peer: Peer<'a>) -> Self {
        Self::of(Call::client(tracing, path, peer, false))
    }

    pub(super) fn server(tracing: Tracing, path: &'static str) -> Self {
        Self::of(Call::server(tracing, path, false))
    }

    fn of(call: Call<'a>) -> Self {
        Self {
            call: Some(call),
            code: Code::Cancelled,
            carried: Carried::default(),
        }
    }

    pub(super) fn carried(&self) -> Carried {
        self.carried.clone()
    }

    pub(super) fn end(&mut self, code: Code) {
        self.code = code;
    }
}

impl Drop for StreamCall<'_> {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            call.end(self.code, self.carried.get());
        }
    }
}
