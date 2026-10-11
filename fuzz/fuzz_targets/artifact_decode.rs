#![no_main]

//! RFC0015.11 / RFC0015.12 / RFC0015.13 — the small persisted artifacts the
//! process reads back from object storage. Their bytes may be torn, written
//! by an older or newer build, or not written by this process at all; a
//! decoder that panics on them, or half-applies them, turns every restart
//! into a crash loop or a silently wrong state (#892: an unreadable
//! self-written snapshot made every restart rebuild from the WAL).
//!
//! The first input byte picks the decoder; the rest is the artifact:
//!
//! | `data[0] % 5` | artifact | decoder |
//! |---|---|---|
//! | 0 | miner snapshot (RFC 0001 §6.9) | `snapshot::recover`, then `MinerCluster::restore_tenant` |
//! | 1 | partition `manifest.json` (RFC 0009 §3.4) | `Manifest::from_json` |
//! | 2 | `miner/template_ids.v1.json` (RFC 0059 §3.1) | the high-water `parse` (`fuzzing` feature) |
//! | 3 | `template_map.v2.json.zst` frame (RFC 0033 §3.2) | `TemplateMap::from_artifact_bytes` |
//! | 4 | the same frame's JSON body, zstd-compressed here | `TemplateMap::from_artifact_bytes` |
//!
//! Selector 4 lets the fuzzer mutate the template map's JSON directly; behind
//! selector 3 almost every mutation only breaks the zstd frame. One binary
//! rather than four keeps the `ClusterFuzzLite` target count, and its share
//! of the shared budget, down: each decoder is a few functions of JSON over
//! the same `serde_json`, and coverage is still attributed per function.
//!
//! Oracles, each panicking with a message naming its scenario:
//!
//! - **RFC0015.11, no panic and an allowed outcome.** Every decoder returns
//!   `Ok` or a typed error of the kind its contract names. `recover(Some(_))`
//!   is `(Some, Restored)` or `(None, UnknownOrCorruptDiscarded)`, and it
//!   restores only a byte 0 of `SNAPSHOT_VERSION`. A template-map read is
//!   `Valid`, `Torn`, an `UnknownVersion` other than the reader's own, or the
//!   loud tenant-mismatch error.
//! - **RFC0015.12, never a partial state.** A snapshot `restore_tenant`
//!   rejects leaves the cluster exactly as it was: no state for the tenant,
//!   no template ids allocated. A template map is only ever `Valid` whole.
//! - **RFC0015.13, decode → encode is a fixpoint.** What decodes re-encodes
//!   with the writer's own encoder and decodes back equal: the snapshot
//!   payload, the manifest, the high-water and the template map (the encoded
//!   bytes are a fixpoint too). A restored snapshot captures back the
//!   decoded state field for field, leaves, structured map, adopted
//!   templates and horizon, in the writer's canonical form (#892 was a leaf
//!   the writer could not place again), and that capture restores to itself.
//!
//! Memory: inputs are the fuzzer's (4 KiB by default), and the only expanding
//! decoder, the template-map zstd frame, stops at its 64 MiB body bound
//! (classified `Torn`), well inside `-rss_limit_mb=2048`.

use libfuzzer_sys::fuzz_target;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::template_ids::TemplateIdsError;
use ourios_ingester::template_ids::fuzzing as high_water;
use ourios_miner::cluster::{MAX_TEMPLATE_ID, MinerCluster};
use ourios_miner::snapshot::{
    self, AdoptedTemplateRecord, LeafRecord, LegacyMark, ParamTypeRecord, ProvenanceRecord,
    RecoveryOutcome, SNAPSHOT_VERSION, SnapshotState, load_snapshot,
};
use ourios_parquet::{Manifest, ManifestError};
use ourios_querier::{ArtifactRead, QueryError, TemplateMap};

#[path = "../common/telemetry.rs"]
mod telemetry;

/// The tenant every artifact is read for. A template map's body names its
/// tenant, so the dictionary carries this string.
const TENANT: &str = "fuzz-tenant";

/// The `format_version` `TemplateMap` reads and writes. The querier does not
/// export it; it is also the `v2` in `TEMPLATE_MAP_FILENAME`.
const TEMPLATE_MAP_FORMAT_VERSION: u32 = 2;

/// The version byte of a pre-RFC 0052 snapshot, which `legacy_v1_mark` reads.
const LEGACY_SNAPSHOT_VERSION: u8 = 1;

fuzz_target!(init: telemetry::init(), |data: &[u8]| {
    let Some((&selector, artifact)) = data.split_first() else {
        return;
    };
    match selector % 5 {
        0 => miner_snapshot(artifact),
        1 => manifest(artifact),
        2 => template_ids_high_water(artifact),
        3 => template_map(artifact),
        _ => template_map(&compress(artifact)),
    }
});

fn miner_snapshot(bytes: &[u8]) {
    assert_legacy_mark_classified(bytes);
    let Some(state) = recovered_state(bytes) else {
        return;
    };
    assert_payload_round_trips(&state);
    let tenant = TenantId::new(TENANT);
    let Some(miner) = restore_or_assert_isolated(&tenant, &state) else {
        return;
    };
    assert_restore_faithful(&miner, &tenant, &state);
}

/// RFC0015.11: `legacy_v1_mark` reads as legacy exactly the byte-0 = 1
/// artefacts.
fn assert_legacy_mark_classified(bytes: &[u8]) {
    let legacy = snapshot::legacy_v1_mark(bytes);
    assert_eq!(
        matches!(legacy, LegacyMark::NotLegacy),
        bytes.first() != Some(&LEGACY_SNAPSHOT_VERSION),
        "RFC0015.11: legacy_v1_mark classified a byte 0 of {:?} as {legacy:?}",
        bytes.first(),
    );
}

/// RFC0015.11: `recover(Some(_))` restores only what `load_snapshot` reads
/// under `SNAPSHOT_VERSION`, and discards everything else.
fn recovered_state(bytes: &[u8]) -> Option<SnapshotState> {
    let state = match snapshot::recover(Some(bytes)) {
        (Some(state), RecoveryOutcome::Restored) => state,
        (None, RecoveryOutcome::UnknownOrCorruptDiscarded) => {
            assert!(
                load_snapshot(bytes).is_err(),
                "RFC0015.11: recover discarded a snapshot load_snapshot reads"
            );
            return None;
        }
        (state, outcome) => panic!(
            "RFC0015.11: recover(Some(_)) returned {outcome:?} with a state: {}",
            state.is_some()
        ),
    };
    assert_eq!(
        bytes.first(),
        Some(&SNAPSHOT_VERSION),
        "RFC0015.11: recover restored a snapshot whose version byte is not {SNAPSHOT_VERSION}",
    );
    Some(state)
}

/// RFC0015.13: the decoded payload survives its own encoding.
fn assert_payload_round_trips(state: &SnapshotState) {
    let encoded = snapshot::snapshot(state)
        .unwrap_or_else(|e| panic!("RFC0015.13: a decoded snapshot does not re-encode: {e}"));
    match load_snapshot(&encoded) {
        Ok(again) => assert!(
            again == *state,
            "RFC0015.13: the snapshot payload did not survive its own encoding"
        ),
        Err(e) => panic!("RFC0015.13: the re-encoded snapshot does not decode: {e}"),
    }
}

/// Restore `state` into a fresh cluster. RFC0015.12: a rejected restore
/// leaves the cluster exactly as it was, no tenant state and no template id
/// allocated, and yields `None`.
fn restore_or_assert_isolated(tenant: &TenantId, state: &SnapshotState) -> Option<MinerCluster> {
    let mut miner = MinerCluster::new(MinerConfig::default());
    let Err(e) = miner.restore_tenant(tenant, state) else {
        return Some(miner);
    };
    assert!(
        miner.tenant_ids().is_empty() && miner.template_count(tenant) == 0,
        "RFC0015.12: a rejected restore ({e:?}) left state behind for the tenant"
    );
    assert_eq!(
        miner.highest_allocated(),
        0,
        "RFC0015.12: a rejected restore ({e:?}) moved the template-id allocator"
    );
    None
}

/// RFC0015.13: the restored tree captures back the decoded state in its
/// canonical form, field for field, and that capture restores to itself.
fn assert_restore_faithful(miner: &MinerCluster, tenant: &TenantId, state: &SnapshotState) {
    let restored = capture(miner, tenant);
    let expected = canonical(state);
    assert!(
        restored == expected,
        "RFC0015.13: a restored snapshot does not capture back what it restored\n\
         restored: {restored:?}\nexpected: {expected:?}"
    );
    let mut again = MinerCluster::new(MinerConfig::default());
    if let Err(e) = again.restore_tenant(tenant, &restored) {
        panic!("RFC0015.13: the snapshot of a restored tree does not restore: {e:?}");
    }
    assert!(
        capture(&again, tenant) == restored,
        "RFC0015.13: restore → snapshot is not a fixpoint (the restored tree differs \
         from the tree it was captured from)"
    );
}

/// What the snapshot writer persists for `tenant`: its state, stamped with
/// its own folded horizon.
fn capture(miner: &MinerCluster, tenant: &TenantId) -> SnapshotState {
    let mut state = miner.snapshot_state(tenant);
    state.wal_high_water = miner.folded_horizon(tenant).cloned();
    state
}

/// `state` as a restore must capture it back: every field the payload
/// carries, in the writer's canonical form. The only rewrites are the ones
/// the format documents: lists that are sets come back sorted and
/// deduplicated, an empty provenance is the pre-RFC 0050 default (`{Mined}`
/// on a leaf, `{UpstreamDerived}` on an owned adoption), an owned adoption
/// has no tree leaf and so is version 1, and a tree-backed adoption carries
/// no provenance or associations of its own (its leaf does).
fn canonical(state: &SnapshotState) -> SnapshotState {
    let mut leaves: Vec<LeafRecord> = state
        .leaves
        .iter()
        .map(|leaf| LeafRecord {
            slot_types: leaf.slot_types.iter().map(|slot| type_set(slot)).collect(),
            provenance: provenance_set(&leaf.provenance, ProvenanceRecord::Mined),
            upstream_associations: string_set(&leaf.upstream_associations),
            ..leaf.clone()
        })
        .collect();
    leaves.sort_by_key(|leaf| leaf.template_id);

    let mut structured_templates = state.structured_templates.clone();
    structured_templates.sort_by_key(|record| record.template_id);

    let mut adopted_templates: Vec<AdoptedTemplateRecord> = state
        .adopted_templates
        .iter()
        .map(|record| {
            if record.owned {
                AdoptedTemplateRecord {
                    template_version: 1,
                    provenance: provenance_set(
                        &record.provenance,
                        ProvenanceRecord::UpstreamDerived,
                    ),
                    upstream_associations: string_set(&record.upstream_associations),
                    ..record.clone()
                }
            } else {
                AdoptedTemplateRecord {
                    provenance: Vec::new(),
                    upstream_associations: Vec::new(),
                    upstream_association_overflow: 0,
                    ..record.clone()
                }
            }
        })
        .collect();
    adopted_templates
        .sort_by(|a, b| (a.template_id, &a.canonical).cmp(&(b.template_id, &b.canonical)));

    SnapshotState {
        leaves,
        structured_templates,
        wal_high_water: state.wal_high_water.clone(),
        adopted_templates,
    }
}

/// A slot's recorded types as a set, in `ParamType` declaration order with
/// the reader-side `Unknown(n)` ordinals last, which the format promises to
/// carry verbatim.
fn type_set(types: &[ParamTypeRecord]) -> Vec<ParamTypeRecord> {
    let rank = |t: &ParamTypeRecord| match t {
        ParamTypeRecord::Ip => (0, 0),
        ParamTypeRecord::Uuid => (1, 0),
        ParamTypeRecord::Num => (2, 0),
        ParamTypeRecord::Hex => (3, 0),
        ParamTypeRecord::Ts => (4, 0),
        ParamTypeRecord::Path => (5, 0),
        ParamTypeRecord::Str => (6, 0),
        ParamTypeRecord::Overflow => (7, 0),
        ParamTypeRecord::Unknown(n) => (8, *n),
    };
    let mut set = types.to_vec();
    set.sort_by_key(rank);
    set.dedup();
    set
}

/// A provenance list as a set in declaration order; empty reads as
/// `default`.
fn provenance_set(
    provenance: &[ProvenanceRecord],
    default: ProvenanceRecord,
) -> Vec<ProvenanceRecord> {
    let rank = |p: &ProvenanceRecord| match p {
        ProvenanceRecord::Mined => 0,
        ProvenanceRecord::UpstreamDerived => 1,
        ProvenanceRecord::ProducerDeclared => 2,
    };
    if provenance.is_empty() {
        return vec![default];
    }
    let mut set = provenance.to_vec();
    set.sort_by_key(rank);
    set.dedup();
    set
}

fn string_set(strings: &[String]) -> Vec<String> {
    let mut set = strings.to_vec();
    set.sort_unstable();
    set.dedup();
    set
}

fn manifest(bytes: &[u8]) {
    let manifest = match Manifest::from_json(bytes) {
        Ok(manifest) => manifest,
        Err(ManifestError::Io(e)) => {
            panic!("RFC0015.11: Manifest::from_json reported an I/O error on bytes: {e}")
        }
        Err(_) => return,
    };
    if let Err(e) = manifest.validate() {
        panic!("RFC0015.11: Manifest::from_json accepted a manifest validate rejects: {e}");
    }
    let json = manifest
        .to_json()
        .unwrap_or_else(|e| panic!("RFC0015.13: a decoded manifest does not re-encode: {e}"));
    match Manifest::from_json(&json) {
        Ok(again) => {
            assert!(
                again == manifest,
                "RFC0015.13: the manifest did not survive its own encoding"
            );
            assert!(
                again.to_json().ok().as_deref() == Some(json.as_slice()),
                "RFC0015.13: manifest encode is not a byte fixpoint"
            );
        }
        Err(e) => panic!("RFC0015.13: the re-encoded manifest does not decode: {e}"),
    }
}

fn template_ids_high_water(bytes: &[u8]) {
    match high_water::parse(bytes) {
        Ok(id) => {
            assert!(
                id <= MAX_TEMPLATE_ID,
                "RFC0015.11: the high-water parsed to {id}, past the template-id domain"
            );
            round_trip_high_water(id);
        }
        Err(TemplateIdsError::Malformed { .. }) => {}
        Err(e) => panic!("RFC0015.11: the high-water parse failed with a non-decode error: {e}"),
    }
    // The encoder's side over the whole id domain, not only the ids that
    // happen to parse.
    if let Some(word) = bytes.first_chunk::<8>() {
        round_trip_high_water(u64::from_le_bytes(*word) % (MAX_TEMPLATE_ID + 1));
    }
}

fn round_trip_high_water(id: u64) {
    let encoded = high_water::encode(id);
    match high_water::parse(&encoded) {
        Ok(again) => assert_eq!(again, id, "RFC0015.13: the high-water did not round-trip"),
        Err(e) => panic!("RFC0015.13: the encoded high-water {id} does not parse: {e}"),
    }
}

fn template_map(frame: &[u8]) {
    let tenant = TenantId::new(TENANT);
    let map = match TemplateMap::from_artifact_bytes(frame, &tenant) {
        Ok(ArtifactRead::Valid(map)) => map,
        Ok(ArtifactRead::Torn { .. }) => return,
        Ok(ArtifactRead::UnknownVersion { format_version }) => {
            assert_ne!(
                format_version, TEMPLATE_MAP_FORMAT_VERSION,
                "RFC0015.11: the reader's own format_version classified UnknownVersion"
            );
            return;
        }
        Ok(other) => panic!("RFC0015.11: unexpected template-map disposition {other:?}"),
        Err(QueryError::Storage { detail }) if detail.contains(" claims tenant ") => return,
        Err(e) => panic!("RFC0015.11: a template-map read failed with {e}"),
    };

    let encoded = map
        .to_artifact_bytes()
        .unwrap_or_else(|e| panic!("RFC0015.13: a Valid template map does not re-encode: {e}"));
    let again = match TemplateMap::from_artifact_bytes(&encoded, &tenant) {
        Ok(ArtifactRead::Valid(again)) => again,
        Ok(other) => panic!("RFC0015.13: a re-encoded template map reads as {other:?}"),
        Err(e) => panic!("RFC0015.13: a re-encoded template map fails to read: {e}"),
    };
    assert!(
        again.folded_files() == map.folded_files()
            && again.registry() == map.registry()
            && again.alias_map().classes(&tenant) == map.alias_map().classes(&tenant),
        "RFC0015.13: the template map did not survive its own encoding"
    );
    assert!(
        again.to_artifact_bytes().ok().as_deref() == Some(encoded.as_slice()),
        "RFC0015.13: template-map encode is not a byte fixpoint"
    );
}

/// The input as a template-map frame, compressed the way the writer does.
fn compress(json: &[u8]) -> Vec<u8> {
    zstd::encode_all(json, zstd::DEFAULT_COMPRESSION_LEVEL)
        .unwrap_or_else(|e| panic!("zstd compression of a byte slice failed: {e}"))
}
