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
//!   bytes are a fixpoint too). A restored snapshot captures back every
//!   leaf's template, version, key and wildcard route, its structured map
//!   and its horizon (#892 was a leaf the writer could not place again), and
//!   capture → restore → capture is a fixpoint.
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
    self, LegacyMark, RecoveryOutcome, SNAPSHOT_VERSION, SnapshotState, TokenRecord, load_snapshot,
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
    let legacy = snapshot::legacy_v1_mark(bytes);
    assert_eq!(
        matches!(legacy, LegacyMark::NotLegacy),
        bytes.first() != Some(&LEGACY_SNAPSHOT_VERSION),
        "RFC0015.11: legacy_v1_mark classified a byte 0 of {:?} as {legacy:?}",
        bytes.first(),
    );

    let state = match snapshot::recover(Some(bytes)) {
        (Some(state), RecoveryOutcome::Restored) => state,
        (None, RecoveryOutcome::UnknownOrCorruptDiscarded) => {
            assert!(
                load_snapshot(bytes).is_err(),
                "RFC0015.11: recover discarded a snapshot load_snapshot reads"
            );
            return;
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

    let encoded = snapshot::snapshot(&state)
        .unwrap_or_else(|e| panic!("RFC0015.13: a decoded snapshot does not re-encode: {e}"));
    match load_snapshot(&encoded) {
        Ok(again) => assert!(
            again == state,
            "RFC0015.13: the snapshot payload did not survive its own encoding"
        ),
        Err(e) => panic!("RFC0015.13: the re-encoded snapshot does not decode: {e}"),
    }

    let tenant = TenantId::new(TENANT);
    let mut miner = MinerCluster::new(MinerConfig::default());
    if let Err(e) = miner.restore_tenant(&tenant, &state) {
        assert!(
            miner.tenant_ids().is_empty() && miner.template_count(&tenant) == 0,
            "RFC0015.12: a rejected restore ({e:?}) left state behind for the tenant"
        );
        assert_eq!(
            miner.highest_allocated(),
            0,
            "RFC0015.12: a rejected restore ({e:?}) moved the template-id allocator"
        );
        return;
    }
    let restored = capture(&miner, &tenant);
    assert!(
        leaf_identities(&restored) == leaf_identities(&state),
        "RFC0015.13: a restored snapshot does not capture back the leaves it restored \
         (template, version, key or wildcard route changed)"
    );
    assert!(
        structured_keys(&restored) == structured_keys(&state),
        "RFC0015.13: a restored snapshot does not capture back its structured-template map"
    );
    assert!(
        restored.wal_high_water == state.wal_high_water,
        "RFC0015.13: a restored snapshot does not capture back its folded horizon"
    );
    let mut again = MinerCluster::new(MinerConfig::default());
    if let Err(e) = again.restore_tenant(&tenant, &restored) {
        panic!("RFC0015.13: the snapshot of a restored tree does not restore: {e:?}");
    }
    assert!(
        capture(&again, &tenant) == restored,
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

/// What restore must reproduce exactly for each leaf: the fields it places
/// and keys the leaf by. Slot types, provenance and associations are
/// normalised on restore (an empty provenance reads as `{Mined}`), so they
/// are held to the capture → restore → capture fixpoint instead.
type LeafIdentity<'a> = (
    u64,
    &'a [TokenRecord],
    u32,
    u8,
    Option<&'a str>,
    &'a [usize],
);

/// Sorted by `template_id`, which a restore that succeeded holds unique.
fn leaf_identities(state: &SnapshotState) -> Vec<LeafIdentity<'_>> {
    let mut leaves: Vec<_> = state
        .leaves
        .iter()
        .map(|leaf| {
            (
                leaf.template_id,
                leaf.template.as_slice(),
                leaf.template_version,
                leaf.severity_number,
                leaf.scope_name.as_deref(),
                leaf.wildcard_routed.as_slice(),
            )
        })
        .collect();
    leaves.sort_by_key(|leaf| leaf.0);
    leaves
}

fn structured_keys(state: &SnapshotState) -> Vec<(u64, u8, Option<&str>, Option<&str>)> {
    let mut keys: Vec<_> = state
        .structured_templates
        .iter()
        .map(|s| {
            (
                s.template_id,
                s.severity_number,
                s.scope_name.as_deref(),
                s.event_name.as_deref(),
            )
        })
        .collect();
    keys.sort_unstable();
    keys
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
