//! RFC 0020 — server configuration file, the §5 acceptance scenarios.
//!
//! Scenario ids (RFC0020.1–.6) are pinned to tests so the RFC→test mapping is
//! greppable (per `docs/verification.md` §2.3). The scenarios live where the
//! code they exercise does (RFC 0020 §3.6):
//!
//! - **`.2`** (substitution semantics) is here — an end-to-end check through the
//!   public `ourios_server::config::file::parse` entry point.
//! - **`.1`/`.3`/`.4`/`.5`** exercise the resolved (private) `ServerConfig` — the
//!   file→`ServerConfig` mapping (`server_config_from_file`), the `--config`
//!   selection (the `clap` `Cli`), and the shared `build_*` validators —
//!   so they are unit tests in `src/main.rs` (`rfc0020_1_*` … `rfc0020_5_*`),
//!   which can reach those items. The malformed-reference / unknown-key arms of
//!   `.5` are additionally covered in `config::file`.
//! - **`.6`** (secret hygiene across the file path) is the remaining stub, filled
//!   by the secret-hygiene green slice.
//!
//! See `docs/rfcs/0020-configuration-file.md` §5 / §6.

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
    endpoint: ${env:MISSING}
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

// RFC0020.1 / .3 / .4 / .5 are unit tests in `src/main.rs` — they exercise the
// resolved (private) `ServerConfig`, the `--config` selection, and the shared
// `build_*` validators (see the module docs above).

/// Scenario RFC0020.6 — secret hygiene across the file path.
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
#[ignore = "RFC0020.6 stub — implemented in the green slice"]
fn rfc0020_6_secret_hygiene_across_the_file_path() {
    todo!(
        "RFC0020.6 — resolved secret never logged; config error names the key/env-var, never the value"
    );
}
