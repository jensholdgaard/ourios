//! Token-level masking for the Drain-derived miner.
//!
//! Per RFC 0001 §4.2 / §6.2 step 2: tokens matching a configured
//! shape rule are replaced with their type tag (e.g. `<NUM>`) and
//! the original bytes are emitted as a typed parameter. The tree
//! walk routes against the type tag; reconstruction (§6.6) walks
//! the parallel `typed_params` to recover the original line bytes.
//!
//! This PR ships the minimal default rule set the tree walk needs
//! to start working: UUID, IPv4, NUM. The other RFC §6.1 variants
//! (`Hex`, `Ts`, `Path`, `Str`, `Overflow`) are reserved on the
//! enum because the Parquet record schema (§6.1) is the data
//! contract, but they have no `mask()` emitter yet — `Str` is
//! added by the widening PR (§6.2 step 5b), `Overflow` by the
//! byte-limit PR (§6.5), and `Hex` / `Ts` / `Path` by future
//! masking-rule PRs.

/// The type assigned to a masked parameter slot.
///
/// Matches RFC 0001 §6.1's `ParamType`. Not every variant has a
/// `mask()` emitter in this PR (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamType {
    Ip,
    Uuid,
    Num,
    Hex,
    Ts,
    Path,
    Str,
    Overflow,
}

/// One typed parameter extracted by the masking pass.
///
/// `value` borrows from the input tokens — masking is a pure
/// classification step that allocates only the output `Vec`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedParam<'a> {
    pub type_tag: ParamType,
    pub value: &'a str,
}

/// Output of [`mask`].
///
/// `tokens.len()` always equals the input token count
/// (position-preserving): each entry is either the original token
/// (unmasked) or the type-tag string (masked). `typed_params`
/// holds, in match order, one entry per masked position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Masked<'a> {
    pub tokens: Vec<&'a str>,
    pub typed_params: Vec<TypedParam<'a>>,
}

/// Apply the default masking rules to a tokenized line.
///
/// Rules are tried in order: UUID, IPv4, NUM (most specific first,
/// so a 36-char UUID is not misclassified as `NUM`, and a dotted
/// quad like `1.2.3.4` is not split into `NUM` tokens by the
/// upstream tokenizer — punctuation stays inside tokens per
/// `RFC0001.3`). The first match wins; an unmatched token stays
/// literal. Per RFC 0001 §6.2 the emitted `tokens` are what the
/// tree walk descends with, and `typed_params` is what
/// reconstruction (§6.6) substitutes back.
#[must_use]
pub fn mask<'a>(tokens: &[&'a str]) -> Masked<'a> {
    let mut out_tokens = Vec::with_capacity(tokens.len());
    let mut typed_params = Vec::new();
    for &tok in tokens {
        if let Some((type_tag, tag_str)) = classify(tok) {
            out_tokens.push(tag_str);
            typed_params.push(TypedParam {
                type_tag,
                value: tok,
            });
        } else {
            out_tokens.push(tok);
        }
    }
    Masked {
        tokens: out_tokens,
        typed_params,
    }
}

fn classify(token: &str) -> Option<(ParamType, &'static str)> {
    if is_uuid(token) {
        Some((ParamType::Uuid, "<UUID>"))
    } else if is_ipv4(token) {
        Some((ParamType::Ip, "<IP>"))
    } else if is_num(token) {
        Some((ParamType::Num, "<NUM>"))
    } else {
        None
    }
}

/// 8-4-4-4-12 hex digits with `-` separators (RFC 4122 textual).
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    for (i, &b) in s.as_bytes().iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// Four dot-separated decimal octets, each `0..=255`. Octets with
/// a leading zero (other than the single digit `0`) are rejected
/// to avoid colliding with zero-padded numeric fields.
fn is_ipv4(s: &str) -> bool {
    let mut parts = 0u8;
    for octet in s.split('.') {
        parts += 1;
        if parts > 4 || octet.is_empty() || octet.len() > 3 {
            return false;
        }
        if octet.len() > 1 && octet.starts_with('0') {
            return false;
        }
        match octet.parse::<u16>() {
            Ok(v) if v <= 255 => {}
            _ => return false,
        }
    }
    parts == 4
}

/// All ASCII digits, optional leading `-`, length ≥ 1 digit.
fn is_num(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_matches_rfc_section_3_5_worked_example() {
        // Arrange — the input from RFC 0001 §3.5 (after tokenize):
        //   "user 42 logged in from 10.0.0.1"
        let tokens = ["user", "42", "logged", "in", "from", "10.0.0.1"];

        // Act
        let r = mask(&tokens);

        // Assert — RFC §3.5 says the miner sees:
        //   user <NUM> logged in from <IP>
        // with the original 42 and 10.0.0.1 captured as typed params.
        assert_eq!(
            r.tokens,
            vec!["user", "<NUM>", "logged", "in", "from", "<IP>"],
        );
        assert_eq!(r.typed_params.len(), 2);
        assert_eq!(r.typed_params[0].type_tag, ParamType::Num);
        assert_eq!(r.typed_params[0].value, "42");
        assert_eq!(r.typed_params[1].type_tag, ParamType::Ip);
        assert_eq!(r.typed_params[1].value, "10.0.0.1");
    }

    #[test]
    fn mask_classifies_uuid() {
        // Arrange
        let tokens = ["request", "550e8400-e29b-41d4-a716-446655440000"];

        // Act
        let r = mask(&tokens);

        // Assert
        assert_eq!(r.tokens, vec!["request", "<UUID>"]);
        assert_eq!(r.typed_params.len(), 1);
        assert_eq!(r.typed_params[0].type_tag, ParamType::Uuid);
        assert_eq!(
            r.typed_params[0].value,
            "550e8400-e29b-41d4-a716-446655440000",
        );
    }

    #[test]
    fn mask_classifies_negative_integer_as_num() {
        // Arrange
        let tokens = ["delta", "-5"];

        // Act
        let r = mask(&tokens);

        // Assert
        assert_eq!(r.tokens, vec!["delta", "<NUM>"]);
        assert_eq!(r.typed_params.len(), 1);
        assert_eq!(r.typed_params[0].type_tag, ParamType::Num);
        assert_eq!(r.typed_params[0].value, "-5");
    }

    #[test]
    fn mask_unrecognized_tokens_stay_literal() {
        // Arrange
        let tokens = ["hello", "world", "not_a_number_or_ip"];

        // Act
        let r = mask(&tokens);

        // Assert
        assert_eq!(r.tokens, vec!["hello", "world", "not_a_number_or_ip"]);
        assert!(r.typed_params.is_empty());
    }

    #[test]
    fn mask_typed_param_value_borrows_from_input() {
        // Arrange — pin the lifetime contract: `value` is a slice
        // of the input token, never an owned/duplicated string. A
        // bug that called `.to_string()` somewhere would still pass
        // string-equality assertions but fail this pointer check.
        let owned = String::from("42");
        let input_ptr = owned.as_str().as_ptr();
        let tokens = [owned.as_str()];

        // Act
        let r = mask(&tokens);

        // Assert
        let param_ptr = r.typed_params[0].value.as_ptr();
        assert_eq!(
            param_ptr, input_ptr,
            "typed_params[0].value must borrow from the input token, not allocate",
        );
    }

    #[test]
    fn is_ipv4_rejects_octet_above_255() {
        // Arrange
        let s = "10.0.0.256";

        // Act
        let r = is_ipv4(s);

        // Assert
        assert!(!r);
    }

    #[test]
    fn is_ipv4_rejects_leading_zero_octet() {
        // Arrange — "01" is ambiguous (octal in some languages);
        // reject to avoid surprising matches on padded fields.
        let s = "10.0.0.01";

        // Act
        let r = is_ipv4(s);

        // Assert
        assert!(!r);
    }
}
