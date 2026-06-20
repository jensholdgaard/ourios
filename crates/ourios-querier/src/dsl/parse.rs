//! The string-DSL front-end (RFC 0002 surface β): a hand-rolled tokenizer +
//! recursive-descent parser implementing the §7 grammar exactly, producing
//! the shared [`Query`] IR. Errors are the Ourios-owned [`DslError`] and cite
//! the offending token/clause — never a `datafusion`/`arrow`/SQL term
//! (hazard `CLAUDE.md` §4.6).

use super::DslError;
use super::ir::{
    AggFn, Call, CmpOp, DriftQuery, Field, OrdOp, Predicate, Query, SeverityName, SeverityValue,
    Stage, Statement, Time, Value,
};

/// Parse a β-surface query string into a [`Statement`] — a log query or a
/// RFC 0010 `drift` query, dispatched on the leading token (`drift` is the
/// only verb head; anything else parses as the RFC 0002 `predicate { | stage }`
/// pipeline).
///
/// # Errors
///
/// Returns [`DslError`] for any lexical or grammatical violation of the §7
/// grammar or the RFC 0010 §6.1 `drift` head, citing the offending
/// token/clause.
pub fn parse_statement(input: &str) -> Result<Statement, DslError> {
    let tokens = tokenize(input)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
    };
    // `drift` is a reserved verb head (RFC 0010 §6.1): a query that opens with
    // it is a drift query, not a `drift == …` log predicate.
    let statement = if matches!(parser.peek(), Some(Tok::Ident(s)) if s == "drift") {
        Statement::Drift(parser.parse_drift_query()?)
    } else {
        Statement::Logs(parser.parse_query()?)
    };
    parser.expect_eof()?;
    Ok(statement)
}

/// Parse a β-surface query string into the shared [`Query`] IR (the RFC 0002
/// log-query pipeline).
///
/// # Errors
///
/// Returns [`DslError`] for any lexical or grammatical violation of §7,
/// citing the offending token/clause. A RFC 0010 `drift` head is rejected
/// here — use [`parse_statement`] to accept it.
pub fn parse(input: &str) -> Result<Query, DslError> {
    match parse_statement(input)? {
        Statement::Logs(query) => Ok(query),
        Statement::Drift(_) => Err(DslError::new(
            concat!(
                "`drift` is an audit-stream query, not a log query; ",
                "it has no place in a log-record pipeline"
            )
            .to_string(),
        )),
    }
}

// ---- tokenizer ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare identifier / keyword (letters, digits, `_`, starting non-digit).
    Ident(String),
    /// A double-quoted string literal (already unescaped).
    Str(String),
    /// A number literal kept as its lexeme (int or float).
    Number(String),
    /// A duration lexeme such as `30s` (digits + unit suffix).
    Duration(String),
    /// An RFC 3339 timestamp lexeme.
    Timestamp(String),
    Pipe,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Minus,
    Bang,
    BangTilde,
    Tilde,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Match,
    AndAnd,
    OrOr,
}

#[allow(clippy::too_many_lines)]
fn tokenize(input: &str) -> Result<Vec<Tok>, DslError> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' => i += 1,
            // A query is a single-line YAML-safe scalar (§4 P7 / RFC0002.10):
            // a literal newline or other control char outside a string is not
            // whitespace, it is a malformed query.
            b'\n' | b'\r' => {
                return Err(DslError::new(format!(
                    "literal newline at byte {i}; a query must be a single line"
                )));
            }
            c if c < 0x20 => {
                return Err(DslError::new(format!(
                    "literal control character at byte {i}; a query must be a single line"
                )));
            }
            b'|' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    out.push(Tok::OrOr);
                    i += 2;
                } else {
                    out.push(Tok::Pipe);
                    i += 1;
                }
            }
            b'&' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    out.push(Tok::AndAnd);
                    i += 2;
                } else {
                    return Err(DslError::new(format!(
                        "unexpected '&' at byte {i}; did you mean '&&'?"
                    )));
                }
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            b'=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Tok::EqEq);
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'~' {
                    out.push(Tok::Match);
                    i += 2;
                } else {
                    return Err(DslError::new(format!(
                        "lone '=' at byte {i}; comparisons use '==' (or '=~')"
                    )));
                }
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Tok::NotEq);
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'~' {
                    out.push(Tok::BangTilde);
                    i += 2;
                } else {
                    out.push(Tok::Bang);
                    i += 1;
                }
            }
            b'~' => {
                out.push(Tok::Tilde);
                i += 1;
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Tok::Le);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'"' => {
                let (s, next) = lex_string(input, i)?;
                out.push(Tok::Str(s));
                i = next;
            }
            c if c.is_ascii_digit() => {
                let (tok, next) = lex_numeric(input, i)?;
                out.push(tok);
                i = next;
            }
            c if c.is_ascii_alphabetic() => {
                let (ident, next) = lex_ident(input, i);
                out.push(Tok::Ident(ident));
                i = next;
            }
            other => {
                return Err(DslError::new(format!(
                    "unexpected character {:?} at byte {i}",
                    other as char
                )));
            }
        }
    }
    Ok(out)
}

/// Lex a double-quoted string starting at `start` (the opening quote).
/// Handles the §7 escapes (`\" \\ \n \t \r \uXXXX`) and rejects literal
/// newlines / control characters (queries are single-line, §4 P7).
fn lex_string(input: &str, start: usize) -> Result<(String, usize), DslError> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut s = String::new();
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'"' => return Ok((s, i + 1)),
            b'\\' => {
                i += 1;
                let e = *bytes.get(i).ok_or_else(|| {
                    DslError::new("unterminated escape at end of string".to_string())
                })?;
                match e {
                    b'"' => s.push('"'),
                    b'\\' => s.push('\\'),
                    b'n' => s.push('\n'),
                    b't' => s.push('\t'),
                    b'r' => s.push('\r'),
                    b'u' => {
                        let hex = input.get(i + 1..i + 5).ok_or_else(|| {
                            DslError::new(
                                "truncated '\\u' escape; expected 4 hex digits".to_string(),
                            )
                        })?;
                        let code = u32::from_str_radix(hex, 16).map_err(|_| {
                            DslError::new(format!(
                                "invalid '\\u' escape: {hex:?} is not 4 hex digits"
                            ))
                        })?;
                        let ch = char::from_u32(code).ok_or_else(|| {
                            DslError::new(format!("'\\u{hex}' is not a Unicode scalar value"))
                        })?;
                        s.push(ch);
                        i += 4;
                    }
                    _ => {
                        return Err(DslError::new(format!(
                            "invalid escape '\\{}' in string",
                            e as char
                        )));
                    }
                }
                i += 1;
            }
            // A literal newline / control char is not a valid string char:
            // queries are single-line (§4 P7 / RFC0002.10).
            b'\n' | b'\r' => {
                return Err(DslError::new(
                    "literal newline in a string; write it as an escape (\\n)".to_string(),
                ));
            }
            c if c < 0x20 => {
                return Err(DslError::new(
                    "literal control character in string; write it as an escape (e.g. \\n)"
                        .to_string(),
                ));
            }
            _ => {
                // Copy the full UTF-8 scalar, not just one byte.
                let ch_len = utf8_len(c);
                let chunk = input
                    .get(i..i + ch_len)
                    .ok_or_else(|| DslError::new("invalid UTF-8 in string literal".to_string()))?;
                s.push_str(chunk);
                i += ch_len;
            }
        }
    }
    Err(DslError::new(
        "unterminated string literal (missing closing '\"')".to_string(),
    ))
}

fn utf8_len(first: u8) -> usize {
    match first {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// Lex a numeric run starting at a digit: a number (`500`, `0.7`), a duration
/// (`30s`), or an RFC 3339 timestamp (`2026-01-02T03:04:05Z`).
fn lex_numeric(input: &str, start: usize) -> Result<(Tok, usize), DslError> {
    let bytes = input.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // Timestamp: a 4-digit run followed by '-' begins an RFC 3339 date.
    if i - start == 4 && i < bytes.len() && bytes[i] == b'-' {
        return lex_timestamp(input, start);
    }
    // Float: digits '.' digits.
    if i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        return Ok((Tok::Number(input[start..i].to_string()), i));
    }
    // Duration: integer + a single unit suffix.
    if i < bytes.len() && matches!(bytes[i], b's' | b'm' | b'h' | b'd' | b'w') {
        // A unit must not be glued to a longer identifier (e.g. `1hour`).
        let after = i + 1;
        if after < bytes.len() && (bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_') {
            return Err(DslError::new(format!(
                "malformed duration near byte {start}; expected <int>(s|m|h|d|w)"
            )));
        }
        return Ok((Tok::Duration(input[start..after].to_string()), after));
    }
    Ok((Tok::Number(input[start..i].to_string()), i))
}

/// Lex an RFC 3339 timestamp lexeme starting at `start`. We accept the
/// characters RFC 3339 uses and validate the structure minimally; the
/// compiler does full validation later.
fn lex_timestamp(input: &str, start: usize) -> Result<(Tok, usize), DslError> {
    let bytes = input.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() || matches!(c, b'-' | b':' | b'.' | b'+' | b'T' | b't' | b'Z' | b'z')
        {
            i += 1;
        } else {
            break;
        }
    }
    let lexeme = &input[start..i];
    validate_rfc3339(lexeme)?;
    Ok((Tok::Timestamp(lexeme.to_string()), i))
}

/// Minimal RFC 3339 shape check: `YYYY-MM-DDThh:mm:ss` with an optional
/// fractional part and a `Z`/`±hh:mm` offset. Range-checks the obvious
/// fields so a malformed timestamp is caught at parse time, not later.
fn validate_rfc3339(s: &str) -> Result<(), DslError> {
    let err = || DslError::new(format!("malformed RFC 3339 timestamp {s:?}"));
    let bytes = s.as_bytes();
    // YYYY-MM-DDT
    if bytes.len() < 20 {
        return Err(err());
    }
    let digit = |b: u8| b.is_ascii_digit();
    if !(digit(bytes[0]) && digit(bytes[1]) && digit(bytes[2]) && digit(bytes[3])) {
        return Err(err());
    }
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(err());
    }
    if !matches!(bytes[10], b'T' | b't') {
        return Err(err());
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return Err(err());
    }
    let two = |a: u8, b: u8| -> Option<u32> {
        if digit(a) && digit(b) {
            Some(u32::from(a - b'0') * 10 + u32::from(b - b'0'))
        } else {
            None
        }
    };
    let month = two(bytes[5], bytes[6]).ok_or_else(err)?;
    let day = two(bytes[8], bytes[9]).ok_or_else(err)?;
    let hour = two(bytes[11], bytes[12]).ok_or_else(err)?;
    let min = two(bytes[14], bytes[15]).ok_or_else(err)?;
    let sec = two(bytes[17], bytes[18]).ok_or_else(err)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return Err(err());
    }
    // Trailing offset, optionally preceded by a fractional second. The offset
    // must be a real `Z`/`±hh:mm` — a bare `+` or `+05` is malformed RFC 3339.
    let mut tail = &s[19..];
    if let Some(rest) = tail.strip_prefix('.') {
        let frac_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        if frac_len == 0 {
            return Err(err());
        }
        tail = &rest[frac_len..];
    }
    if !is_rfc3339_offset(tail) {
        return Err(err());
    }
    Ok(())
}

/// True for an RFC 3339 zone designator: `Z`/`z`, or a full `±hh:mm` offset
/// (`hh` ≤ 23, `mm` ≤ 59). Rejects a bare `+`, `+05`, or other partial offset.
fn is_rfc3339_offset(tail: &str) -> bool {
    if tail.eq_ignore_ascii_case("z") {
        return true;
    }
    let bytes = tail.as_bytes();
    if bytes.len() != 6 || !matches!(bytes[0], b'+' | b'-') || bytes[3] != b':' {
        return false;
    }
    let digit = |b: u8| b.is_ascii_digit();
    if !(digit(bytes[1]) && digit(bytes[2]) && digit(bytes[4]) && digit(bytes[5])) {
        return false;
    }
    let hh = u32::from(bytes[1] - b'0') * 10 + u32::from(bytes[2] - b'0');
    let mm = u32::from(bytes[4] - b'0') * 10 + u32::from(bytes[5] - b'0');
    hh <= 23 && mm <= 59
}

fn lex_ident(input: &str, start: usize) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut i = start;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    (input[start..i].to_string(), i)
}

// ---- parser -------------------------------------------------------------

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<&'a Tok> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume the next token expecting it to equal `want`, else error.
    fn expect(&mut self, want: &Tok, what: &str) -> Result<(), DslError> {
        if self.eat(want) {
            Ok(())
        } else {
            Err(DslError::new(format!(
                "expected {what}, found {}",
                describe(self.peek())
            )))
        }
    }

    fn expect_eof(&self) -> Result<(), DslError> {
        match self.peek() {
            None => Ok(()),
            other => Err(DslError::new(format!(
                "unexpected trailing input: {}",
                describe(other)
            ))),
        }
    }

    /// `query = predicate , { "|" , stage }`.
    fn parse_query(&mut self) -> Result<Query, DslError> {
        let predicate = self.parse_predicate()?;
        let mut stages = Vec::new();
        while self.eat(&Tok::Pipe) {
            stages.push(self.parse_stage()?);
        }
        Ok(Query { predicate, stages })
    }

    /// `drift_query = "drift" , "from" , time , "to" , time` (RFC 0010 §6.1).
    /// A closed verb head: it admits no `|` stages (the projection, grouping,
    /// and ordering are fixed by §6.3), so a trailing pipe is rejected by the
    /// caller's `expect_eof`.
    fn parse_drift_query(&mut self) -> Result<DriftQuery, DslError> {
        self.pos += 1; // consume `drift`
        if !self.eat_keyword("from") {
            return Err(DslError::new(format!(
                "expected `from` after `drift`, found {}",
                describe(self.peek())
            )));
        }
        let from = self.parse_time()?;
        if !self.eat_keyword("to") {
            return Err(DslError::new(format!(
                "expected `to` after the drift window's `from` bound, found {}",
                describe(self.peek())
            )));
        }
        let to = self.parse_time()?;
        Ok(DriftQuery { from, to })
    }

    /// `predicate = or_expr`.
    fn parse_predicate(&mut self) -> Result<Predicate, DslError> {
        self.parse_or()
    }

    /// `or_expr = and_expr , { ("or" | "||") , and_expr }`.
    fn parse_or(&mut self) -> Result<Predicate, DslError> {
        let first = self.parse_and()?;
        let mut terms = vec![first];
        loop {
            if self.eat(&Tok::OrOr) || self.eat_keyword("or") {
                terms.push(self.parse_and()?);
            } else {
                break;
            }
        }
        Ok(Predicate::or(terms))
    }

    /// `and_expr = unary , { ("and" | "&&") , unary }`.
    fn parse_and(&mut self) -> Result<Predicate, DslError> {
        let first = self.parse_unary()?;
        let mut terms = vec![first];
        loop {
            if self.eat(&Tok::AndAnd) || self.eat_keyword("and") {
                terms.push(self.parse_unary()?);
            } else {
                break;
            }
        }
        Ok(Predicate::and(terms))
    }

    /// Consume the next token iff it is the identifier keyword `kw`.
    fn eat_keyword(&mut self, kw: &str) -> bool {
        if let Some(Tok::Ident(s)) = self.peek() {
            if s == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// `unary = [ "not" | "!" ] , ( comparison | call | bool_lit | "(" predicate ")" )`.
    fn parse_unary(&mut self) -> Result<Predicate, DslError> {
        if self.eat(&Tok::Bang) || self.eat_keyword("not") {
            let inner = self.parse_unary()?;
            return Ok(Predicate::Not(Box::new(inner)));
        }
        if self.eat(&Tok::LParen) {
            let inner = self.parse_predicate()?;
            self.expect(&Tok::RParen, "')' to close the group")?;
            return Ok(inner);
        }
        self.parse_atom()
    }

    /// A comparison, a call, or a bool literal — dispatched on the lead token.
    fn parse_atom(&mut self) -> Result<Predicate, DslError> {
        let ident = match self.peek() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected a predicate term (field, function, or true/false), found {}",
                    describe(other)
                )));
            }
        };
        match ident.as_str() {
            "true" => {
                self.pos += 1;
                Ok(Predicate::Bool(true))
            }
            "false" => {
                self.pos += 1;
                Ok(Predicate::Bool(false))
            }
            "severity" => self.parse_severity_cmp(),
            "matches" | "contains" | "starts_with" | "ends_with" | "resolves_to"
                if self.is_call() =>
            {
                Ok(Predicate::Call(self.parse_call()?))
            }
            _ => self.parse_scalar_cmp(),
        }
    }

    /// True if the current ident is immediately followed by `(` — a call.
    fn is_call(&self) -> bool {
        matches!(self.tokens.get(self.pos + 1), Some(Tok::LParen))
    }

    /// `severity_cmp = "severity" , ord_op , ( severity_name | number )`.
    fn parse_severity_cmp(&mut self) -> Result<Predicate, DslError> {
        self.pos += 1; // consume `severity`
        let op = self.parse_ord_op_strict()?;
        let value = match self.next() {
            Some(Tok::Ident(name)) => {
                let sev = parse_severity_name(name).ok_or_else(|| {
                    DslError::new(format!(
                        "{name:?} is not a severity name (trace|debug|info|warn|error|fatal)"
                    ))
                })?;
                SeverityValue::Name(sev)
            }
            Some(Tok::Number(n)) => {
                let v = n.parse::<i64>().map_err(|_| {
                    DslError::new(format!("severity number {n:?} is not an integer"))
                })?;
                SeverityValue::Number(v)
            }
            other => {
                return Err(DslError::new(format!(
                    "severity must compare against a severity name or number, found {}",
                    describe(other)
                )));
            }
        };
        Ok(Predicate::Severity { op, value })
    }

    /// Parse an ordering operator, rejecting the regex operators — severity is
    /// numeric and `=~`/`!~` are not defined on it (§7 `severity_cmp`).
    fn parse_ord_op_strict(&mut self) -> Result<OrdOp, DslError> {
        match self.next() {
            Some(Tok::EqEq) => Ok(OrdOp::Eq),
            Some(Tok::NotEq) => Ok(OrdOp::Ne),
            Some(Tok::Lt) => Ok(OrdOp::Lt),
            Some(Tok::Le) => Ok(OrdOp::Le),
            Some(Tok::Gt) => Ok(OrdOp::Gt),
            Some(Tok::Ge) => Ok(OrdOp::Ge),
            Some(Tok::Match | Tok::BangTilde) => Err(DslError::new(
                "severity is numeric (SeverityNumber); regex operators '=~'/'!~' are not \
                 allowed on it"
                    .to_string(),
            )),
            other => Err(DslError::new(format!(
                "expected an ordering operator (==, !=, <, <=, >, >=), found {}",
                describe(other)
            ))),
        }
    }

    /// `scalar_cmp = scalar_path , cmp_op , literal`.
    fn parse_scalar_cmp(&mut self) -> Result<Predicate, DslError> {
        let field = self.parse_scalar_path()?;
        let op = self.parse_cmp_op()?;
        let value = self.parse_literal()?;
        Ok(Predicate::Comparison { field, op, value })
    }

    /// `cmp_op = ord_op | "=~" | "!~"`.
    fn parse_cmp_op(&mut self) -> Result<CmpOp, DslError> {
        match self.next() {
            Some(Tok::EqEq) => Ok(CmpOp::Ord(OrdOp::Eq)),
            Some(Tok::NotEq) => Ok(CmpOp::Ord(OrdOp::Ne)),
            Some(Tok::Lt) => Ok(CmpOp::Ord(OrdOp::Lt)),
            Some(Tok::Le) => Ok(CmpOp::Ord(OrdOp::Le)),
            Some(Tok::Gt) => Ok(CmpOp::Ord(OrdOp::Gt)),
            Some(Tok::Ge) => Ok(CmpOp::Ord(OrdOp::Ge)),
            Some(Tok::Match) => Ok(CmpOp::Match),
            Some(Tok::BangTilde) => Ok(CmpOp::NotMatch),
            other => Err(DslError::new(format!(
                "expected a comparison operator (==, !=, <, <=, >, >=, =~, !~), found {}",
                describe(other)
            ))),
        }
    }

    /// `literal = string | number | boolean | "null" | duration | timestamp`.
    fn parse_literal(&mut self) -> Result<Value, DslError> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Value::Str(s.clone())),
            Some(Tok::Duration(d)) => Ok(Value::Duration(d.clone())),
            Some(Tok::Timestamp(t)) => Ok(Value::Timestamp(t.clone())),
            Some(Tok::Number(n)) => parse_number(n, false),
            Some(Tok::Minus) => match self.next() {
                Some(Tok::Number(n)) => parse_number(n, true),
                other => Err(DslError::new(format!(
                    "expected a number after '-', found {}",
                    describe(other)
                ))),
            },
            Some(Tok::Ident(id)) => match id.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                _ => Err(DslError::new(format!(
                    "expected a literal (string, number, boolean, null, duration, or \
                     timestamp); the bare identifier {id:?} is not a value — quote it as \
                     \"{id}\" if you meant a string"
                ))),
            },
            other => Err(DslError::new(format!(
                "expected a literal, found {}",
                describe(other)
            ))),
        }
    }

    /// `call = str_fn "(" path "," string ")" | "resolves_to" "(" number ")"`.
    fn parse_call(&mut self) -> Result<Call, DslError> {
        let name = match self.next() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected a function name, found {}",
                    describe(other)
                )));
            }
        };
        self.expect(&Tok::LParen, "'(' after the function name")?;
        if name == "resolves_to" {
            let n = match self.next() {
                Some(Tok::Number(n)) => n.clone(),
                other => {
                    return Err(DslError::new(format!(
                        "resolves_to(...) takes a single template-id number, found {}",
                        describe(other)
                    )));
                }
            };
            let id = n.parse::<u64>().map_err(|_| {
                DslError::new(format!(
                    "resolves_to(...) template id {n:?} is not a non-negative integer"
                ))
            })?;
            self.expect(&Tok::RParen, "')' to close resolves_to(...)")?;
            return Ok(Call::ResolvesTo(id));
        }
        let field = self.parse_path()?;
        require_string_operand(&name, &field)?;
        self.expect(
            &Tok::Comma,
            &format!("',' before {name}(...)'s string argument"),
        )?;
        let arg = match self.next() {
            Some(Tok::Str(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "{name}(...) requires a string second argument, found {}",
                    describe(other)
                )));
            }
        };
        self.expect(&Tok::RParen, &format!("')' to close {name}(...)"))?;
        Ok(match name.as_str() {
            "matches" => Call::Matches { field, arg },
            "contains" => Call::Contains { field, arg },
            "starts_with" => Call::StartsWith { field, arg },
            "ends_with" => Call::EndsWith { field, arg },
            other => {
                return Err(DslError::new(format!("unknown function {other:?}")));
            }
        })
    }

    /// `path = field | "resource" key_tail | "attr" key_tail` (severity allowed).
    fn parse_path(&mut self) -> Result<Field, DslError> {
        self.parse_field_like(true)
    }

    /// `scalar_path = nonsev_field | "resource" key_tail | "attr" key_tail`.
    fn parse_scalar_path(&mut self) -> Result<Field, DslError> {
        self.parse_field_like(false)
    }

    /// Shared field/path parsing. `allow_severity` distinguishes `path`
    /// (severity allowed, only inside calls/aggs) from `scalar_path`
    /// (severity reserved for `severity_cmp`).
    fn parse_field_like(&mut self, allow_severity: bool) -> Result<Field, DslError> {
        let name = match self.next() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected a field name, found {}",
                    describe(other)
                )));
            }
        };
        match name.as_str() {
            "resource" => Ok(Field::Resource(self.parse_key_tail()?)),
            "attr" => Ok(Field::Attr(self.parse_key_tail()?)),
            "severity" if allow_severity => Ok(Field::Severity),
            "severity" => Err(DslError::new(
                "severity may only be used in a severity comparison \
                 (e.g. `severity >= error`), not as a scalar path"
                    .to_string(),
            )),
            other => bare_field(other).ok_or_else(|| {
                DslError::new(format!(
                    "unknown field {other:?}; expected one of the §7 fields \
                     (body, ts, observed_ts, trace_id, span_id, scope, flags, service, \
                     template_id, confidence, lossy, severity) or resource./attr."
                ))
            }),
        }
    }

    /// `key_tail = ( "." dotted_key ) | ( "[" string "]" )`.
    fn parse_key_tail(&mut self) -> Result<String, DslError> {
        if self.eat(&Tok::LBracket) {
            let key = match self.next() {
                Some(Tok::Str(s)) => s.clone(),
                other => {
                    return Err(DslError::new(format!(
                        "expected a quoted attribute key inside [...], found {}",
                        describe(other)
                    )));
                }
            };
            self.expect(&Tok::RBracket, "']' to close the bracketed key")?;
            Ok(key)
        } else if self.eat(&Tok::Dot) {
            self.parse_dotted_key()
        } else {
            Err(DslError::new(format!(
                "expected an attribute key (`.key` or `[\"key\"]`), found {}",
                describe(self.peek())
            )))
        }
    }

    /// `dotted_key = ident , { "." , ident }`.
    fn parse_dotted_key(&mut self) -> Result<String, DslError> {
        let mut key = match self.next() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected an attribute key segment, found {}",
                    describe(other)
                )));
            }
        };
        while self.eat(&Tok::Dot) {
            match self.next() {
                Some(Tok::Ident(s)) => {
                    key.push('.');
                    key.push_str(s);
                }
                other => {
                    return Err(DslError::new(format!(
                        "expected an attribute key segment after '.', found {}",
                        describe(other)
                    )));
                }
            }
        }
        Ok(key)
    }

    // ---- stages ----

    fn parse_stage(&mut self) -> Result<Stage, DslError> {
        let name = match self.peek() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected a pipe stage (range, count, sum, min, max, avg, sort, \
                     limit, project, render), found {}",
                    describe(other)
                )));
            }
        };
        match name.as_str() {
            "range" => self.parse_range_stage(),
            "count" => self.parse_count_stage(),
            "sum" | "min" | "max" | "avg" => self.parse_agg_stage(),
            "sort" => self.parse_sort_stage(),
            "limit" => self.parse_limit_stage(),
            "project" => self.parse_project_stage(),
            "render" => {
                self.pos += 1;
                Ok(Stage::Render)
            }
            other => Err(DslError::new(format!(
                "unknown pipe stage {other:?}; expected range, count, sum, min, max, \
                 avg, sort, limit, project, or render"
            ))),
        }
    }

    fn parse_range_stage(&mut self) -> Result<Stage, DslError> {
        self.pos += 1; // `range`
        self.expect(&Tok::LParen, "'(' after range")?;
        let from = self.parse_time()?;
        self.expect(&Tok::Comma, "',' between the range bounds")?;
        let to = self.parse_time()?;
        self.expect(&Tok::RParen, "')' to close range(...)")?;
        Ok(Stage::Range(from, to))
    }

    /// `time = "now" | ( [ "-" ] , duration ) | timestamp`.
    fn parse_time(&mut self) -> Result<Time, DslError> {
        let neg = self.eat(&Tok::Minus);
        match self.next() {
            Some(Tok::Ident(s)) if s == "now" && !neg => Ok(Time::Now),
            Some(Tok::Duration(d)) => Ok(Time::Duration {
                neg,
                literal: d.clone(),
            }),
            Some(Tok::Timestamp(t)) if !neg => Ok(Time::Timestamp(t.clone())),
            other => Err(DslError::new(format!(
                "expected a time bound (now, a duration like -1h, or an RFC 3339 \
                 timestamp), found {}",
                describe(other)
            ))),
        }
    }

    fn parse_count_stage(&mut self) -> Result<Stage, DslError> {
        self.pos += 1; // `count`
        let by = self.parse_optional_by()?;
        Ok(Stage::Count { by })
    }

    fn parse_agg_stage(&mut self) -> Result<Stage, DslError> {
        let func = match self.next() {
            Some(Tok::Ident(s)) => match s.as_str() {
                "sum" => AggFn::Sum,
                "min" => AggFn::Min,
                "max" => AggFn::Max,
                "avg" => AggFn::Avg,
                other => {
                    return Err(DslError::new(format!("unknown aggregate {other:?}")));
                }
            },
            other => {
                return Err(DslError::new(format!(
                    "expected an aggregate function, found {}",
                    describe(other)
                )));
            }
        };
        self.expect(&Tok::LParen, "'(' after the aggregate function")?;
        let path = self.parse_path()?;
        self.expect(&Tok::RParen, "')' to close the aggregate")?;
        let by = self.parse_optional_by()?;
        Ok(Stage::Agg { func, path, by })
    }

    /// `[ "by" , field_list ]`.
    fn parse_optional_by(&mut self) -> Result<Vec<Field>, DslError> {
        if self.eat_keyword("by") {
            self.parse_field_list()
        } else {
            Ok(Vec::new())
        }
    }

    /// `field_list = field , { "," , field }` — `field` includes `severity`.
    fn parse_field_list(&mut self) -> Result<Vec<Field>, DslError> {
        let mut fields = vec![self.parse_field()?];
        while self.eat(&Tok::Comma) {
            fields.push(self.parse_field()?);
        }
        Ok(fields)
    }

    /// `field = nonsev_field | "severity"` (no resource./attr. — those are
    /// `path`/`scalar_path`, not `field`, per §7 `field_list`).
    fn parse_field(&mut self) -> Result<Field, DslError> {
        let name = match self.next() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected a field, found {}",
                    describe(other)
                )));
            }
        };
        match name.as_str() {
            "severity" => Ok(Field::Severity),
            other => bare_field(other).ok_or_else(|| {
                DslError::new(format!(
                    "unknown field {other:?} in a field list; expected a bare top-level \
                     field (resource./attr. paths are not allowed here)"
                ))
            }),
        }
    }

    fn parse_sort_stage(&mut self) -> Result<Stage, DslError> {
        self.pos += 1; // `sort`
        // `sort_key = field | ident` — any bare identifier (a field or an
        // aggregate output like `count`). Captured as its lexeme.
        let key = match self.next() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(DslError::new(format!(
                    "expected a sort key (a field or an aggregate output like count), found {}",
                    describe(other)
                )));
            }
        };
        // Optional `asc` / `desc`; default ascending.
        let desc = if self.eat_keyword("desc") {
            true
        } else {
            let _ = self.eat_keyword("asc");
            false
        };
        Ok(Stage::Sort { key, desc })
    }

    fn parse_limit_stage(&mut self) -> Result<Stage, DslError> {
        self.pos += 1; // `limit`
        let n = match self.next() {
            Some(Tok::Number(n)) => n.clone(),
            other => {
                return Err(DslError::new(format!(
                    "limit takes a non-negative integer, found {}",
                    describe(other)
                )));
            }
        };
        let n = n
            .parse::<u64>()
            .map_err(|_| DslError::new(format!("limit {n:?} is not a non-negative integer")))?;
        Ok(Stage::Limit(n))
    }

    fn parse_project_stage(&mut self) -> Result<Stage, DslError> {
        self.pos += 1; // `project`
        let fields = self.parse_field_list()?;
        Ok(Stage::Project(fields))
    }
}

/// Reject a non-string first operand to a string function
/// (`matches`/`contains`/`starts_with`/`ends_with`) — §6.1 requires a string
/// operand, so this is a parse-time type error, not a silent coercion. Shared
/// with the structured surface so both reject the same calls (RFC0002.2).
pub(crate) fn require_string_operand(name: &str, field: &Field) -> Result<(), DslError> {
    if field.is_string_operand() {
        Ok(())
    } else {
        Err(DslError::new(format!(
            "{name}(...) requires a string first operand; {} is not a string field",
            field_name(field)
        )))
    }
}

/// Validate a §7 `sort_key` — a single bare identifier (a field name or an
/// aggregate output like `count`). The string surface lexes it as one `ident`
/// token and the serialiser writes it unquoted, so a structured `sort.key`
/// must satisfy the same grammar to round-trip (RFC0002.2 / RFC0002.7).
pub(crate) fn validate_sort_key(key: &str) -> Result<(), DslError> {
    let mut chars = key.chars();
    let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic());
    if head_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(DslError::new(format!(
            "sort key {key:?} is not a bare identifier (a field or an aggregate \
             output like count)"
        )))
    }
}

/// A leak-free name for a [`Field`] in an error message.
fn field_name(field: &Field) -> String {
    match field {
        Field::Body => "body".to_string(),
        Field::Severity => "severity".to_string(),
        Field::Ts => "ts".to_string(),
        Field::ObservedTs => "observed_ts".to_string(),
        Field::TraceId => "trace_id".to_string(),
        Field::SpanId => "span_id".to_string(),
        Field::Scope => "scope".to_string(),
        Field::EventName => "event_name".to_string(),
        Field::Flags => "flags".to_string(),
        Field::Service => "service".to_string(),
        Field::TemplateId => "template_id".to_string(),
        Field::Confidence => "confidence".to_string(),
        Field::Lossy => "lossy".to_string(),
        Field::Resource(k) => format!("resource {k:?}"),
        Field::Attr(k) => format!("attr {k:?}"),
    }
}

/// Map a bare non-severity field name to its [`Field`], or `None` if unknown.
fn bare_field(name: &str) -> Option<Field> {
    Some(match name {
        "body" => Field::Body,
        "ts" => Field::Ts,
        "observed_ts" => Field::ObservedTs,
        "trace_id" => Field::TraceId,
        "span_id" => Field::SpanId,
        "scope" => Field::Scope,
        "event_name" => Field::EventName,
        "flags" => Field::Flags,
        "service" => Field::Service,
        "template_id" => Field::TemplateId,
        "confidence" => Field::Confidence,
        "lossy" => Field::Lossy,
        _ => return None,
    })
}

/// Parse a standalone `time` lexeme (a `range(...)` bound) into a [`Time`],
/// reusing the string-DSL `time` grammar so the structured surface agrees
/// (RFC0002.2). Rejects trailing input.
pub(crate) fn parse_time_pub(s: &str) -> Result<Time, DslError> {
    let tokens = tokenize(s)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
    };
    let time = parser.parse_time()?;
    parser.expect_eof()?;
    Ok(time)
}

/// Case-insensitive severity-name lookup, shared with the structured surface.
pub(crate) fn parse_severity_name_pub(name: &str) -> Option<SeverityName> {
    parse_severity_name(name)
}

/// Map a case-insensitive severity name to its [`SeverityName`].
fn parse_severity_name(name: &str) -> Option<SeverityName> {
    Some(match name.to_ascii_lowercase().as_str() {
        "trace" => SeverityName::Trace,
        "debug" => SeverityName::Debug,
        "info" => SeverityName::Info,
        "warn" => SeverityName::Warn,
        "error" => SeverityName::Error,
        "fatal" => SeverityName::Fatal,
        _ => return None,
    })
}

/// Parse a number lexeme into an int or float [`Value`], applying `neg`.
fn parse_number(lexeme: &str, neg: bool) -> Result<Value, DslError> {
    if lexeme.contains('.') {
        let v = lexeme
            .parse::<f64>()
            .map_err(|_| DslError::new(format!("malformed number {lexeme:?}")))?;
        Ok(Value::Float(if neg { -v } else { v }))
    } else {
        let v = lexeme
            .parse::<i64>()
            .map_err(|_| DslError::new(format!("integer {lexeme:?} is out of range")))?;
        Ok(Value::Int(if neg { -v } else { v }))
    }
}

/// A short, leak-free description of a token for error messages.
fn describe(tok: Option<&Tok>) -> String {
    match tok {
        None => "end of input".to_string(),
        Some(Tok::Ident(s)) => format!("identifier {s:?}"),
        Some(Tok::Str(s)) => format!("string {s:?}"),
        Some(Tok::Number(n)) => format!("number {n}"),
        Some(Tok::Duration(d)) => format!("duration {d}"),
        Some(Tok::Timestamp(t)) => format!("timestamp {t}"),
        Some(Tok::Pipe) => "'|'".to_string(),
        Some(Tok::LParen) => "'('".to_string(),
        Some(Tok::RParen) => "')'".to_string(),
        Some(Tok::LBracket) => "'['".to_string(),
        Some(Tok::RBracket) => "']'".to_string(),
        Some(Tok::Comma) => "','".to_string(),
        Some(Tok::Dot) => "'.'".to_string(),
        Some(Tok::Minus) => "'-'".to_string(),
        Some(Tok::Bang) => "'!'".to_string(),
        Some(Tok::BangTilde) => "'!~'".to_string(),
        Some(Tok::Tilde) => "'~'".to_string(),
        Some(Tok::EqEq) => "'=='".to_string(),
        Some(Tok::NotEq) => "'!='".to_string(),
        Some(Tok::Lt) => "'<'".to_string(),
        Some(Tok::Le) => "'<='".to_string(),
        Some(Tok::Gt) => "'>'".to_string(),
        Some(Tok::Ge) => "'>='".to_string(),
        Some(Tok::Match) => "'=~'".to_string(),
        Some(Tok::AndAnd) => "'&&'".to_string(),
        Some(Tok::OrOr) => "'||'".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ir::{Field, Predicate, Stage, Value};

    #[test]
    fn parses_match_all_and_match_none() {
        // Arrange / Act / Assert
        assert_eq!(parse("true").unwrap().predicate, Predicate::Bool(true));
        assert_eq!(parse("false").unwrap().predicate, Predicate::Bool(false));
    }

    #[test]
    fn parses_scalar_comparison() {
        // Act
        let q = parse("template_id == 42").unwrap();
        // Assert
        assert_eq!(
            q.predicate,
            Predicate::Comparison {
                field: Field::TemplateId,
                op: CmpOp::Ord(OrdOp::Eq),
                value: Value::Int(42),
            }
        );
    }

    #[test]
    fn parses_severity_name_case_insensitively() {
        // Act
        let q = parse("severity >= ERROR").unwrap();
        // Assert
        assert_eq!(
            q.predicate,
            Predicate::Severity {
                op: OrdOp::Ge,
                value: SeverityValue::Name(SeverityName::Error),
            }
        );
    }

    #[test]
    fn rejects_regex_operator_on_severity() {
        // Act
        let err = parse("severity =~ error").unwrap_err();
        // Assert
        assert!(err.message().contains("regex"), "{}", err.message());
    }

    #[test]
    fn rejects_severity_as_a_scalar_path() {
        // Act
        let err = parse("severity == \"x\"").unwrap_err();
        // Assert — `==` against a string routes through severity_cmp and the
        // string is not a severity name.
        assert!(!err.message().is_empty());
    }

    #[test]
    fn parses_bracketed_and_dotted_attr_keys() {
        // Act
        let dotted = parse("attr.http.status_code == 500").unwrap();
        let bracketed = parse("resource[\"k8s.pod.name\"] == \"p\"").unwrap();
        // Assert
        assert_eq!(
            dotted.predicate,
            Predicate::Comparison {
                field: Field::Attr("http.status_code".into()),
                op: CmpOp::Ord(OrdOp::Eq),
                value: Value::Int(500),
            }
        );
        assert!(matches!(
            bracketed.predicate,
            Predicate::Comparison { field: Field::Resource(k), .. } if k == "k8s.pod.name"
        ));
    }

    #[test]
    fn parses_and_or_not_precedence() {
        // `a or b and c` = Or[a, And[b, c]]; `not` binds tightest.
        let q = parse("lossy == true or template_id == 1 and confidence < 0.5").unwrap();
        match q.predicate {
            Predicate::Or(terms) => {
                assert_eq!(terms.len(), 2);
                assert!(matches!(terms[1], Predicate::And(_)));
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn parses_calls_with_arity_checks() {
        assert!(parse("contains(body, \"x\")").is_ok());
        assert!(parse("resolves_to(7)").is_ok());
        assert!(parse("contains(body)").is_err());
        assert!(parse("contains(body, \"x\", \"y\")").is_err());
        assert!(parse("resolves_to(\"x\")").is_err());
    }

    #[test]
    fn rejects_unterminated_string_and_literal_newline() {
        assert!(parse("body == \"oops").is_err());
        assert!(parse("body == \"a\nb\"").is_err());
    }

    #[test]
    fn parses_string_escapes() {
        let q = parse(r#"body == "a\tb\n\"c\\A""#).unwrap();
        assert_eq!(
            q.predicate,
            Predicate::Comparison {
                field: Field::Body,
                op: CmpOp::Ord(OrdOp::Eq),
                value: Value::Str("a\tb\n\"c\\A".into()),
            }
        );
    }

    #[test]
    fn rejects_bare_identifier_as_value() {
        let err = parse("template_id == X").unwrap_err();
        assert!(
            err.message().contains("bare identifier"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn parses_full_pipeline() {
        let q = parse(
            "service == \"api\" | range(-1h, now) | count by template_id | sort count desc | limit 10",
        )
        .unwrap();
        assert_eq!(q.stages.len(), 4);
        assert!(matches!(q.stages[0], Stage::Range(_, _)));
        assert!(matches!(q.stages[3], Stage::Limit(10)));
    }

    #[test]
    fn parse_time_pub_handles_all_three_forms() {
        assert_eq!(parse_time_pub("now").unwrap(), Time::Now);
        assert_eq!(
            parse_time_pub("-30s").unwrap(),
            Time::Duration {
                neg: true,
                literal: "30s".into()
            }
        );
        assert!(matches!(
            parse_time_pub("2026-01-02T03:04:05Z").unwrap(),
            Time::Timestamp(_)
        ));
        assert!(parse_time_pub("now extra").is_err());
    }

    #[test]
    fn rejects_malformed_duration_and_timestamp() {
        assert!(parse("ts >= -1hour").is_err());
        assert!(parse_time_pub("2026-13-02T03:04:05Z").is_err());
    }

    #[test]
    fn flattens_same_kind_groups() {
        // Arrange / Act — a parenthesised `and` inside an `and` (and likewise
        // for `or`) must flatten to one associative-normal node so the IR is
        // canonical and round-trips.
        let and = parse("body == 1 and (service == \"x\" and template_id == 2)").unwrap();
        let or = parse("body == 1 or (service == \"x\" or template_id == 2)").unwrap();
        // Assert — three flat terms, no nested same-kind child.
        match and.predicate {
            Predicate::And(terms) => {
                assert_eq!(terms.len(), 3);
                assert!(!terms.iter().any(|t| matches!(t, Predicate::And(_))));
            }
            other => panic!("expected flat And, got {other:?}"),
        }
        match or.predicate {
            Predicate::Or(terms) => {
                assert_eq!(terms.len(), 3);
                assert!(!terms.iter().any(|t| matches!(t, Predicate::Or(_))));
            }
            other => panic!("expected flat Or, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_string_operand_in_string_function() {
        // severity / lossy / ts are not string fields (§6.1).
        let err = parse("contains(severity, \"x\")").unwrap_err();
        assert!(err.message().contains("string"), "{}", err.message());
        assert!(parse("starts_with(lossy, \"x\")").is_err());
        assert!(parse("matches(ts, \"x\")").is_err());
        // body / service / attr. are string operands and stay valid.
        assert!(parse("contains(body, \"x\")").is_ok());
        assert!(parse("contains(attr.k, \"x\")").is_ok());
    }

    #[test]
    fn rejects_literal_newline_outside_a_string() {
        assert!(parse("body == 1\n| limit 1").is_err());
        assert!(parse("body\t== 1").is_ok());
    }

    #[test]
    fn requires_a_real_rfc3339_offset() {
        assert!(parse_time_pub("2026-01-02T03:04:05+").is_err());
        assert!(parse_time_pub("2026-01-02T03:04:05+05").is_err());
        assert!(parse_time_pub("2026-01-02T03:04:05+05:00").is_ok());
        assert!(parse_time_pub("2026-01-02T03:04:05.250+05:00").is_ok());
        assert!(parse_time_pub("2026-01-02T03:04:05Z").is_ok());
        assert!(parse_time_pub("2026-01-02T03:04:05+25:00").is_err());
    }

    #[test]
    fn validate_sort_key_rejects_non_idents() {
        assert!(validate_sort_key("count").is_ok());
        assert!(validate_sort_key("template_id").is_ok());
        assert!(validate_sort_key("attr.http.status_code").is_err());
        assert!(validate_sort_key("").is_err());
        assert!(validate_sort_key("3rd").is_err());
    }

    #[test]
    fn parses_drift_head_with_all_time_forms() {
        // Arrange / Act — `drift from <t1> to <t2>` over each §7 time form.
        use crate::dsl::ir::Statement;
        let rel = parse_statement("drift from -7d to now").unwrap();
        let abs =
            parse_statement("drift from 2026-06-01T00:00:00Z to 2026-06-02T00:00:00Z").unwrap();
        // Assert
        match rel {
            Statement::Drift(d) => {
                assert_eq!(
                    d.from,
                    Time::Duration {
                        neg: true,
                        literal: "7d".into()
                    }
                );
                assert_eq!(d.to, Time::Now);
            }
            Statement::Logs(_) => panic!("expected Drift, got Logs"),
        }
        assert!(matches!(
            abs,
            Statement::Drift(d) if matches!(d.from, Time::Timestamp(_)) && matches!(d.to, Time::Timestamp(_))
        ));
    }

    #[test]
    fn drift_head_forecloses_trailing_stages() {
        // A drift query is a closed verb head (RFC 0010 §6.1): a trailing `|`
        // stage is rejected, not silently composed.
        assert!(parse_statement("drift from -7d to now | limit 10").is_err());
        assert!(parse_statement("drift from -7d to now | count by template_id").is_err());
    }

    #[test]
    fn drift_head_requires_from_and_to() {
        assert!(parse_statement("drift").is_err());
        assert!(parse_statement("drift from -7d").is_err());
        assert!(parse_statement("drift -7d to now").is_err());
        assert!(parse_statement("drift from -7d now").is_err());
    }

    #[test]
    fn parse_rejects_drift_as_a_log_query() {
        // The RFC 0002 `parse` entry stays log-only: a drift head is an error
        // there (callers wanting it use `parse_statement`).
        let err = parse("drift from -7d to now").unwrap_err();
        assert!(err.message().contains("audit-stream"), "{}", err.message());
    }

    #[test]
    fn drift_is_not_a_reserved_field_in_a_log_query() {
        // `drift` is only special as the *leading* verb head (RFC 0010 §6.1):
        // once a query has started, a `drift` token is an ordinary identifier
        // (here an attribute key), so a log query never swallows it as a verb.
        // The query must contain a `drift` token after the head, else the test
        // wouldn't exercise the thing it names.
        use crate::dsl::ir::{Field, Predicate, Statement};
        let s = parse_statement("attr.drift == \"x\" | limit 5").unwrap();
        let Statement::Logs(query) = s else {
            panic!("a mid-query `drift` token must stay a Logs query, got {s:?}");
        };
        assert!(matches!(
            query.predicate,
            Predicate::Comparison { field: Field::Attr(ref k), .. } if k == "drift"
        ));
    }
}
