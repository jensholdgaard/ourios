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

/// The moved receiver modules: reaching any of them as the next path
/// segment after `ourios_ingester::receiver::` is an offence.
const FORBIDDEN_MODULES: &[&str] = &["auth", "tls", "tls_serve", "propagation"];

/// The moved names formerly re-exported at the receiver root;
/// server/querier code must take these from `ourios_serving` now.
/// (`ReceiveError` and the pipeline types stay legitimately
/// ingester-owned, so only the moved names are policed.)
const FORBIDDEN_ROOT_NAMES: &[&str] = &[
    "AuthBinding",
    "AuthError",
    "AuthResolver",
    "GraphIdentity",
    "authenticate_bearer",
    "HeaderExtractor",
    "MetadataExtractor",
    "extract_context",
    "extract_context_from_metadata",
    "TlsSettings",
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

/// What follows one `ourios_ingester::receiver::` occurrence: either a
/// brace group (possibly spanning lines — `{auth, tls::TlsSettings}`)
/// or a plain path tail. Returned as the span to test names against,
/// so brace-grouped, nested, and multi-line import forms are all in
/// scope — a line-based fragment search misses the grouped ones.
fn reached_span(text: &str, after: usize) -> &str {
    let rest = &text[after..];
    let trimmed = rest.trim_start();
    if let Some(group) = trimmed.strip_prefix('{') {
        let mut depth = 1usize;
        for (i, c) in group.char_indices() {
            match c {
                '{' => depth += 1,
                '}' if depth == 1 => return &group[..i],
                '}' => depth -= 1,
                _ => {}
            }
        }
        group
    } else {
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(rest.len());
        &rest[..end]
    }
}

fn names_offend(span: &str) -> bool {
    let mentions = |name: &str| {
        span.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|word| word == name)
    };
    FORBIDDEN_MODULES.iter().copied().any(mentions)
        || FORBIDDEN_ROOT_NAMES.iter().copied().any(mentions)
}

/// Given the workspace after the move, When `ourios-server` and
/// `ourios-querier` sources (src + tests) are searched for the moved
/// modules or the moved root-re-exported names reached through any
/// `ourios_ingester::receiver::` path — plain, nested, or
/// brace-grouped across lines — Then no match exists.
#[test]
fn rfc0051_1_server_and_querier_shed_the_ingest_serving_paths() {
    const RECEIVER_PATH: &str = "ourios_ingester::receiver::";

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
        let mut from = 0;
        while let Some(hit) = text[from..].find(RECEIVER_PATH) {
            let after = from + hit + RECEIVER_PATH.len();
            let span = reached_span(&text, after);
            if names_offend(span) {
                let line = text[..from + hit].lines().count();
                offences.push(format!(
                    "{}:{}: ourios_ingester::receiver::{}",
                    path.display(),
                    line,
                    span.trim()
                ));
            }
            from = after;
        }
    }
    assert!(
        offences.is_empty(),
        "RFC0051.1: serving plumbing must come from ourios-serving, found:\n{}",
        offences.join("\n")
    );
}

#[cfg(test)]
mod gate_self_tests {
    use super::{names_offend, reached_span};

    /// The scanner sees through every import form the gate must catch —
    /// including the brace-grouped and multi-line shapes a line-based
    /// fragment search misses — and stays quiet on the paths that
    /// legitimately remain ingester-owned.
    #[test]
    fn scanner_catches_grouped_and_nested_forms() {
        const P: &str = "ourios_ingester::receiver::";
        let catches = [
            "use ourios_ingester::receiver::auth::AuthResolver;",
            "use ourios_ingester::receiver::AuthResolver;",
            "ourios_ingester::receiver::AuthResolver::static_only(None)",
            "use ourios_ingester::receiver::{AuthResolver, IngestPipeline};",
            "use ourios_ingester::receiver::{\n    CommitCoordinator,\n    TlsSettings,\n};",
            "use ourios_ingester::receiver::{tls::{ALPN_HTTP}, pipeline::Journal};",
        ];
        for text in catches {
            let after = text.find(P).expect("test data") + P.len();
            assert!(
                names_offend(reached_span(text, after)),
                "must catch: {text}"
            );
        }
        let passes = [
            "use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline};",
            "use ourios_ingester::receiver::pipeline::RotationHook;",
            "use ourios_ingester::receiver::ReceiveError;",
        ];
        for text in passes {
            let after = text.find(P).expect("test data") + P.len();
            assert!(
                !names_offend(reached_span(text, after)),
                "must pass: {text}"
            );
        }
    }
}
