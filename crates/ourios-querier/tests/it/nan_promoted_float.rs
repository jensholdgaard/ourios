//! OTLP fidelity for NaN in a promoted `Float64` attribute column.
//!
//! Parquet float min/max statistics leave NaN out, so a column chunk of
//! one finite value plus NaNs has `min == max` and no nulls. With
//! `execution.collect_statistics` on, `DataFusion` treats that column as
//! constant and returns the statistic for the NaN rows
//! (apache/datafusion#26490). The querier keeps statistics collection off;
//! this test pins the behaviour itself, so a `DataFusion` upgrade or a
//! session change that brings the substitution back fails here. This is
//! the behaviour test of RFC0021.10.

use ourios_parquet::{PromotedAttributes, PromotedClass, PromotedKey};

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, kv, rec_with_attrs, write_all_with_promoted};
use ourios_core::otlp::any_value::Value as AvValue;
use ourios_core::otlp::{AnyValue, KeyValue};

fn cost(value: f64) -> KeyValue {
    KeyValue {
        key: "cost_usd".to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::DoubleValue(value)),
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_nan_in_a_promoted_float_column_is_never_read_as_the_statistics_constant() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let records: Vec<_> = [1.5, f64::NAN, 1.5]
        .into_iter()
        .enumerate()
        .map(|(i, value)| {
            rec_with_attrs(
                "a",
                TS0 + i as u64 * 1_000,
                vec![kv("service.name", "agent")],
                vec![cost(value)],
            )
        })
        .collect();
    let promoted = PromotedAttributes::new_typed(
        [],
        [PromotedKey {
            key: "cost_usd".into(),
            class: PromotedClass::F64,
        }],
    );
    write_all_with_promoted(bucket.path(), &records, &promoted);

    let querier = ourios_querier::Querier::new(bucket.path()).with_promoted_attributes(promoted);
    let run = |dsl: &'static str| {
        let querier = &querier;
        async move {
            let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
            querier
                .run_query(
                    &query,
                    &ourios_core::tenant::TenantId::new("a"),
                    NOW,
                    DEFAULT_WINDOW_NS,
                    Some(&crate::common::no_aliases()),
                )
                .await
                .expect("query")
        }
    };

    // Arrow orders NaN above every number, so `< 2.0` matches the two
    // finite rows. Read as the statistic, the NaN row would match too.
    let below = run("attr.cost_usd < 2.0 | count").await;
    assert_eq!(
        below.rows, 2,
        "the NaN row must not compare as 1.5; 3 means the column was read as the \
         min == max statistic (apache/datafusion#26490)",
    );

    // A non-finite aggregate is reported as NULL (JSON has no NaN). Read as
    // the statistic, the sum would be a finite 4.5.
    let sum = run("template_id == 1 | sum(attr.cost_usd)").await;
    let group = &sum.aggregate.as_ref().expect("aggregate map")[0];
    assert_eq!(group.count, 3);
    assert_eq!(
        group.value.flatten(),
        None,
        "the NaN row makes the sum non-finite, reported as NULL",
    );
}
