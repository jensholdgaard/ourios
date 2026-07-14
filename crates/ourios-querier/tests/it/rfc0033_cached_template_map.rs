//! RFC 0033 §5 — the cached template-map artifact, all seven scenarios.
//!
//! `.1` (the artifact-format + fold green slice), `.3` (the publish
//! green slice), `.2`/`.4`/`.5` (the freshness + write-through green
//! slice — the cached read path wired into the query surface), `.6`
//! (the comparative green slice — the cold-vs-warm collapse, local arm)
//! and `.7` (the observability green slice) are live. The `.3` S3
//! `If-Match` half runs in the `s3-integration` CI job
//! (`template_map_publish_cas_on_s3`, the RFC 0013 localstack pattern).
//!
//! Placement note: the scenarios live here because the artifact's
//! machinery — the folds (`template_registry.rs` / `alias_store.rs` /
//! `audit_scan.rs`), the freshness check, and the write-through — is
//! querier code (RFC 0033 §3.5), matching how RFC 0031 kept its
//! cross-cutting stubs in the one crate owning the harness. Two
//! scenarios additionally run outside this binary: the `.6`
//! headline-corpus measurable through the RFC 0031 comparative harness
//! in `ourios-bench` (RFC 0033 §6), which reports each pair's cold/warm
//! template-map acquisition, and `.7` in its own integration binary
//! (`tests/rfc0033_7_observability.rs`) because it installs a
//! process-global in-memory `MeterProvider` — the RFC 0016
//! metrics-test shape.

use std::path::Path;

use crate::common::{
    DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, at, simple, widened, write_all, write_audit,
};
use ourios_core::alias::ActorId;
use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
use ourios_core::tenant::TenantId;
use ourios_parquet::Store;
use ourios_querier::{
    ArtifactRead, CacheOutcome, PublishOutcome, Querier, QueryError, QueryResult, StoreRef,
    TEMPLATE_MAP_FILENAME, TemplateMap, derive_alias_map, derive_template_map,
    derive_template_registry, load_or_derive,
};
use proptest::prelude::*;
use tempfile::TempDir;

/// The generated histories' tenant.
const TENANT: &str = "T";
/// Template ids are drawn from `1..=ID_POOL` — small so histories
/// collide on ids (same-key registry interactions, overlapping alias
/// classes) rather than scattering.
const ID_POOL: u64 = 5;

/// A canonical-form template string (space-joined tokens, `<*>` for
/// wildcards) — the exact `format_template` encoding the audit stream
/// stores and `parse_template` consumes.
fn arb_template() -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![Just("<*>".to_string()), "[a-z]{1,4}"], 1..5)
        .prop_map(|tokens| tokens.join(" "))
}

fn arb_change() -> impl Strategy<Value = TemplateChange> {
    prop_oneof![
        arb_template().prop_map(|new_template| TemplateChange::Created { new_template }),
        (1u32..4, arb_template(), arb_template()).prop_map(
            |(old_version, old_template, new_template)| TemplateChange::Widened {
                old_version,
                new_version: old_version + 1,
                old_template,
                new_template,
                positions_widened: vec![0],
            }
        ),
        (1u32..4, arb_template()).prop_map(|(old_version, template)| {
            TemplateChange::TypeExpanded {
                old_version,
                new_version: old_version + 1,
                old_template: template.clone(),
                new_template: template,
                slots_expanded: Vec::new(),
            }
        }),
        (1u32..4, arb_template(), arb_template()).prop_map(
            |(version, current_template, would_be_template)| {
                TemplateChange::RejectedDegenerate {
                    version,
                    current_template,
                    would_be_template,
                    would_be_positions: vec![0],
                }
            }
        ),
    ]
}

fn arb_payload() -> impl Strategy<Value = AuditPayload> {
    prop_oneof![
        4 => (1u64..=ID_POOL, arb_change()).prop_map(|(template_id, change)| {
            AuditPayload::Template {
                template_id,
                triggering_line_hash: hash_triggering_line(b"line"),
                triggering_line_sample: None,
                change,
            }
        }),
        1 => (1u64..=ID_POOL, prop::collection::vec(1u64..=ID_POOL, 1..3)).prop_map(
            |(representative_id, member_ids)| AuditPayload::AliasAsserted {
                representative_id,
                member_ids,
                actor: ActorId::new("op-prop").expect("non-empty actor"),
                reason: String::new(),
            }
        ),
        1 => (1u64..=ID_POOL).prop_map(|representative_id| AuditPayload::AliasRetracted {
            representative_id,
            member_ids: Vec::new(),
            actor: ActorId::new("op-prop").expect("non-empty actor"),
            reason: String::new(),
        }),
    ]
}

/// Timestamps drawn tie-heavy (a 0..4 ns pool, so same-nanosecond ties
/// are frequent across a history) with an occasional hour / next-day
/// spread so events land in more than one audit partition.
fn arb_timestamp() -> impl Strategy<Value = u64> {
    (
        0u64..4,
        prop_oneof![Just(0u64), Just(HOUR_NS), Just(25 * HOUR_NS)],
    )
        .prop_map(|(tie, spread)| TS0 + tie + spread)
}

/// A per-tenant audit-event history: creations / widenings /
/// type-expansions / rejections and alias assertions / retractions at
/// arbitrary (tie-heavy) timestamps. `write_audit` flushes it through
/// the production `ParquetAuditSink` — one audit Parquet file per
/// event, so every history also exercises the §3.7.1 cross-file order.
fn arb_history() -> impl Strategy<Value = Vec<AuditEvent>> {
    prop::collection::vec(
        (arb_timestamp(), arb_payload()).prop_map(|(ts, payload)| AuditEvent {
            tenant_id: TenantId::new(TENANT),
            timestamp: at(ts),
            payload,
        }),
        1..12,
    )
}

/// A tenant's audit root under `bucket` (plain tenant names only — no
/// percent-encoding needed for these fixtures).
fn audit_root(bucket: &Path, tenant: &str) -> std::path::PathBuf {
    bucket.join("audit").join(format!("tenant_id={tenant}"))
}

/// The tenant's live audit `*.parquet` set as tenant-root-relative,
/// `/`-joined, sorted keys — an independent walk the artifact's
/// `folded_files` frontier must match exactly.
fn live_audit_set(bucket: &Path, tenant: &str) -> Vec<String> {
    let root = audit_root(bucket, tenant);
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        // Test oracle: an unreadable dir must fail the test loudly, not
        // shrink the expected frontier and pass for the wrong reason.
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                let rel = path.strip_prefix(&root).expect("under tenant root");
                let segments: Vec<&str> = rel
                    .components()
                    .map(|c| match c {
                        std::path::Component::Normal(s) => s.to_str().expect("utf-8"),
                        other => panic!("unexpected component {other:?}"),
                    })
                    .collect();
                out.push(segments.join("/"));
            }
        }
    }
    out.sort_unstable();
    out
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: ourios_testgen::proptest_cases(16),
        ..ProptestConfig::default()
    })]

    /// Scenario RFC0033.1 — cached fold ≡ fresh fold (property).
    /// See `docs/rfcs/0033-cached-template-map.md` §5.
    ///
    /// The cached path is serialize → deserialize → the two folds (the
    /// artifact-format slice's half of the scenario; frontier-checked
    /// resolution through storage is the freshness slice, RFC0033.2).
    /// The query-answer arm runs `resolves_to` through the public
    /// `run_query` twice — the storage-derived fold vs. the
    /// artifact-carried alias map injected — and the frontier is
    /// checked against an independent walk of the audit subtree.
    #[test]
    fn rfc0033_1_cached_fold_equals_fresh_fold(history in arb_history()) {
        let bucket = TempDir::new().expect("temp dir");
        write_audit(bucket.path(), &history);
        // Data rows for every id in the pool, so the query arm counts
        // real rows through whatever alias classes the history folded.
        let data: Vec<_> = (1..=ID_POOL)
            .map(|id| simple(TENANT, id, TS0 + id))
            .collect();
        write_all(bucket.path(), &data);

        let tenant = TenantId::new(TENANT);
        let backend = StoreRef::Local(bucket.path());

        // One scan, both folds — then through the artifact bytes (the
        // v2 zstd frame — the exact published-object encoding).
        let (derived, _bytes_read) =
            derive_template_map(backend, &tenant).expect("derive template map");
        let bytes = derived.to_artifact_bytes().expect("serialize");
        let ArtifactRead::Valid(cached) =
            TemplateMap::from_artifact_bytes(&bytes, &tenant).expect("read artifact")
        else {
            panic!("a just-serialized artifact must read back Valid");
        };

        // Cache-hit registry == the fresh fold, for every key.
        let fresh_registry =
            derive_template_registry(backend, &tenant).expect("fresh registry");
        prop_assert_eq!(cached.registry(), &fresh_registry);

        // Cache-hit alias map == the fresh fold, for every class.
        let fresh_aliases = derive_alias_map(backend, &tenant).expect("fresh alias map");
        prop_assert_eq!(
            cached.alias_map().classes(&tenant),
            fresh_aliases.classes(&tenant)
        );

        // The frontier is exactly the audit file set that was folded,
        // in canonical (tenant-root-relative, sorted) form.
        let live_set = live_audit_set(bucket.path(), TENANT);
        prop_assert_eq!(cached.folded_files(), live_set.as_slice());

        // The query answer through either path is identical: probe an
        // aliased id when the history built a class (the id where the
        // two paths could diverge), else any pool id.
        let probe = cached
            .alias_map()
            .classes(&tenant)
            .first()
            .and_then(|class| class.first().copied())
            .unwrap_or(1);
        let query =
            ourios_querier::dsl::parse(&format!("resolves_to({probe})")).expect("parse");
        let querier = Querier::new(bucket.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let fresh_rows = runtime
            .block_on(querier.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS, None))
            .expect("fresh-path query")
            .rows;
        let cached_rows = runtime
            .block_on(querier.run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(cached.alias_map()),
            ))
            .expect("artifact-path query")
            .rows;
        prop_assert_eq!(fresh_rows, cached_rows);
    }
}

/// An `alias_asserted` audit event — the freshness probe: whether a
/// `resolves_to` answer reflects it is exactly whether the fold that
/// answered the query saw the audit file it landed in.
fn alias_asserted(
    tenant: &str,
    representative_id: u64,
    member_ids: Vec<u64>,
    ts_ns: u64,
) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts_ns),
        payload: AuditPayload::AliasAsserted {
            representative_id,
            member_ids,
            actor: ActorId::new("op-it").expect("non-empty actor"),
            reason: String::new(),
        },
    }
}

/// Scenario RFC0033.2 — staleness is detected and never served.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
///
/// The freshness probe is the query *answer*: `resolves_to(1)` counts 1
/// row until the alias assertion `{1, 2}` is folded and 3 rows after, so
/// a stale artifact served would be visible as a wrong count — no cache
/// internals needed to detect it.
#[test]
fn rfc0033_2_staleness_detected_never_served() {
    let bucket = TempDir::new().expect("temp dir");
    let tenant = TenantId::new(TENANT);
    let backend = StoreRef::Local(bucket.path());
    write_audit(
        bucket.path(),
        &[widened(TENANT, 1, 1, TS0), widened(TENANT, 2, 1, TS0 + 1)],
    );
    write_all(
        bucket.path(),
        &[
            simple(TENANT, 1, TS0 + 10),
            simple(TENANT, 2, TS0 + 11),
            simple(TENANT, 2, TS0 + 12),
        ],
    );

    // The first query misses (no artifact), folds fresh, and its
    // write-through publishes at frontier S.
    assert_eq!(next_query_rows(bucket.path(), &tenant), 1);
    let artifact = audit_root(bucket.path(), TENANT).join(TEMPLATE_MAP_FILENAME);
    assert!(artifact.exists(), "the miss write-through published at S");
    let frontier_s = live_audit_set(bucket.path(), TENANT);

    // New audit files appear after the publish: the operator asserts the
    // alias class {1, 2}.
    write_audit(
        bucket.path(),
        &[alias_asserted(TENANT, 1, vec![2], TS0 + HOUR_NS)],
    );
    let live_wide = live_audit_set(bucket.path(), TENANT);
    assert!(
        live_wide.len() > frontier_s.len(),
        "the assertion landed in a new audit file",
    );

    // The frontier check fails, the artifact is bypassed, and the answer
    // equals the no-cache fold over the live set — including the new
    // file's alias event (an artifact served stale would still say 1).
    assert_eq!(next_query_rows(bucket.path(), &tenant), 3);

    // ...and the querier republished at the new frontier, both folds.
    let bytes = std::fs::read(&artifact).expect("read artifact");
    let read = TemplateMap::from_artifact_bytes(&bytes, &tenant).expect("read");
    let ArtifactRead::Valid(republished) = read else {
        panic!("the republished artifact must be Valid, got {read:?}");
    };
    assert_eq!(republished.folded_files(), live_wide.as_slice());
    assert_eq!(
        republished.alias_map().classes(&tenant),
        vec![std::collections::BTreeSet::from([1u64, 2])],
    );

    // A subsequent unchanged-store lookup is a cache hit: the acquisition
    // is exactly the artifact GET (frontier equal — zero audit GETs).
    let (held, acquisition_bytes, outcome) = load_or_derive(backend, &tenant).expect("lookup");
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(
        acquisition_bytes,
        std::fs::metadata(&artifact).expect("stat artifact").len(),
        "a hit's acquisition bytes are the artifact GET's bytes",
    );
    assert_eq!(held.folded_files(), live_wide.as_slice());

    // The removal direction: frontier validity is SET equality, not
    // subset. Delete the alias event's audit file — the artifact (now a
    // superset frontier) must be detected stale and the fold over the
    // shrunken live set must answer.
    for rel in live_wide.iter().filter(|k| !frontier_s.contains(k)) {
        std::fs::remove_file(audit_root(bucket.path(), TENANT).join(rel))
            .expect("remove the appended audit file");
    }
    assert_eq!(live_audit_set(bucket.path(), TENANT), frontier_s);
    assert_eq!(
        next_query_rows(bucket.path(), &tenant),
        1,
        "files disappeared ⇒ stale ⇒ the fold over the live set answers",
    );
    let bytes = std::fs::read(&artifact).expect("read artifact");
    let read = TemplateMap::from_artifact_bytes(&bytes, &tenant).expect("read");
    let ArtifactRead::Valid(reshrunk) = read else {
        panic!("the shrink republish must be Valid, got {read:?}");
    };
    assert_eq!(reshrunk.folded_files(), frontier_s.as_slice());
    assert!(reshrunk.alias_map().classes(&tenant).is_empty());
}

/// `resolves_to(1)` through the public query surface — the shared
/// "next query" of RFC0033.2/.3/.5.
fn resolves_query(bucket: &Path, tenant: &TenantId) -> Result<QueryResult, QueryError> {
    let query = ourios_querier::dsl::parse("resolves_to(1)").expect("parse");
    let querier = Querier::new(bucket);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(querier.run_query(&query, tenant, NOW, DEFAULT_WINDOW_NS, None))
}

/// The scenario's "next query": `resolves_to(1)` through the public
/// query surface. Returns the matching-row count; the `expect` is the
/// no-error-surfaced arm of RFC0033.3.
fn next_query_rows(bucket: &Path, tenant: &TenantId) -> u64 {
    resolves_query(bucket, tenant)
        .expect("the query must succeed, publish debris notwithstanding")
        .rows
}

/// Scenario RFC0033.3 — crash/tear safety around the publish.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
///
/// Crash simulation follows the house precedent for interrupted
/// atomic commits (`execution.rs`'s uncommitted `*.parquet.tmp`, the
/// RFC 0009 orphan stance): the dying writer got its bytes into the
/// `.tmp` and never reached the rename, so the test writes the `.tmp`
/// and does not rename. The tear is a real publish then an in-place
/// truncation of the committed name. The CAS loss runs through the
/// `Store` seam's create-if-absent half here (both writers observed
/// absence); the `If-Match` half is `template_map_publish_cas_on_s3`.
#[test]
fn rfc0033_3_crash_tear_safety() {
    let bucket = TempDir::new().expect("temp dir");
    write_audit(bucket.path(), &[widened(TENANT, 1, 1, TS0)]);
    write_all(bucket.path(), &[simple(TENANT, 1, TS0 + 1)]);
    let tenant = TenantId::new(TENANT);
    let backend = StoreRef::Local(bucket.path());
    let tenant_dir = bucket
        .path()
        .join("audit")
        .join(format!("tenant_id={TENANT}"));
    let artifact = tenant_dir.join(TEMPLATE_MAP_FILENAME);
    let tmp = tenant_dir.join(format!("{TEMPLATE_MAP_FILENAME}.tmp"));

    // The undisturbed baseline: the fold and the query answer every
    // corrupted read below must still produce.
    let (map, _) = derive_template_map(backend, &tenant).expect("derive");
    let baseline_rows = next_query_rows(bucket.path(), &tenant);
    assert_eq!(
        map.publish(backend, None).expect("publish"),
        PublishOutcome::Published,
    );
    assert!(artifact.exists(), "the rename committed the final name");
    assert!(!tmp.exists(), "a completed publish leaves no .tmp");

    // A stray `.tmp` from a crashed publish: the read path opens only
    // the v2 key (`template_map.v2.json.zst`), so the committed
    // artifact still reads back Valid, the audit walk's frontier is
    // unchanged, and the next query succeeds with the correct answer.
    std::fs::write(&tmp, br#"{"format_version":1,"partial"#).expect("plant stray tmp");
    let read = TemplateMap::from_artifact_bytes(
        &std::fs::read(&artifact).expect("read artifact"),
        &tenant,
    )
    .expect("read");
    let ArtifactRead::Valid(committed) = read else {
        panic!("the committed artifact must stay Valid under a stray .tmp, got {read:?}");
    };
    assert_eq!(committed.registry(), map.registry());
    let (refold, _) = derive_template_map(backend, &tenant).expect("fold under a stray tmp");
    assert_eq!(
        refold.folded_files(),
        map.folded_files(),
        "the stray .tmp must be invisible to the audit walk",
    );
    assert_eq!(next_query_rows(bucket.path(), &tenant), baseline_rows);

    // A torn artifact at the final name (a real publish, then the
    // compressed object truncated in place — a broken zstd frame, the
    // §3.3 amendment's decompression-failure arm): treated as absent —
    // the §3.7 `torn` outcome — while the fresh fold still answers the
    // query correctly, no error surfaced.
    let good_bytes = std::fs::read(&artifact).expect("read artifact");
    std::fs::write(&artifact, &good_bytes[..good_bytes.len() / 2]).expect("tear artifact");
    let torn =
        TemplateMap::from_artifact_bytes(&std::fs::read(&artifact).expect("read torn"), &tenant)
            .expect("torn is a disposition, not an error");
    assert!(matches!(torn, ArtifactRead::Torn { .. }), "got {torn:?}");
    let (healed, _) = derive_template_map(backend, &tenant).expect("fold under a torn artifact");
    assert_eq!(healed.registry(), map.registry());
    assert_eq!(next_query_rows(bucket.path(), &tenant), baseline_rows);

    // ...and that fresh fold's write-through overwrites the torn
    // artifact: the store self-heals.
    assert_eq!(
        healed.publish(backend, None).expect("republish"),
        PublishOutcome::Published,
    );
    let read =
        TemplateMap::from_artifact_bytes(&std::fs::read(&artifact).expect("read healed"), &tenant)
            .expect("read");
    assert!(
        matches!(read, ArtifactRead::Valid(_)),
        "the store must self-heal to a Valid artifact, got {read:?}",
    );

    // A CAS loss to a concurrent writer, through the Store seam: both
    // writers derived a correct fold and observed absence; the loser's
    // stale expectation comes back LostRace — an outcome, not an error,
    // so its query is never failed — and the store holds the winner's
    // valid, readable artifact.
    write_audit(bucket.path(), &[widened(TENANT, 1, 2, TS0 + HOUR_NS)]);
    let (wider, _) = derive_template_map(backend, &tenant).expect("derive at the wider frontier");
    assert!(
        wider.folded_files().len() > map.folded_files().len(),
        "the two writers' folds must be distinguishable",
    );
    let cas_root = TempDir::new().expect("cas temp dir");
    let store = Store::local(cas_root.path()).expect("store");
    let remote = StoreRef::Remote(&store);
    assert_eq!(
        map.publish(remote, None).expect("winner"),
        PublishOutcome::Published,
    );
    assert_eq!(
        wider.publish(remote, None).expect("a lost race must be Ok"),
        PublishOutcome::LostRace,
    );
    let held = store
        .get_blocking(&format!("audit/tenant_id={TENANT}/{TEMPLATE_MAP_FILENAME}"))
        .expect("get the published artifact");
    let ArtifactRead::Valid(held) = TemplateMap::from_artifact_bytes(&held, &tenant).expect("read")
    else {
        panic!("the store must hold a valid readable artifact after a lost race");
    };
    assert_eq!(held.folded_files(), map.folded_files());
}

/// The RFC0033.3 S3 arm on a real S3 backend (`LocalStack`): the full
/// conditional-put ladder — create-if-absent wins, a concurrent create
/// loses, an `If-Match` swap at the observed `ETag` wins, the now-stale
/// `ETag` loses — with every lost race a non-error outcome and the
/// object valid and readable throughout (RFC 0033 §3.4, the
/// `Manifest::publish_cas` precedent). Run by the `s3-integration` CI
/// job by exact name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0033.3 S3 arm; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn template_map_publish_cas_on_s3() {
    use testcontainers_modules::localstack::LocalStack;
    use testcontainers_modules::testcontainers::ImageExt;
    use testcontainers_modules::testcontainers::core::ExecCommand;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

    // Two correct folds at different frontiers, derived from a local
    // audit fixture (the publish target is the store under test, not
    // the fold source).
    let bucket = TempDir::new().expect("temp dir");
    let tenant = TenantId::new(TENANT);
    write_audit(bucket.path(), &[widened(TENANT, 1, 1, TS0)]);
    let (map_a, _) =
        derive_template_map(StoreRef::Local(bucket.path()), &tenant).expect("derive a");
    write_audit(bucket.path(), &[widened(TENANT, 1, 2, TS0 + HOUR_NS)]);
    let (map_b, _) =
        derive_template_map(StoreRef::Local(bucket.path()), &tenant).expect("derive b");

    // The RFC 0013 localstack harness pattern: start LocalStack, create
    // the bucket with the image's own `awslocal`, point `Store::s3` at
    // it via the endpoint override (credentials from the CI job's
    // `AWS_*` env — LocalStack accepts any).
    let container = LocalStack::default()
        .with_env_var("SERVICES", "s3")
        .start()
        .await
        .expect("start localstack");
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(4566)
        .await
        .expect("container port");
    let mut mb = container
        .exec(ExecCommand::new([
            "awslocal".to_string(),
            "s3".to_string(),
            "mb".to_string(),
            "s3://ourios-it-templatemap".to_string(),
        ]))
        .await
        .expect("exec awslocal s3 mb");
    // Drain both streams before reading the exit code — testcontainers
    // reports `exit_code()` as `None` until the output is consumed.
    let stdout =
        String::from_utf8_lossy(&mb.stdout_to_vec().await.expect("mb stdout")).into_owned();
    let stderr =
        String::from_utf8_lossy(&mb.stderr_to_vec().await.expect("mb stderr")).into_owned();
    let code = mb.exit_code().await.expect("mb exit code");
    assert_eq!(
        code,
        Some(0),
        "awslocal s3 mb failed (code {code:?}): stdout={stdout:?} stderr={stderr:?}",
    );
    let s3 = Store::s3(
        ourios_parquet::S3Config::new("ourios-it-templatemap")
            .with_endpoint(format!("http://{host}:{port}"))
            .with_region("us-east-1"),
    )
    .expect("build s3 store");
    let remote = StoreRef::Remote(&s3);
    let key = format!("audit/tenant_id={TENANT}/{TEMPLATE_MAP_FILENAME}");

    // Create-if-absent wins; a concurrent create (stale absence) loses
    // harmlessly and the store still holds A.
    assert_eq!(
        map_a.publish(remote, None).expect("create"),
        PublishOutcome::Published,
    );
    assert_eq!(
        map_b.publish(remote, None).expect("a lost create is Ok"),
        PublishOutcome::LostRace,
    );
    let (bytes, e_tag) = s3
        .get_with_etag_blocking_opt(&key)
        .expect("get")
        .expect("artifact present");
    let e_tag = e_tag.expect("s3 exposes an ETag");
    let ArtifactRead::Valid(held) =
        TemplateMap::from_artifact_bytes(&bytes, &tenant).expect("read")
    else {
        panic!("the artifact must read back Valid after a lost create");
    };
    assert_eq!(held.folded_files(), map_a.folded_files());

    // An `If-Match` swap at the observed `ETag` wins; the now-stale
    // `ETag` loses harmlessly, and the object stays valid throughout.
    assert_eq!(
        map_b.publish(remote, Some(&e_tag)).expect("swap"),
        PublishOutcome::Published,
    );
    assert_eq!(
        map_a
            .publish(remote, Some(&e_tag))
            .expect("a lost swap is Ok"),
        PublishOutcome::LostRace,
    );
    let (bytes, _) = s3
        .get_with_etag_blocking_opt(&key)
        .expect("get")
        .expect("artifact present");
    let ArtifactRead::Valid(held) =
        TemplateMap::from_artifact_bytes(&bytes, &tenant).expect("read")
    else {
        panic!("the artifact must read back Valid after a lost swap");
    };
    assert_eq!(held.folded_files(), map_b.folded_files());
}

/// A body-rendering query (`severity >= 0 | limit 10`) through the
/// public query surface — the RFC0033.4 probe.
fn body_query(bucket: &Path, tenant: &TenantId) -> QueryResult {
    let query = ourios_querier::dsl::parse("severity >= 0 | limit 10").expect("parse");
    let querier = Querier::new(bucket);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime
        .block_on(querier.run_query(&query, tenant, NOW, DEFAULT_WINDOW_NS, None))
        .expect("body-rendering query")
}

/// Scenario RFC0033.4 — additive and advisory (back-compat).
/// See `docs/rfcs/0033-cached-template-map.md` §5.
///
/// The pre-RFC behaviour is byte-precise on the local backend: the fold
/// counts each audit `*.parquet` file's length, so "reads the audit
/// stream exactly as today" is `registry_bytes_read == the summed file
/// sizes on disk` — the figure the pre-0033 registry derivation
/// reported.
#[test]
fn rfc0033_4_additive_and_advisory() {
    let bucket = TempDir::new().expect("temp dir");
    let tenant = TenantId::new(TENANT);
    write_audit(bucket.path(), &[widened(TENANT, 1, 1, TS0)]);
    write_all(
        bucket.path(),
        &[simple(TENANT, 1, TS0 + 1), simple(TENANT, 1, TS0 + 2)],
    );
    let live_before = live_audit_set(bucket.path(), TENANT);
    let audit_bytes: u64 = live_before
        .iter()
        .map(|rel| {
            std::fs::metadata(audit_root(bucket.path(), TENANT).join(rel))
                .expect("stat audit file")
                .len()
        })
        .sum();

    // Store with no artifact (old data): the result is the pre-RFC
    // binary's — every row rendered from the audit-stream fold, and the
    // fold reads the audit stream exactly as today, byte for byte.
    let first = body_query(bucket.path(), &tenant);
    assert_eq!(first.rows, 2);
    assert_eq!(first.records.len(), 2);
    assert_eq!(
        first.registry_bytes_read, audit_bytes,
        "no artifact ⇒ the acquisition is exactly today's audit-stream fold",
    );

    // The miss write-through published — and the artifact's presence
    // changes nothing a `*.parquet` scan sees: the audit walk returns
    // the same file set as without it.
    let artifact = audit_root(bucket.path(), TENANT).join(TEMPLATE_MAP_FILENAME);
    assert!(artifact.exists(), "the miss write-through published");
    assert_eq!(live_audit_set(bucket.path(), TENANT), live_before);

    // Artifact present: identical answer (the cache is advisory), the
    // acquisition now one small GET.
    let second = body_query(bucket.path(), &tenant);
    assert_eq!(second.rows, first.rows);
    assert_eq!(second.records, first.records);
    assert_eq!(
        second.registry_bytes_read,
        std::fs::metadata(&artifact).expect("stat artifact").len(),
        "a hit's acquisition is exactly the artifact GET",
    );

    // Deleting the artifact between two queries changes neither query's
    // answer — the only cost is re-derivation (the acquisition returns
    // to today's fold figure).
    std::fs::remove_file(&artifact).expect("delete artifact");
    let third = body_query(bucket.path(), &tenant);
    assert_eq!(third.rows, first.rows);
    assert_eq!(third.records, first.records);
    assert_eq!(third.registry_bytes_read, audit_bytes);
}

/// Scenario RFC0033.5 — tenant isolation.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
fn rfc0033_5_tenant_isolation() {
    let bucket = TempDir::new().expect("temp dir");
    let backend = StoreRef::Local(bucket.path());
    let alpha = TenantId::new("alpha");
    let beta = TenantId::new("beta");
    // Distinct histories: alpha has one template and no aliases; beta has
    // two templates and the alias class {1, 2} — the same probe query
    // answers differently per tenant, so a cross-read would be visible.
    write_audit(bucket.path(), &[widened("alpha", 1, 1, TS0)]);
    write_audit(
        bucket.path(),
        &[
            widened("beta", 1, 1, TS0),
            widened("beta", 2, 1, TS0 + 1),
            alias_asserted("beta", 1, vec![2], TS0 + 2),
        ],
    );
    write_all(
        bucket.path(),
        &[
            simple("alpha", 1, TS0 + 10),
            simple("beta", 1, TS0 + 10),
            simple("beta", 2, TS0 + 11),
        ],
    );

    // Each tenant's first query warms its own artifact, under its own
    // `tenant_id=<enc>` audit root — and answers only from its own fold:
    // alpha resolves 1 → {1} (1 row); beta resolves 1 → {1, 2} (2 rows).
    assert_eq!(next_query_rows(bucket.path(), &alpha), 1);
    assert_eq!(next_query_rows(bucket.path(), &beta), 2);
    let alpha_artifact = audit_root(bucket.path(), "alpha").join(TEMPLATE_MAP_FILENAME);
    let beta_artifact = audit_root(bucket.path(), "beta").join(TEMPLATE_MAP_FILENAME);
    assert!(alpha_artifact.exists(), "alpha's write-through published");
    assert!(beta_artifact.exists(), "beta's write-through published");

    // Each cache hit serves only that tenant's registry and alias map.
    let (alpha_map, _, outcome) = load_or_derive(backend, &alpha).expect("alpha lookup");
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(
        alpha_map.registry(),
        &derive_template_registry(backend, &alpha).expect("alpha registry"),
    );
    assert!(alpha_map.alias_map().classes(&alpha).is_empty());
    let (beta_map, _, outcome) = load_or_derive(backend, &beta).expect("beta lookup");
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(
        beta_map.registry(),
        &derive_template_registry(backend, &beta).expect("beta registry"),
    );
    assert_eq!(
        beta_map.alias_map().classes(&beta),
        vec![std::collections::BTreeSet::from([1u64, 2])],
    );
    assert_eq!(
        alpha_map.folded_files(),
        live_audit_set(bucket.path(), "alpha").as_slice()
    );
    assert_eq!(
        beta_map.folded_files(),
        live_audit_set(bucket.path(), "beta").as_slice()
    );

    // The queries stay isolated cache-warm too.
    assert_eq!(next_query_rows(bucket.path(), &alpha), 1);
    assert_eq!(next_query_rows(bucket.path(), &beta), 2);

    // An artifact whose body `tenant_id` differs from the tenant of the
    // path it was fetched from fails the query LOUDLY — end to end
    // through the public surface — never silently serving or ignoring
    // foreign data (the row-vs-path stance).
    std::fs::copy(&beta_artifact, &alpha_artifact).expect("plant beta's artifact under alpha");
    match resolves_query(bucket.path(), &alpha)
        .expect_err("a foreign artifact under alpha's root must fail alpha's query")
    {
        QueryError::Storage { detail } => assert!(
            detail.contains("claims tenant beta under tenant alpha"),
            "the failure must name the row-vs-path mismatch: {detail}",
        ),
        other => panic!("expected Storage, got {other:?}"),
    }
    // Beta is untouched by alpha's corruption.
    assert_eq!(next_query_rows(bucket.path(), &beta), 2);
}

/// Scenario RFC0033.6 — the measured tax collapses (RFC 0031 channel).
/// See `docs/rfcs/0033-cached-template-map.md` §5.
///
/// The locally-runnable arm: the §3.5 write-through abstains unless the
/// serialized artifact is strictly smaller than the audit bytes it
/// folded, and the committed gate form is `warm × 10 ≤ cold`, so this
/// store's audit stream must dwarf the artifact by more than the gate —
/// 64 single-event audit Parquet files (each paying the full Parquet
/// envelope) against one JSON row per template. Cold-vs-warm runs
/// through the public query surface: cold pays the fold and publishes,
/// warm pays exactly one artifact GET.
///
/// The headline-corpus numbers the §5 scenario names (otel-demo-v8,
/// 4.9 M records; run #8 cold baseline `registry_bytes_read` =
/// 513,862 B constant per query) come from the RFC 0031 comparative
/// harness (`crates/ourios-bench/tests/rfc0031_comparative.rs`), where
/// the artifact persists across pairs and each pair's report carries
/// its cold/warm acquisition — recorded in `docs/benchmarks.md`
/// alongside the run #8 baseline when that dispatch runs.
#[test]
fn rfc0033_6_measured_tax_collapses() {
    let bucket = TempDir::new().expect("temp dir");
    let tenant = TenantId::new(TENANT);
    let events: Vec<AuditEvent> = (1..=64)
        .map(|id| widened(TENANT, id, 1, TS0 + id))
        .collect();
    write_audit(bucket.path(), &events);
    write_all(
        bucket.path(),
        &[
            simple(TENANT, 1, TS0 + 100),
            simple(TENANT, 2, TS0 + 101),
            simple(TENANT, 3, TS0 + 102),
        ],
    );
    let audit_bytes: u64 = live_audit_set(bucket.path(), TENANT)
        .iter()
        .map(|rel| {
            std::fs::metadata(audit_root(bucket.path(), TENANT).join(rel))
                .expect("stat audit file")
                .len()
        })
        .sum();

    // Cold: no artifact — the acquisition is the full audit-stream fold,
    // byte for byte, and the miss write-through publishes (the artifact
    // won against the abstention rule).
    let cold = body_query(bucket.path(), &tenant);
    assert_eq!(cold.rows, 3);
    assert_eq!(
        cold.registry_bytes_read, audit_bytes,
        "cold acquisition is the audit-stream fold, byte for byte",
    );
    let artifact = audit_root(bucket.path(), TENANT).join(TEMPLATE_MAP_FILENAME);
    assert!(artifact.exists(), "the cold miss write-through published");
    let artifact_len = std::fs::metadata(&artifact).expect("stat artifact").len();

    // Warm: a fresh query's only registry-path GET is the artifact, so
    // the acquisition equals its object size exactly — and the answer
    // is unchanged (the cache is advisory).
    let warm = body_query(bucket.path(), &tenant);
    assert_eq!(warm.rows, cold.rows);
    assert_eq!(warm.records, cold.records);
    assert_eq!(
        warm.registry_bytes_read, artifact_len,
        "warm acquisition equals the artifact object's byte size exactly",
    );

    // The committed gate form: warm/cold ≤ 1/10 — the ratio, not an
    // absolute byte count, so it holds as the corpus evolves.
    assert!(
        warm.registry_bytes_read.saturating_mul(10) <= cold.registry_bytes_read,
        "the measured tax must collapse: warm={} cold={} (gate: warm x 10 <= cold)",
        warm.registry_bytes_read,
        cold.registry_bytes_read,
    );
    // The §3.2 amendment's lever, recorded: the artifact ships as one
    // zstd frame, so the warm GET pays the compressed bytes.
    let decompressed = zstd::decode_all(
        std::fs::read(&artifact)
            .expect("read published artifact")
            .as_slice(),
    )
    .expect("the published artifact decompresses as zstd")
    .len();
    eprintln!(
        "rfc0033.6 local arm: cold={} B (audit fold, {} files), warm={} B \
         (artifact GET, compressed; {decompressed} B JSON decompressed)",
        cold.registry_bytes_read,
        events.len(),
        warm.registry_bytes_read,
    );
}
