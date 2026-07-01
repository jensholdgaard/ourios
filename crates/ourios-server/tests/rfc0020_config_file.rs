//! RFC 0020 — server configuration file, the §5 acceptance scenarios (red).
//!
//! These are `#[ignore]`d stubs that pin the scenario ids (RFC0020.1–.6) so the
//! mapping from RFC to test is greppable from the `red` stage on (per
//! `docs/verification.md` §2.3). They compile but are not yet implemented; the
//! green slices fill them in.
//!
//! At green most of these move to where the code lives — a `config` module in
//! `src/main.rs` / `src/config/` (per RFC 0020 §3.6): the substitution resolver
//! (`.2`) and schema/precedence/validation (`.1`/`.3`/`.5`) are unit-testable
//! there, `.4` is the existing `config_from_env` regression with no `--config`,
//! and `.6` extends the RFC0019.6 secret-redaction test to the file path. Some
//! may stay here as end-to-end checks of the resolved `ServerConfig`.
//!
//! See `docs/rfcs/0020-configuration-file.md` §5 / §6.

/// Scenario RFC0020.1 — a complete file resolves to the expected `ServerConfig`.
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
#[ignore = "RFC0020.1 stub — implemented in the green slice"]
fn rfc0020_1_complete_file_resolves_to_expected_server_config() {
    todo!("RFC0020.1 — a --config file maps to the same ServerConfig the equivalent env produces");
}

/// Scenario RFC0020.2 — environment substitution follows the `OTel` Config WG model.
/// See `docs/rfcs/0020-configuration-file.md` §5.
///
/// Green: the resolver conformance vectors live in
/// `ourios_server::config::env_subst` (the WG input→output table + escape /
/// non-recursion invariants + proptest), and the scalar-value walk that applies
/// them to a parsed file — scalar-only (mapping keys verbatim), non-recursive
/// (no injected YAML structure), `$$` escaping, type-after-substitution — lives
/// in `ourios_server::config::file`. This end-to-end check pins the two together
/// through the public `parse` entry point.
#[test]
fn rfc0020_2_env_substitution_follows_the_otel_config_wg_model() {
    let lookup = |name: &str| match name {
        "BUCKET" => Some("logs".to_owned()),
        "WINDOW" => Some("1800".to_owned()),
        _ => None,
    };
    let yaml = "\
storage:
  backend: ${env:BACKEND:-s3}
  s3:
    bucket: ${BUCKET}
    endpoint: ${env:MISSING:-}
    prefix: a$$b
querier:
  enabled: ${QUERIER_ON:-true}
  default_window_secs: ${env:WINDOW}
";
    let cfg = ourios_server::config::file::parse(yaml, &lookup).expect("valid file");

    assert_eq!(cfg.storage.backend.as_deref(), Some("s3")); // :-default on unset
    assert_eq!(cfg.storage.s3.bucket.as_deref(), Some("logs")); // ${env:} / ${}
    assert_eq!(cfg.storage.s3.endpoint.as_deref(), Some("")); // undefined, no default
    assert_eq!(cfg.storage.s3.prefix.as_deref(), Some("a$b")); // $$ → literal $
    assert_eq!(cfg.querier.enabled.as_deref(), Some("true"));
    assert_eq!(cfg.querier.default_window_secs.as_deref(), Some("1800"));

    // A malformed reference in a scalar value fails the whole file (no partial
    // resolution), naming the reference and not any resolved value.
    let err = ourios_server::config::file::parse("storage:\n  backend: ${1BAD}\n", &lookup)
        .expect_err("malformed reference");
    assert!(err.to_string().contains("${1BAD}"));
}

/// Scenario RFC0020.3 — file is authoritative; a bare env var does not override.
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
#[ignore = "RFC0020.3 stub — implemented in the green slice"]
fn rfc0020_3_file_is_authoritative_bare_env_does_not_override() {
    todo!("RFC0020.3 — a file value wins over a bare OURIOS_* env var with --config present");
}

/// Scenario RFC0020.4 — no `--config` preserves the env-only path.
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
#[ignore = "RFC0020.4 stub — implemented in the green slice"]
fn rfc0020_4_no_config_preserves_the_env_only_path() {
    todo!("RFC0020.4 — without --config, config_from_env behaviour is unchanged");
}

/// Scenario RFC0020.5 — invalid configuration fails fast.
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
#[ignore = "RFC0020.5 stub — implemented in the green slice"]
fn rfc0020_5_invalid_configuration_fails_fast() {
    todo!(
        "RFC0020.5 — malformed ${{...}} ref / unknown key / invalid value errors at startup, no partial apply"
    );
}

/// Scenario RFC0020.6 — secret hygiene across the file path.
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
#[ignore = "RFC0020.6 stub — implemented in the green slice"]
fn rfc0020_6_secret_hygiene_across_the_file_path() {
    todo!(
        "RFC0020.6 — resolved secret never logged; config error names the key/env-var, never the value"
    );
}
