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
//! Plaintext only: mutual TLS (RFC 0030) arrives through [`Peers`]'s
//! endpoint and the server's incoming stream.

mod client;
mod server;
mod trace;
pub mod wire;

pub use client::{GroupNetwork, PeerClient, Peers};
pub use server::ReplicationServer;

/// The peer protocol version every request carries (B22).
pub const VERSION: u32 = 1;

pub const SERVICE: &str = "ourios.wal.v1.Replication";

const APPEND: &str = "/ourios.wal.v1.Replication/Append";
const VOTE: &str = "/ourios.wal.v1.Replication/Vote";
const PRE_VOTE: &str = "/ourios.wal.v1.Replication/PreVote";
const INSTALL_SNAPSHOT: &str = "/ourios.wal.v1.Replication/InstallSnapshot";

#[cfg(test)]
mod tests;
