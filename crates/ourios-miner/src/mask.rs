//! Token-level masking for the Drain-derived miner.
//!
//! Per RFC 0001 §4.2 / §6.2 step 2: tokens matching a configured
//! shape rule are replaced with their type tag (e.g. `<NUM>`) and
//! the original bytes are emitted as a typed parameter. The tree
//! walk routes against the type tag; reconstruction (§6.6) walks
//! the parallel `typed_params` to recover the original line bytes.
//!
//! Default rule set: UUID, IPv4, TS, NUM (integer or decimal), each
//! matching a whole token. The other RFC §6.1 variants (`Hex`,
//! `Path`, `Str`, `Overflow`) are reserved on [`ParamType`] because
//! the Parquet record schema (§6.1) is the data contract, but they
//! have no `mask()` emitter — `Str` comes from widening (§6.2 step
//! 5b), `Overflow` from the byte limit (§6.5), and `Hex` / `Path`
//! are unassigned. Internally the masking-emit subset is the
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
/// Rules are tried in order: UUID, IPv4, TS, NUM (most specific
/// first, so a 36-char UUID is not misclassified as `NUM`, and a
/// dotted quad like `1.2.3.4` is not split into `NUM` tokens by the
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
/// (`Hex`, `Path`, `Str`, `Overflow`) live only on `ParamType`,
/// where they belong — `Str` is added by widening (§6.2 step 5b),
/// `Overflow` by the byte-limit check (§6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MaskTag {
    Uuid,
    Ip,
    Ts,
    Num,
}

impl MaskTag {
    /// The tag string this variant emits in the masked-token
    /// sequence (`<UUID>`, `<IP>`, `<TS>`, `<NUM>`). Total — every
    /// `MaskTag` has a tag by construction.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Uuid => "<UUID>",
            Self::Ip => "<IP>",
            Self::Ts => "<TS>",
            Self::Num => "<NUM>",
        }
    }
}

impl From<MaskTag> for ParamType {
    fn from(t: MaskTag) -> Self {
        match t {
            MaskTag::Uuid => Self::Uuid,
            MaskTag::Ip => Self::Ip,
            MaskTag::Ts => Self::Ts,
            MaskTag::Num => Self::Num,
        }
    }
}

/// The tag string `mask()` emits for `t`, or `None` when `t` is
/// not a mask-emitted type. The partial inverse of
/// `From<MaskTag> for ParamType`: only `Uuid` / `Ip` / `Ts` / `Num` have
/// a tag; every other `ParamType` arises from widening, overflow,
/// or future masking rules and never appears as a masked token.
/// Snapshot restore uses this to reconstruct the descend-path tag
/// string a path-position wildcard was created with.
pub(crate) fn tag_str_for(t: ParamType) -> Option<&'static str> {
    match t {
        ParamType::Uuid => Some(MaskTag::Uuid.as_str()),
        ParamType::Ip => Some(MaskTag::Ip.as_str()),
        ParamType::Ts => Some(MaskTag::Ts.as_str()),
        ParamType::Num => Some(MaskTag::Num.as_str()),
        _ => None,
    }
}

fn classify(token: &str) -> Option<MaskTag> {
    if is_uuid(token) {
        Some(MaskTag::Uuid)
    } else if is_ipv4(token) {
        Some(MaskTag::Ip)
    } else if is_ts(token.as_bytes()) {
        Some(MaskTag::Ts)
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

/// An integer (`-? DIGITS`) or a decimal (`-? DIGITS "." DIGITS`).
fn is_num(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    match body.split_once('.') {
        Some((int, frac)) => is_digits(int) && is_digits(frac),
        None => is_digits(body),
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// RFC 0001 §6.2 step 2 `TS`: a whole-token `DATE`, `TIME` or
/// `DATE "T" TIME`, at fixed field widths with range-checked fields.
/// A shape, not a calendar: `2026-02-31` is a `DATE`.
fn is_ts(b: &[u8]) -> bool {
    is_date(b) || is_time(b) || is_datetime(b)
}

fn is_datetime(b: &[u8]) -> bool {
    match (b.get(..10), b.get(10), b.get(11..)) {
        (Some(date), Some(b'T'), Some(time)) => is_date(date) && is_time(time),
        _ => false,
    }
}

/// `YYYY s MM s DD` with the same `s` (`-` or `/`) in both places.
fn is_date(b: &[u8]) -> bool {
    b.len() == 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && matches!(b[4], b'-' | b'/')
        && b[7] == b[4]
        && two_digits(b, 5).is_some_and(|m| (1..=12).contains(&m))
        && two_digits(b, 8).is_some_and(|d| (1..=31).contains(&d))
}

/// `hh:mm:ss [FRAC] [ZONE]`; `ss` allows 60 for a leap second.
fn is_time(b: &[u8]) -> bool {
    let clock = b.len() >= 8
        && two_digits(b, 0).is_some_and(|h| h <= 23)
        && b[2] == b':'
        && two_digits(b, 3).is_some_and(|m| m <= 59)
        && b[5] == b':'
        && two_digits(b, 6).is_some_and(|s| s <= 60);
    clock && strip_fraction(&b[8..]).is_some_and(is_zone)
}

/// Strips an optional `.` / `,` fraction of 1–9 digits; `None` when
/// the separator is present but the digit run is empty or too long.
fn strip_fraction(b: &[u8]) -> Option<&[u8]> {
    match b.first() {
        Some(b'.' | b',') => {
            let digits = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
            (1..=9).contains(&digits).then(|| &b[1 + digits..])
        }
        _ => Some(b),
    }
}

/// Empty, `Z`, `±hh`, `±hhmm` or `±hh:mm`.
fn is_zone(b: &[u8]) -> bool {
    match b {
        [] | [b'Z'] => true,
        [b'+' | b'-', rest @ ..] => {
            let hours = two_digits(rest, 0).is_some_and(|h| h <= 23);
            let minutes = match &rest[2.min(rest.len())..] {
                [] => true,
                [b':', mm @ ..] | mm => mm.len() == 2 && two_digits(mm, 0).is_some_and(|m| m <= 59),
            };
            hours && minutes
        }
        _ => false,
    }
}

/// The two ASCII digits at `at..at + 2` as a number, or `None`.
fn two_digits(b: &[u8], at: usize) -> Option<u8> {
    match b.get(at..at + 2)? {
        [hi @ b'0'..=b'9', lo @ b'0'..=b'9'] => Some((hi - b'0') * 10 + (lo - b'0')),
        _ => None,
    }
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

    const EMITTED: [(MaskTag, &str, ParamType); 4] = [
        (MaskTag::Uuid, "<UUID>", ParamType::Uuid),
        (MaskTag::Ip, "<IP>", ParamType::Ip),
        (MaskTag::Ts, "<TS>", ParamType::Ts),
        (MaskTag::Num, "<NUM>", ParamType::Num),
    ];

    // Regression for the `MaskTag` split: pin the canonical tag
    // strings so a future rename of the &'static str literals in
    // `MaskTag::as_str` would fail loudly here rather than silently
    // change the on-the-wire masked-token sequence the tree walk
    // routes against.
    #[test]
    fn mask_tag_as_str_pins_canonical_strings() {
        for (tag, s, _) in EMITTED {
            assert_eq!(tag.as_str(), s, "{tag:?}");
        }
    }

    // Regression for the `MaskTag` split: pin the From mapping so
    // a refactor that swapped two arms would still type-check but
    // would silently mis-attribute typed-param data on the wire.
    #[test]
    fn param_type_from_mask_tag_pins_mapping() {
        for (tag, _, t) in EMITTED {
            assert_eq!(ParamType::from(tag), t, "{tag:?}");
        }
    }

    // Pin `tag_str_for` as the partial inverse of the two mappings
    // above: exactly the mask-emitted types get a tag string,
    // matching what `classify` emits; every other `ParamType` gets
    // `None` (so snapshot restore rejects a path-position wildcard
    // that could not have come from mask emission).
    #[test]
    fn tag_str_for_maps_only_mask_emitted_types() {
        for (_, s, t) in EMITTED {
            assert_eq!(tag_str_for(t), Some(s), "{t:?}");
        }
        for t in [
            ParamType::Hex,
            ParamType::Path,
            ParamType::Str,
            ParamType::Overflow,
            ParamType::Unknown(99),
        ] {
            assert_eq!(tag_str_for(t), None, "{t:?} is not mask-emitted");
        }
    }

    fn classified(token: &str) -> Option<ParamType> {
        mask(&[token]).typed_params.first().map(|p| p.type_tag)
    }

    /// RFC0001.17 — whole-token dates, times of day and date-times.
    #[test]
    fn mask_classifies_whole_token_timestamps_as_ts() {
        let tokens = ["2026/10/17", "16:55:02", "traces", "export:"];

        let r = mask(&tokens);

        assert_eq!(
            r,
            Masked {
                tokens: vec!["<TS>", "<TS>", "traces", "export:"],
                wildcard_positions: vec![0, 1],
                typed_params: vec![
                    TypedParam {
                        type_tag: ParamType::Ts,
                        value: "2026/10/17"
                    },
                    TypedParam {
                        type_tag: ParamType::Ts,
                        value: "16:55:02"
                    },
                ],
            }
        );
        for ts in [
            "2026-10-17",
            "2026-02-31",
            "00:00:00",
            "23:59:60",
            "16:55:02.674004",
            "16:55:02,674",
            "16:55:02.123456789",
            "16:55:02Z",
            "2026-10-18T13:28:29.674004Z",
            "2026-10-18T13:28:29+02",
            "2026-10-18T13:28:29+0200",
            "2026-10-18T13:28:29.674004+02:00",
            "2026/10/18T13:28:29-05:30",
        ] {
            assert_eq!(classified(ts), Some(ParamType::Ts), "{ts}");
        }
    }

    /// RFC0001.18 — near-timestamp shapes are not `TS`.
    #[test]
    fn mask_rejects_near_timestamp_shapes() {
        for not_ts in [
            "2026-13-01",
            "2026-10-32",
            "2026-00-10",
            "2026-10-00",
            "2026-10/17",
            "26-10-17",
            "24:00:00",
            "9:05:00",
            "12:60:00",
            "12:00:61",
            "2026-10-17T25:00:00Z",
            "16:55:02.",
            "16:55:02,",
            "16:55:02.1234567890",
            "16:55:02+2",
            "16:55:02+24:00",
            "16:55:02+05:60",
            "16:55:02+05:3",
            "16:55:02+053",
            "16:55:02z",
            "16:55:02 ",
            "2026-10-17t13:28:29",
            "2026-10-17 ",
        ] {
            assert_ne!(classified(not_ts), Some(ParamType::Ts), "{not_ts}");
        }
    }

    /// RFC0001.19 — whole-token decimals mask to `<NUM>`; other dotted
    /// shapes do not, and IPv4 still wins over `NUM`.
    #[test]
    fn mask_classifies_decimals_as_num() {
        let tokens = ["backing", "off", "for", "0.3786463", "seconds"];

        let r = mask(&tokens);

        assert_eq!(
            r,
            Masked {
                tokens: vec!["backing", "off", "for", "<NUM>", "seconds"],
                wildcard_positions: vec![3],
                typed_params: vec![TypedParam {
                    type_tag: ParamType::Num,
                    value: "0.3786463"
                }],
            }
        );
        for (token, expected) in [
            ("0.1", Some(ParamType::Num)),
            ("-1.5", Some(ParamType::Num)),
            ("10.0.0.1", Some(ParamType::Ip)),
            ("1.2.3", None),
            (".5", None),
            ("5.", None),
            ("-.5", None),
            ("1e5", None),
            ("0x1F", None),
            ("6.3ms", None),
            ("1,5", None),
            ("-", None),
            (".", None),
        ] {
            assert_eq!(classified(token), expected, "{token}");
        }
    }
}
