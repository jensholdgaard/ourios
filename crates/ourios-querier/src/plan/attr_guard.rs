//! RFC 0069 §3.2's predicate guard: a comparison over an attribute cell
//! that does not decode is SQL `NULL`, so neither it nor its negation
//! matches the row, and the query does not fail.
//!
//! [`attr_cmp`](super::attr_cmp) answers `NULL` itself for such a cell. Its
//! `LIKE` prefilter does not: a cell the prefilter rules out is `false`
//! before the comparison runs, and `not` would turn that into a match. Under
//! an odd number of `not`s, [`guard_negations`] therefore makes the whole
//! JSON arm `NULL` for a cell that does not decode. Elsewhere `NULL` and
//! `false` filter out the same rows, so the arm stays as compiled and only
//! the rows the prefilter keeps are decoded.

use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{Array, AsArray, BooleanBuilder};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::common::{Result, ScalarValue, exec_err};
use datafusion::logical_expr::{
    BinaryExpr, ColumnarValue, Expr, Operator, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl,
    Signature, Volatility, when,
};
use datafusion::prelude::lit;
use ourios_core::otlp::canonical;

use super::attr_value;

const NAME: &str = "ourios_attrs_decodable";

static UDF: LazyLock<Arc<ScalarUDF>> = LazyLock::new(|| {
    Arc::new(ScalarUDF::from(Decodable {
        signature: Signature::any(1, Volatility::Immutable),
    }))
});

/// Whether a stored attribute cell decodes, as the reader decodes it.
pub(super) fn decodable(json: &str) -> bool {
    json == "[]" || canonical::decode_attributes(json.as_bytes()).is_ok()
}

/// `expr` with every JSON-arm comparison under an odd number of `not`s
/// made `NULL` for an attribute cell that does not decode.
pub(super) fn guard_negations(expr: Expr) -> Result<Expr> {
    guard(expr, false)
}

fn guard(expr: Expr, negated: bool) -> Result<Expr> {
    match expr {
        Expr::Not(inner) => Ok(Expr::Not(Box::new(guard(*inner, !negated)?))),
        Expr::BinaryExpr(BinaryExpr {
            left,
            op: Operator::And,
            right,
        }) if negated && attr_value::is_attr_cmp(&right) => guarded(*left, *right),
        other => other
            .map_children(|child| guard(child, negated).map(Transformed::yes))
            .map(|transformed| transformed.data),
    }
}

/// `prefilter AND cmp` as `CASE WHEN prefilter THEN cmp WHEN <cell
/// decodes> THEN false END`: `NULL` for a cell that does not decode,
/// whichever branch it reaches.
fn guarded(prefilter: Expr, cmp: Expr) -> Result<Expr> {
    let Some(cell) = attr_value::cell_of(&cmp) else {
        return Ok(prefilter.and(cmp));
    };
    when(prefilter, cmp)
        .when(UDF.call(vec![cell]), lit(false))
        .end()
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Decodable {
    signature: Signature,
}

impl ScalarUDFImpl for Decodable {
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
        let [json] = args.args.as_slice() else {
            return exec_err!("{NAME} takes one column");
        };
        let array = match json {
            ColumnarValue::Scalar(json) => {
                let answer = json.try_as_str().flatten().map(decodable);
                return Ok(ColumnarValue::Scalar(ScalarValue::Boolean(answer)));
            }
            ColumnarValue::Array(array) => array,
        };
        let mut out = BooleanBuilder::with_capacity(array.len());
        match array.data_type() {
            DataType::Utf8 => fill(array.as_string::<i32>().iter(), &mut out),
            DataType::LargeUtf8 => fill(array.as_string::<i64>().iter(), &mut out),
            DataType::Utf8View => fill(array.as_string_view().iter(), &mut out),
            other => return exec_err!("{NAME} takes a string column, got {other}"),
        }
        Ok(ColumnarValue::Array(Arc::new(out.finish())))
    }
}

fn fill<'s>(rows: impl Iterator<Item = Option<&'s str>>, out: &mut BooleanBuilder) {
    for row in rows {
        out.append_option(row.map(decodable));
    }
}
