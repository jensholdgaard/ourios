//! The replicated WAL (RFC 0065).
//!
//! Only the cluster layout's hashes exist so far: [`placement`] maps a
//! tenant to its group and a group to its voters and tail replicas
//! (RFC 0065 §6.1).

pub mod placement;
