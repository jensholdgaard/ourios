//! `ourios_attr_cmp(json, key, expected, eq)`: whether the first top-level
//! attribute named `key` in a canonical-JSON attribute column holds
//! `expected` (`eq`), or a different value of the same type (`!eq`; a
//! missing key never matches). This is the JSON arm of an attribute
//! comparison (RFC 0022 §3.3). A substring match alone also hits the same
//! key/value pair nested inside another attribute's map value. First
//! occurrence wins, as in the promoted-column projection
//! (`promoted::project_string_value`).

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
}

impl<'a> Cmp<'a> {
    fn new(key: &'a str, expected: &'a str, eq: bool) -> Self {
        let quoted = serde_json::to_string(key).unwrap_or_default();
        let type_prefix = expected.find(':').map_or(expected, |i| &expected[..=i]);
        Self {
            expected,
            type_prefix,
            eq,
            needle: format!("\"key\":{quoted}"),
            pair: format!("{{\"key\":{quoted},\"value\":{expected}}}"),
            typed: format!("{{\"key\":{quoted},\"value\":{type_prefix}"),
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
    /// string values, so the text there answers. Otherwise the row is
    /// scanned for its first top-level occurrence.
    fn eval(&self, json: &str) -> Result<bool> {
        let Some(at) = json.find(&self.needle) else {
            return Ok(false);
        };
        if !json[..at].contains("\"kvlistValue\"") {
            // `pair` and `typed` open with the `{` before the needle.
            let rest = &json[at..];
            let is = |text: &str| rest.starts_with(&text[1..]);
            return Ok(if self.eq {
                is(&self.pair)
            } else {
                is(&self.typed) && !is(&self.pair)
            });
        }
        let Some(value) = self.first_top_level(json)? else {
            return Ok(false);
        };
        Ok(if self.eq {
            value == self.expected
        } else {
            value.starts_with(self.type_prefix) && value != self.expected
        })
    }

    /// The stored text of the first top-level `key`'s value, which is
    /// what [`canonical::encode_any_value`] writes for it. A top-level
    /// pair's members sit at depth 2 (array, then object). The scan keeps
    /// no stack, so value depth is no limit. A row that ends early fails
    /// the query rather than drop out of the result.
    fn first_top_level<'j>(&self, json: &'j str) -> Result<Option<&'j str>> {
        let b = json.as_bytes();
        let mut depth = 0usize;
        for token in Tokens::new(b, 0) {
            let (i, _, c) = token?;
            match c {
                b'"' if depth == 2 && b[i..].starts_with(self.needle.as_bytes()) => {
                    return value_after(json, i + self.needle.len());
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

    /// The index after the string that opens at `b[i]`.
    fn string_end(&self, i: usize) -> Result<usize> {
        let mut j = i + 1;
        loop {
            match self.b.get(j) {
                Some(b'\\') => j += 2,
                Some(b'"') => return Ok(j + 1),
                Some(_) => j += 1,
                None => return Err(truncated()),
            }
        }
    }
}

impl Iterator for Tokens<'_> {
    type Item = Result<(usize, usize, u8)>;

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.i;
        let c = *self.b.get(i)?;
        let end = if c == b'"' {
            self.string_end(i)
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
    use ourios_core::otlp::any_value::Value;
    use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};

    use ourios_testgen::strategies::adversarial;
    use proptest::prelude::*;

    use super::{Cmp, canonical, canonical_value};

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
                let actual = decoded
                    .iter()
                    .find(|kv| kv.key == key)
                    .and_then(|kv| kv.value.as_ref())
                    .map(|v| String::from_utf8(canonical::encode_any_value(v).expect("encode")).expect("utf-8"));
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
}
