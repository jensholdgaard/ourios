//! RFC 0069 §3.2's predicate guard: a comparison over an attribute cell
//! the filter cannot read is SQL `NULL`, so neither it nor its negation
//! matches the row, and the query does not fail.
//!
//! [`attr_cmp`](super::attr_cmp) answers `NULL` itself for such a cell. Its
//! `LIKE` prefilter does not: a cell the prefilter rules out is `false`
//! before the comparison runs, and `not` would turn that into a match. Under
//! an odd number of `not`s, [`guard_negations`] therefore makes the whole
//! JSON arm `NULL` for a cell the filter cannot read. Elsewhere `NULL` and
//! `false` filter out the same rows, so the arm stays as compiled.

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

use super::attr_value;

const NAME: &str = "ourios_attrs_readable";

static UDF: LazyLock<Arc<ScalarUDF>> = LazyLock::new(|| {
    Arc::new(ScalarUDF::from(Readable {
        signature: Signature::any(1, Volatility::Immutable),
    }))
});

/// Whether the filter can read a stored attribute cell: a closed JSON
/// list, as every cell the encoder writes is. A full decode would cost
/// several times RFC0062.6's budget per row, so a cell corrupted inside
/// but still closed is read by its text (maintainer, 2026-11-23).
pub(super) fn readable(json: &str) -> bool {
    json.starts_with('[') && json.ends_with(']')
}

/// `expr` with every JSON-arm comparison under an odd number of `not`s
/// made `NULL` for an attribute cell the filter cannot read.
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

/// `prefilter AND cmp` as `CASE WHEN prefilter THEN cmp WHEN <cell is
/// readable> THEN false END`: `NULL` for a cell the filter cannot read,
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
struct Readable {
    signature: Signature,
}

impl ScalarUDFImpl for Readable {
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
                let answer = json.try_as_str().flatten().map(readable);
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
        out.append_option(row.map(readable));
    }
}

#[cfg(test)]
mod tests {
    use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
    use datafusion::logical_expr::{Expr, not};
    use datafusion::prelude::{col, lit};

    use super::guard_negations;
    use crate::plan::attr_value::attr_cmp;

    /// A JSON arm as `json_arm` compiles it: a prefilter `AND` the UDF.
    fn arm(key: &str) -> Expr {
        col("attributes").like(lit("%x%")).and(attr_cmp(
            col("attributes"),
            key,
            r#"{"stringValue":"x"}"#,
            true,
        ))
    }

    /// How many arms `guard_negations` guarded in `expr`.
    fn guards(expr: Expr) -> usize {
        let mut cases = 0;
        guard_negations(expr)
            .expect("guard")
            .apply(|node| {
                if matches!(node, Expr::Case(_)) {
                    cases += 1;
                }
                Ok(TreeNodeRecursion::Continue)
            })
            .expect("walk");
        cases
    }

    /// An arm is guarded exactly when an odd number of `not`s sit above it,
    /// through any `and` / `or` between them.
    #[test]
    fn only_arms_under_an_odd_number_of_nots_are_guarded() {
        assert_eq!(guards(arm("k")), 0, "a positive arm keeps its plan");
        assert_eq!(guards(not(arm("k"))), 1);
        assert_eq!(guards(not(not(arm("k")))), 0, "two nots cancel");
        assert_eq!(guards(not(not(not(arm("k"))))), 1);
        assert_eq!(guards(not(arm("k").or(arm("j")))), 2, "both under the not");
        assert_eq!(guards(not(arm("k").and(arm("j")))), 2);
        assert_eq!(
            guards(arm("k").and(not(arm("j")))),
            1,
            "only the negated arm"
        );
        assert_eq!(guards(not(lit(true).and(not(arm("k"))))), 0, "even again");
        assert_eq!(
            guards(not(col("p").is_null().and(arm("k")))),
            1,
            "a promoted fallback under its presence check"
        );
    }

    /// A cell is readable when it is a closed list, as the encoder writes
    /// every cell; a cell corrupted inside but still closed is the known
    /// gap the cheap check accepts.
    #[test]
    fn a_cell_is_readable_when_it_is_a_closed_list() {
        use super::readable;
        assert!(readable("[]"));
        assert!(readable(r#"[{"key":"k","value":{"stringValue":"x"}}]"#));
        assert!(
            !readable(r#"[{"key":"k","value":{"stringValue":"x"}}"#),
            "truncated"
        );
        assert!(!readable("not json"));
        assert!(!readable(r#"{"key":"k"}"#), "not a list");
        assert!(!readable(""));
        assert!(readable(r#"[{"key":"k",garbage]"#), "the accepted gap");
    }
}
