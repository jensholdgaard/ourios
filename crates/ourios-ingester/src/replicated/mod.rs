//! The replicated WAL (RFC 0065).
//!
//! The cluster layout (RFC 0065 §6.1): [`placement`] maps a tenant to its
//! group and a group to its voters and tail replicas, [`layout`] records
//! the voting layout in the store and checks every start against it, and
//! [`replica_id`] keeps a replica's identity on its own disk. [`raft`] is
//! a group's consensus storage, and [`write`](mod@write) and [`ingest`] the write
//! path over it (§6.3), with [`forward`] its leader's side of `Forward`
//! and [`route`] its sender's (§6.8), not yet wired into a served role.

pub mod forward;
pub mod ingest;
pub mod layout;
pub mod placement;
pub mod raft;
pub mod replica_id;
pub mod route;
pub mod write;
