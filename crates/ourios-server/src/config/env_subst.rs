//! Environment-variable substitution for the RFC 0020 configuration file.
//!
//! Mirrors the OpenTelemetry Configuration WG data model, applied to a single
//! scalar string value (the YAML walk that picks the scalar nodes is a separate
//! layer — RFC 0020 §3.3):
//!
//! - `${env:NAME}` and the prefix-less `${NAME}` both resolve `NAME` from the
//!   environment (via the injected `lookup`).
//! - `${NAME:-default}` / `${env:NAME:-default}` substitute `default` when
//!   `NAME` is unset **or empty**; an undefined reference with no default
//!   resolves to the empty string.
//! - `$$` is an escape for a literal `$`.
//! - A resolved value is inserted **verbatim** and never re-scanned —
//!   substitution is non-recursive and cannot inject further references (the
//!   security boundary in §3.3).
//! - A `${…}` reference that does not conform to the grammar
//!   (`${ [env:] NAME [:-DEFAULT] }`, `NAME` = `[A-Za-z_][A-Za-z0-9_]*`) is a
//!   [`MalformedReference`] error (RFC0020 §3.3 rule 8). An unterminated `${`
//!   (no `}`) is left verbatim — it is not a reference.
//!
//! See `docs/rfcs/0020-configuration-file.md` §3.3.

use std::fmt;

/// A `${…}` substitution reference that does not conform to the RFC 0020
/// grammar (e.g. `${1BAD}`, `${A$B}`, `${X:?oops}`).
///
/// Carries the offending reference text only — never a resolved value, so the
/// error is safe to log even when other scalars hold secrets (RFC 0019 §3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedReference {
    /// The non-conforming `${…}` reference, verbatim.
    pub reference: String,
}

impl fmt::Display for MalformedReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed substitution reference {}", self.reference)
    }
}

impl std::error::Error for MalformedReference {}

/// Resolve every substitution reference in one scalar value.
///
/// `lookup` maps an environment-variable name to its value (`None` when unset);
/// production passes `|n| std::env::var(n).ok()`. The value is treated as data:
/// it is inserted verbatim and never re-scanned for further references.
///
/// # Errors
///
/// Returns [`MalformedReference`] if a `${…}` reference does not conform to the
/// grammar. Resolution is all-or-nothing — on error no partial result escapes
/// (the caller fails the whole file, RFC0020 §3.3 rule 8 / RFC0020.5).
pub fn resolve(
    input: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, MalformedReference> {
    // The WG algorithm is escape-first: "consume the input left to right,
    // identify the next escape sequence, and match the content *since the prior
    // escape* against SUBSTITUTION-REF". So we split on `$$`, substitute each
    // segment, and emit a literal `$` between them. A `$$` inside a would-be
    // `${…}` therefore BREAKS the reference — it is not part of it and the ref
    // does not resolve (normative table row `${X:-$${Y}}` → `${X:-${Y}}`). This
    // is intentional; do not "fix" it by scanning refs across `$$`.
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let Some(i) = rest.find("$$") else {
            substitute_segment(rest, lookup, &mut out)?;
            return Ok(out);
        };
        substitute_segment(&rest[..i], lookup, &mut out)?;
        out.push('$');
        rest = &rest[i + 2..];
    }
}

/// Substitute every `${…}` reference in an escape-free segment into `out`.
///
/// The segment contains no `$$` — [`resolve`] has already split those out — so
/// a lone `$` here is only ever literal text or the start of a `${…}`.
fn substitute_segment(
    segment: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    out: &mut String,
) -> Result<(), MalformedReference> {
    let mut rest = segment;
    loop {
        let Some(open) = rest.find("${") else {
            out.push_str(rest);
            return Ok(());
        };
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        // The first `}` closes the reference: `GENERIC` is brace-free, so a
        // nested `${…}` in a default is captured as literal default text and is
        // not resolved (non-recursive — see §3.3). No `}` anywhere ⇒ not a
        // reference, so emit `${` verbatim and carry on.
        let Some(close) = after.find('}') else {
            out.push_str("${");
            rest = after;
            continue;
        };
        let body = &after[..close];
        let value = resolve_reference(body, lookup).ok_or_else(|| MalformedReference {
            reference: format!("${{{body}}}"),
        })?;
        out.push_str(&value);
        rest = &after[close + 1..];
    }
}

/// Resolve a reference body (the text between `${` and `}`). `None` ⇒ malformed.
fn resolve_reference(body: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let (name, default) = parse_body(body)?;
    Some(match lookup(name) {
        Some(value) if !value.is_empty() => value,
        // Unset or empty ⇒ the default, or empty when there is none.
        _ => default.unwrap_or("").to_owned(),
    })
}

/// Parse `[env:] NAME [:-DEFAULT]`. The optional `env:` prefix is consumed only
/// when what follows conforms; otherwise the whole body is parsed as
/// `NAME[:-DEFAULT]` (matching the WG grammar's backtracking, so `${env:-x}`
/// reads as name `env`, default `x`).
fn parse_body(body: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = body.strip_prefix("env:") {
        if let Some(parsed) = parse_env_substitution(rest) {
            return Some(parsed);
        }
    }
    parse_env_substitution(body)
}

/// Parse `NAME [:-DEFAULT]`, validating `NAME`. `None` ⇒ `NAME` is invalid.
fn parse_env_substitution(s: &str) -> Option<(&str, Option<&str>)> {
    let (name, default) = match s.split_once(":-") {
        Some((name, default)) => (name, Some(default)),
        None => (s, None),
    };
    is_valid_name(name).then_some((name, default))
}

/// `ENV-NAME = [A-Za-z_][A-Za-z0-9_]*` — the `OTel` Config WG / env-provider rule.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::{MalformedReference, resolve};

    /// A lookup over a fixed map (the WG worked-example environment).
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn wg_env() -> impl Fn(&str) -> Option<String> {
        // The environment from the OTel Config WG substitution worked examples.
        env(&[
            ("STRING_VALUE", "value"),
            ("BOOL_VALUE", "true"),
            ("INT_VALUE", "1"),
            ("FLOAT_VALUE", "1.1"),
            ("INVALID_MAP_VALUE", "value\nkey:value"),
            ("DO_NOT_REPLACE_ME", "Never use this value"),
            ("REPLACE_ME", "${DO_NOT_REPLACE_ME}"),
            ("VALUE_WITH_ESCAPE", "value$$"),
        ])
    }

    /// The string-level rows of the WG `data-model` substitution table — the
    /// rows whose behaviour is decided at the scalar-value level (the YAML
    /// node-type rows are exercised by the schema layer in a later slice).
    #[test]
    fn matches_the_otel_config_wg_vector_table() {
        let lookup = wg_env();
        let cases: &[(&str, &str)] = &[
            ("${STRING_VALUE}", "value"),
            ("${env:STRING_VALUE}", "value"),
            ("${INVALID_MAP_VALUE}", "value\nkey:value"),
            ("foo ${STRING_VALUE} ${FLOAT_VALUE}", "foo value 1.1"),
            ("${UNDEFINED_KEY}", ""),
            ("${UNDEFINED_KEY:-fallback}", "fallback"),
            ("${UNDEFINED_KEY:-${STRING_VALUE}}", "${STRING_VALUE}"),
            ("${REPLACE_ME}", "${DO_NOT_REPLACE_ME}"),
            ("$${STRING_VALUE}", "${STRING_VALUE}"),
            ("$$${STRING_VALUE}", "$value"),
            ("$$$${STRING_VALUE}", "$${STRING_VALUE}"),
            ("$${STRING_VALUE:-fallback}", "${STRING_VALUE:-fallback}"),
            (
                "$${STRING_VALUE:-${STRING_VALUE}}",
                "${STRING_VALUE:-value}",
            ),
            (
                "${UNDEFINED_KEY:-$${UNDEFINED_KEY}}",
                "${UNDEFINED_KEY:-${UNDEFINED_KEY}}",
            ),
            ("${VALUE_WITH_ESCAPE}", "value$$"),
            ("a $$ b", "a $ b"),
            ("a $ b", "a $ b"),
        ];
        for (input, want) in cases {
            assert_eq!(
                resolve(input, &lookup).as_deref(),
                Ok(*want),
                "input {input:?}"
            );
        }
    }

    /// The WG algorithm processes the `$$` escape **before** matching a
    /// `${…}` reference, so a `$$` inside a would-be reference (or its default)
    /// breaks the reference rather than being part of it — the ref does not
    /// resolve. Mirrors the normative table row `${X:-$${Y}}` → `${X:-${Y}}`.
    #[test]
    fn escape_is_processed_before_reference_matching() {
        let lookup = env(&[("NAME", "v")]); // `A`/`B` are unset
        let cases: &[(&str, &str)] = &[
            // `$$` splits the ref: `${NAME:-foo` has no `}` ⇒ literal, then
            // `$` (escape), then `bar}` ⇒ the default is NOT applied.
            ("${NAME:-foo$$bar}", "${NAME:-foo$bar}"),
            // `${A` (no `}` before `$$`) is not a reference ⇒ no malformed-ref
            // error; the `$$` collapses to one `$`.
            ("${A$$B}", "${A$B}"),
            // A single `$` in a default IS part of the default and resolves.
            ("${A:-foo$bar}", "foo$bar"),
        ];
        for (input, want) in cases {
            assert_eq!(
                resolve(input, &lookup).as_deref(),
                Ok(*want),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn default_applies_when_unset_or_empty() {
        let lookup = env(&[("SET", "v"), ("EMPTY", "")]);
        assert_eq!(resolve("${SET:-d}", &lookup).unwrap(), "v");
        assert_eq!(resolve("${EMPTY:-d}", &lookup).unwrap(), "d");
        assert_eq!(resolve("${UNSET:-d}", &lookup).unwrap(), "d");
        assert_eq!(resolve("${UNSET:-}", &lookup).unwrap(), "");
    }

    #[test]
    fn env_prefix_is_equivalent_and_backtracks() {
        let lookup = env(&[("NAME", "v"), ("env", "e")]);
        assert_eq!(resolve("${NAME}", &lookup).unwrap(), "v");
        assert_eq!(resolve("${env:NAME}", &lookup).unwrap(), "v");
        // `${env:-x}` backtracks to name `env`, default `x` (env is set ⇒ "e").
        assert_eq!(resolve("${env:-x}", &lookup).unwrap(), "e");
    }

    #[test]
    fn malformed_references_error_naming_the_reference() {
        let lookup = env(&[]);
        for bad in ["${1BAD}", "${A$B}", "${X:?oops}", "${ NAME}", "${}"] {
            let err = resolve(bad, &lookup).expect_err(bad);
            assert_eq!(
                err,
                MalformedReference {
                    reference: bad.to_owned()
                }
            );
        }
    }

    #[test]
    fn unterminated_open_is_literal() {
        let lookup = env(&[("X", "v")]);
        assert_eq!(resolve("${X", &lookup).unwrap(), "${X");
        assert_eq!(resolve("a ${X b", &lookup).unwrap(), "a ${X b");
    }

    proptest! {
        /// Never panics on arbitrary input.
        #[test]
        fn never_panics(input in ".*") {
            let lookup = |_: &str| None;
            let _ = resolve(&input, &lookup);
        }

        /// A `$`-free string is returned unchanged.
        #[test]
        fn dollar_free_is_identity(input in "[^$]*") {
            let lookup = |_: &str| None;
            prop_assert_eq!(resolve(&input, &lookup).unwrap(), input);
        }

        /// A resolved value is inserted verbatim — non-recursive: even a value
        /// that itself looks like a reference or an escape is not re-scanned.
        #[test]
        fn value_inserted_verbatim(value in ".{0,32}") {
            let v = value.clone();
            let lookup = move |n: &str| (n == "X").then(|| v.clone());
            // Non-empty values round-trip exactly; empty falls back to "" too.
            prop_assert_eq!(resolve("${X}", &lookup).unwrap(), value);
        }

        /// `$$` always escapes to a single literal `$` (no reference, no value).
        #[test]
        fn double_dollar_escapes(segs in proptest::collection::vec("[^$]*", 1..6)) {
            let input = segs.join("$$");
            let want = segs.join("$");
            let lookup = |_: &str| None;
            prop_assert_eq!(resolve(&input, &lookup).unwrap(), want);
        }
    }
}
