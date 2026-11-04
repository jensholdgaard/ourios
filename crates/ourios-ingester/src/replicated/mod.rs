//! The replicated WAL (RFC 0065).
//!
//! Only the cluster layout exists so far (RFC 0065 §6.1): [`placement`]
//! maps a tenant to its group and a group to its voters and tail
//! replicas, [`layout`] records the voting layout in the store and checks
//! every start against it, and [`replica_id`] keeps a replica's identity
//! on its own disk.

pub mod layout;
pub mod placement;
pub mod replica_id;
