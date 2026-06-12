//! Token-level masking for the Drain-derived miner.
//!
//! Per RFC 0001 §4.2 / §6.2 step 2: tokens matching a configured
//! shape rule are replaced with their type tag (e.g. `<NUM>`) and
//! the original bytes are emitted as a typed parameter. The tree
//! walk routes against the type tag; reconstruction (§6.6) walks
//! the parallel `typed_params` to recover the original line bytes.
//!
//! Default rule set: UUID, IPv4, NUM. The other RFC §6.1
//! variants (`Hex`, `Ts`, `Path`, `Str`, `Overflow`) are reserved
//! on [`ParamType`] because the Parquet record schema (§6.1) is
//! the data contract, but they have no `mask()` emitter — `Str`
//! is added by the widening PR (§6.2 step 5b), `Overflow` by the
//! byte-limit PR (§6.5), and `Hex` / `Ts` / `Path` by future
//! masking-rule PRs. Internally the masking-emit subset is the
//! private `MaskTag` enum, kept separate from [`ParamType`] so its
//! tag-string and [`ParamType`] mappings are exhaustive total
//! functions the compiler will hold us to.
//!
//! `ParamType` itself lives in `ourios-core::audit` so the audit
//! event schema and the masker share one type rather than a
//! near-identical pair.

pub use ourios_core::audit::ParamType;

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
/// (unmasked) or the type-tag string (masked).
///
/// `wildcard_positions` lists the line-token indices where mask
/// substituted a tag, ascending by construction (single forward
/// pass). It is **parallel** to [`Self::typed_params`]: the
/// `k`-th masked position is `wildcard_positions[k]`, and that
/// position's `ParamType` is `typed_params[k].type_tag`. Consumers
/// that need a per-position classification must use this field
/// rather than re-matching against the tag-string content of
/// [`Self::tokens`] — the tag strings collide with literal log
/// tokens that happen to read `"<NUM>"` / `"<IP>"` / `"<UUID>"`
/// (mask leaves such literals unchanged), so string-shape
/// inference mis-classifies them as the corresponding `ParamType`.
///
/// `typed_params` holds, in match order, one entry per masked
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Masked<'a> {
    pub tokens: Vec<&'a str>,
    pub wildcard_positions: Vec<usize>,
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
    let mut wildcard_positions = Vec::new();
    let mut typed_params = Vec::new();
    for (i, &tok) in tokens.iter().enumerate() {
        if let Some(tag) = classify(tok) {
            out_tokens.push(tag.as_str());
            wildcard_positions.push(i);
            typed_params.push(TypedParam {
                type_tag: tag.into(),
                value: tok,
            });
        } else {
            out_tokens.push(tok);
        }
    }
    Masked {
        tokens: out_tokens,
        wildcard_positions,
        typed_params,
    }
}

/// The subset of [`ParamType`] variants that `mask()` can emit
/// with a tag string.
///
/// Splitting this from `ParamType` lets [`MaskTag::as_str`] and
/// [`From<MaskTag> for ParamType`] both be exhaustive total
/// functions: the compiler refuses to add a new `MaskTag`
/// variant without a tag and a `ParamType` mapping. Variants
/// reserved by RFC 0001 §6.1 but not emitted by `mask()`
/// (`Hex`, `Ts`, `Path`, `Str`, `Overflow`) live only on
/// `ParamType`, where they belong — `Str` is added by widening
/// (§6.2 step 5b), `Overflow` by the byte-limit check (§6.5),
/// `Hex` / `Ts` / `Path` by future masking-rule PRs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MaskTag {
    Uuid,
    Ip,
    Num,
}

impl MaskTag {
    /// The tag string this variant emits in the masked-token
    /// sequence (`<UUID>`, `<IP>`, `<NUM>`). Total — every
    /// `MaskTag` has a tag by construction.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Uuid => "<UUID>",
            Self::Ip => "<IP>",
            Self::Num => "<NUM>",
        }
    }
}

impl From<MaskTag> for ParamType {
    fn from(t: MaskTag) -> Self {
        match t {
            MaskTag::Uuid => Self::Uuid,
            MaskTag::Ip => Self::Ip,
            MaskTag::Num => Self::Num,
        }
    }
}

/// The tag string `mask()` emits for `t`, or `None` when `t` is
/// not a mask-emitted type. The partial inverse of
/// `From<MaskTag> for ParamType`: only `Uuid` / `Ip` / `Num` have
/// a tag; every other `ParamType` arises from widening, overflow,
/// or future masking rules and never appears as a masked token.
/// Snapshot restore uses this to reconstruct the descend-path tag
/// string a path-position wildcard was created with.
pub(crate) fn tag_str_for(t: ParamType) -> Option<&'static str> {
    match t {
        ParamType::Uuid => Some(MaskTag::Uuid.as_str()),
        ParamType::Ip => Some(MaskTag::Ip.as_str()),
        ParamType::Num => Some(MaskTag::Num.as_str()),
        _ => None,
    }
}

fn classify(token: &str) -> Option<MaskTag> {
    if is_uuid(token) {
        Some(MaskTag::Uuid)
    } else if is_ipv4(token) {
        Some(MaskTag::Ip)
    } else if is_num(token) {
        Some(MaskTag::Num)
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
        // wildcard_positions records the *input* indices where
        // mask emitted a tag; parallel to typed_params (§6.2
        // alignment used by the cluster's type-expansion logic).
        assert_eq!(r.wildcard_positions, vec![1, 5]);
    }

    #[test]
    fn mask_wildcard_positions_are_ascending_and_parallel_to_typed_params() {
        // Pin the §6.2 alignment contract: `wildcard_positions[k]`
        // is the input-token index whose original value is
        // `typed_params[k].value`. Consumers (cluster type-expansion)
        // rely on this parallel ordering.
        let tokens = ["alpha", "10.0.0.1", "beta", "42", "gamma", "-7"];

        let r = mask(&tokens);

        assert_eq!(r.wildcard_positions, vec![1, 3, 5]);
        assert_eq!(r.typed_params.len(), 3);
        for (k, p) in r.wildcard_positions.iter().copied().enumerate() {
            assert_eq!(
                r.typed_params[k].value, tokens[p],
                "typed_params[{k}].value must equal the input token at wildcard_positions[{k}] = {p}",
            );
        }
    }

    #[test]
    fn mask_does_not_classify_literal_mask_tag_token() {
        // §6.2 ambiguity: an input log line containing the literal
        // string "<NUM>" is **not** classified by the mask rules
        // (it's not all digits, not a UUID, not an IPv4). The
        // surface effect is that `wildcard_positions` does not
        // include that position and `typed_params` carries no
        // entry for it — consumers that key off `Masked`'s typed
        // metadata see "no classification here". This is the
        // contract the cluster's `param_type_for_line_position`
        // helper relies on to avoid the literal-vs-tag collision
        // the audit-stream classifier would otherwise produce.
        let tokens = ["value", "<NUM>", "<IP>", "<UUID>", "ok"];

        let r = mask(&tokens);

        assert_eq!(r.tokens, tokens, "literal tags pass through unchanged");
        assert!(
            r.wildcard_positions.is_empty(),
            "literals matching tag strings must not be classified: {:?}",
            r.wildcard_positions,
        );
        assert!(r.typed_params.is_empty());
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

    // Regression for the `MaskTag` split: pin the canonical tag
    // strings so a future rename of the &'static str literals in
    // `MaskTag::as_str` would fail loudly here rather than silently
    // change the on-the-wire masked-token sequence the tree walk
    // routes against.
    #[test]
    fn mask_tag_as_str_pins_canonical_strings() {
        // Arrange — the three currently-emitted variants

        // Act + Assert
        assert_eq!(MaskTag::Uuid.as_str(), "<UUID>");
        assert_eq!(MaskTag::Ip.as_str(), "<IP>");
        assert_eq!(MaskTag::Num.as_str(), "<NUM>");
    }

    // Regression for the `MaskTag` split: pin the From mapping so
    // a refactor that swapped two arms would still type-check but
    // would silently mis-attribute typed-param data on the wire.
    #[test]
    fn param_type_from_mask_tag_pins_mapping() {
        // Arrange — the three currently-emitted variants

        // Act + Assert
        assert_eq!(ParamType::from(MaskTag::Uuid), ParamType::Uuid);
        assert_eq!(ParamType::from(MaskTag::Ip), ParamType::Ip);
        assert_eq!(ParamType::from(MaskTag::Num), ParamType::Num);
    }

    // Pin `tag_str_for` as the partial inverse of the two mappings
    // above: exactly the mask-emitted types get a tag string,
    // matching what `classify` emits; every other `ParamType` gets
    // `None` (so snapshot restore rejects a path-position wildcard
    // that could not have come from mask emission).
    #[test]
    fn tag_str_for_maps_only_mask_emitted_types() {
        assert_eq!(tag_str_for(ParamType::Uuid), Some("<UUID>"));
        assert_eq!(tag_str_for(ParamType::Ip), Some("<IP>"));
        assert_eq!(tag_str_for(ParamType::Num), Some("<NUM>"));
        for t in [
            ParamType::Hex,
            ParamType::Ts,
            ParamType::Path,
            ParamType::Str,
            ParamType::Overflow,
            ParamType::Unknown(99),
        ] {
            assert_eq!(tag_str_for(t), None, "{t:?} is not mask-emitted");
        }
    }
}
