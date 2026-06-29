//! The weaver-generated `SCHEMA_URL` constant carries the conventions schema
//! version (the registry manifest's `schema_url`). These assertions are
//! version-agnostic invariants — they don't pin the exact version, so a schema
//! bump doesn't break them, but a missing/garbled URL does.

#[test]
fn schema_url_points_at_the_ourios_conventions_schema() {
    const PREFIX: &str = "https://ourios.dev/schemas/ourios-";
    let url = ourios_semconv::SCHEMA_URL;
    let version = url
        .strip_prefix(PREFIX)
        .unwrap_or_else(|| panic!("SCHEMA_URL is the Ourios conventions schema URL, got {url:?}"));
    // `version` looks like `0.1.0.yaml` (MAJOR.MINOR.PATCH + the `.yaml` doc
    // suffix). Assert a numeric MAJOR.MINOR (so a versionless `ourios-.yaml`
    // fails) and that the document's final dot-segment is `yaml` (so
    // `…yaml.bak` / `…yaml?x` fail) — version-agnostic, no exact pin. Uses
    // dot-segment checks rather than `starts_with`/`ends_with` patterns to stay
    // clear of `clippy::case_sensitive_file_extension_comparisons`.
    let mut segments = version.split('.');
    let major = segments.next().unwrap_or("");
    let minor = segments.next().unwrap_or("");
    assert!(
        !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()),
        "SCHEMA_URL has a numeric MAJOR version, got {url:?}",
    );
    assert!(
        !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()),
        "SCHEMA_URL has a numeric MINOR version, got {url:?}",
    );
    assert!(
        version.rsplit('.').next() == Some("yaml"),
        "SCHEMA_URL is a .yaml schema document, got {url:?}",
    );
}
