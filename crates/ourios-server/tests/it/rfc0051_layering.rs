//! Scenario RFC0051.1 — the querier role sheds the ingest crate.
//! See `docs/rfcs/0051-serving-crate-extraction.md` §5.
//!
//! The serving plumbing (auth, TLS, propagation) lives in
//! `ourios-serving`; nothing under `ourios-server` or `ourios-querier`
//! may reach it through `ourios_ingester::receiver::*` paths again —
//! that path is exactly the coupling RFC 0051 removed (the querier
//! role compiling the ingest pipeline to get an auth check). The
//! deprecated shims in `ourios-ingester` exist for *external*
//! consumers only (RFC0051.7) and die in the next breaking release.

use std::path::{Path, PathBuf};

/// The moved modules, as the path fragments that must not reappear.
const FORBIDDEN: &[&str] = &[
    "ourios_ingester::receiver::auth",
    "ourios_ingester::receiver::tls",
    "ourios_ingester::receiver::tls_serve",
    "ourios_ingester::receiver::propagation",
];

/// Names the serving plumbing re-exported at the receiver root before
/// RFC 0051; server/querier code must import these from
/// `ourios_serving` now. (`ReceiveError` and the pipeline types stay
/// legitimately ingester-owned, so only the moved names are policed.)
const FORBIDDEN_ROOT_REEXPORTS: &[&str] = &[
    "receiver::AuthBinding",
    "receiver::AuthError",
    "receiver::AuthResolver",
    "receiver::GraphIdentity",
    "receiver::authenticate_bearer",
    "receiver::HeaderExtractor",
    "receiver::MetadataExtractor",
    "receiver::extract_context",
    "receiver::TlsSettings",
];

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("readable source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Given the workspace after the move, When `ourios-server` and
/// `ourios-querier` sources (src + tests) are searched for the moved
/// modules' old paths, Then no match exists.
#[test]
fn rfc0051_1_server_and_querier_shed_the_ingest_serving_paths() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/");
    let mut sources = Vec::new();
    for crate_dir in ["ourios-server", "ourios-querier"] {
        for tree in ["src", "tests"] {
            let root = workspace.join(crate_dir).join(tree);
            if root.exists() {
                rust_sources(&root, &mut sources);
            }
        }
    }
    assert!(
        sources.len() > 20,
        "sanity: the walk found only {} files — wrong root?",
        sources.len()
    );

    let mut offences = Vec::new();
    for path in sources {
        // This file names the forbidden fragments as its test data.
        if path.ends_with("rfc0051_layering.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable source file");
        for (lineno, line) in text.lines().enumerate() {
            if FORBIDDEN.iter().any(|f| line.contains(f))
                || (line.contains("ourios_ingester::")
                    && FORBIDDEN_ROOT_REEXPORTS.iter().any(|f| line.contains(f)))
            {
                offences.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    lineno + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "RFC0051.1: serving plumbing must come from ourios-serving, found:\n{}",
        offences.join("\n")
    );
}
