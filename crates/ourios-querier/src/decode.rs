//! Result decoding — aggregate/count batches into Ourios result types
//! (epic #745 wave 1; moved verbatim from the crate root).

// Split from the crate root (epic #745 wave 1); the parent scope is
// the import surface so every pre-split `crate::X` path resolves
// unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Pull the single aggregate count out of the result batches. A
/// `COUNT(*)` with no grouping always returns exactly one
/// `Int64` row; anything else means the plan/return-type changed
/// out from under us, so it's a surfaced error rather than a
/// silent (and wrong) zero.
pub(super) fn count_value(batches: &[RecordBatch]) -> Result<u64, QueryError> {
    let bad = |detail: String| QueryError::Storage {
        detail: format!("count aggregate: {detail}"),
    };
    if batches.len() != 1 {
        return Err(bad(format!(
            "expected exactly 1 result batch, got {}",
            batches.len(),
        )));
    }
    let batch = &batches[0];
    if batch.num_rows() != 1 || batch.num_columns() != 1 {
        return Err(bad(format!(
            "expected exactly 1 row × 1 column, got {}×{}",
            batch.num_rows(),
            batch.num_columns(),
        )));
    }
    let col = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| bad("count column is not Int64".to_string()))?;
    if col.is_null(0) {
        return Err(bad("count is null".to_string()));
    }
    u64::try_from(col.value(0)).map_err(|_| bad("negative count".to_string()))
}

/// The empty result for a query that provably scans nothing, shaped for its
/// stage set: a plain query is all-zero; an aggregation query carries the
/// map the engine would produce over an empty scan — no groups when
/// grouped, the single zero-count total for a bare `count`.
pub(super) fn empty_result(aggregate: Option<&plan::Aggregate>) -> QueryResult {
    let aggregate = aggregate.map(|agg| {
        if agg.by.is_empty() {
            vec![AggregateGroup {
                key: Vec::new(),
                count: 0,
                // A bare scalar over an empty scan is NULL (`Some(None)`); a
                // bare `count` has no scalar (`None`).
                value: agg.scalar.is_some().then_some(None),
            }]
        } else {
            Vec::new()
        }
    });
    QueryResult {
        aggregate,
        ..QueryResult::default()
    }
}

/// [`decode_aggregate`]'s output: the group map, the total matching-row
/// count (included + excluded — the same total a plain count would report),
/// and the excluded-row tally (RFC0002.15).
pub(super) struct DecodedAggregate {
    pub(super) groups: Vec<AggregateGroup>,
    pub(super) rows: u64,
    pub(super) excluded: u64,
}

/// Decode the grouped-count batches into [`AggregateGroup`]s: the key
/// columns are `group_0..group_{n-1}` ([`plan::group_column_name`]), the
/// count column [`plan::COUNT_COLUMN`]. A row whose key carries a NULL
/// (a short/NULL `param(n)` slot, an absent OPTIONAL field column)
/// contributes to no group and lands in the excluded tally instead — the
/// §6.3 amendment disposition, with no synthetic "absent" key. Groups are
/// sorted by key for a deterministic result.
pub(super) fn decode_aggregate(
    batches: &[RecordBatch],
    n_terms: usize,
    has_value: bool,
) -> Result<DecodedAggregate, QueryError> {
    use datafusion::arrow::array::Float64Array;
    let bad = |detail: String| QueryError::Storage {
        detail: format!("aggregate: {detail}"),
    };
    let mut groups = Vec::new();
    let mut rows: u64 = 0;
    let mut excluded: u64 = 0;
    for batch in batches {
        let counts = batch
            .column_by_name(plan::COUNT_COLUMN)
            .ok_or_else(|| bad("result is missing the count column".to_string()))?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| bad("count column is not Int64".to_string()))?;
        // The scalar-aggregate column is present iff the stage carried a
        // sum/min/max/avg; a group whose values were all NULL (every cast
        // failed) aggregates to NULL ⇒ `value: None` for that group.
        let values = if has_value {
            Some(
                batch
                    .column_by_name(plan::VALUE_COLUMN)
                    .ok_or_else(|| bad("result is missing the value column".to_string()))?
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or_else(|| bad("value column is not Float64".to_string()))?,
            )
        } else {
            None
        };
        let key_columns = (0..n_terms)
            .map(|i| {
                let name = plan::group_column_name(i);
                batch
                    .column_by_name(&name)
                    .ok_or_else(|| bad(format!("result is missing group column `{name}`")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for row in 0..batch.num_rows() {
            if counts.is_null(row) {
                return Err(bad("count is null".to_string()));
            }
            let count =
                u64::try_from(counts.value(row)).map_err(|_| bad("negative count".to_string()))?;
            rows = rows
                .checked_add(count)
                .ok_or_else(|| bad("total row count overflows u64".to_string()))?;
            // `None` for any cell ⇒ the whole key is NULL-bearing ⇒ the
            // row's count lands in the excluded tally, not in a group.
            let key = key_columns
                .iter()
                .map(|column| group_key_string(column.as_ref(), row))
                .collect::<Result<Option<Vec<String>>, QueryError>>()?;
            match key {
                Some(key) => {
                    // `None` ⇒ no scalar requested; `Some(None)` ⇒ scalar is
                    // NULL; `Some(Some(v))` ⇒ a finite value. A non-finite
                    // result (NaN/±inf — from a crafted `"NaN"`/`"inf"` input,
                    // or `sum` overflow) is degraded to NULL: JSON cannot
                    // represent it, so serializing it would 500 the whole query
                    // (`serde_json`). NULL matches the RFC0002.18 skip.
                    let value = values.map(|v| {
                        (!v.is_null(row))
                            .then(|| v.value(row))
                            .filter(|x| x.is_finite())
                    });
                    groups.push(AggregateGroup { key, count, value });
                }
                None => {
                    excluded = excluded
                        .checked_add(count)
                        .ok_or_else(|| bad("excluded row count overflows u64".to_string()))?;
                }
            }
        }
    }
    groups.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(DecodedAggregate {
        groups,
        rows,
        excluded,
    })
}

/// Render one group-key cell as its result-map string form (RFC 0002 §6.3
/// amendment): `None` for NULL (the excluded disposition), the stored
/// UTF-8 bytes for the `params` extraction, RFC 3339 UTC for timestamps
/// (the `bucket(…)` window start), hex for the fixed-size id columns, and
/// the natural rendering for the scalar column types. The type set is
/// exactly what [`plan::group_exprs`] can emit; anything else is a plan
/// contract drift, surfaced rather than guessed at.
pub(super) fn group_key_string(
    array: &dyn Array,
    row: usize,
) -> Result<Option<String>, QueryError> {
    use datafusion::arrow::array::{
        BinaryArray, BinaryViewArray, BooleanArray, FixedSizeBinaryArray, Float32Array,
        Float64Array, LargeBinaryArray, LargeStringArray, StringArray, StringViewArray,
        TimestampNanosecondArray, UInt8Array, UInt32Array, UInt64Array,
    };
    use datafusion::arrow::datatypes::{DataType, TimeUnit};

    if array.is_null(row) {
        return Ok(None);
    }
    let bad = || QueryError::Storage {
        detail: format!(
            "count aggregate: group column has unsupported type {:?}",
            array.data_type(),
        ),
    };
    let cell = |s: String| Ok(Some(s));
    macro_rules! typed {
        ($ty:ty) => {
            array.as_any().downcast_ref::<$ty>().ok_or_else(bad)?
        };
    }
    match array.data_type() {
        DataType::Null => Ok(None),
        DataType::Utf8 => cell(typed!(StringArray).value(row).to_string()),
        DataType::LargeUtf8 => cell(typed!(LargeStringArray).value(row).to_string()),
        DataType::Utf8View => cell(typed!(StringViewArray).value(row).to_string()),
        // The stored string form (§6.3): params are written from UTF-8
        // strings, so lossy decoding is exact for Ourios-written rows and
        // never fails on a foreign/degraded file.
        DataType::Binary => {
            cell(String::from_utf8_lossy(typed!(BinaryArray).value(row)).into_owned())
        }
        DataType::LargeBinary => {
            cell(String::from_utf8_lossy(typed!(LargeBinaryArray).value(row)).into_owned())
        }
        DataType::BinaryView => {
            cell(String::from_utf8_lossy(typed!(BinaryViewArray).value(row)).into_owned())
        }
        DataType::FixedSizeBinary(_) => {
            use std::fmt::Write as _;
            let mut hex = String::new();
            for byte in typed!(FixedSizeBinaryArray).value(row) {
                let _ = write!(hex, "{byte:02x}");
            }
            cell(hex)
        }
        DataType::Boolean => cell(typed!(BooleanArray).value(row).to_string()),
        DataType::UInt8 => cell(typed!(UInt8Array).value(row).to_string()),
        DataType::UInt32 => cell(typed!(UInt32Array).value(row).to_string()),
        DataType::UInt64 => cell(typed!(UInt64Array).value(row).to_string()),
        DataType::Int64 => cell(typed!(Int64Array).value(row).to_string()),
        DataType::Float32 => cell(typed!(Float32Array).value(row).to_string()),
        DataType::Float64 => cell(typed!(Float64Array).value(row).to_string()),
        // RFC 3339 UTC — the §6.3 bucket-key serialisation (subseconds
        // rendered only when present).
        DataType::Timestamp(TimeUnit::Nanosecond, _) => cell(
            chrono::DateTime::from_timestamp_nanos(typed!(TimestampNanosecondArray).value(row))
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        ),
        _ => Err(bad()),
    }
}
