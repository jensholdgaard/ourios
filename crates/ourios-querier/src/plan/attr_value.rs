//! `ourios_attr_cmp(json, key, expected, eq)`: whether `key` resolves, in a
//! canonical-JSON attribute column, to `expected` (`eq`), or to a different
//! value of the same type (`!eq`; a key that resolves to nothing never
//! matches). This is the JSON arm of an attribute comparison (RFC 0022
//! §3.3). `key` resolves by RFC 0062 §3.1: the first top-level attribute
//! named `key`, or else a dotted key as a path into map values. A
//! substring match alone also hits the same key/value pair nested inside
//! another attribute's map value. First occurrence wins, as in the
//! promoted-column projection (`promoted::project_string_value`).

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{Array, AsArray, BooleanBuilder};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::{Result, ScalarValue, exec_datafusion_err, exec_err};
use datafusion::logical_expr::{
    ColumnarValue, Expr, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::prelude::lit;
use ourios_core::otlp::canonical;

const NAME: &str = "ourios_attr_cmp";

/// The member name every stored map value opens with.
const KVLIST: &str = "\"kvlistValue\"";

static UDF: LazyLock<Arc<ScalarUDF>> = LazyLock::new(|| {
    Arc::new(ScalarUDF::from(AttrCmp {
        signature: Signature::any(4, Volatility::Immutable),
    }))
});

/// `ourios_attr_cmp(column, key, expected, eq)` as an expression, where
/// `expected` is the [`canonical_value`] of the literal.
pub(super) fn attr_cmp(column: Expr, key: &str, expected: &str, eq: bool) -> Expr {
    UDF.call(vec![column, lit(key), lit(expected), lit(eq)])
}

/// The canonical JSON a stored value of `value` has, so a comparison is
/// byte-aligned with the stored text.
pub(super) fn canonical_value(
    value: ourios_core::otlp::any_value::Value,
) -> Result<String, canonical::CanonicalJsonError> {
    let bytes = canonical::encode_any_value(&ourios_core::otlp::AnyValue { value: Some(value) })?;
    // The encoder emits `serde_json` output, which is always UTF-8.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct AttrCmp {
    signature: Signature,
}

impl ScalarUDFImpl for AttrCmp {
    fn name(&self) -> &str {
        NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Boolean)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [
            json,
            ColumnarValue::Scalar(key),
            ColumnarValue::Scalar(expected),
            ColumnarValue::Scalar(ScalarValue::Boolean(Some(eq))),
        ] = args.args.as_slice()
        else {
            return exec_err!("{NAME} takes a column and literal key, value and mode");
        };
        let (Some(Some(key)), Some(Some(expected))) = (key.try_as_str(), expected.try_as_str())
        else {
            return exec_err!("{NAME} takes a string key and value");
        };
        let cmp = Cmp::new(key, expected, *eq);
        let array = match json {
            ColumnarValue::Scalar(json) => {
                let hit = json
                    .try_as_str()
                    .flatten()
                    .map(|j| cmp.eval(j))
                    .transpose()?;
                return Ok(ColumnarValue::Scalar(ScalarValue::Boolean(hit)));
            }
            ColumnarValue::Array(array) => array,
        };
        let mut out = BooleanBuilder::with_capacity(array.len());
        match array.data_type() {
            DataType::Utf8 => cmp.fill(array.as_string::<i32>().iter(), &mut out)?,
            DataType::LargeUtf8 => cmp.fill(array.as_string::<i64>().iter(), &mut out)?,
            DataType::Utf8View => cmp.fill(array.as_string_view().iter(), &mut out)?,
            other => return exec_err!("{NAME} takes a string column, got {other}"),
        }
        Ok(ColumnarValue::Array(Arc::new(out.finish())))
    }
}

struct Cmp<'a> {
    key: &'a str,
    expected: &'a str,
    /// `{"stringValue":` / `{"intValue":`: how every value of the
    /// expected type starts.
    type_prefix: &'a str,
    eq: bool,
    /// `"key":"<key>"`, `{"key":"<key>","value":<expected>}` and
    /// `{"key":"<key>","value":<type_prefix>`, as the canonical encoder
    /// writes them.
    needle: String,
    pair: String,
    typed: String,
    /// For a [`tabled`] key, the needle of every run of `key` from its
    /// start or a dot to a dot or its end: the heads and tails a path
    /// splits the key into. Any other key is resolved in one pass per list
    /// with no needles, since the table grows with the square of the parts
    /// times the key's length.
    segments: Vec<(Range<usize>, String)>,
}

/// The most dot-separated parts a [`tabled`] key may have: 136 needles at
/// 16 parts, while a few thousand parts would retain gigabytes. `OTel`
/// attribute names have well under 16.
pub(super) const MAX_TABLED_SEGMENTS: usize = 16;

/// The longest [`tabled`] key, so its needle table stays under about
/// 140 KiB however long the parts are.
const MAX_TABLED_KEY_BYTES: usize = 1024;

/// Whether a key is short enough to keep a needle per run of its parts
/// (and, in the `!=` prefilter, a pattern per tail).
pub(super) fn tabled(key: &str) -> bool {
    key.len() <= MAX_TABLED_KEY_BYTES && key.split('.').count() <= MAX_TABLED_SEGMENTS
}

/// `"key":"<part>"`, as the canonical encoder writes a pair's key.
fn needle(part: &str) -> String {
    let quoted = serde_json::to_string(part).unwrap_or_default();
    format!("\"key\":{quoted}")
}

impl<'a> Cmp<'a> {
    fn new(key: &'a str, expected: &'a str, eq: bool) -> Self {
        let type_prefix = expected.find(':').map_or(expected, |i| &expected[..=i]);
        let segments = if tabled(key) {
            let dots = key.match_indices('.').map(|(i, _)| i);
            let starts: Vec<usize> = std::iter::once(0)
                .chain(dots.clone().map(|i| i + 1))
                .collect();
            let ends: Vec<usize> = dots.chain(std::iter::once(key.len())).collect();
            starts
                .iter()
                .flat_map(|&s| ends.iter().filter(move |&&e| e >= s).map(move |&e| s..e))
                .map(|span| {
                    let text = needle(&key[span.clone()]);
                    (span, text)
                })
                .collect()
        } else {
            Vec::new()
        };
        let whole = needle(key);
        Self {
            key,
            expected,
            type_prefix,
            eq,
            pair: format!("{{{whole},\"value\":{expected}}}"),
            typed: format!("{{{whole},\"value\":{type_prefix}"),
            needle: whole,
            segments,
        }
    }

    fn fill<'s>(
        &self,
        rows: impl Iterator<Item = Option<&'s str>>,
        out: &mut BooleanBuilder,
    ) -> Result<()> {
        for row in rows {
            out.append_option(row.map(|json| self.eval(json)).transpose()?);
        }
        Ok(())
    }

    /// The key's first occurrence is the first top-level one unless a map
    /// value opens before it, and JSON escaping keeps the pair out of
    /// string values, so the text there answers. A row without the key
    /// can hold a path only through a map. Otherwise the row is resolved.
    fn eval(&self, json: &str) -> Result<bool> {
        let value = match json.find(&self.needle) {
            Some(at) if !json[..at].contains(KVLIST) => return Ok(self.text_answer(&json[at..])),
            None if !self.key.contains('.') || !json.contains(KVLIST) => return Ok(false),
            _ => self.resolve(json)?,
        };
        Ok(value.is_some_and(|value| self.value_answer(value)))
    }

    /// The answer from the stored text at the key's top-level pair;
    /// `pair` and `typed` open with the `{` before the needle.
    fn text_answer(&self, rest: &str) -> bool {
        let is = |text: &str| rest.starts_with(&text[1..]);
        if self.eq {
            is(&self.pair)
        } else {
            is(&self.typed) && !is(&self.pair)
        }
    }

    fn value_answer(&self, value: &str) -> bool {
        if self.eq {
            value == self.expected
        } else {
            value.starts_with(self.type_prefix) && value != self.expected
        }
    }

    /// RFC 0062 §3.1 on the stored row: the value of the first pair named
    /// by the whole key, or else, the longest head first, the rest of the
    /// key resolved inside the map held by the first pair the head names.
    ///
    /// A worklist of open lists replaces recursion, so stored nesting
    /// costs heap, not stack. The first value found is the answer, since
    /// every list it sits under would return it unchanged. No list is
    /// opened twice for one row: a map is reached only through its own
    /// pair, whose key fixes where the rest of the key starts. A key of
    /// few parts finds each head with its own substring search, at most
    /// [`MAX_TABLED_SEGMENTS`] passes over a list; a longer key finds
    /// them all in one pass, so a list costs one scan however many parts
    /// the key has.
    fn resolve<'j>(&self, row: &'j str) -> Result<Option<&'j str>> {
        let mut open = Vec::new();
        let mut next = Some((row, 0));
        loop {
            if let Some((list, start)) = next.take() {
                match self.open(list, start)? {
                    Opened::Whole(value) => return Ok(Some(value)),
                    Opened::List(frame) => open.push(frame),
                }
            }
            let Some(frame) = open.last_mut() else {
                return Ok(None);
            };
            match self.next_map(frame)? {
                Some(map) => next = Some(map),
                None => {
                    open.pop();
                }
            }
        }
    }

    /// `list` with the key resolved from byte `start` on: the value its
    /// first pair named by that rest holds (rule 1), or the list to try
    /// the heads of.
    fn open<'j>(&self, list: &'j str, start: usize) -> Result<Opened<'j>> {
        #[cfg(test)]
        tests::count(1, 0);
        let whole = start..self.key.len();
        if !self.segments.is_empty() {
            return Ok(match first_pair(list, &self.needle_of(&whole))? {
                Some(value) => Opened::Whole(value),
                None => Opened::List(Frame {
                    list,
                    start,
                    heads: Heads::Search { end: whole.end },
                }),
            });
        }
        let found = scan_list(list, self.escaped_from(start))?;
        Ok(match found {
            Scanned::Whole(value) => Opened::Whole(value),
            Scanned::Heads(heads) => Opened::List(Frame {
                list,
                start,
                heads: Heads::Found(self.head_maps(start, heads)),
            }),
        })
    }

    /// The next map to resolve the rest of the key in, longest head first,
    /// with where that rest starts.
    fn next_map<'j>(&self, frame: &mut Frame<'j>) -> Result<Option<(&'j str, usize)>> {
        match &mut frame.heads {
            Heads::Found(maps) => Ok(maps.pop()),
            Heads::Search { end } => self.search_head(frame.list, frame.start, end),
        }
    }

    /// The longest head before `end` whose first pair in `list` holds a
    /// map, moving `end` past every head tried.
    fn search_head<'j>(
        &self,
        list: &'j str,
        start: usize,
        end: &mut usize,
    ) -> Result<Option<(&'j str, usize)>> {
        while let Some(dot) = self.key[start..*end].rfind('.').map(|i| start + i) {
            *end = dot;
            let head = first_pair(list, &self.needle_of(&(start..dot)))?;
            if let Some(map) = head.and_then(map_entries) {
                return Ok(Some((map, dot + 1)));
            }
        }
        Ok(None)
    }

    /// The maps the scanned heads hold, each with where the rest of the
    /// key starts inside it, shortest head first so the longest pops first.
    fn head_maps<'j>(&self, start: usize, heads: Vec<(usize, &'j str)>) -> Vec<(&'j str, usize)> {
        let mut dots = self.key[start..].match_indices('.').map(|(i, _)| start + i);
        let mut taken = 0;
        heads
            .into_iter()
            .filter_map(|(k, value)| {
                let dot = dots.nth(k - taken)?;
                taken = k + 1;
                map_entries(value).map(|map| (map, dot + 1))
            })
            .collect()
    }

    /// The escaped text of the key from byte `start`, which follows a dot
    /// or is the key's start; escaping keeps every `.` as it is.
    fn escaped_from(&self, start: usize) -> &str {
        // `needle` is `"key":"<escaped key>"`.
        let escaped = &self.needle[KEY_OPEN.len()..self.needle.len() - 1];
        let at = match self.key[..start].matches('.').count() {
            0 => 0,
            dots => escaped
                .match_indices('.')
                .nth(dots - 1)
                .map_or(escaped.len(), |(i, _)| i + 1),
        };
        &escaped[at..]
    }

    fn needle_of(&self, span: &Range<usize>) -> Cow<'_, str> {
        if let Some((_, tabled)) = self.segments.iter().find(|(s, _)| s == span) {
            return Cow::Borrowed(tabled);
        }
        #[cfg(test)]
        tests::count_needle();
        Cow::Owned(needle(&self.key[span.clone()]))
    }

    #[cfg(test)]
    fn tabled_needle_bytes(&self) -> usize {
        self.segments.iter().map(|(_, n)| n.len()).sum()
    }

    /// The same comparison resolved as a key of many parts is.
    #[cfg(test)]
    fn without_table(mut self) -> Self {
        self.segments.clear();
        self
    }
}

/// A list being resolved, with where the rest of the key starts in it and
/// the heads of that rest still to try.
struct Frame<'j> {
    list: &'j str,
    start: usize,
    heads: Heads<'j>,
}

enum Heads<'j> {
    /// A key of few parts: the heads before `end` are still to search for.
    Search { end: usize },
    /// A key of many parts: the maps its scan found, shortest head first.
    Found(Vec<(&'j str, usize)>),
}

/// A list once opened: the value rule 1 found in it, or its heads.
enum Opened<'j> {
    Whole(&'j str),
    List(Frame<'j>),
}

/// What one pass over a list found for the rest of the key.
enum Scanned<'j> {
    /// The first pair named by the whole rest: rule 1, so the answer.
    Whole(&'j str),
    /// The first pair named by each head, as (dot index in the rest,
    /// value), in head order.
    Heads(Vec<(usize, &'j str)>),
}

/// One pass over `list` for the rest of the key, `rest` as the canonical
/// encoder escapes it.
fn scan_list<'j>(list: &'j str, rest: &str) -> Result<Scanned<'j>> {
    #[cfg(test)]
    tests::count(0, list.len());
    let mut heads = Vec::new();
    for pair in PairKeys::new(list) {
        let (stored, end) = pair?;
        match classify_key(stored, rest) {
            Named::Whole => return Ok(Scanned::Whole(value_at(list, end)?)),
            Named::Head(k) => heads.push((k, value_at(list, end)?)),
            Named::Other => {}
        }
    }
    // A stable sort keeps the first pair of each head ahead of the others.
    heads.sort_by_key(|&(k, _)| k);
    heads.dedup_by_key(|&mut (k, _)| k);
    Ok(Scanned::Heads(heads))
}

/// The value stored after a pair's key that ends at `end`, empty for a
/// pair stored without one.
fn value_at(list: &str, end: usize) -> Result<&str> {
    Ok(value_after(list, end)?.unwrap_or(""))
}

/// The keys of a stored list's top-level pairs, as stored (escaped, no
/// quotes), each with the index after its closing quote. A list's pairs
/// have their members at depth 2 (array, then object); a list that ends
/// early is an error.
struct PairKeys<'j> {
    list: &'j str,
    tokens: Tokens<'j>,
    depth: usize,
}

impl<'j> PairKeys<'j> {
    fn new(list: &'j str) -> Self {
        Self {
            list,
            tokens: Tokens::new(list.as_bytes(), 0),
            depth: 0,
        }
    }
}

/// The key in `list` whose opening quote is at `open`, and the index after it.
fn key_at(list: &str, open: usize) -> Result<(&str, usize)> {
    let end = string_end(list.as_bytes(), open)?;
    Ok((&list[open + 1..end - 1], end))
}

impl<'j> Iterator for PairKeys<'j> {
    type Item = Result<(&'j str, usize)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_key().transpose()
    }
}

impl<'j> PairKeys<'j> {
    fn next_key(&mut self) -> Result<Option<(&'j str, usize)>> {
        let b = self.list.as_bytes();
        for token in self.tokens.by_ref() {
            let (i, _, c) = token?;
            match c {
                b'"' if self.depth == 2 && b[i..].starts_with(KEY_OPEN.as_bytes()) => {
                    return key_at(self.list, i + KEY_OPEN.len() - 1).map(Some);
                }
                b'[' | b'{' => self.depth += 1,
                b']' | b'}' => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
        }
        // An unclosed list is reported once; the walk has ended either way.
        if std::mem::take(&mut self.depth) == 0 {
            Ok(None)
        } else {
            Err(truncated())
        }
    }
}

/// How a pair's key, the text in `"key":"…"`, names the rest of the key
/// being resolved: all of it, its head before the `k`th dot, or neither.
enum Named {
    Whole,
    Head(usize),
    Other,
}

/// `stored` and `rest` are JSON-escaped the same way, one character at
/// a time, and no escape holds a `.`, so `stored` names a head exactly
/// when it is a prefix of `rest` that a `.` follows.
fn classify_key(stored: &str, rest: &str) -> Named {
    match rest.strip_prefix(stored) {
        Some("") => Named::Whole,
        Some(rest) if rest.starts_with('.') => Named::Head(stored.matches('.').count()),
        _ => Named::Other,
    }
}

/// A pair's `key` member, up to the opening quote of the key itself.
const KEY_OPEN: &str = "\"key\":\"";

/// The stored text of the value of the first pair in `list` whose key
/// is `needle`, which is what [`canonical::encode_any_value`] writes for
/// it; empty for a pair stored without a value. A list's pairs have their
/// members at depth 2 (array, then object). The scan keeps no stack, so
/// value depth is no limit. A row that ends early fails the query rather
/// than drop out of the result.
fn first_pair<'j>(list: &'j str, needle: &str) -> Result<Option<&'j str>> {
    #[cfg(test)]
    tests::count(0, list.len());
    if !list.contains(needle) {
        return Ok(None);
    }
    let b = list.as_bytes();
    let mut depth = 0usize;
    for token in Tokens::new(b, 0) {
        let (i, _, c) = token?;
        match c {
            b'"' if depth == 2 && b[i..].starts_with(needle.as_bytes()) => {
                return Ok(Some(value_after(list, i + needle.len())?.unwrap_or("")));
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if depth == 0 {
        Ok(None)
    } else {
        Err(truncated())
    }
}

/// The pair list inside a stored map value, `None` for any other value.
fn map_entries(value: &str) -> Option<&str> {
    value
        .strip_prefix("{\"kvlistValue\":{\"values\":")?
        .strip_suffix("}}")
}

/// The value text after a pair's key at `at`, `None` for a pair stored
/// without one.
fn value_after(json: &str, at: usize) -> Result<Option<&str>> {
    let Some(rest) = json[at..].strip_prefix(",\"value\":") else {
        return Ok(None);
    };
    let start = json.len() - rest.len();
    Ok(Some(&json[start..value_end(json.as_bytes(), start)?]))
}

/// The index after the value that starts at `b[start]`: an object, an
/// array, a string, or a scalar such as the `null` an empty value can be
/// stored as.
fn value_end(b: &[u8], start: usize) -> Result<usize> {
    let mut depth = 0usize;
    for token in Tokens::new(b, start) {
        let (i, end, c) = token?;
        match c {
            b'"' if depth == 0 => return Ok(end),
            b'[' | b'{' => depth += 1,
            b']' | b'}' | b',' if depth == 0 => return Ok(i),
            b']' | b'}' if depth == 1 => return Ok(end),
            b']' | b'}' => depth -= 1,
            _ => {}
        }
    }
    Err(truncated())
}

fn truncated() -> datafusion::common::DataFusionError {
    exec_datafusion_err!("{NAME}: stored attributes end early")
}

/// The bytes of a JSON text outside its strings, as `(index, end, byte)`;
/// a string comes through once, as its opening quote with `end` after
/// its closing one.
struct Tokens<'b> {
    b: &'b [u8],
    i: usize,
}

impl<'b> Tokens<'b> {
    fn new(b: &'b [u8], i: usize) -> Self {
        Self { b, i }
    }
}

/// The index after the string that opens at `b[i]`.
fn string_end(b: &[u8], i: usize) -> Result<usize> {
    let mut j = i + 1;
    loop {
        match b.get(j) {
            Some(b'\\') => j += 2,
            Some(b'"') => return Ok(j + 1),
            Some(_) => j += 1,
            None => return Err(truncated()),
        }
    }
}

impl Iterator for Tokens<'_> {
    type Item = Result<(usize, usize, u8)>;

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.i;
        let c = *self.b.get(i)?;
        let end = if c == b'"' {
            string_end(self.b, i)
        } else {
            Ok(i + 1)
        };
        // A string that never closes ends the walk after its error.
        self.i = *end.as_ref().unwrap_or(&self.b.len());
        Some(end.map(|end| (i, end, c)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ourios_core::otlp::any_value::Value;
    use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};

    use ourios_testgen::strategies::adversarial;
    use proptest::prelude::*;

    use super::{Cmp, MAX_TABLED_SEGMENTS, canonical, canonical_value};

    thread_local! {
        /// Lists opened, and the bytes of the lists scanned for pairs.
        static RESOLVES: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
        /// Needles built while resolving, outside the table.
        static NEEDLES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn count(opened: usize, scanned: usize) {
        RESOLVES.with(|c| {
            let (n, bytes) = c.get();
            c.set((n + opened, bytes + scanned));
        });
    }

    pub(super) fn count_needle() {
        NEEDLES.with(|c| c.set(c.get() + 1));
    }

    fn take_resolves() -> (usize, usize) {
        RESOLVES.with(|c| c.replace((0, 0)))
    }

    fn take_needles() -> usize {
        NEEDLES.with(|c| c.replace(0))
    }

    fn kv(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
            ..Default::default()
        }
    }

    fn stored(attrs: &[KeyValue]) -> String {
        String::from_utf8(canonical::encode_attributes(attrs).expect("encode")).expect("utf-8")
    }

    fn string(s: &str) -> Value {
        Value::StringValue(s.to_owned())
    }

    fn map(entries: Vec<KeyValue>) -> Value {
        Value::KvlistValue(KeyValueList { values: entries })
    }

    /// `(attr.k == v, attr.k != v)` for the stored row.
    fn cmp(json: &str, v: &str) -> (bool, bool) {
        let expected = canonical_value(string(v)).expect("encode");
        let run = |eq| Cmp::new("k", &expected, eq).eval(json).expect("eval");
        (run(true), run(false))
    }

    /// `attrs` compared as stored, and behind a leading map value, which
    /// sends the comparison down the full scan.
    fn assert_both_paths(attrs: &[KeyValue], values: [&str; 2], expected: [(bool, bool); 2]) {
        let behind_a_map = [&[kv("m", map(Vec::new()))], attrs].concat();
        for json in [stored(attrs), stored(&behind_a_map)] {
            assert_eq!(values.map(|v| cmp(&json, v)), expected, "{json}");
        }
    }

    #[test]
    fn the_first_top_level_occurrence_wins() {
        let attrs = vec![kv("k", string("a")), kv("k", string("x"))];
        assert_both_paths(&attrs, ["a", "x"], [(true, false), (false, true)]);
    }

    #[test]
    fn a_nested_pair_is_not_a_top_level_attribute() {
        let json = stored(&[kv("outer", map(vec![kv("k", string("x"))]))]);
        assert!(json.contains("{\"key\":\"k\",\"value\":{\"stringValue\":\"x\"}}"));
        assert_eq!([cmp(&json, "x"), cmp(&json, "y")], [(false, false); 2]);
    }

    #[test]
    fn a_top_level_pair_next_to_a_map_is_found() {
        let json = stored(&[
            kv("outer", map(vec![kv("k", string("y"))])),
            kv("k", string("x")),
        ]);
        assert_eq!(
            [cmp(&json, "x"), cmp(&json, "y")],
            [(true, false), (false, true)]
        );
    }

    #[test]
    fn a_map_valued_key_matches_no_string() {
        let json = stored(&[kv("k", map(vec![kv("k", string("x"))]))]);
        assert_eq!(cmp(&json, "x"), (false, false));
    }

    #[test]
    fn an_absent_key_matches_nothing_and_an_unparsable_row_errors() {
        assert_eq!(
            cmp(&stored(&[kv("other", string("x"))]), "x"),
            (false, false)
        );
        let expected = canonical_value(string("x")).expect("encode");
        let broken = r#"[{"key":"m","value":{"kvlistValue":{"values":[{"key":"k""#;
        assert!(Cmp::new("k", &expected, true).eval(broken).is_err());
    }

    #[test]
    fn pair_text_inside_a_string_value_is_not_a_pair() {
        let text = r#"{"key":"k","value":{"stringValue":"x"}}"#;
        let attrs = vec![kv("note", string(text)), kv("k", string("y"))];
        assert_both_paths(&attrs, ["x", "y"], [(false, true), (true, false)]);
    }

    #[test]
    fn an_escaped_key_is_matched_as_stored() {
        let key = "a\"b\\c";
        let json = stored(&[kv("m", map(Vec::new())), kv(key, string("x"))]);
        let expected = canonical_value(string("x")).expect("encode");
        assert!(Cmp::new(key, &expected, true).eval(&json).expect("eval"));
    }

    #[test]
    fn value_depth_is_no_limit() {
        let deep = (0..200).fold(string("x"), |v, _| map(vec![kv("k", v)]));
        let json = stored(&[kv("d", deep), kv("k", string("x"))]);
        assert_eq!(cmp(&json, "x"), (true, false));
    }

    /// A key of thousands of parts resolves by the same rule without
    /// retaining a needle per run of parts, which would be gigabytes.
    #[test]
    fn a_key_of_many_parts_resolves_without_a_needle_table() {
        let parts = 2_000;
        let key = vec!["a"; parts].join(".");
        assert_eq!(key.len(), 3_999);
        let head = vec!["a"; parts - 1].join(".");
        let expected = canonical_value(string("x")).expect("encode");
        let rows = [
            (stored(&[kv(&key, string("x"))]), (true, false)),
            (
                stored(&[kv(&head, map(vec![kv("a", string("x"))]))]),
                (true, false),
            ),
            (
                stored(&[kv(&head, map(vec![kv("a", string("y"))]))]),
                (false, true),
            ),
            (stored(&[kv("other", map(Vec::new()))]), (false, false)),
        ];
        let eq = Cmp::new(&key, &expected, true);
        let ne = Cmp::new(&key, &expected, false);
        assert_eq!(eq.tabled_needle_bytes() + ne.tabled_needle_bytes(), 0);
        for (json, want) in rows {
            let got = (eq.eval(&json).expect("eval"), ne.eval(&json).expect("eval"));
            assert_eq!(got, want, "{}", &json[..80]);
        }
        let few = Cmp::new("a.b.c", &expected, true);
        assert_eq!(few.segments.len(), 6, "a key of few parts keeps its table");
        // Two parts, but over the byte cap: no table, the same answer.
        let head = "h".repeat(2_000);
        let long = format!("{head}.b");
        let json = stored(&[kv(&head, map(vec![kv("b", string("x"))]))]);
        let cmp = Cmp::new(&long, &expected, true);
        assert_eq!(cmp.tabled_needle_bytes(), 0);
        assert!(cmp.eval(&json).expect("eval"));
    }

    /// Maps keyed `a`, `a.a` and `a.a.a` at every level, each a head of
    /// the `rest` parts still to resolve, so the search enters every one
    /// of them. Only the chain of `a` heads, which it tries last, ends in
    /// the value.
    fn branching(levels: usize, rest: usize, last_chain: bool) -> Vec<KeyValue> {
        let a = |n: usize| vec!["a"; n].join(".");
        match levels {
            0 if last_chain => vec![kv(&a(rest), string("x"))],
            0 => Vec::new(),
            _ => (1..=3)
                .map(|n| {
                    let inner = branching(levels - 1, rest.saturating_sub(n), last_chain && n == 1);
                    kv(&a(n), map(inner))
                })
                .collect(),
        }
    }

    /// Each map is resolved at most once per row, and a key of many parts
    /// scans each list once, so the work is linear in the row.
    #[test]
    fn crafted_nested_heads_resolve_in_linear_work() {
        for (levels, parts) in [(6, 24), (5, 16)] {
            let key = vec!["a"; parts].join(".");
            let attrs = branching(levels, parts, true);
            let json = stored(&attrs);
            let maps = (1..=levels)
                .map(|l| 3usize.pow(u32::try_from(l).expect("small")))
                .sum::<usize>();
            let expected = canonical_value(string("x")).expect("encode");
            let oracle = resolve_decoded(&attrs, &key);
            assert_eq!(
                oracle.as_deref(),
                Some(expected.as_str()),
                "the value is reachable"
            );
            take_resolves();
            let got = (
                Cmp::new(&key, &expected, true).eval(&json).expect("eval"),
                Cmp::new(&key, &expected, false).eval(&json).expect("eval"),
            );
            let (calls, bytes) = take_resolves();
            assert_eq!(got, (true, false), "{parts} parts");
            // Two comparisons, each resolving every map and the row at most
            // once. A key of many parts scans a list once; a shorter one
            // once per part at most. Nested lists overlap, so the lists of
            // one level add up to at most the row.
            let passes = if parts > MAX_TABLED_SEGMENTS {
                1
            } else {
                parts
            };
            assert!(
                calls <= 2 * (maps + 1),
                "{calls} resolve calls for {maps} maps"
            );
            let bound = 2 * passes * (levels + 1) * json.len();
            assert!(
                bytes <= bound,
                "{bytes} bytes scanned, bound {bound}, {parts} parts"
            );
            assert_eq!(
                take_needles(),
                0,
                "every needle comes from the table or none is needed"
            );
        }
    }

    /// A row a key of many parts cannot resolve in, but whose value passes
    /// the `==` prefilter, costs one pass and builds no needle.
    #[test]
    fn a_key_of_many_parts_rejects_an_unrelated_map_in_one_pass() {
        let key = vec!["a"; 2_000].join(".");
        let json = stored(&[kv("other", map(vec![kv("id", string("x"))]))]);
        let expected = canonical_value(string("x")).expect("encode");
        assert!(json.contains("\"x\"}}"), "the row passes the prefilter");
        take_resolves();
        take_needles();
        let got = (
            Cmp::new(&key, &expected, true).eval(&json).expect("eval"),
            Cmp::new(&key, &expected, false).eval(&json).expect("eval"),
        );
        assert_eq!(got, (false, false));
        let (opened, bytes) = take_resolves();
        assert_eq!(
            (opened, bytes),
            (2, 2 * json.len()),
            "one pass over the row each"
        );
        assert_eq!(take_needles(), 0);
    }

    /// One map per part, `a = {a = {… = "x"}}`, written as the encoder
    /// writes it. Stored nesting is bounded at ingest today (#1136), but
    /// the lookup must not rely on it.
    fn nested_chain(depth: usize) -> String {
        let open = r#"[{"key":"a","value":{"kvlistValue":{"values":"#;
        let mut json = open.repeat(depth - 1);
        json.push_str(r#"[{"key":"a","value":{"stringValue":"x"}}]"#);
        json.push_str(&"}}}]".repeat(depth - 1));
        json
    }

    /// The lookup keeps no Rust frame per map: 2,000 nested maps resolve
    /// on a thread whose stack would not hold a frame for each.
    #[test]
    fn deep_nesting_resolves_on_a_small_stack() {
        let depth = 2_000;
        let json = nested_chain(depth);
        assert!(
            canonical::decode_attributes(nested_chain(8).as_bytes()).is_ok(),
            "the shape decodes"
        );
        let key = vec!["a"; depth].join(".");
        let got = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                let expected = canonical_value(string("x")).expect("encode");
                let eq = Cmp::new(&key, &expected, true).eval(&json).expect("eval");
                let ne = Cmp::new(&key, &expected, false).eval(&json).expect("eval");
                let short = Cmp::new("a.a", &expected, true).eval(&json).expect("eval");
                (eq, ne, short)
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow");
        assert_eq!(got, (true, false, false));
    }

    /// A pinned case of `the_scan_agrees_with_a_full_decode`: a nested key
    /// `a.b` that only a path resolves at the top level, `a` → `b`.
    #[test]
    fn the_scan_agrees_with_a_full_decode_on_a_path_beside_a_nested_dotted_key() {
        let attrs = vec![
            kv("a", map(vec![kv("b", string("x"))])),
            kv("outer", map(vec![kv("a.b", string("unrelated"))])),
        ];
        let json = stored(&attrs);
        let mut keys = Vec::new();
        all_keys(&attrs, &mut keys);
        for key in keys {
            let actual = resolve_decoded(&attrs, &key);
            for v in ["x", "y", "unrelated"] {
                let expected = canonical_value(string(v)).expect("encode");
                let type_prefix = expected
                    .find(':')
                    .map_or(expected.as_str(), |i| &expected[..=i]);
                let oracle = actual.as_deref().map_or((false, false), |a| {
                    (a == expected, a.starts_with(type_prefix) && a != expected)
                });
                let got = (
                    Cmp::new(&key, &expected, true).eval(&json).expect("eval"),
                    Cmp::new(&key, &expected, false).eval(&json).expect("eval"),
                );
                assert_eq!(got, oracle, "key {key:?} expected {expected}");
            }
        }
        let x = canonical_value(string("x")).expect("encode");
        assert!(
            Cmp::new("a.b", &x, true).eval(&json).expect("eval"),
            "a.b resolves to a → b"
        );
    }

    /// Every key in `attrs`, nested map keys included.
    fn all_keys(attrs: &[KeyValue], out: &mut Vec<String>) {
        for kv in attrs {
            out.push(kv.key.clone());
            if let Some(Value::KvlistValue(m)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                all_keys(&m.values, out);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: ourios_testgen::proptest_cases(32),
            ..ProptestConfig::default()
        })]

        #[test]
        fn the_scan_agrees_with_a_full_decode(record in adversarial(), v in "[a-z]{0,2}") {
            let json = stored(&record.attributes);
            let decoded = canonical::decode_attributes(json.as_bytes()).expect("decode");
            let mut keys = Vec::new();
            all_keys(&decoded, &mut keys);
            for key in keys {
                let actual = resolve_decoded(&decoded, &key);
                let mut values = vec![canonical_value(string(&v)).expect("encode")];
                // A query literal is a string or an integer.
                values.extend(actual.clone().filter(|a| {
                    a.starts_with("{\"stringValue\":") || a.starts_with("{\"intValue\":")
                }));
                for expected in values {
                    let type_prefix = expected.find(':').map_or(expected.as_str(), |i| &expected[..=i]);
                    let oracle = actual.as_deref().map_or((false, false), |a| {
                        (a == expected, a.starts_with(type_prefix) && a != expected)
                    });
                    let got = (
                        Cmp::new(&key, &expected, true).eval(&json).expect("eval"),
                        Cmp::new(&key, &expected, false).eval(&json).expect("eval"),
                    );
                    prop_assert_eq!(got, oracle, "key {:?} expected {} in {}", key, expected, json);
                }
            }
        }
    }

    /// RFC 0062 §3.1 on decoded attributes: the first pair named `key`,
    /// else the longest head that holds a map with the rest resolving in
    /// it, as stored text; empty for a pair without a value.
    fn resolve_decoded(attrs: &[KeyValue], key: &str) -> Option<String> {
        if let Some(kv) = attrs.iter().find(|kv| kv.key == key) {
            return Some(kv.value.as_ref().map_or_else(String::new, |v| {
                String::from_utf8(canonical::encode_any_value(v).expect("encode")).expect("utf-8")
            }));
        }
        key.rmatch_indices('.').find_map(|(dot, _)| {
            let head = attrs.iter().find(|kv| kv.key == key[..dot])?;
            match head.value.as_ref()?.value.as_ref()? {
                Value::KvlistValue(map) => resolve_decoded(&map.values, &key[dot + 1..]),
                _ => None,
            }
        })
    }

    /// Every key and every dotted join of a key path from the top level.
    fn keys_and_paths(attrs: &[KeyValue], prefix: &str, out: &mut BTreeSet<String>) {
        for kv in attrs {
            let path = if prefix.is_empty() {
                kv.key.clone()
            } else {
                format!("{prefix}.{}", kv.key)
            };
            if let Some(Value::KvlistValue(m)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                keys_and_paths(&m.values, &path, out);
            }
            out.insert(kv.key.clone());
            out.insert(path);
        }
    }

    #[test]
    fn a_dotted_key_resolves_by_the_rfc_0062_rule() {
        let c = |v: &str| kv("c", string(v));
        // RFC 0062 §3.1's table for `a.b.c`.
        let cases = [
            (
                vec![
                    kv("a.b.c", string("x")),
                    kv("a", map(vec![kv("b", map(vec![c("y")]))])),
                ],
                Some("x"),
            ),
            (
                vec![
                    kv("a.b", map(vec![c("x")])),
                    kv("a", map(vec![kv("b.c", string("y"))])),
                ],
                Some("x"),
            ),
            (vec![kv("a", map(vec![kv("b.c", string("y"))]))], Some("y")),
            (
                vec![kv("a", map(vec![kv("b", map(vec![c("x")]))]))],
                Some("x"),
            ),
            (vec![kv("a", string("s"))], None),
            (
                vec![kv(
                    "a",
                    Value::ArrayValue(ourios_core::otlp::ArrayValue {
                        values: vec![AnyValue {
                            value: Some(map(vec![kv("b", map(vec![c("x")]))])),
                        }],
                    }),
                )],
                None,
            ),
        ];
        for (attrs, resolves_to) in cases {
            let json = stored(&attrs);
            for v in ["x", "y", "s"] {
                let expected = canonical_value(string(v)).expect("encode");
                let run = |eq| Cmp::new("a.b.c", &expected, eq).eval(&json).expect("eval");
                let want = resolves_to.map_or((false, false), |r| (r == v, r != v));
                assert_eq!((run(true), run(false)), want, "{v} in {json}");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: ourios_testgen::proptest_cases(32),
            ..ProptestConfig::default()
        })]

        /// Scenario RFC0062.5 — the UDF agrees with a reference resolver.
        /// See `docs/rfcs/0062-attribute-map-paths.md` §5.
        #[test]
        fn rfc0062_5_the_udf_agrees_with_a_reference_resolver(
            record in adversarial(),
            v in "[a-z]{0,2}",
        ) {
            let json = stored(&record.attributes);
            let decoded = canonical::decode_attributes(json.as_bytes()).expect("decode");
            let mut keys = BTreeSet::new();
            keys_and_paths(&decoded, "", &mut keys);
            for key in keys {
                let actual = resolve_decoded(&decoded, &key);
                let mut values = vec![canonical_value(string(&v)).expect("encode")];
                // A query literal is a string or an integer.
                values.extend(actual.clone().filter(|a| {
                    a.starts_with("{\"stringValue\":") || a.starts_with("{\"intValue\":")
                }));
                for expected in values {
                    let type_prefix = expected.find(':').map_or(expected.as_str(), |i| &expected[..=i]);
                    let oracle = actual.as_deref().map_or((false, false), |a| {
                        (a == expected, a.starts_with(type_prefix) && a != expected)
                    });
                    let got = (
                        Cmp::new(&key, &expected, true).eval(&json).expect("eval"),
                        Cmp::new(&key, &expected, false).eval(&json).expect("eval"),
                    );
                    prop_assert_eq!(got, oracle, "key {:?} expected {} in {}", key, expected, json);
                    // The one-pass resolution of a key of many parts agrees too.
                    let wide = (
                        Cmp::new(&key, &expected, true).without_table().eval(&json).expect("eval"),
                        Cmp::new(&key, &expected, false).without_table().eval(&json).expect("eval"),
                    );
                    prop_assert_eq!(wide, oracle, "one pass: key {:?} expected {} in {}", key, expected, json);
                    // The `==` prefilter of a dotted key (`json_arm`) keeps every match.
                    let value_tail = format!("{}}}", &expected[type_prefix.len()..]);
                    prop_assert!(!got.0 || json.contains(&value_tail), "{} drops {}", value_tail, json);
                }
            }
        }
    }
}
