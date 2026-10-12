//! RFC 0033 §3.3 amendment (2026-10-12) — the fold state every full fold
//! records in the published artifact, which the incremental path extends.

use std::path::Path;

use crate::common::{TS0, at, compaction, rejected_degenerate, widened, write_audit};
use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
use ourios_core::tenant::TenantId;
use ourios_querier::{
    ArtifactRead, CacheOutcome, StoreRef, TEMPLATE_MAP_FILENAME, TemplateMap, load_or_derive,
};
use tempfile::TempDir;

const TENANT: &str = "T";

fn created(template_id: u64, ts_ns: u64, template: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: at(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: template.to_string(),
            },
        },
    }
}

fn published(bucket: &Path, tenant: &TenantId) -> TemplateMap {
    let path = bucket
        .join("audit")
        .join(format!("tenant_id={TENANT}"))
        .join(TEMPLATE_MAP_FILENAME);
    let bytes = std::fs::read(&path).expect("read published artifact");
    match TemplateMap::from_artifact_bytes(&bytes, tenant).expect("read") {
        ArtifactRead::Valid(map) => map,
        other => panic!("the published artifact must be Valid, got {other:?}"),
    }
}

/// A full fold records the greatest fold-relevant timestamp (a later
/// rejection or compaction event does not count), the binding conflicts,
/// and depth 0, and the write-through publishes them.
#[test]
fn full_fold_records_and_publishes_the_fold_state() {
    let bucket = TempDir::new().expect("temp dir");
    let tenant = TenantId::new(TENANT);
    write_audit(
        bucket.path(),
        &[
            created(7, TS0, "user <*> logged in"),
            created(7, TS0 + 5, "user <*> logged out"),
            widened(TENANT, 3, 1, TS0 + 9),
            rejected_degenerate(TENANT, 3, 2, TS0 + 50),
            compaction(TENANT, TS0 + 60),
        ],
    );

    let (map, _, outcome) =
        load_or_derive(StoreRef::Local(bucket.path()), &tenant).expect("acquire");
    assert!(
        matches!(outcome, CacheOutcome::Miss { .. }),
        "got {outcome:?}"
    );

    let state = map
        .fold_state()
        .expect("a full fold records its fold state");
    assert_eq!(state.high_water_ns(), Some(TS0 + 9));
    assert_eq!(state.binding_conflicts(), &[(7, 1)]);
    assert_eq!(state.incremental_depth(), 0);

    let stored = published(bucket.path(), &tenant);
    assert_eq!(stored.fold_state(), map.fold_state());
}

/// A tenant whose audit stream holds nothing fold-relevant folds to an
/// empty map with a `null` high-water.
#[test]
fn a_fold_with_nothing_relevant_has_no_high_water() {
    let bucket = TempDir::new().expect("temp dir");
    let tenant = TenantId::new(TENANT);
    write_audit(bucket.path(), &[compaction(TENANT, TS0)]);

    let (map, _, _) = load_or_derive(StoreRef::Local(bucket.path()), &tenant).expect("acquire");

    let state = map.fold_state().expect("fold state");
    assert_eq!(state.high_water_ns(), None);
    assert!(state.binding_conflicts().is_empty());
    assert!(map.registry().is_empty());
}
