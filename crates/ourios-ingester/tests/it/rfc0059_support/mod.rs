//! Shared rig for the RFC 0059 scenarios: a receiver that mints, cuts,
//! publishes and reclaims through the production barrier and
//! housekeeping, then restarts through startup recovery against the
//! store's template-id high-water.

// The shared-`tests/` module shape: each scenario uses part of the rig.
#![allow(dead_code)]

mod audit;
mod fixture;
mod hooked_store;
mod node;
mod publishing;
mod put_gate;
mod renaming;
mod restarted;
mod store_hooks;

pub use audit::audit_bindings;
pub use fixture::Fixture;
pub use node::Node;
pub use publishing::{cut, cut_and_reclaim, publish, structured_logs};
pub use put_gate::PutGate;
pub use renaming::{assert_equivalent_up_to_renaming, ids_per_tenant};
pub use restarted::Restarted;
pub use store_hooks::Hooks;

pub use crate::rfc0052_barrier_support::{audit_events, rows};
