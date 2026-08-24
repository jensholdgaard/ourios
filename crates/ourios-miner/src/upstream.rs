//! Upstream-template grammar and alignment (RFC 0050 §3.1 / §3.4).
//!
//! A record may arrive already templated: the collector-contrib
//! `drainprocessor` annotates records with a `log.record.template`
//! string. That string is a *claim* about the body, produced by a
//! different tokenizer with its own masking rules, so nothing here
//! trusts it: [`parse_template`] admits exactly the one v1 grammar
//! RFC 0050 §3.1 pins down, and [`align`] verifies the claim
//! against the body under the miner's own tokenization
//! ([`crate::tokenize`]). Only a template that passes both is
//! eligible for adoption; every rejection routes the record to the
//! ordinary mining path.
//!
//! This module is pure: no config, no metrics, no interning. The
//! §3.2 byte cap runs *before* [`parse_template`] (the caller's
//! job — nothing here may do work proportional to an unbounded
//! input), and the adopt/observe/ignore dispatch, the RFC 0023
//! budget and the audit event live with the cluster.
//!
//! # Why alignment success implies byte-identical reconstruction
//!
//! [`align`] tokenizes the body with [`crate::tokenize::tokenize`],
//! whose contract is lossless: interleaving `separators` and
//! `tokens` reproduces the input exactly. Alignment only ever maps
//! template positions onto whole body tokens — a literal must
//! byte-equal its token, a wildcard captures its token verbatim —
//! so a successful alignment's `(params, separators)` interleaved
//! with the template is the body, byte for byte. That is invariant
//! §3.3 (`CLAUDE.md`) holding by construction rather than by
//! re-check; the property tests in `tests/it/rfc0050_properties.rs`
//! pin it corpus-wide (RFC0050.4).

use crate::tokenize::{TokenizeError, Tokenized, tokenize};
use crate::tree::OwnedToken;

/// One position of a parsed upstream template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamToken<'t> {
    /// Must byte-equal the body token at the same position.
    Literal(&'t str),
    /// Matches exactly one body token (RFC 0050 §3.1), capturing it
    /// as a parameter. `name` is `Some` for a `<name>` mask token
    /// (the drainprocessor's `masking_rules` emit these), `None`
    /// for the anonymous `<*>`. v1 keeps the name only through
    /// alignment — repeats are positional (§3.1), and the interned
    /// leaf normalises every wildcard to [`OwnedToken::Wildcard`].
    Wildcard { name: Option<&'t str> },
}

/// A `log.record.template` value that passed the §3.1 grammar.
///
/// Borrows from the attribute string; the caller decides whether
/// anything about it outlives the ingest call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTemplate<'t> {
    tokens: Vec<UpstreamToken<'t>>,
    wildcards: usize,
}

impl<'t> UpstreamTemplate<'t> {
    /// The parsed token sequence, in template order.
    #[must_use]
    pub fn tokens(&self) -> &[UpstreamToken<'t>] {
        &self.tokens
    }

    /// Number of wildcard positions (= parameters an alignment
    /// yields).
    #[must_use]
    pub fn wildcard_count(&self) -> usize {
        self.wildcards
    }

    /// The template in the tree's stored form. Mask names do not
    /// survive: `<ip>` and `<*>` both intern as
    /// [`OwnedToken::Wildcard`], which is deliberate — two upstream
    /// strings that differ only in mask names describe the same
    /// shape and must share a leaf (RFC 0050 §3.3).
    #[must_use]
    pub fn to_owned_tokens(&self) -> Vec<OwnedToken> {
        self.tokens
            .iter()
            .map(|t| match t {
                UpstreamToken::Literal(s) => OwnedToken::Fixed((*s).to_string()),
                UpstreamToken::Wildcard { .. } => OwnedToken::Wildcard,
            })
            .collect()
    }
}

/// The foreign placeholder syntax that got a template rejected
/// (RFC 0050 §3.1 "rejected outright"). Guessing which convention
/// a bare string is written in is how a store silently mis-parses
/// a line, so each recognised foreign syntax is refused by name
/// rather than treated as literal bytes that happen never to
/// match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlaceholderKind {
    /// `%` immediately followed by an ASCII letter (`%s`, `%d`, …).
    /// Deliberately broad: a genuine literal like `100%CPU` is
    /// also refused, and the record is mined — the safe fallback —
    /// rather than risking a printf template adopted as literals.
    Printf,
    /// `{}` or `{name}` where `name` is `[A-Za-z0-9_.]*` — the
    /// message-template convention #2064 describes. Outside the v1
    /// grammar (RFC 0050 §3.7); brace runs carrying other bytes
    /// (JSON fragments such as `{"a":1}`) stay literal.
    MessageTemplate,
    /// `$` immediately followed by `[A-Za-z_{]` (`$var`, `${var}`).
    ShellVar,
}

impl std::fmt::Display for PlaceholderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Printf => "printf",
            Self::MessageTemplate => "message-template",
            Self::ShellVar => "shell-variable",
        };
        f.write_str(s)
    }
}

/// Why [`parse_template`] refused a string (RFC 0050 §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GrammarError {
    /// No tokens — an all-whitespace or empty template can match
    /// nothing.
    Empty,
    /// The template string itself failed the miner's tokenizer
    /// (embedded NUL).
    Tokenize(TokenizeError),
    /// A literal token embeds the anonymous wildcard marker
    /// (`foo<*>bar`, `<*><*>`). An embedded `<*>` is unambiguously
    /// a mask over *part* of a token, which v1's
    /// one-wildcard-one-token rule cannot align — this is also the
    /// §3.1 "two adjacent wildcards" case, since whitespace
    /// tokenization makes standalone wildcard tokens never
    /// adjacent. An embedded `<name>` is *not* rejected here: it
    /// is indistinguishable from legitimate literal text
    /// (`id=<null>`), so it stays literal and alignment decides.
    EmbeddedWildcard { token_index: usize },
    /// A token carries a recognised non-§3.1 placeholder syntax.
    ForeignPlaceholder {
        token_index: usize,
        kind: PlaceholderKind,
    },
}

impl std::fmt::Display for GrammarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("template has no tokens"),
            Self::Tokenize(e) => write!(f, "template failed tokenization: {e}"),
            Self::EmbeddedWildcard { token_index } => {
                write!(f, "token {token_index} embeds `<*>` inside a literal")
            }
            Self::ForeignPlaceholder { token_index, kind } => {
                write!(f, "token {token_index} carries {kind} placeholder syntax")
            }
        }
    }
}

impl std::error::Error for GrammarError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tokenize(e) => Some(e),
            _ => None,
        }
    }
}

/// Parse a `log.record.template` value against the RFC 0050 §3.1
/// v1 grammar.
///
/// The grammar admits exactly one shape: whitespace-separated
/// tokens where a token is either the anonymous wildcard `<*>`, a
/// named wildcard `<name>` (any non-empty bracket body without
/// `<`/`>`), or a literal. Everything else is refused, never
/// guessed at.
///
/// # Errors
///
/// See [`GrammarError`]. Every error means "mine this record
/// instead" — parsing never partially succeeds.
pub fn parse_template(template: &str) -> Result<UpstreamTemplate<'_>, GrammarError> {
    let Tokenized { tokens, .. } = tokenize(template).map_err(GrammarError::Tokenize)?;
    if tokens.is_empty() {
        return Err(GrammarError::Empty);
    }

    let mut parsed = Vec::with_capacity(tokens.len());
    let mut wildcards = 0;
    for (token_index, raw) in tokens.into_iter().enumerate() {
        if let Some(name) = wildcard_name(raw) {
            wildcards += 1;
            let name = (name != "*").then_some(name);
            parsed.push(UpstreamToken::Wildcard { name });
            continue;
        }
        if raw.contains("<*>") {
            return Err(GrammarError::EmbeddedWildcard { token_index });
        }
        if let Some(kind) = foreign_placeholder(raw) {
            return Err(GrammarError::ForeignPlaceholder { token_index, kind });
        }
        parsed.push(UpstreamToken::Literal(raw));
    }

    Ok(UpstreamTemplate {
        tokens: parsed,
        wildcards,
    })
}

/// `Some(inner)` when the whole token is `<inner>` with a
/// non-empty, bracket-free body — the §3.1 wildcard shape. A
/// token like `<a><b>` or `user=<ip>` fails this and falls through
/// to literal classification.
fn wildcard_name(token: &str) -> Option<&str> {
    let inner = token.strip_prefix('<')?.strip_suffix('>')?;
    (!inner.is_empty() && !inner.contains(['<', '>'])).then_some(inner)
}

fn foreign_placeholder(token: &str) -> Option<PlaceholderKind> {
    let bytes = token.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let next = bytes.get(i + 1);
        match b {
            b'%' if next.is_some_and(u8::is_ascii_alphabetic) => {
                return Some(PlaceholderKind::Printf);
            }
            b'$' if next.is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_' || *n == b'{') => {
                return Some(PlaceholderKind::ShellVar);
            }
            b'{' => {
                let rest = &bytes[i + 1..];
                if let Some(close) = rest.iter().position(|&c| c == b'}') {
                    let body = &rest[..close];
                    if body
                        .iter()
                        .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')
                    {
                        return Some(PlaceholderKind::MessageTemplate);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// One captured parameter of a successful alignment, in template
/// order. Repeated mask names yield repeated entries (§3.1
/// "repeats are positional").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlignedParam<'t, 'b> {
    /// The `<name>` of the wildcard that captured this value, if
    /// it had one.
    pub name: Option<&'t str>,
    /// The body token, verbatim.
    pub value: &'b str,
}

/// A verified template-to-body alignment (RFC 0050 §3.4): the
/// parameters each wildcard captured and the body's inter-token
/// separators. Interleaving `separators` with the template's
/// literals and these parameter values reproduces the body byte
/// for byte — see the module docs for why that holds by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alignment<'t, 'b> {
    pub params: Vec<AlignedParam<'t, 'b>>,
    /// `separators.len() == template token count + 1`, the
    /// [`crate::tokenize`] convention the reconstruction walk
    /// expects.
    pub separators: Vec<&'b str>,
}

/// Why [`align`] refused to match a template against a body
/// (RFC 0050 §3.4 step 3: fall back to mining, never adopt
/// silently).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AlignError {
    /// The body failed the miner's tokenizer; such a record could
    /// not be mined either and takes the existing parse-failure
    /// path.
    Tokenize(TokenizeError),
    /// Token counts differ. Covers both §3.1 rejection shapes that
    /// only manifest against a body: a wildcard that would have to
    /// match zero tokens, and body tokens the template leaves
    /// unconsumed. A mask spanning whitespace (the drainprocessor's
    /// own documented caveat) lands here too: the masked body has
    /// more tokens than the template.
    TokenCountMismatch {
        template_tokens: usize,
        body_tokens: usize,
    },
    /// The literal at `token_index` is not byte-identical to the
    /// body token there — a literal segment that does not appear
    /// in the body in order (§3.1), e.g. a template from a
    /// different line.
    LiteralMismatch { token_index: usize },
}

impl std::fmt::Display for AlignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokenize(e) => write!(f, "body failed tokenization: {e}"),
            Self::TokenCountMismatch {
                template_tokens,
                body_tokens,
            } => write!(
                f,
                "template has {template_tokens} tokens, body has {body_tokens}"
            ),
            Self::LiteralMismatch { token_index } => {
                write!(f, "literal at token {token_index} does not match the body")
            }
        }
    }
}

impl std::error::Error for AlignError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tokenize(e) => Some(e),
            _ => None,
        }
    }
}

/// Align a parsed upstream template against a body (RFC 0050
/// §3.4 step 1): recover the parameters and the inter-token
/// separators, or say precisely why the template's claim about
/// this line is false.
///
/// Matching is over UTF-8 bytes — no normalisation, no case
/// folding, no whitespace collapsing (§3.1). Each wildcard
/// consumes exactly one body token; each literal must byte-equal
/// its body token.
///
/// # Errors
///
/// See [`AlignError`]. Every error means "mine this record
/// instead".
pub fn align<'t, 'b>(
    template: &UpstreamTemplate<'t>,
    body: &'b str,
) -> Result<Alignment<'t, 'b>, AlignError> {
    let Tokenized { tokens, separators } = tokenize(body).map_err(AlignError::Tokenize)?;

    if tokens.len() != template.tokens.len() {
        return Err(AlignError::TokenCountMismatch {
            template_tokens: template.tokens.len(),
            body_tokens: tokens.len(),
        });
    }

    let mut params = Vec::with_capacity(template.wildcards);
    for (token_index, (t, b)) in template.tokens.iter().zip(&tokens).enumerate() {
        match t {
            UpstreamToken::Literal(lit) => {
                if lit != b {
                    return Err(AlignError::LiteralMismatch { token_index });
                }
            }
            UpstreamToken::Wildcard { name } => {
                params.push(AlignedParam {
                    name: *name,
                    value: b,
                });
            }
        }
    }

    Ok(Alignment { params, separators })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(t: &str) -> UpstreamTemplate<'_> {
        parse_template(t).expect("template should parse")
    }

    /// Interleave the alignment back into a line — the §3.4 step-2
    /// check the tests assert explicitly even though it holds by
    /// construction.
    fn reassemble(template: &UpstreamTemplate<'_>, a: &Alignment<'_, '_>) -> String {
        let mut out = String::from(a.separators[0]);
        let mut next_param = 0;
        for (i, t) in template.tokens().iter().enumerate() {
            match t {
                UpstreamToken::Literal(s) => out.push_str(s),
                UpstreamToken::Wildcard { .. } => {
                    out.push_str(a.params[next_param].value);
                    next_param += 1;
                }
            }
            out.push_str(a.separators[i + 1]);
        }
        out
    }

    #[test]
    fn exact_match_adopts_with_params_and_separators() {
        let t = parse("user <*> logged in from <ip>");
        assert_eq!(t.wildcard_count(), 2);
        let body = "user alice logged in from 10.0.0.7";
        let a = align(&t, body).expect("aligns");
        assert_eq!(a.params.len(), 2);
        assert_eq!(a.params[0].value, "alice");
        assert_eq!(a.params[0].name, None);
        assert_eq!(a.params[1].value, "10.0.0.7");
        assert_eq!(a.params[1].name, Some("ip"));
        assert_eq!(reassemble(&t, &a), body);
    }

    #[test]
    fn separators_survive_byte_for_byte() {
        // Double spaces, a tab, leading + trailing whitespace: the
        // §3.3 invariant is about these exact bytes.
        let t = parse("a <*> b");
        let body = "  a\tx  b ";
        let a = align(&t, body).expect("aligns");
        assert_eq!(a.separators, vec!["  ", "\t", "  ", " "]);
        assert_eq!(reassemble(&t, &a), body);
    }

    #[test]
    fn unicode_whitespace_is_a_token_boundary() {
        // U+00A0 NBSP is whitespace under char::is_whitespace, so
        // the template and body tokenize identically around it.
        let t = parse("start\u{a0}<*>");
        let body = "start\u{a0}value";
        let a = align(&t, body).expect("aligns");
        assert_eq!(a.params[0].value, "value");
        assert_eq!(reassemble(&t, &a), body);
    }

    #[test]
    fn repeated_mask_names_are_positional() {
        let t = parse("<id> to <id>");
        let a = align(&t, "a to b").expect("aligns");
        assert_eq!(a.params.len(), 2);
        assert_eq!(a.params[0].value, "a");
        assert_eq!(a.params[1].value, "b");
        assert_eq!(a.params[0].name, Some("id"));
        assert_eq!(a.params[1].name, Some("id"));
    }

    #[test]
    fn matching_is_byte_exact_no_case_folding() {
        let t = parse("User <*> logged");
        assert_eq!(
            align(&t, "user alice logged").unwrap_err(),
            AlignError::LiteralMismatch { token_index: 0 },
        );
    }

    #[test]
    fn mask_spanning_whitespace_is_a_count_mismatch() {
        // The drainprocessor's documented caveat: a mask that
        // swallowed whitespace makes the template shorter than the
        // body. Never adopted.
        let t = parse("connect to <*>");
        assert_eq!(
            align(&t, "connect to host a on port b").unwrap_err(),
            AlignError::TokenCountMismatch {
                template_tokens: 3,
                body_tokens: 7,
            },
        );
    }

    #[test]
    fn template_longer_than_body_is_a_count_mismatch() {
        let t = parse("a b c <*>");
        assert_eq!(
            align(&t, "a b").unwrap_err(),
            AlignError::TokenCountMismatch {
                template_tokens: 4,
                body_tokens: 2,
            },
        );
    }

    #[test]
    fn template_from_a_different_line_is_a_literal_mismatch() {
        let t = parse("user <*> logged out");
        assert_eq!(
            align(&t, "user alice logged in").unwrap_err(),
            AlignError::LiteralMismatch { token_index: 3 },
        );
    }

    #[test]
    fn wildcard_never_matches_zero_tokens() {
        let t = parse("start <*> end");
        assert_eq!(
            align(&t, "start end").unwrap_err(),
            AlignError::TokenCountMismatch {
                template_tokens: 3,
                body_tokens: 2,
            },
        );
    }

    #[test]
    fn unconsumed_body_bytes_are_a_count_mismatch() {
        let t = parse("done");
        assert_eq!(
            align(&t, "done extra").unwrap_err(),
            AlignError::TokenCountMismatch {
                template_tokens: 1,
                body_tokens: 2,
            },
        );
    }

    #[test]
    fn printf_syntax_is_refused() {
        assert_eq!(
            parse_template("User %s logged in").unwrap_err(),
            GrammarError::ForeignPlaceholder {
                token_index: 1,
                kind: PlaceholderKind::Printf,
            },
        );
    }

    #[test]
    fn message_template_syntax_is_refused() {
        assert_eq!(
            parse_template("User {user.id} logged in").unwrap_err(),
            GrammarError::ForeignPlaceholder {
                token_index: 1,
                kind: PlaceholderKind::MessageTemplate,
            },
        );
        assert_eq!(
            parse_template("value {}").unwrap_err(),
            GrammarError::ForeignPlaceholder {
                token_index: 1,
                kind: PlaceholderKind::MessageTemplate,
            },
        );
    }

    #[test]
    fn shell_variable_syntax_is_refused() {
        assert_eq!(
            parse_template("path $HOME missing").unwrap_err(),
            GrammarError::ForeignPlaceholder {
                token_index: 1,
                kind: PlaceholderKind::ShellVar,
            },
        );
        assert_eq!(
            parse_template("path ${HOME} missing").unwrap_err(),
            GrammarError::ForeignPlaceholder {
                token_index: 1,
                kind: PlaceholderKind::ShellVar,
            },
        );
    }

    #[test]
    fn json_braces_stay_literal() {
        // A brace run whose body is not a bare identifier is
        // ordinary literal bytes, not message-template syntax.
        let t = parse(r#"payload {"a":1} sent"#);
        let a = align(&t, r#"payload {"a":1} sent"#).expect("aligns");
        assert!(a.params.is_empty());
    }

    #[test]
    fn embedded_anonymous_wildcard_is_refused() {
        assert_eq!(
            parse_template("user=<*> ok").unwrap_err(),
            GrammarError::EmbeddedWildcard { token_index: 0 },
        );
        // The "two adjacent wildcards" case: only expressible
        // inside one token, and that token embeds `<*>`.
        assert_eq!(
            parse_template("<*><*>").unwrap_err(),
            GrammarError::EmbeddedWildcard { token_index: 0 },
        );
    }

    #[test]
    fn embedded_named_mask_stays_literal_and_alignment_decides() {
        // `user=<ip>` is indistinguishable from literal text, so
        // it parses as a literal; a body carrying the mask
        // literally matches, a real masked value does not.
        let t = parse("src user=<ip>");
        assert!(matches!(t.tokens()[1], UpstreamToken::Literal("user=<ip>")));
        assert!(align(&t, "src user=<ip>").is_ok());
        assert_eq!(
            align(&t, "src user=10.0.0.7").unwrap_err(),
            AlignError::LiteralMismatch { token_index: 1 },
        );
    }

    #[test]
    fn bracketed_literal_reads_as_named_wildcard_and_still_reconstructs() {
        // `<init>` (a Java constructor frame) classifies as a
        // named wildcard. That is safe: it consumes exactly the
        // body token and the value round-trips, so reconstruction
        // holds; the only cost is an unnecessary parameter.
        let t = parse("at <init> frame");
        let a = align(&t, "at <init> frame").expect("aligns");
        assert_eq!(a.params[0].value, "<init>");
        assert_eq!(reassemble(&t, &a), "at <init> frame");
    }

    #[test]
    fn empty_and_whitespace_templates_are_refused() {
        assert_eq!(parse_template("").unwrap_err(), GrammarError::Empty);
        assert_eq!(parse_template("   \t ").unwrap_err(), GrammarError::Empty);
    }

    #[test]
    fn nul_in_template_or_body_is_a_tokenizer_failure() {
        assert!(matches!(
            parse_template("a\0b").unwrap_err(),
            GrammarError::Tokenize(TokenizeError::EmbeddedNul { .. }),
        ));
        let t = parse("a <*>");
        assert!(matches!(
            align(&t, "a b\0c").unwrap_err(),
            AlignError::Tokenize(TokenizeError::EmbeddedNul { .. }),
        ));
    }

    #[test]
    fn to_owned_tokens_normalises_mask_names() {
        let t = parse("user <ip> and <*>");
        assert_eq!(
            t.to_owned_tokens(),
            vec![
                OwnedToken::Fixed("user".to_string()),
                OwnedToken::Wildcard,
                OwnedToken::Fixed("and".to_string()),
                OwnedToken::Wildcard,
            ],
        );
    }

    #[test]
    fn errors_are_std_error_with_sources() {
        fn assert_error<E: std::error::Error>(_: &E) {}
        let g = parse_template("a\0").unwrap_err();
        assert_error(&g);
        assert!(std::error::Error::source(&g).is_some());
        let a = AlignError::LiteralMismatch { token_index: 0 };
        assert_error(&a);
        assert!(std::error::Error::source(&a).is_none());
    }
}
