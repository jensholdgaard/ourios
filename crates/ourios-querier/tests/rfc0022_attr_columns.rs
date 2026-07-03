//! RFC 0022 §5 — query-side promoted attribute columns.
//!
//! Scenarios RFC0022.3–.7 (old-file parity, operator gating, pruning,
//! projection-blind read path, promoted-set drift). Stubs are tagged
//! `#[ignore]` so the default `cargo test` run stays green while the
//! RFC is red; each is un-`#[ignore]`d by the green slice that
//! implements it. The writer-side scenarios (`.1`/`.2`) live in
//! `crates/ourios-parquet/tests/rfc0022_promoted_columns.rs`.

/// Scenario RFC0022.3 — old files answer identically (§3.9 / §3.4).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.3 stub — implemented in the predicate-compile green slice"]
fn rfc0022_3_pre_amendment_files_answer_identically() {
    todo!(
        "RFC0022.3 — a scan spanning a pre-amendment file and a post-amendment \
         file answers ==/!= attribute queries row-for-row identically to the \
         pure-LIKE compile"
    );
}

/// Scenario RFC0022.4 — full operator set on promoted keys only.
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.4 stub — implemented in the predicate-compile green slice"]
fn rfc0022_4_operator_set_gated_on_promotion() {
    todo!(
        "RFC0022.4 — ordering/regex answer from the typed arm only (pre-amendment \
         rows never match, §3.3 silent non-match); non-promoted keys still reject \
         with InvalidQuery; ==/!= unchanged"
    );
}

/// Scenario RFC0022.5 — promoted predicates prune (pillar 2).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.5 stub — implemented in the pruning green slice"]
fn rfc0022_5_promoted_predicates_prune() {
    todo!(
        "RFC0022.5 — a selective equality query on a promoted key shows \
         pruned > 0 via the RFC 0016 scanned/pruned counters on a \
         multi-row-group corpus; B1/B2 unchanged"
    );
}

/// Scenario RFC0022.6 — the read path is projection-blind (§3.1).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.6 stub — implemented in the predicate-compile green slice"]
fn rfc0022_6_read_path_is_projection_blind() {
    todo!(
        "RFC0022.6 — rows round-trip from the JSON columns exactly as before; a \
         hand-forged file whose promoted cell disagrees with the JSON is \
         invisible through the RFC 0017 read path"
    );
}

/// Scenario RFC0022.7 — promoted-set drift across deploys (§3.4).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.7 stub — implemented in the pruning green slice"]
fn rfc0022_7_promoted_set_drift_unions_cleanly() {
    todo!(
        "RFC0022.7 — one scan over files written under configured sets {{}}, \
         {{a}}, {{a,b}} unions schemas without error; predicates on a and b \
         answer correctly from every file"
    );
}
