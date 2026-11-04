//! The Raft peer transport: `openraft`'s `RaftNetworkV2` over the gRPC
//! service `ourios.wal.v1.Replication` (RFC 0065 §4.3, §6.8).
//!
//! A node runs one [`ReplicationServer`] for all its groups and one
//! [`Peers`] pool, which holds one channel per peer address. Every request
//! names its protocol version and group; a server refuses an unknown
//! version with `FAILED_PRECONDITION` (B22) and a group it does not host
//! with `NOT_FOUND`, and the client reports both to `openraft` as an
//! unreachable peer, so it backs off rather than retrying at once.
//!
//! Peer RPCs are traced as gRPC client and server spans under the RPC
//! conventions (RFC 0038 §3.5, RFC 0065 §6.9), except an append without
//! entries, which only warns when it fails. Plaintext only: mutual TLS
//! (RFC 0030) arrives through [`Peers`]'s endpoint and the server's
//! incoming stream.

mod client;
mod server;
pub mod wire;

pub use client::{GroupNetwork, PeerClient, Peers};
pub use server::ReplicationServer;

use tonic::Code;

/// The peer protocol version every request carries (B22).
pub const VERSION: u32 = 1;

pub const SERVICE: &str = "ourios.wal.v1.Replication";

const APPEND: &str = "/ourios.wal.v1.Replication/Append";
const VOTE: &str = "/ourios.wal.v1.Replication/Vote";
const PRE_VOTE: &str = "/ourios.wal.v1.Replication/PreVote";
const INSTALL_SNAPSHOT: &str = "/ourios.wal.v1.Replication/InstallSnapshot";

/// The `rpc.method` of a request path: the path without its leading `/`.
fn rpc_method(path: &'static str) -> &'static str {
    path.strip_prefix('/').unwrap_or(path)
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
}

/// Whether a request gets a span. An append without entries is a
/// heartbeat or a commit-only append, sent every heartbeat interval per
/// group and peer: a span each would be most of a node's trace volume.
trait Traced {
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

/// One peer RPC's telemetry: its span, or for an untraced request only a
/// warning when it fails, so an outage seen only by heartbeats still
/// shows.
struct Call<'a> {
    side: Side,
    path: &'static str,
    peer: Option<&'a str>,
    span: Option<tracing::Span>,
}

impl<'a> Call<'a> {
    /// `peer` is the server's host on a client call; `span` is `None` for
    /// an untraced request.
    fn new(
        side: Side,
        path: &'static str,
        peer: Option<&'a str>,
        span: Option<tracing::Span>,
    ) -> Self {
        Self {
            side,
            path,
            peer,
            span,
        }
    }

    /// The span to run the call under; a disabled one when untraced.
    fn span(&self) -> tracing::Span {
        self.span.clone().unwrap_or_else(tracing::Span::none)
    }

    fn finish(self, code: Code) {
        let name = code_name(code);
        match self.span {
            Some(span) => {
                span.record("rpc.response.status_code", name);
                if self.side.fails(code) {
                    ourios_core::telemetry::record_span_error(&span, name);
                }
            }
            None if self.side.fails(code) => tracing::warn!(
                rpc.system.name = "grpc",
                rpc.method = rpc_method(self.path),
                rpc.response.status_code = name,
                server.address = self.peer,
                error.type = name,
                "peer RPC failed",
            ),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests;
