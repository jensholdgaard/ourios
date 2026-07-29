//! Plan-time resolution of a `body ==` literal against the tenant's
//! template registry (RFC 0044 §3.1, the template arm's candidate set).
//!
//! The literal is tokenized with the miner's own tokenizer and unified
//! position-wise against every registered `(template_id, version)`'s
//! canonical tokens: `Fixed` tokens must match byte-for-byte, `Wildcard`
//! positions capture the literal's token as the implied parameter value.
//! The result is a *candidate superset*: separators and stored parameter
//! values (overflow spills, RFC 0023) are per-record, so exact equality
//! is settled at scan time against the candidates — but only ever inside
//! row groups the candidate `template_id`s admit, which is what keeps
//! the arm prunable. Soundness of the whole inversion rests on the
//! `CLAUDE.md` §3.3 bit-identical reconstruction invariant.

use ourios_miner::tokenize::tokenize;
use ourios_miner::tree::OwnedToken;

use crate::template_registry::TemplateRegistry;

/// One template the literal could have mined to: the version-qualified
/// id plus the parameter values implied by the literal's tokens at the
/// template's wildcard positions (in wildcard order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLiteralMatch {
    pub template_id: u64,
    pub template_version: u32,
    /// The literal's tokens at the template's `Wildcard` positions.
    /// Empty for a zero-parameter template — the whole-body-is-the-
    /// template case (event-name-bodied `GenAI` records).
    pub params: Vec<String>,
}

/// Resolve `literal` to every registry entry it unifies with.
///
/// A literal the tokenizer rejects (embedded NUL — the parse-failure
/// path at ingest, which always retains the body) matches no template:
/// such records are only ever reachable through the physical body arm,
/// so an empty candidate set is exact, not lossy.
#[must_use]
pub fn body_literal_candidates(
    registry: &TemplateRegistry,
    literal: &str,
) -> Vec<BodyLiteralMatch> {
    let Ok(tokenized) = tokenize(literal) else {
        return Vec::new();
    };
    let mut matches: Vec<BodyLiteralMatch> = registry
        .iter()
        .filter_map(|(&(template_id, template_version), tokens)| {
            unify(tokens, &tokenized.tokens).map(|params| BodyLiteralMatch {
                template_id,
                template_version,
                params,
            })
        })
        .collect();
    // Deterministic plan output: registry iteration order is a HashMap's.
    matches.sort_by_key(|m| (m.template_id, m.template_version));
    matches
}

/// Position-wise unification: same arity, every `Fixed` equal, every
/// `Wildcard` capturing. Returns the captured parameter values in
/// wildcard order.
fn unify(template: &[OwnedToken], literal_tokens: &[&str]) -> Option<Vec<String>> {
    if template.len() != literal_tokens.len() {
        return None;
    }
    let mut params = Vec::new();
    for (token, &literal_token) in template.iter().zip(literal_tokens) {
        match token {
            OwnedToken::Fixed(fixed) if fixed == literal_token => {}
            OwnedToken::Fixed(_) => return None,
            OwnedToken::Wildcard => params.push(literal_token.to_owned()),
        }
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_miner::tree::parse_template;

    fn registry(entries: &[((u64, u32), &str)]) -> TemplateRegistry {
        entries
            .iter()
            .map(|&(key, canonical)| (key, parse_template(canonical)))
            .collect()
    }

    #[test]
    fn zero_param_template_matches_its_exact_body() {
        let reg = registry(&[((7, 1), "claude_code.api_request")]);
        assert_eq!(
            body_literal_candidates(&reg, "claude_code.api_request"),
            vec![BodyLiteralMatch {
                template_id: 7,
                template_version: 1,
                params: Vec::new(),
            }],
        );
    }

    #[test]
    fn parameterized_template_captures_the_implied_params_in_order() {
        let reg = registry(&[((3, 2), "user <*> logged in from <*>")]);
        assert_eq!(
            body_literal_candidates(&reg, "user 4711 logged in from 10.0.0.3"),
            vec![BodyLiteralMatch {
                template_id: 3,
                template_version: 2,
                params: vec!["4711".to_owned(), "10.0.0.3".to_owned()],
            }],
        );
    }

    #[test]
    fn a_literal_may_unify_with_several_templates_deterministically() {
        let reg = registry(&[
            ((9, 1), "user <*> logged in"),
            ((2, 1), "<*> <*> logged in"),
            ((5, 1), "user rooted logged out"),
        ]);
        assert_eq!(
            body_literal_candidates(&reg, "user 42 logged in"),
            vec![
                BodyLiteralMatch {
                    template_id: 2,
                    template_version: 1,
                    params: vec!["user".to_owned(), "42".to_owned()],
                },
                BodyLiteralMatch {
                    template_id: 9,
                    template_version: 1,
                    params: vec!["42".to_owned()],
                },
            ],
        );
    }

    #[test]
    fn arity_and_fixed_token_mismatches_do_not_unify() {
        let reg = registry(&[((1, 1), "user <*> logged in")]);
        assert!(body_literal_candidates(&reg, "user 42 logged out").is_empty());
        assert!(body_literal_candidates(&reg, "user 42 logged in twice").is_empty());
        assert!(body_literal_candidates(&reg, "user 42").is_empty());
    }

    #[test]
    fn versions_of_one_template_are_independent_candidates() {
        let reg = registry(&[((4, 1), "job <*> finished"), ((4, 2), "job <*> <*>")]);
        assert_eq!(
            body_literal_candidates(&reg, "job 8 finished"),
            vec![
                BodyLiteralMatch {
                    template_id: 4,
                    template_version: 1,
                    params: vec!["8".to_owned()],
                },
                BodyLiteralMatch {
                    template_id: 4,
                    template_version: 2,
                    params: vec!["8".to_owned(), "finished".to_owned()],
                },
            ],
        );
    }

    #[test]
    fn a_nul_bearing_literal_matches_no_template() {
        // The parse-failure ingest path retains such bodies verbatim, so
        // the physical arm alone is exact for them.
        let reg = registry(&[((1, 1), "claude_code.api_request")]);
        assert!(body_literal_candidates(&reg, "claude\0code").is_empty());
    }
}
