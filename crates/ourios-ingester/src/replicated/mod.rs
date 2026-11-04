//! The replicated WAL (RFC 0065).
//!
//! The cluster layout (RFC 0065 §6.1): [`placement`] maps a tenant to its
//! group and a group to its voters and tail replicas, [`layout`] records
//! the voting layout in the store and checks every start against it, and
//! [`replica_id`] keeps a replica's identity on its own disk. [`raft`] is
//! a group's consensus storage, not yet wired into a served role.

pub mod layout;
pub mod placement;
pub mod raft;
pub mod replica_id;
