//! RFC 0033 §5 — the cached template-map artifact, all seven scenarios.
//!
//! `.1` is live (the artifact-format + fold green slice); the
//! remaining stubs are `#[ignore]`d so the default run stays green
//! while their slices land, each naming the slice that discharges it.
//!
//! Placement note: all seven stubs live here because the artifact's
//! machinery — the folds (`template_registry.rs` / `alias_store.rs` /
//! `audit_scan.rs`), the freshness check, and the write-through — is
//! querier code (RFC 0033 §3.5), matching how RFC 0031 kept its
//! cross-cutting stubs in the one crate owning the harness. The `.6`
//! comparative measurable is discharged through the RFC 0031
//! comparative harness in `ourios-bench` (RFC 0033 §6); its stub
//! stays here so §5→stub traceability is one file.

use std::path::Path;

use crate::common::{DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, at, simple, write_all, write_audit};
use ourios_core::alias::ActorId;
use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
use ourios_core::tenant::TenantId;
use ourios_querier::{
    ArtifactRead, Querier, StoreRef, TemplateMap, derive_alias_map, derive_template_map,
    derive_template_registry,
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

/// The tenant's live audit `*.parquet` set as tenant-root-relative,
/// `/`-joined, sorted keys — an independent walk the artifact's
/// `folded_files` frontier must match exactly.
fn live_audit_set(bucket: &Path) -> Vec<String> {
    let root = bucket.join("audit").join(format!("tenant_id={TENANT}"));
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

        // One scan, both folds — then through the artifact bytes.
        let (derived, _bytes_read) =
            derive_template_map(backend, &tenant).expect("derive template map");
        let json = derived.to_json().expect("serialize");
        let ArtifactRead::Valid(cached) =
            TemplateMap::from_json(&json, &tenant).expect("read artifact")
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
        let live_set = live_audit_set(bucket.path());
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

/// Scenario RFC0033.2 — staleness is detected and never served.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.2 stub — implemented in the freshness + write-through green slice"]
fn rfc0033_2_staleness_detected_never_served() {
    todo!(
        "RFC0033.2 — artifact published at frontier S, then new audit \
         files appear: the frontier check fails, the artifact is \
         bypassed, the answer equals the no-cache fold over the live \
         set incl. the new files; the querier republishes at the new \
         frontier and a subsequent unchanged-store query is a hit; \
         same when files DISAPPEAR from the live set (set equality, \
         not subset)"
    );
}

/// Scenario RFC0033.3 — crash/tear safety around the publish.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.3 stub — implemented in the publish green slice"]
fn rfc0033_3_crash_tear_safety() {
    todo!(
        "RFC0033.3 — publish interrupted mid-write (stray \
         template_map.json.tmp, truncated/corrupt template_map.json, \
         S3 CAS loss to a concurrent writer): next query ignores the \
         .tmp, treats a torn artifact as absent (fresh fold, correct \
         answer, no error surfaced), a CAS loss discards the losing \
         write without failing its query; the torn case emits the \
         §3.7 torn outcome and the fresh fold's write-through \
         overwrites the torn artifact (self-heal)"
    );
}

/// Scenario RFC0033.4 — additive and advisory (back-compat).
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.4 stub — implemented in the freshness + write-through green slice"]
fn rfc0033_4_additive_and_advisory() {
    todo!(
        "RFC0033.4 — store with no artifact: a body-rendering query's \
         result is identical to the pre-RFC binary's and the fold \
         reads the audit stream exactly as today; deleting the \
         artifact between two queries changes neither answer; the \
         artifact's presence changes nothing a *.parquet scan sees \
         (audit walk/listing returns the same file set with and \
         without it)"
    );
}

/// Scenario RFC0033.5 — tenant isolation.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.5 stub — implemented in the freshness + write-through green slice"]
fn rfc0033_5_tenant_isolation() {
    todo!(
        "RFC0033.5 — two tenants with distinct histories and published \
         artifacts: each cache hit serves only that tenant's registry \
         and alias map (paths under tenant_id=<enc>); an artifact \
         whose body tenant_id differs from the tenant of the path it \
         was fetched from fails the query loudly (the row-vs-path \
         stance), never silently serving or ignoring foreign data"
    );
}

/// Scenario RFC0033.6 — the measured tax collapses (RFC 0031 channel).
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.6 stub — implemented in the comparative green slice (ourios-bench RFC 0031 harness arm, cold-vs-warm)"]
fn rfc0033_6_measured_tax_collapses() {
    todo!(
        "RFC0033.6 — RFC 0031 headline-corpus shape (otel-demo-v8, \
         4.9 M records; run #8 baseline registry_bytes_read = \
         513,862 B constant per query) ingested, warm published \
         artifact: a cache-warm body-rendering query's \
         QueryResult::registry_bytes_read equals the artifact \
         object's byte size exactly, warm/cold registry_bytes_read \
         <= 1/10 (the gate is the ratio, not an absolute byte \
         count), and both numbers are recorded in docs/benchmarks.md \
         alongside the run #8 baseline"
    );
}

/// Scenario RFC0033.7 — observable outcomes.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.7 stub — implemented in the observability green slice"]
fn rfc0033_7_observable_outcomes() {
    todo!(
        "RFC0033.7 — served querier with the RFC 0016 OTel metrics \
         pipeline active; queries drive a miss, a hit, a staleness, \
         and a torn artifact: the §3.7 lookup-outcome and \
         publish-outcome instruments record each with the correct \
         outcome attribute, the publish-size instrument records the \
         artifact size, and the instrument names exist in the \
         semconv registry (weaver-generated constants, no \
         hand-written flat names)"
    );
}
