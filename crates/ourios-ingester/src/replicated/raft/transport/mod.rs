//! The Raft peer transport: `openraft`'s `RaftNetworkV2` over the gRPC
//! service `ourios.wal.v1.Replication` (RFC 0065 §4.3, §6.8).
//!
//! A node runs one [`ReplicationServer`] for all its groups and one
//! [`Peers`] pool, which holds one channel per peer address. Every request
//! names its protocol version, its group and the replica it is meant for.
//! A server refuses an unknown version with `FAILED_PRECONDITION` (B22),
//! and a group it does not host, or a replica other than the one it hosts
//! for that group, with `NOT_FOUND`. The client reports both to `openraft`
//! as an unreachable peer, which backs off and credits it with nothing.
//!
//! A message is at most [`wire::MAX_MESSAGE_BYTES`], one maximum-sized WAL
//! frame plus its envelope, and an append carries as many entries as fit,
//! reporting a partial success for the rest.
//!
//! Replication runs over `StreamAppend`, one bidirectional stream per
//! `openraft` replication session or heartbeat. Every message on it is an
//! append request with its own header, and every header must equal the
//! first, which routes the stream: a stream addresses one replica. An
//! append over the size limit travels as several follow-on messages
//! rather than a partial success, so the stream keeps going. The follower
//! answers each message in order once its log has flushed it.
//!
//! Peers authenticate each other with mutual TLS (B21): the server takes
//! RFC 0030's reloading acceptor, refusing a caller whose certificate the
//! peer CA did not sign, and [`Peers::mutual_tls`] presents the node's
//! certificate from the same settings and verifies each peer's for the
//! host its address names.

mod client;
mod connect;
mod forward;
mod identify;
mod links;
mod server;
mod trace;
pub mod wire;

pub use client::{GroupNetwork, PeerClient, Peers};
pub use forward::{
    ForwardRequest, ForwardResponse, ForwardTarget, Forwarded, Outcome, Refusal, Rejected,
    Rejection,
};
pub use identify::{HIGHEST_CONTROL_KIND, IdentifyError, Identity};
pub use server::ReplicationServer;

/// The peer protocol version every request carries (B22).
pub const VERSION: u32 = 1;

pub const SERVICE: &str = "ourios.wal.v1.Replication";

const APPEND: &str = "/ourios.wal.v1.Replication/Append";
const STREAM_APPEND: &str = "/ourios.wal.v1.Replication/StreamAppend";
const VOTE: &str = "/ourios.wal.v1.Replication/Vote";
const PRE_VOTE: &str = "/ourios.wal.v1.Replication/PreVote";
const INSTALL_SNAPSHOT: &str = "/ourios.wal.v1.Replication/InstallSnapshot";
const FORWARD: &str = "/ourios.wal.v1.Replication/Forward";
const IDENTIFY: &str = "/ourios.wal.v1.Replication/Identify";

#[cfg(test)]
mod tests;
