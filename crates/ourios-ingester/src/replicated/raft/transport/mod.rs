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
//! conventions (RFC 0038 §3.5, RFC 0065 §6.9). Plaintext only: mutual TLS
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

fn record_status(span: &tracing::Span, side: Side, code: Code) {
    let name = code_name(code);
    span.record("rpc.response.status_code", name);
    if side.fails(code) {
        ourios_core::telemetry::record_span_error(span, name);
    }
}

#[cfg(test)]
mod tests;
