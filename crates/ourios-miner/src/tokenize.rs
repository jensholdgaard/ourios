//! Tokenization for the Drain-derived miner.
//!
//! Per RFC 0001 §6.2 step 1: split on Unicode whitespace only;
//! every other byte (including punctuation) stays inside a token.
//! The captured separators array (length = `tokens.len() + 1`)
//! supports byte-identical reconstruction per §6.6.

/// The result of tokenizing a single log line.
///
/// Borrows from the input — no allocation per token / separator.
/// `separators.len() == tokens.len() + 1` always: `separators[0]`
/// is the leading whitespace before the first token,
/// `separators[i + 1]` is the whitespace between token `i` and
/// token `i + 1` (or the trailing whitespace after the last
/// token). For an input with no tokens, `separators` carries one
/// entry containing the entire input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokenized<'a> {
    pub tokens: Vec<&'a str>,
    pub separators: Vec<&'a str>,
}

/// Why `tokenize` rejected an input. Per RFC 0001 §6.2 step 1 a
/// tokenizer failure routes the line to the parse-failure path
/// with `lossy_flag = true` and the body retained verbatim
/// (hazard H7.2), so any future failure mode added here must
/// flow through the same emit path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenizeError {
    /// The input contained an embedded NUL byte (U+0000). NUL is
    /// the canonical "this is a binary blob, not text" signal in
    /// log-shipping pipelines (truncated frames, mis-decoded
    /// protobuf, certificate noise); admitting it would let the
    /// miner build templates over non-text payloads. Carries the
    /// byte offset of the first NUL so a reader inspecting the
    /// retained body has a pointer to the suspicious region.
    EmbeddedNul { offset: usize },
}

/// Split `line` on Unicode whitespace.
///
/// Whitespace is every codepoint matching [`char::is_whitespace`]:
/// the full ASCII whitespace set (space U+0020, tab U+0009, LF
/// U+000A, VT U+000B, FF U+000C, CR U+000D), plus the broader
/// Unicode whitespace classes (U+0085, U+00A0, U+1680,
/// U+2000–U+200A, U+2028, U+2029, U+202F, U+205F, U+3000). Every
/// other byte — including punctuation such as `=`, `:`, `,`, `;`,
/// `[`, `]`, `(`, `)` — stays inside the token; structured
/// separators are the masking layer's responsibility (RFC 0001
/// §4.2).
///
/// # Errors
///
/// Returns [`TokenizeError::EmbeddedNul`] if `line` contains a NUL
/// byte.
///
/// Other tokenizer-failure modes named in RFC 0001 §6.2 step 1:
///
/// - **Malformed UTF-8** is structurally impossible at this entry
///   — the `&str` invariant guarantees valid UTF-8, so this
///   function never sees it.
/// - **Line longer than `max_line_bytes`** is **not** caught
///   here. The miner does not yet expose a `max_line_bytes`
///   config; `ingest_string` instead enforces an upstream
///   post-tokenization cap on the *token count* (≤ `u16::MAX`)
///   to keep widening-position audit payloads in range. A line
///   of arbitrary byte length is admitted into this function so
///   long as its UTF-8 is valid and it carries no NUL; a
///   `max_line_bytes` byte cap will land as a configurable
///   pre-tokenize guard in a future PR. Until then a single
///   pathological long line is bounded only by the `u16::MAX`
///   token-count cap downstream.
pub fn tokenize(line: &str) -> Result<Tokenized<'_>, TokenizeError> {
    if let Some(offset) = line.bytes().position(|b| b == 0) {
        return Err(TokenizeError::EmbeddedNul { offset });
    }

    let mut tokens = Vec::new();
    let mut separators = Vec::new();

    let mut sep_start = 0;
    let mut tok_start: Option<usize> = None;

    for (i, c) in line.char_indices() {
        match (tok_start, c.is_whitespace()) {
            (None, false) => {
                separators.push(&line[sep_start..i]);
                tok_start = Some(i);
            }
            (Some(start), true) => {
                tokens.push(&line[start..i]);
                tok_start = None;
                sep_start = i;
            }
            _ => {}
        }
    }

    let end = line.len();
    if let Some(start) = tok_start {
        tokens.push(&line[start..end]);
        separators.push(&line[end..end]);
    } else {
        separators.push(&line[sep_start..end]);
    }

    Ok(Tokenized { tokens, separators })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_rejects_embedded_nul_with_byte_offset() {
        // RFC §6.2 step 1: embedded NUL is a tokenizer failure.
        // The returned offset must point at the first NUL so a
        // reader inspecting the retained `body` can locate the
        // suspicious region.
        let line = "user 42\0secret";
        let err = tokenize(line).unwrap_err();
        assert_eq!(err, TokenizeError::EmbeddedNul { offset: 7 });
    }

    #[test]
    fn tokenize_rejects_leading_nul() {
        let line = "\0";
        let err = tokenize(line).unwrap_err();
        assert_eq!(err, TokenizeError::EmbeddedNul { offset: 0 });
    }

    #[test]
    fn tokenize_accepts_nul_free_input() {
        // Round-trip an ordinary line to confirm the validation
        // step is a guard, not a behavior change for clean inputs.
        let r = tokenize("hello world").expect("nul-free");
        assert_eq!(r.tokens, vec!["hello", "world"]);
    }
}
