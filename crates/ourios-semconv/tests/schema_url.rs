//! The weaver-generated `SCHEMA_URL` constant carries the conventions schema
//! version (the registry manifest's `schema_url`). These assertions are
//! version-agnostic invariants — they don't pin the exact version, so a schema
//! bump doesn't break them, but a missing/garbled URL does.

#[test]
fn schema_url_points_at_the_ourios_conventions_schema() {
    const PREFIX: &str = "https://ourios.dev/schemas/ourios-";
    assert!(
        ourios_semconv::SCHEMA_URL.starts_with(PREFIX),
        "SCHEMA_URL is the Ourios conventions schema URL, got {:?}",
        ourios_semconv::SCHEMA_URL,
    );
    assert!(
        ourios_semconv::SCHEMA_URL.len() > PREFIX.len(),
        "SCHEMA_URL carries a version after the prefix, got {:?}",
        ourios_semconv::SCHEMA_URL,
    );
}
