//! RFC 0062 — attribute paths into map values, the served surfaces (H6).
//! See `docs/rfcs/0062-attribute-map-paths.md` §5.
//!
//! The stub is `#[ignore]`d so the default run stays green while the
//! RFC is red; it names the slice that discharges it.

/// Scenario RFC0062.8 — no engine name reaches the user (H6).
/// See `docs/rfcs/0062-attribute-map-paths.md` §5.
#[test]
#[ignore = "RFC0062.8 stub — implemented with the §3.1 path resolution"]
fn rfc0062_8_no_engine_name_reaches_the_user() {
    todo!(
        "RFC0062.8 — through the HTTP query endpoint and MCP `query_logs`, \
         `attr.user.id == 7` against `user = {{id: \"u7\"}}` matches nothing and \
         `attr.user..id` is a DSL error, and no response or error names \
         `ourios_attr_cmp`, `LIKE` or a DataFusion function; the query-schema \
         resource states the RFC 0062 §3.1 rule"
    );
}
