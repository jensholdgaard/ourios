//! RFC 0033 §5 — the cached template-map artifact, all seven scenarios.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: all seven stubs live here because the artifact's
//! machinery — the folds (`template_registry.rs` / `alias_store.rs` /
//! `audit_scan.rs`), the freshness check, and the write-through — is
//! querier code (RFC 0033 §3.5), matching how RFC 0031 kept its
//! cross-cutting stubs in the one crate owning the harness. The `.6`
//! comparative measurable is discharged through the RFC 0031
//! comparative harness in `ourios-bench` (RFC 0033 §6); its stub
//! stays here so §5→stub traceability is one file.

/// Scenario RFC0033.1 — cached fold ≡ fresh fold (property).
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
#[ignore = "RFC0033.1 stub — implemented in the artifact-format + fold green slice"]
fn rfc0033_1_cached_fold_equals_fresh_fold() {
    todo!(
        "RFC0033.1 — proptest: any generated per-tenant audit history \
         (creations/widenings/type-expansions/rejections, alias \
         assertions/retractions, arbitrary timestamps incl. \
         same-nanosecond ties) flushed to audit Parquet; fresh fold \
         published, second read resolves via the artifact (frontier \
         equal, cache hit): hit registry == derive_template_registry \
         and hit alias map == derive_alias_map for every key, and the \
         query answer through either path is identical"
    );
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
