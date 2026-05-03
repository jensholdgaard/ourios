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

/// Split `line` on Unicode whitespace.
///
/// Whitespace is every codepoint matching [`char::is_whitespace`]:
/// ASCII space / tab / CR / LF, plus the broader Unicode
/// whitespace classes (U+0085, U+00A0, U+1680, U+2000–U+200A,
/// U+2028, U+2029, U+202F, U+205F, U+3000). Every other byte —
/// including punctuation such as `=`, `:`, `,`, `;`, `[`, `]`,
/// `(`, `)` — stays inside the token; structured separators are
/// the masking layer's responsibility (RFC 0001 §4.2).
#[must_use]
pub fn tokenize(line: &str) -> Tokenized<'_> {
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
        separators.push("");
    } else {
        separators.push(&line[sep_start..end]);
    }

    Tokenized { tokens, separators }
}
