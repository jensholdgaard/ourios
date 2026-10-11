//! Version-keyed template snapshots for the miner oracles, taken from the
//! live tree right after each ingest; //! shared as a source file (each target pulls it in with `#[path]`, like
//! `telemetry.rs`) so the fuzz package keeps no library API (RFC 0015 §3.1).

#![deny(unsafe_code)]

//! Templates are looked up by `(template_id, template_version)`, the read
//! path's registry key (RFC 0017 §3.5). A property batch rarely widens a
//! template, so rendering every row against the leaf's *latest* tokens passes
//! there; a fuzzer widens within its first few inputs, and a row stamped
//! version N must render against the N-version tokens.

use std::collections::HashMap;

use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::tree::OwnedToken;

/// `(template_id, template_version) → tokens`, snapshotted from the live
/// tree right after the ingest that stamped the version (the in-memory
/// truth, with no detour through the audit stream's template strings).
pub type Templates = HashMap<(u64, u32), Vec<OwnedToken>>;

/// Record the tokens of the leaf `record` was stamped with, unless that
/// `(template_id, template_version)` is already known. Must run right after
/// the ingest that emitted `record`: a later widening bumps the leaf past
/// the record's version, and the older tokens are gone from the tree.
///
/// # Panics
///
/// When the tenant exposes no leaf at the record's key — the miner stamped a
/// `(template_id, template_version)` it never exposed, so no reader could
/// ever render the row.
pub fn snapshot_leaf(
    miner: &MinerCluster,
    tenant: &TenantId,
    record: &MinedRecord,
    templates: &mut Templates,
) {
    let key = (record.template_id, record.template_version);
    if templates.contains_key(&key) {
        return;
    }
    let leaf = miner
        .templates_for(tenant)
        .into_iter()
        .find(|leaf| (leaf.template_id, leaf.template_version) == key)
        .unwrap_or_else(|| {
            panic!("RFC0015.1 / RFC 0017 §3.5: record stamped {key:?} but no leaf has that id + version")
        });
    templates.insert(key, leaf.template);
}

pub fn template_of<'a>(
    index: usize,
    record: &MinedRecord,
    templates: &'a Templates,
) -> &'a [OwnedToken] {
    let key = (record.template_id, record.template_version);
    templates.get(&key).map_or_else(
        || panic!("RFC0015.1: record {index}: no snapshot for {key:?} — snapshot_leaf must run after every ingest"),
        Vec::as_slice,
    )
}
