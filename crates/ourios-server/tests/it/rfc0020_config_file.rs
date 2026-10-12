//! RFC 0020 — server configuration file, the §5 acceptance scenarios.
//!
//! Scenario ids (RFC0020.1–.6) are pinned to tests so the RFC→test mapping is
//! greppable (per `docs/verification.md` §2.3). The scenarios live where the
//! code they exercise does (RFC 0020 §3.6):
//!
//! - **`.2`** (substitution semantics) is here — an end-to-end check through the
//!   public `ourios_server::config::file::parse` entry point.
//! - **`.1`/`.3`/`.5`** exercise the resolved `ServerConfig` — the
//!   file→`ServerConfig` mapping (`server_config_from_file`) and the shared
//!   `build_*` validators — so they are unit tests beside that code in
//!   `src/config/resolve.rs` (`rfc0020_1_*` … `rfc0020_5_*`; moved with
//!   the epic #745 wave-1 extraction). **`.4`** (the `--config` `clap`
//!   selection) stays a `src/main.rs` unit test with the `Cli` it
//!   exercises. The malformed-reference / unknown-key arms of `.5` are
//!   additionally covered in `config::file`.
//! - **`.6`** (secret hygiene across the file path) is here for its public
//!   surface — inline-literal-credential rejection (error names the key, not the
//!   value) and `Debug` redaction; the resolved-secret-plus-sibling-error case is
//!   a `config::resolve` unit test.
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

/// Scenario RFC0020.6 — secret hygiene across the file path (public surface).
///
/// Object-store credentials must be `${env:…}` references, not inline literals
/// (§3.5): an inline literal is rejected with an error that names the key, never
/// the value; and a resolved credential is redacted in the config's `Debug`. The
/// resolved-secret-plus-sibling-error case is a unit test in `src/main.rs`
/// (`rfc0020_6_*`, which reaches the private mapping).
/// See `docs/rfcs/0020-configuration-file.md` §5.
#[test]
fn rfc0020_6_secret_hygiene_across_the_file_path() {
    use ourios_server::config::file::parse;

    // An inline-literal credential is rejected; the error names the key only.
    let err = parse(
        "storage:\n  s3:\n    secret_access_key: AKIAINLINELITERAL\n",
        &|_| None,
    )
    .expect_err("inline literal");
    let msg = err.to_string();
    assert!(msg.contains("secret_access_key"), "names the key: {msg}");
    assert!(!msg.contains("AKIAINLINELITERAL"), "never the value: {msg}");

    // A resolved credential (a real value) is redacted in the config's `Debug`.
    let secret = "s3cr3t-resolved-value";
    let cfg = parse(
        "storage:\n  s3:\n    bucket: b\n    secret_access_key: ${env:SECRET}\n",
        &|name| (name == "SECRET").then(|| secret.to_owned()),
    )
    .expect("valid file");
    assert_eq!(cfg.storage.s3.secret_access_key.as_deref(), Some(secret));
    let rendered = format!("{:?}", cfg.storage.s3);
    assert!(
        !rendered.contains(secret),
        "the secret must not render: {rendered}"
    );
    assert!(
        rendered.contains("<redacted>"),
        "shows presence only: {rendered}"
    );
}

/// Every configuration file the repository ships or documents parses under the
/// RFC 0020 schema: the dogfood config, the config files the Helm chart's golden
/// render mounts, and each fenced `yaml` block in `docs/guides/` whose first
/// top-level key is an Ourios section (Collector snippets are skipped). A parser
/// swap or a schema change that breaks a real config fails here, not at startup.
#[test]
fn every_example_config_in_the_repo_parses() {
    use std::collections::BTreeMap;
    use std::path::Path;

    use ourios_server::config::file::parse;

    #[derive(serde::Deserialize)]
    struct Manifest {
        kind: Option<String>,
        data: Option<BTreeMap<String, String>>,
    }

    const SECTIONS: [&str; 6] = [
        "storage",
        "receiver",
        "querier",
        "compaction",
        "auth",
        "miner",
    ];

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |rel: &str| {
        std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    };
    let mut configs: Vec<(String, String)> = vec![(
        "dogfood-config.yaml".to_owned(),
        read("dogfood-config.yaml"),
    )];

    let render = read("deploy/helm/testdata/default-render.yaml");
    let manifests: Vec<Manifest> =
        serde_saphyr::from_multiple(&render).expect("golden Helm render is valid YAML");
    for manifest in manifests {
        if manifest.kind.as_deref() != Some("ConfigMap") {
            continue;
        }
        for (name, text) in manifest.data.into_iter().flatten() {
            configs.push((format!("default-render.yaml ConfigMap {name}"), text));
        }
    }

    let mut guides: Vec<_> = std::fs::read_dir(root.join("docs/guides"))
        .expect("docs/guides")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    guides.sort();
    for guide in guides {
        let text = std::fs::read_to_string(&guide).expect("read guide");
        let mut block: Option<String> = None;
        for line in text.lines() {
            match (&mut block, line.trim_end()) {
                (None, "```yaml") => block = Some(String::new()),
                (Some(_), "```") => {
                    let yaml = block.take().unwrap_or_default();
                    let first_key = yaml
                        .lines()
                        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
                        .and_then(|l| l.split_once(':'))
                        .map(|(key, _)| key.to_owned());
                    if first_key.is_some_and(|key| SECTIONS.contains(&key.as_str())) {
                        configs.push((guide.display().to_string(), yaml));
                    }
                }
                (Some(buf), _) => {
                    buf.push_str(line);
                    buf.push('\n');
                }
                (None, _) => {}
            }
        }
    }

    assert!(
        configs.len() >= 6,
        "expected the dogfood config, three Helm role configs and the guide examples; \
         found {}",
        configs.len()
    );
    for (origin, yaml) in &configs {
        if let Err(e) = parse(yaml, &|_| None) {
            panic!("{origin} does not parse: {e}");
        }
    }
}
