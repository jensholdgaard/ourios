//! RFC 0033 §3.3 amendment (2026-10-12) — the guarded incremental fold:
//! Scenario RFC0033.10 (incremental ≡ full fold, property), RFC0033.11
//! (the guard falls back, never guesses) and RFC0033.12 (late files and
//! concurrent writers) on the local backend and a conditional-put store.
//!
//! Every assertion compares against `derive_template_map`, the full fold
//! over the same listing, which is the source of truth the incremental
//! path must reproduce.

use std::path::{Path, PathBuf};

use crate::common::{
    DEFAULT_WINDOW_NS, NOW, TS0, at, localstack_s3, simple, write_all, write_audit,
};
use ourios_core::alias::ActorId;
use ourios_core::audit::AuditSink;
use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
use ourios_core::tenant::TenantId;
use ourios_parquet::{AuditWriter, ParquetAuditSink, Store, derive_audit_partition};
use ourios_querier::{
    ArtifactRead, CacheOutcome, INCREMENTAL_DEPTH_CAP, PublishOutcome, Querier, QueryError,
    StoreRef, TEMPLATE_MAP_FILENAME, TemplateMap, derive_template_map, load_or_derive,
};
use proptest::prelude::*;
use tempfile::TempDir;

const TENANT: &str = "T";
const ID_POOL: u64 = 4;
/// Relevant events of segment `j` sit within 3 ns of
/// `SEGMENT_BASE + j * SEGMENT_STEP`, so each segment is strictly later
/// than every earlier one.
const SEGMENT_BASE: u64 = TS0 + 100;
const SEGMENT_STEP: u64 = 10;

fn tenant() -> TenantId {
    TenantId::new(TENANT)
}

fn event(ts_ns: u64, payload: AuditPayload) -> AuditEvent {
    AuditEvent {
        tenant_id: tenant(),
        timestamp: at(ts_ns),
        payload,
    }
}

fn template(template_id: u64, change: TemplateChange) -> AuditPayload {
    AuditPayload::Template {
        template_id,
        triggering_line_hash: hash_triggering_line(b"line"),
        triggering_line_sample: None,
        change,
    }
}

fn created(template_id: u64, text: &str) -> AuditPayload {
    template(
        template_id,
        TemplateChange::Created {
            new_template: text.to_string(),
        },
    )
}

fn compaction() -> AuditPayload {
    AuditPayload::Compaction {
        partition: "year=2026/month=06/day=01/hour=00".to_string(),
        input_files: vec!["a.parquet".to_string()],
        output_file: "b.parquet".to_string(),
        generation: 1,
        rows: 1,
    }
}

fn arb_template() -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![Just("<*>".to_string()), "[a-c]{1,2}"], 1..4)
        .prop_map(|tokens| tokens.join(" "))
}

fn actor() -> ActorId {
    ActorId::new("op-prop").expect("non-empty actor")
}

/// A fold-relevant payload: every template change that binds a key, and
/// alias assertions and retractions.
fn arb_relevant() -> impl Strategy<Value = AuditPayload> {
    let id = 1u64..=ID_POOL;
    prop_oneof![
        3 => (id.clone(), arb_template()).prop_map(|(id, t)| created(id, &t)),
        2 => (id.clone(), 1u32..3, arb_template()).prop_map(|(id, v, t)| template(
            id,
            TemplateChange::Widened {
                old_version: v,
                new_version: v + 1,
                old_template: "<*>".to_string(),
                new_template: t,
                positions_widened: vec![0],
            },
        )),
        1 => (id.clone(), 1u32..3, arb_template()).prop_map(|(id, v, t)| template(
            id,
            TemplateChange::TypeExpanded {
                old_version: v,
                new_version: v + 1,
                old_template: t.clone(),
                new_template: t,
                slots_expanded: Vec::new(),
            },
        )),
        1 => (id.clone(), arb_template()).prop_map(|(id, t)| template(
            id,
            TemplateChange::Adopted {
                template_version: 1,
                new_template: t,
            },
        )),
        2 => (id.clone(), prop::collection::vec(1u64..=ID_POOL, 1..3)).prop_map(
            |(representative_id, member_ids)| AuditPayload::AliasAsserted {
                representative_id,
                member_ids,
                actor: actor(),
                reason: String::new(),
            }
        ),
        1 => id.prop_map(|representative_id| AuditPayload::AliasRetracted {
            representative_id,
            member_ids: Vec::new(),
            actor: actor(),
            reason: String::new(),
        }),
    ]
}

/// A payload neither fold consumes: a degenerate-widening rejection or a
/// compaction event.
fn arb_irrelevant() -> impl Strategy<Value = AuditPayload> {
    prop_oneof![
        (1u64..=ID_POOL).prop_map(|id| template(
            id,
            TemplateChange::RejectedDegenerate {
                version: 1,
                current_template: "a".to_string(),
                would_be_template: "<*>".to_string(),
                would_be_positions: vec![0],
            },
        )),
        Just(compaction()),
    ]
}

/// One segment of a history: `(offset, payload, relevant)` triples in file
/// order. Offsets are tie-heavy, so equal timestamps are common.
fn arb_segment() -> impl Strategy<Value = Vec<(u64, AuditPayload, bool)>> {
    prop::collection::vec(
        prop_oneof![
            4 => (0u64..4, arb_relevant()).prop_map(|(o, p)| (o, p, true)),
            1 => (0u64..4, arb_irrelevant()).prop_map(|(o, p)| (o, p, false)),
        ],
        1..7,
    )
}

/// Segment `j`'s events with timestamps that keep every fold-relevant
/// event strictly later than any earlier segment's. Irrelevant events sit
/// before every segment, so they are older than the high-water.
fn guarded(j: u64, segment: &[(u64, AuditPayload, bool)]) -> Vec<AuditEvent> {
    segment
        .iter()
        .map(|(offset, payload, relevant)| {
            let ts = if *relevant {
                SEGMENT_BASE + j * SEGMENT_STEP + offset
            } else {
                TS0 + offset
            };
            event(ts, payload.clone())
        })
        .collect()
}

/// Segment events with timestamps drawn across the whole history, so the
/// guard may pass or reject.
fn unconstrained(segment: &[(u64, AuditPayload, bool)], spread: u64) -> Vec<AuditEvent> {
    segment
        .iter()
        .enumerate()
        .map(|(i, (offset, payload, _))| {
            let ts = SEGMENT_BASE + (offset * 7 + i as u64 * 3) % spread;
            event(ts, payload.clone())
        })
        .collect()
}

fn artifact_path(bucket: &Path) -> PathBuf {
    bucket
        .join("audit")
        .join(format!("tenant_id={TENANT}"))
        .join(TEMPLATE_MAP_FILENAME)
}

fn stored(bucket: &Path) -> TemplateMap {
    let bytes = std::fs::read(artifact_path(bucket)).expect("read artifact");
    match TemplateMap::from_artifact_bytes(&bytes, &tenant()).expect("read") {
        ArtifactRead::Valid(map) => map,
        other => panic!("the stored artifact must be Valid, got {other:?}"),
    }
}

/// Rewrite the stored artifact's JSON body in place, recompressed.
fn rewrite_artifact(bucket: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = artifact_path(bucket);
    let json = zstd::decode_all(std::fs::read(&path).expect("read").as_slice()).expect("decode");
    let mut body: serde_json::Value = serde_json::from_slice(&json).expect("json");
    edit(&mut body);
    let bytes = zstd::encode_all(serde_json::to_vec(&body).expect("encode").as_slice(), 3)
        .expect("compress");
    std::fs::write(&path, bytes).expect("write artifact");
}

/// The canonical decompressed JSON body with `incremental_depth` set to 0.
fn normalized(map: &TemplateMap) -> serde_json::Value {
    let bytes = map.to_artifact_bytes().expect("serialize");
    let json = zstd::decode_all(bytes.as_slice()).expect("decode");
    let mut body: serde_json::Value = serde_json::from_slice(&json).expect("json");
    body["fold_state"]["incremental_depth"] = 0.into();
    body
}

fn depth(map: &TemplateMap) -> u32 {
    map.fold_state().expect("fold state").incremental_depth()
}

fn full_fold(bucket: &Path) -> TemplateMap {
    derive_template_map(StoreRef::Local(bucket), &tenant())
        .expect("full fold")
        .0
}

fn acquire(bucket: &Path) -> (TemplateMap, CacheOutcome) {
    let (map, _, outcome) = load_or_derive(StoreRef::Local(bucket), &tenant()).expect("acquire");
    (map, outcome)
}

/// Fold the current audit set in full and publish it directly (no
/// abstention), as the base an incremental step extends.
fn publish_full_fold(bucket: &Path) -> TemplateMap {
    let map = full_fold(bucket);
    map.publish(StoreRef::Local(bucket), None).expect("publish");
    map
}

/// `resolves_to(probe)` row count through the public surface, with the
/// alias map either derived from storage or the one `aliases` carries.
fn resolves_rows(bucket: &Path, probe: u64, aliases: Option<&TemplateMap>) -> u64 {
    let query = ourios_querier::dsl::parse(&format!("resolves_to({probe})")).expect("parse");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime
        .block_on(Querier::new(bucket).run_query(
            &query,
            &tenant(),
            NOW,
            DEFAULT_WINDOW_NS,
            aliases.map(TemplateMap::alias_map),
        ))
        .expect("query")
        .rows
}

fn assert_equivalent(acquired: &TemplateMap, full: &TemplateMap) -> Result<(), TestCaseError> {
    assert_same_folds(acquired, full)?;
    assert_same_fold_state(acquired, full)
}

/// The registry, alias map and frontier are the full fold's.
fn assert_same_folds(acquired: &TemplateMap, full: &TemplateMap) -> Result<(), TestCaseError> {
    prop_assert_eq!(acquired.registry(), full.registry());
    prop_assert_eq!(
        acquired.alias_map().classes(&tenant()),
        full.alias_map().classes(&tenant())
    );
    prop_assert_eq!(acquired.folded_files(), full.folded_files());
    Ok(())
}

/// The high-water, binding conflicts and canonical body (depth aside) are
/// the full fold's.
fn assert_same_fold_state(acquired: &TemplateMap, full: &TemplateMap) -> Result<(), TestCaseError> {
    let (a, f) = (
        acquired.fold_state().expect("fold state"),
        full.fold_state().expect("fold state"),
    );
    prop_assert_eq!(a.high_water_ns(), f.high_water_ns());
    prop_assert_eq!(a.binding_conflicts(), f.binding_conflicts());
    prop_assert_eq!(normalized(acquired), normalized(full));
    Ok(())
}

/// `acquired` holds the full fold's registry and frontier at
/// `expected_depth`.
fn assert_folds_to(acquired: &TemplateMap, full: &TemplateMap, expected_depth: u32, label: &str) {
    assert_eq!(acquired.registry(), full.registry(), "{label}: registry");
    assert_eq!(
        acquired.folded_files(),
        full.folded_files(),
        "{label}: frontier"
    );
    assert_eq!(depth(acquired), expected_depth, "{label}: depth");
}

/// The stored local artifact is `expected` at `expected_depth`.
fn assert_stored(bucket: &Path, expected: &TemplateMap, expected_depth: u32, label: &str) {
    assert_folds_to(&stored(bucket), expected, expected_depth, label);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: ourios_testgen::proptest_cases(16),
        ..ProptestConfig::default()
    })]

    /// Scenario RFC0033.10 — incremental fold ≡ full fold (property).
    /// See `docs/rfcs/0033-cached-template-map.md` §5.
    #[test]
    fn rfc0033_10_incremental_fold_equals_full_fold(
        segments in prop::collection::vec(arb_segment(), 2..5),
    ) {
        let bucket = TempDir::new().expect("temp dir");
        let data: Vec<_> = (1..=ID_POOL).map(|id| simple(TENANT, id, TS0 + id)).collect();
        write_all(bucket.path(), &data);
        write_audit(bucket.path(), &guarded(0, &segments[0]));
        publish_full_fold(bucket.path());

        for (step, segment) in (1u64..).zip(&segments[1..]) {
            write_audit(bucket.path(), &guarded(step, segment));
            let (acquired, outcome) = acquire(bucket.path());
            prop_assert_eq!(outcome, CacheOutcome::StaleRefreshed);
            prop_assert_eq!(u64::from(depth(&acquired)), step, "the incremental path ran");
            let full = full_fold(bucket.path());
            assert_equivalent(&acquired, &full)?;
            for probe in 1..=ID_POOL {
                prop_assert_eq!(
                    resolves_rows(bucket.path(), probe, Some(&acquired)),
                    resolves_rows(bucket.path(), probe, Some(&full)),
                );
            }
            acquired.publish(StoreRef::Local(bucket.path()), None).expect("publish the step");
        }
    }

    /// RFC0033.10 companion: with timestamps drawn freely across the
    /// history, the guard passes or rejects, and the acquired map always
    /// equals the full fold.
    #[test]
    fn rfc0033_10_acquisition_equals_full_fold_whatever_the_guard_decides(
        segments in prop::collection::vec(arb_segment(), 2..5),
        spread in 2u64..40,
    ) {
        let bucket = TempDir::new().expect("temp dir");
        write_audit(bucket.path(), &unconstrained(&segments[0], spread));
        publish_full_fold(bucket.path());
        for segment in &segments[1..] {
            write_audit(bucket.path(), &unconstrained(segment, spread));
            let (acquired, _) = acquire(bucket.path());
            assert_equivalent(&acquired, &full_fold(bucket.path()))?;
            acquired.publish(StoreRef::Local(bucket.path()), None).expect("publish");
        }
    }
}

/// A tenant with a published full-fold artifact at high-water `TS0 + 10`.
fn based() -> TempDir {
    let bucket = TempDir::new().expect("temp dir");
    write_audit(
        bucket.path(),
        &[
            event(TS0, created(1, "user <*>")),
            event(TS0 + 10, created(2, "job <*>")),
        ],
    );
    publish_full_fold(bucket.path());
    bucket
}

/// Scenario RFC0033.11 — a delta event before or at the high-water, a
/// removed folded file, the depth cap and a missing fold state each fall
/// back to a full fold whose published artifact restarts at depth 0.
#[test]
fn rfc0033_11_each_guard_row_falls_back_to_a_full_fold() {
    type Setup = fn(&Path);
    let cases: [(&str, Setup); 5] = [
        ("before the high-water", |b| {
            write_audit(b, &[event(TS0 + 9, created(3, "a"))]);
        }),
        ("at the high-water", |b| {
            write_audit(b, &[event(TS0 + 10, created(3, "a"))]);
        }),
        ("files removed", |b| {
            let folded = stored(b).folded_files()[0].clone();
            let root = b.join("audit").join(format!("tenant_id={TENANT}"));
            std::fs::remove_file(root.join(folded)).expect("remove a folded file");
            write_audit(b, &[event(TS0 + 20, created(3, "a"))]);
        }),
        ("depth cap", |b| {
            rewrite_artifact(b, |body| {
                body["fold_state"]["incremental_depth"] = INCREMENTAL_DEPTH_CAP.into();
            });
            write_audit(b, &[event(TS0 + 20, created(3, "a"))]);
        }),
        ("no fold state", |b| {
            rewrite_artifact(b, |body| {
                body.as_object_mut().expect("object").remove("fold_state");
            });
            write_audit(b, &[event(TS0 + 20, created(3, "a"))]);
        }),
    ];
    for (label, setup) in cases {
        let bucket = based();
        setup(bucket.path());
        let (acquired, outcome) = acquire(bucket.path());
        assert_eq!(outcome, CacheOutcome::StaleRefreshed, "{label}");
        let full = full_fold(bucket.path());
        assert_folds_to(&acquired, &full, 0, label);
        assert_stored(
            bucket.path(),
            &full,
            0,
            &format!("{label}: the stored artifact is the new full fold, not the base"),
        );
    }
}

/// RFC0033.11: a delta with only events neither fold consumes, whatever
/// their timestamps, folds incrementally and changes only the frontier and
/// the depth.
#[test]
fn rfc0033_11_irrelevant_delta_folds_incrementally() {
    let bucket = based();
    let before = stored(bucket.path());
    write_audit(bucket.path(), &[event(TS0, compaction())]);

    let (acquired, _) = acquire(bucket.path());

    assert_eq!(depth(&acquired), 1);
    assert_only_frontier_grew(&acquired, &before);
    assert_eq!(
        acquired.folded_files(),
        full_fold(bucket.path()).folded_files()
    );
}

/// `acquired` keeps `before`'s registry and high-water and folds exactly
/// one more file.
fn assert_only_frontier_grew(acquired: &TemplateMap, before: &TemplateMap) {
    assert_eq!(acquired.registry(), before.registry());
    assert_eq!(
        acquired
            .fold_state()
            .map(ourios_querier::FoldState::high_water_ns),
        before
            .fold_state()
            .map(ourios_querier::FoldState::high_water_ns),
    );
    assert_eq!(
        acquired.folded_files().len(),
        before.folded_files().len() + 1
    );
}

/// RFC0033.11: a `null` high-water (nothing fold-relevant folded) admits
/// any delta.
#[test]
fn rfc0033_11_null_high_water_admits_any_delta() {
    let bucket = TempDir::new().expect("temp dir");
    write_audit(bucket.path(), &[event(TS0 + 50, compaction())]);
    publish_full_fold(bucket.path());
    write_audit(bucket.path(), &[event(TS0, created(1, "user <*>"))]);

    let (acquired, _) = acquire(bucket.path());

    assert_eq!(depth(&acquired), 1);
    assert_eq!(acquired.registry(), full_fold(bucket.path()).registry());
}

/// RFC0033.11: an unpublished incremental result extended from a base at
/// depth 255 leaves the stored artifact unchanged; every acquisition folds
/// from that base at depth 256, never more; once a depth-256 result is
/// published, the next stale acquisition is a full fold.
#[cfg(unix)]
#[test]
fn rfc0033_11_unpublished_results_never_exceed_the_cap() {
    use std::os::unix::fs::PermissionsExt;

    let bucket = based();
    rewrite_artifact(bucket.path(), |body| {
        body["fold_state"]["incremental_depth"] = (INCREMENTAL_DEPTH_CAP - 1).into();
    });
    write_audit(bucket.path(), &[event(TS0 + 20, created(3, "a"))]);
    let tenant_dir = bucket
        .path()
        .join("audit")
        .join(format!("tenant_id={TENANT}"));
    let stored_bytes = std::fs::read(artifact_path(bucket.path())).expect("read");
    let set_mode = |mode| {
        std::fs::set_permissions(&tenant_dir, std::fs::Permissions::from_mode(mode))
            .expect("chmod tenant dir");
    };

    set_mode(0o555);
    for _ in 0..2 {
        let (acquired, _) = acquire(bucket.path());
        assert_folds_to(
            &acquired,
            &full_fold(bucket.path()),
            INCREMENTAL_DEPTH_CAP,
            "unpublished",
        );
    }
    let after = std::fs::read(artifact_path(bucket.path())).expect("read");
    set_mode(0o755);
    assert_eq!(
        after, stored_bytes,
        "a failing publish left the stored artifact unchanged"
    );

    let (published, _) = acquire(bucket.path());
    assert_stored(
        bucket.path(),
        &published,
        INCREMENTAL_DEPTH_CAP,
        "the depth-256 result is published",
    );
    write_audit(bucket.path(), &[event(TS0 + 30, created(4, "b"))]);
    let (capped, _) = acquire(bucket.path());
    assert_eq!(depth(&capped), 0, "the cap forces a full fold");
}

/// RFC0033.11: a delta file holding another tenant's row fails the
/// acquisition exactly as the full scan does.
#[test]
fn rfc0033_11_foreign_tenant_row_in_the_delta_fails_loudly() {
    let bucket = based();
    let before = std::fs::read(artifact_path(bucket.path())).expect("read");
    let foreign = AuditEvent {
        tenant_id: TenantId::new("U"),
        ..event(TS0 + 20, created(3, "a"))
    };
    write_audit(bucket.path(), &[foreign]);
    let foreign_root = bucket.path().join("audit").join("tenant_id=U");
    let own_root = bucket
        .path()
        .join("audit")
        .join(format!("tenant_id={TENANT}"));
    move_tree(&foreign_root, &own_root);

    let incremental = load_or_derive(StoreRef::Local(bucket.path()), &tenant())
        .expect_err("a foreign row in the delta must fail the acquisition");
    let full = derive_template_map(StoreRef::Local(bucket.path()), &tenant())
        .expect_err("and the full scan");

    let detail = |e: &QueryError| match e {
        QueryError::Storage { detail } => detail.clone(),
        other => panic!("expected a storage error, got {other:?}"),
    };
    assert!(detail(&incremental).contains("carries a row for tenant U"));
    assert!(detail(&full).contains("carries a row for tenant U"));
    assert_eq!(
        std::fs::read(artifact_path(bucket.path())).expect("read"),
        before,
        "nothing was published",
    );
}

/// Move every file under `from` to the same relative path under `to`.
fn move_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dir") {
        let path = entry.expect("entry").path();
        let target = to.join(path.file_name().expect("name"));
        if path.is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            move_tree(&path, &target);
        } else {
            std::fs::rename(&path, &target).expect("move");
        }
    }
}

/// RFC0033.11: the pre-amendment reader, whose body struct denies no
/// unknown fields, reads an artifact carrying `fold_state`.
#[test]
fn rfc0033_11_pre_amendment_reader_tolerates_fold_state() {
    #[derive(serde::Deserialize)]
    #[expect(
        dead_code,
        reason = "mirrors the pre-amendment body for the parse only"
    )]
    struct PreAmendmentBody {
        format_version: u32,
        tenant_id: String,
        folded_files: Vec<String>,
        registry: Vec<serde_json::Value>,
        alias_map: Vec<serde_json::Value>,
    }
    let bucket = based();
    let json = zstd::decode_all(
        std::fs::read(artifact_path(bucket.path()))
            .expect("read")
            .as_slice(),
    )
    .expect("decode");
    assert!(String::from_utf8_lossy(&json).contains("fold_state"));
    serde_json::from_slice::<PreAmendmentBody>(&json).expect("the old reader parses it");
}

/// RFC0033.12 (local): an older fold renamed over a newer one only makes
/// the artifact staler; the next acquisition still equals the full fold.
#[test]
fn rfc0033_12_older_fold_over_newer_is_only_staler() {
    let bucket = based();
    let older = stored(bucket.path());
    write_audit(bucket.path(), &[event(TS0 + 20, created(3, "a"))]);
    let (newer, _) = acquire(bucket.path());
    assert_eq!(depth(&stored(bucket.path())), depth(&newer));

    older
        .publish(StoreRef::Local(bucket.path()), None)
        .expect("older writer wins the rename");
    write_audit(bucket.path(), &[event(TS0 + 30, created(4, "b"))]);
    let (acquired, outcome) = acquire(bucket.path());

    assert_eq!(outcome, CacheOutcome::StaleRefreshed);
    assert_folds_to(
        &acquired,
        &full_fold(bucket.path()),
        depth(&older) + 1,
        "extended from the older fold",
    );
}

/// RFC0033.12 (local): a late file with a backdated fold-relevant event
/// forces a full fold whose answer includes it.
#[test]
fn rfc0033_12_backdated_late_file_forces_a_full_fold() {
    let bucket = based();
    write_audit(bucket.path(), &[event(TS0 + 20, created(3, "a"))]);
    let (incremental, _) = acquire(bucket.path());
    assert_eq!(depth(&incremental), 1);
    assert_stored(
        bucket.path(),
        &incremental,
        1,
        "the late file lands on a published incremental base",
    );

    write_audit(
        bucket.path(),
        &[event(TS0 + 5, created(1, "user <*> late"))],
    );
    let (acquired, _) = acquire(bucket.path());

    assert_folds_to(&acquired, &full_fold(bucket.path()), 0, "late file");
    assert_eq!(
        acquired
            .fold_state()
            .map(|s| s.binding_conflicts().to_vec()),
        Some(vec![(1, 1)]),
        "the late binding conflicts with the earlier one",
    );
}

/// One audit file holding every event in `events` (the production sink
/// writes a file per event).
fn write_audit_file(bucket: &Path, events: &[AuditEvent]) {
    let store = Store::local(bucket).expect("store");
    let partition = derive_audit_partition(&events[0]).expect("partition");
    let mut writer = AuditWriter::open_in(&store, partition).expect("open audit writer");
    writer.append_events(events).expect("append");
    writer.close().expect("close");
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// `n` creations from `first`, each binding a pseudo-random 64-char
/// template of control characters. JSON escapes each as `\u00XX`, so the
/// compressed artifact outgrows the Parquet column carrying the same text,
/// which is what lets a result reach the acquisition bytes it is weighed
/// against.
fn bulky(first: u64, n: u64, ts_ns: u64) -> Vec<AuditEvent> {
    let mut x = 0x9E37_79B9_7F4A_7C15 ^ first;
    (first..first + n)
        .map(|id| {
            let text: String = (0..64)
                .map(|_| char::from(1 + u8::try_from(xorshift(&mut x) % 8).expect("< 8")))
                .collect();
            event(ts_ns, created(id, &text))
        })
        .collect()
}

fn audit_bytes(bucket: &Path, keys: &[String]) -> u64 {
    let root = bucket.join("audit").join(format!("tenant_id={TENANT}"));
    keys.iter()
        .map(|k| {
            std::fs::metadata(root.join(k))
                .expect("stat audit file")
                .len()
        })
        .sum()
}

fn artifact_len(bucket: &Path) -> u64 {
    std::fs::metadata(artifact_path(bucket))
        .expect("stat artifact")
        .len()
}

/// One acquisition through the write-through, with what it cost and
/// whether it replaced the stored artifact.
struct Measured {
    map: TemplateMap,
    acquisition_bytes: u64,
    result_bytes: u64,
    published: bool,
}

fn acquire_measured(bucket: &Path) -> Measured {
    let before = std::fs::read(artifact_path(bucket)).expect("read");
    let (map, acquisition_bytes, outcome) =
        load_or_derive(StoreRef::Local(bucket), &tenant()).expect("acquire");
    assert_eq!(outcome, CacheOutcome::StaleRefreshed);
    let result = map.to_artifact_bytes().expect("serialize");
    let after = std::fs::read(artifact_path(bucket)).expect("read");
    let published = after == result;
    assert!(
        published || after == before,
        "the write-through publishes the result or leaves the artifact in place",
    );
    Measured {
        map,
        acquisition_bytes,
        result_bytes: result.len() as u64,
        published,
    }
}

/// What an acquisition is expected to have cost and done.
struct Expected {
    depth: u32,
    acquisition_bytes: u64,
    published: bool,
}

/// `m` matches `expected` and its map equals the full fold over `bucket`.
fn assert_measured(m: &Measured, bucket: &Path, expected: &Expected, label: &str) {
    assert_eq!(depth(&m.map), expected.depth, "{label}: depth");
    assert_eq!(
        m.acquisition_bytes, expected.acquisition_bytes,
        "{label}: acquisition bytes"
    );
    assert_eq!(m.published, expected.published, "{label}: published");
    assert_equivalent(&m.map, &full_fold(bucket)).expect("equals the full fold");
}

/// The listed keys `after` adds to `before`.
fn added(before: &[String], after: &[String]) -> Vec<String> {
    after
        .iter()
        .filter(|k| !before.contains(k))
        .cloned()
        .collect()
}

/// RFC0033.11 size boundary, incremental: the write-through weighs the
/// compressed result against the artifact GET plus the delta's bytes. A
/// result above the delta but below base plus delta publishes, where the
/// delta-only rule would have abstained; one at or above base plus delta
/// abstains.
#[test]
fn rfc0033_11_incremental_abstention_weighs_base_plus_delta() {
    let bucket = TempDir::new().expect("temp dir");
    write_audit_file(bucket.path(), &bulky(1, 1000, TS0 + 10));
    let base = publish_full_fold(bucket.path());
    let base_len = artifact_len(bucket.path());
    write_audit(bucket.path(), &[event(TS0 + 20, created(5000, "job <*>"))]);

    let m = acquire_measured(bucket.path());

    let delta = audit_bytes(
        bucket.path(),
        &added(base.folded_files(), m.map.folded_files()),
    );
    assert!(
        delta <= m.result_bytes && m.result_bytes < base_len + delta,
        "the publish window: delta={delta} result={} base={base_len}",
        m.result_bytes,
    );
    let expected = Expected {
        depth: 1,
        acquisition_bytes: base_len + delta,
        published: true,
    };
    assert_measured(&m, bucket.path(), &expected, "artifact GET + delta");

    let bucket = TempDir::new().expect("temp dir");
    write_audit(bucket.path(), &[event(TS0 + 10, created(1, "user <*>"))]);
    let base = publish_full_fold(bucket.path());
    let base_len = artifact_len(bucket.path());
    write_audit_file(bucket.path(), &bulky(2, 3000, TS0 + 20));

    let m = acquire_measured(bucket.path());

    let delta = audit_bytes(
        bucket.path(),
        &added(base.folded_files(), m.map.folded_files()),
    );
    assert!(
        m.result_bytes >= base_len + delta,
        "the abstain side: delta={delta} result={} base={base_len}",
        m.result_bytes,
    );
    let expected = Expected {
        depth: 1,
        acquisition_bytes: base_len + delta,
        published: false,
    };
    assert_measured(&m, bucket.path(), &expected, "artifact GET + delta");
}

/// RFC0033.11 size boundary, out-of-order fallback: the write-through
/// weighs the full fold's result against the artifact GET plus every audit
/// object fetched, the rejected delta read included. A result at or above
/// the folded audit bytes but below that total publishes, where the
/// folded-bytes rule would have abstained; one at or above it abstains.
#[test]
fn rfc0033_11_fallback_abstention_weighs_every_fetched_byte() {
    let bucket = TempDir::new().expect("temp dir");
    write_audit_file(bucket.path(), &bulky(1, 3000, TS0 + 10));
    let base = publish_full_fold(bucket.path());
    let base_len = artifact_len(bucket.path());
    write_audit(bucket.path(), &[event(TS0 + 5, created(9000, "late <*>"))]);

    let m = acquire_measured(bucket.path());

    let delta = audit_bytes(
        bucket.path(),
        &added(base.folded_files(), m.map.folded_files()),
    );
    let folded = audit_bytes(bucket.path(), m.map.folded_files());
    assert!(
        folded <= m.result_bytes && m.result_bytes < m.acquisition_bytes,
        "the publish window: folded={folded} result={} acquisition={}",
        m.result_bytes,
        m.acquisition_bytes,
    );
    let expected = Expected {
        depth: 0,
        acquisition_bytes: base_len + delta + folded,
        published: true,
    };
    let label = "the guard fell back: artifact GET + rejected delta read + full fold";
    assert_measured(&m, bucket.path(), &expected, label);

    // An emptied registry keeps the stored artifact Valid at the same
    // frontier and high-water while its GET stays small, so the bulky
    // audit set it names outweighs the whole acquisition.
    let bucket = TempDir::new().expect("temp dir");
    write_audit_file(bucket.path(), &bulky(1, 3000, TS0 + 10));
    let base = publish_full_fold(bucket.path());
    rewrite_artifact(bucket.path(), |body| {
        body["registry"] = serde_json::json!([]);
    });
    let base_len = artifact_len(bucket.path());
    write_audit(bucket.path(), &[event(TS0 + 5, created(9000, "late <*>"))]);

    let m = acquire_measured(bucket.path());

    let delta = audit_bytes(
        bucket.path(),
        &added(base.folded_files(), m.map.folded_files()),
    );
    let folded = audit_bytes(bucket.path(), m.map.folded_files());
    assert!(
        m.result_bytes >= m.acquisition_bytes,
        "the abstain side: result={} acquisition={}",
        m.result_bytes,
        m.acquisition_bytes,
    );
    let expected = Expected {
        depth: 0,
        acquisition_bytes: base_len + delta + folded,
        published: false,
    };
    let label = "the guard fell back: artifact GET + rejected delta read + full fold";
    assert_measured(&m, bucket.path(), &expected, label);
}

/// Persist `events` to `store`'s audit stream, one file per event.
fn write_audit_to(store: &Store, events: &[AuditEvent]) {
    let mut sink = ParquetAuditSink::new(store.clone());
    for e in events {
        sink.emit(e.clone());
    }
    assert_eq!(
        sink.write_failures(),
        0,
        "audit fixture events must persist"
    );
}

/// The Valid artifact `store` holds for the tenant, and its `ETag`.
fn held(store: &Store) -> (TemplateMap, String) {
    let key = format!("audit/tenant_id={TENANT}/{TEMPLATE_MAP_FILENAME}");
    let (bytes, e_tag) = store
        .get_with_etag_blocking_opt(&key)
        .expect("get")
        .expect("artifact present");
    match TemplateMap::from_artifact_bytes(&bytes, &tenant()).expect("read") {
        ArtifactRead::Valid(map) => (map, e_tag.expect("the store exposes an ETag")),
        other => panic!("the stored artifact must be Valid, got {other:?}"),
    }
}

/// `store` holds `expected`'s frontier at `expected_depth`.
fn assert_held(store: &Store, expected: &TemplateMap, expected_depth: u32, label: &str) {
    let (map, _) = held(store);
    assert_eq!(
        map.folded_files(),
        expected.folded_files(),
        "{label}: frontier"
    );
    assert_eq!(depth(&map), expected_depth, "{label}: depth");
}

/// `acquired` is at `expected_depth` and equals the full fold `full`.
fn assert_acquired(acquired: &TemplateMap, full: &TemplateMap, expected_depth: u32, label: &str) {
    assert_eq!(depth(acquired), expected_depth, "{label}: depth");
    assert_equivalent(acquired, full).expect("equals the full fold");
}

/// Write two creations to `store` and publish their full fold directly.
fn publish_remote_base(store: &Store) {
    let remote = StoreRef::Remote(store);
    write_audit_to(
        store,
        &[
            event(TS0, created(1, "user <*>")),
            event(TS0 + 10, created(2, "job <*>")),
        ],
    );
    let (base, _) = derive_template_map(remote, &tenant()).expect("full fold");
    assert_eq!(
        base.publish(remote, None).expect("publish the base"),
        PublishOutcome::Published,
    );
}

/// The RFC0033.12 arm on a conditional-put store: an incremental result
/// publishes at the observed `ETag`; a second querier that extended the
/// same stale artifact loses the CAS (`lost_race`) with its answer still
/// the full fold's; two queriers racing for real both answer correctly
/// and leave a Valid incremental artifact; and a backdated late file on
/// that published incremental base forces a full fold that includes it.
fn incremental_publish_resolves_by_cas(store: &Store) {
    let remote = StoreRef::Remote(store);
    let full = || derive_template_map(remote, &tenant()).expect("full fold").0;
    let acquire = || load_or_derive(remote, &tenant()).expect("acquire");
    publish_remote_base(store);

    write_audit_to(store, &[event(TS0 + 20, created(3, "a"))]);
    let (_, stale_tag) = held(store);
    let (extended, _, outcome) = acquire();
    assert_eq!(outcome, CacheOutcome::StaleRefreshed);
    assert_acquired(&extended, &full(), 1, "extended");
    assert_held(store, &extended, 1, "the extension is published");
    assert_eq!(
        extended
            .publish(remote, Some(&stale_tag))
            .expect("a lost race is Ok"),
        PublishOutcome::LostRace,
        "the second extension of the same stale artifact loses the CAS",
    );
    assert_held(store, &extended, 1, "the loser's write is discarded");

    write_audit_to(store, &[event(TS0 + 30, created(4, "b"))]);
    let racers: Vec<TemplateMap> = std::thread::scope(|s| {
        let racers = [s.spawn(acquire), s.spawn(acquire)];
        racers
            .map(|r| r.join().expect("querier thread").0)
            .into_iter()
            .collect()
    });
    let at_race = full();
    for racer in &racers {
        assert_equivalent(racer, &at_race).expect("each racer answers the full fold");
    }
    assert_held(
        store,
        &at_race,
        2,
        "the late file lands on a published incremental base",
    );

    write_audit_to(store, &[event(TS0 + 5, created(1, "user <*> late"))]);
    let (late, _, _) = acquire();
    assert_acquired(&late, &full(), 0, "the backdated file forces a full fold");
    assert_eq!(
        late.fold_state().map(|s| s.binding_conflicts().to_vec()),
        Some(vec![(1, 1)]),
    );
    assert_held(store, &late, 0, "the full fold is republished");
}

/// RFC0033.12 on the in-memory store, which honours conditional puts.
#[test]
fn rfc0033_12_incremental_publish_resolves_by_cas_in_memory() {
    incremental_publish_resolves_by_cas(&Store::in_memory());
}

/// RFC0033.12 on a real S3 backend (`LocalStack`). Run by the
/// `s3-integration` CI job by exact name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0033.12 S3 arm; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0033_12_incremental_publish_resolves_by_cas_on_s3() {
    let (_container, s3) = localstack_s3("ourios-it-incremental").await;
    tokio::task::spawn_blocking(move || incremental_publish_resolves_by_cas(&s3))
        .await
        .expect("the S3 arm");
}
