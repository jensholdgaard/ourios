//! RFC 0022 §5 — writer-side promoted attribute columns.
//!
//! Scenarios RFC0022.1/.2 (projection semantics + encodings). Stubs are
//! tagged `#[ignore]` so the default `cargo test` run stays green while
//! the RFC is red; each is un-`#[ignore]`d by the green slice that
//! implements it. The querier-side scenarios (`.3`–`.7`) live in
//! `crates/ourios-querier/tests/rfc0022_attr_columns.rs`.

/// Scenario RFC0022.1 — `service.name` is always projected.
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.1 stub — implemented in the writer-projection green slice"]
fn rfc0022_1_service_name_always_projected() {
    todo!(
        "RFC0022.1 — files carry an OPTIONAL Utf8 `resource.service.name` column \
         (string values byte-identical to the JSON, NULL for absent/non-string); \
         the resource_attributes JSON column is byte-identical to a pre-amendment \
         writer's output"
    );
}

/// Scenario RFC0022.2 — configured keys project the same way.
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.2 stub — implemented in the writer-projection green slice"]
fn rfc0022_2_configured_keys_project() {
    todo!(
        "RFC0022.2 — configured resource/log keys yield `resource.<key>` / \
         `attr.<key>` columns with §3.1 projection semantics (dict + page index + \
         bloom per the encodings row); unconfigured keys produce no column"
    );
}
