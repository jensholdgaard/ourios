//! `ourios_attr_value(json, key)`: the canonical JSON of the value of the
//! first top-level attribute named `key` in a canonical-JSON attribute
//! column, or `NULL` when there is none. The JSON arm of an attribute
//! comparison (RFC 0022 §3.3) confirms its `LIKE` candidates with it: the
//! substring alone also matches the same key/value pair nested inside
//! another attribute's map value. First occurrence wins, as in the
//! promoted-column projection (`promoted::project_string_value`).

use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{Array, AsArray, StringBuilder};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::{Result, ScalarValue, exec_err};
use datafusion::logical_expr::{
    ColumnarValue, Expr, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::prelude::lit;
use ourios_core::otlp::canonical;

const NAME: &str = "ourios_attr_value";

static UDF: LazyLock<Arc<ScalarUDF>> = LazyLock::new(|| {
    Arc::new(ScalarUDF::from(AttrValue {
        signature: Signature::any(2, Volatility::Immutable),
    }))
});

/// `ourios_attr_value(column, key)` as an expression.
pub(super) fn attr_value(column: Expr, key: &str) -> Expr {
    UDF.call(vec![column, lit(key)])
}

/// The canonical JSON [`attr_value`] yields for `value`, so a comparison
/// is byte-aligned with what the function returns.
pub(super) fn canonical_value(
    value: ourios_core::otlp::any_value::Value,
) -> Result<String, canonical::CanonicalJsonError> {
    let bytes = canonical::encode_any_value(&ourios_core::otlp::AnyValue { value: Some(value) })?;
    // The encoder emits `serde_json` output, which is always UTF-8.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct AttrValue {
    signature: Signature,
}

impl ScalarUDFImpl for AttrValue {
    fn name(&self) -> &str {
        NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [json, ColumnarValue::Scalar(key)] = args.args.as_slice() else {
            return exec_err!("{NAME} takes a column and a literal key");
        };
        let Some(Some(key)) = key.try_as_str() else {
            return exec_err!("{NAME} takes a string key");
        };
        let lookup = Lookup::new(key);
        let array = match json {
            ColumnarValue::Scalar(json) => {
                let value = json.try_as_str().flatten().and_then(|j| lookup.first(j));
                return Ok(ColumnarValue::Scalar(ScalarValue::Utf8(value)));
            }
            ColumnarValue::Array(array) => array,
        };
        let mut out = StringBuilder::with_capacity(array.len(), 0);
        match array.data_type() {
            DataType::Utf8 => lookup.fill(array.as_string::<i32>().iter(), &mut out),
            DataType::LargeUtf8 => lookup.fill(array.as_string::<i64>().iter(), &mut out),
            DataType::Utf8View => lookup.fill(array.as_string_view().iter(), &mut out),
            other => return exec_err!("{NAME} takes a string column, got {other}"),
        }
        Ok(ColumnarValue::Array(Arc::new(out.finish())))
    }
}

struct Lookup<'a> {
    key: &'a str,
    /// `"key":"<key>"` as the canonical encoder writes it. A row without
    /// it cannot hold the key, so it skips the decode.
    needle: String,
}

impl<'a> Lookup<'a> {
    fn new(key: &'a str) -> Self {
        let quoted = serde_json::to_string(key).unwrap_or_default();
        Self {
            key,
            needle: format!("\"key\":{quoted}"),
        }
    }

    fn fill<'s>(&self, rows: impl Iterator<Item = Option<&'s str>>, out: &mut StringBuilder) {
        for row in rows {
            out.append_option(row.and_then(|json| self.first(json)));
        }
    }

    /// A row that does not decode yields `NULL`: it matches nothing, as a
    /// row without the key.
    fn first(&self, json: &str) -> Option<String> {
        if !json.contains(&self.needle) {
            return None;
        }
        let attrs = canonical::decode_attributes(json.as_bytes()).ok()?;
        let value = attrs.into_iter().find(|kv| kv.key == self.key)?.value?;
        let bytes = canonical::encode_any_value(&value).ok()?;
        String::from_utf8(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use ourios_core::otlp::any_value::Value;
    use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};

    use super::{Lookup, canonical, canonical_value};

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

    #[test]
    fn the_first_top_level_occurrence_wins() {
        let json = stored(&[kv("k", string("a")), kv("k", string("x"))]);
        assert_eq!(
            Lookup::new("k").first(&json),
            Some(canonical_value(string("a")).expect("encode"))
        );
    }

    #[test]
    fn a_nested_pair_is_not_a_top_level_attribute() {
        let nested = Value::KvlistValue(KeyValueList {
            values: vec![kv("k", string("x"))],
        });
        let json = stored(&[kv("outer", nested)]);
        assert!(json.contains("{\"key\":\"k\",\"value\":{\"stringValue\":\"x\"}}"));
        assert_eq!(Lookup::new("k").first(&json), None);
    }

    #[test]
    fn a_map_valued_key_yields_the_map_not_its_entries() {
        let nested = Value::KvlistValue(KeyValueList {
            values: vec![kv("k", string("x"))],
        });
        let json = stored(&[kv("k", nested.clone())]);
        assert_eq!(
            Lookup::new("k").first(&json),
            Some(canonical_value(nested).expect("encode"))
        );
    }

    #[test]
    fn absent_and_undecodable_rows_are_null() {
        let lookup = Lookup::new("k");
        assert_eq!(lookup.first(&stored(&[kv("other", string("x"))])), None);
        assert_eq!(lookup.first("[{\"key\":\"k\",\"value\""), None);
    }
}
