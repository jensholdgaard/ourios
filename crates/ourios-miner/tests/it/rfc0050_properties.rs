//! RFC 0050 §5 property obligations for the upstream grammar
//! (the RFC0050.4 render-equals-body arm, at the module level).
//!
//! Two properties over generated inputs:
//!
//! 1. **Constructed masks align.** For any tokenized line, masking
//!    an arbitrary subset of tokens with `<*>` yields a template
//!    that parses, aligns, captures exactly the masked tokens as
//!    parameters, and interleaves back to the original line byte
//!    for byte — whatever the separators were.
//! 2. **Arbitrary inputs are safe.** For *any* pair of strings, a
//!    successful parse + align implies byte-identical
//!    reconstruction; everything else is a typed rejection, never
//!    a panic and never a silent partial match.

use ourios_miner::upstream::{Alignment, UpstreamTemplate, UpstreamToken, align, parse_template};
use proptest::prelude::*;

/// Interleave an alignment back into a line — the oracle for the
/// §3.4 byte-identity claim.
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

/// Literal-token alphabet that cannot collide with the wildcard
/// shape (`<`/`>`) or the foreign-placeholder detectors
/// (`%`/`$`/`{`/`}`) — those rejections get their own coverage in
/// the unit table and in `arbitrary_inputs_are_safe`.
fn literal_token() -> impl Strategy<Value = String> {
    prop::string::string_regex("[A-Za-z0-9_.:=,/-]{1,10}").expect("valid regex")
}

fn whitespace() -> impl Strategy<Value = String> {
    prop::string::string_regex("[ \t]{1,3}").expect("valid regex")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(ourios_testgen::proptest_cases(64)))]

    #[test]
    fn constructed_masks_align_and_reconstruct(
        tokens in prop::collection::vec(literal_token(), 1..12),
        mask in prop::collection::vec(any::<bool>(), 12),
        seps in prop::collection::vec(whitespace(), 12),
        lead in prop::option::of(whitespace()),
        trail in prop::option::of(whitespace()),
    ) {
        let mut body = lead.unwrap_or_default();
        for (i, tok) in tokens.iter().enumerate() {
            if i > 0 {
                body.push_str(&seps[i]);
            }
            body.push_str(tok);
        }
        body.push_str(&trail.unwrap_or_default());

        let template_str = tokens
            .iter()
            .enumerate()
            .map(|(i, tok)| if mask[i] { "<*>" } else { tok.as_str() })
            .collect::<Vec<_>>()
            .join(" ");

        let template = parse_template(&template_str).expect("constructed template parses");
        let a = align(&template, &body).expect("constructed template aligns");

        prop_assert_eq!(reassemble(&template, &a), body.clone());

        let expected: Vec<&str> = tokens
            .iter()
            .zip(&mask)
            .filter(|(_, m)| **m)
            .map(|(tok, _)| tok.as_str())
            .collect();
        let captured: Vec<&str> = a.params.iter().map(|p| p.value).collect();
        prop_assert_eq!(captured, expected);
    }

    #[test]
    fn arbitrary_inputs_are_safe(
        template in ".{0,80}",
        body in ".{0,80}",
    ) {
        if let Ok(parsed) = parse_template(&template)
            && let Ok(a) = align(&parsed, &body)
        {
            prop_assert_eq!(reassemble(&parsed, &a), body.clone());
        }
    }
}
