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
    // A real version follows the prefix — at least `MAJOR.MINOR` of digits —
    // rather than just any non-empty tail (so e.g. `ourios-.yaml` fails),
    // without pinning the exact version (a schema bump must not break this).
    let (major, rest) = version
        .split_once('.')
        .unwrap_or_else(|| panic!("SCHEMA_URL carries a dotted version, got {url:?}"));
    assert!(
        !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()),
        "SCHEMA_URL's version starts with a numeric MAJOR, got {url:?}",
    );
    assert!(
        rest.starts_with(|c: char| c.is_ascii_digit()),
        "SCHEMA_URL's version has a numeric MINOR after MAJOR, got {url:?}",
    );
    assert!(
        version.contains(".yaml"),
        "SCHEMA_URL is a .yaml schema document, got {url:?}",
    );
}
