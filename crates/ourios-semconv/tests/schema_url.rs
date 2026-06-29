//! The weaver-generated `SCHEMA_URL` constant must carry the registry
//! manifest's `schema_url`. This asserts only that provenance — the URL points
//! at the Ourios conventions schema. We deliberately do not parse or validate
//! the version out of it: the version lives in the registry (the source of
//! truth), the `semconv` CI no-diff check guarantees this constant matches it,
//! and version parsing is a well-defined paradigm we have no business
//! reimplementing here.

#[test]
fn schema_url_points_at_the_ourios_conventions_schema() {
    assert!(
        ourios_semconv::SCHEMA_URL.starts_with("https://ourios.dev/schemas/ourios-"),
        "SCHEMA_URL is the Ourios conventions schema URL, got {:?}",
        ourios_semconv::SCHEMA_URL,
    );
}
