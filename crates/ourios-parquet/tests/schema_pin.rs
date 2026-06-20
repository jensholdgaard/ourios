//! Scenario RFC0005.10 — Schema declaration is greppable and immutable.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Holds the expected `(name, data_type, nullable)` field list for
//! both `data_schema()` and `audit_schema()` as a hand-written
//! fixture and asserts equality against the production schema.
//! Adding a column to `lib.rs` without updating the fixture (and,
//! by implication, RFC 0005's §3.2 / §3.7 tables) fails this test
//! — the "schema-as-spec" pin RFC 0005 §5 / §6 names.
//!
//! Deliberate duplication: the fixture mirrors `lib.rs`'s schema
//! shape line-for-line. The point is that the duplication forces
//! a developer adding a column to touch both sides, which surfaces
//! the schema change to reviewers as an explicit RFC-amendment
//! signal rather than a quiet implementation tweak.

use std::sync::Arc;

use arrow_schema::{DataType, Field, SchemaRef, TimeUnit};
use ourios_parquet::{audit_schema, data_schema};

fn check_schema_against(expected: &[Field], schema: &SchemaRef) {
    assert_eq!(
        schema.fields().len(),
        expected.len(),
        "field count mismatch — RFC 0005 amendment required to add or remove columns",
    );
    for (i, expected_field) in expected.iter().enumerate() {
        let actual = schema.field(i);
        assert_eq!(
            actual.name(),
            expected_field.name(),
            "field {i} name mismatch (expected {:?}, got {:?})",
            expected_field.name(),
            actual.name(),
        );
        assert_eq!(
            actual.data_type(),
            expected_field.data_type(),
            "field {i} ({}) data_type mismatch",
            expected_field.name(),
        );
        assert_eq!(
            actual.is_nullable(),
            expected_field.is_nullable(),
            "field {i} ({}) nullability mismatch — REQUIRED ↔ OPTIONAL flip is an RFC 0005 §3.8 contract change",
            expected_field.name(),
        );
    }
}

fn utc() -> Arc<str> {
    "UTC".into()
}

fn params_field() -> Field {
    let element = Field::new(
        "element",
        DataType::Struct(
            vec![
                Field::new("type_tag", DataType::Int32, false),
                Field::new("value", DataType::Binary, true),
            ]
            .into(),
        ),
        false,
    );
    Field::new("params", DataType::List(Arc::new(element)), false)
}

fn separators_field() -> Field {
    let element = Field::new("element", DataType::Binary, false);
    Field::new("separators", DataType::List(Arc::new(element)), false)
}

fn positions_widened_field() -> Field {
    let element = Field::new("element", DataType::Int32, false);
    // OPTIONAL since the §3.7 amendment (NULL on compaction rows).
    Field::new("positions_widened", DataType::List(Arc::new(element)), true)
}

fn slots_expanded_field() -> Field {
    let types_added_element = Field::new("element", DataType::Int32, false);
    let slot_expansion = DataType::Struct(
        vec![
            Field::new("slot_index", DataType::Int32, false),
            Field::new(
                "types_added",
                DataType::List(Arc::new(types_added_element)),
                false,
            ),
        ]
        .into(),
    );
    let element = Field::new("element", slot_expansion, false);
    // OPTIONAL since the §3.7 amendment (NULL on compaction rows).
    Field::new("slots_expanded", DataType::List(Arc::new(element)), true)
}

/// Scenario RFC0005.10 — data-file schema half.
#[test]
fn rfc0005_10_data_schema_matches_pinned_field_list() {
    let expected = vec![
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new("template_id", DataType::UInt64, false),
        Field::new("template_version", DataType::UInt32, false),
        Field::new(
            "time_unix_nano",
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc())),
            false,
        ),
        Field::new(
            "observed_time_unix_nano",
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc())),
            true,
        ),
        // OPTIONAL, writer-derived (RFC 0005 §3.2 amendment
        // 2026-06-11); absent only in pre-amendment files.
        Field::new(
            "effective_time_unix_nano",
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc())),
            true,
        ),
        Field::new("severity_number", DataType::UInt8, false),
        Field::new("severity_text", DataType::Utf8, true),
        Field::new("scope_name", DataType::Utf8, true),
        Field::new("scope_version", DataType::Utf8, true),
        Field::new("attributes", DataType::Utf8, false),
        Field::new("dropped_attributes_count", DataType::UInt32, false),
        Field::new("resource_attributes", DataType::Utf8, false),
        Field::new("trace_id", DataType::FixedSizeBinary(16), true),
        Field::new("span_id", DataType::FixedSizeBinary(8), true),
        Field::new("flags", DataType::UInt32, false),
        Field::new("event_name", DataType::Utf8, true),
        Field::new("body_kind", DataType::UInt8, false),
        Field::new("body", DataType::Binary, true),
        params_field(),
        separators_field(),
        Field::new("confidence", DataType::Float32, false),
        Field::new("lossy_flag", DataType::Boolean, false),
        // RFC 0018 §3.1 — additive OPTIONAL columns.
        Field::new("scope_attributes", DataType::Utf8, true),
        Field::new("resource_schema_url", DataType::Utf8, true),
        Field::new("scope_schema_url", DataType::Utf8, true),
    ];
    check_schema_against(&expected, &data_schema());
}

/// Scenario RFC0005.10 — audit-event schema half.
#[test]
fn rfc0005_10_audit_schema_matches_pinned_field_list() {
    let expected = vec![
        Field::new("tenant_id", DataType::Utf8, false),
        Field::new(
            "timestamp",
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc())),
            false,
        ),
        Field::new("event_kind", DataType::UInt8, false),
        Field::new("event_type", DataType::Utf8, false),
        // Template columns: OPTIONAL since the §3.7 amendment
        // (required-by-convention for template kinds; NULL for
        // compaction).
        Field::new("template_id", DataType::UInt64, true),
        Field::new("old_version", DataType::UInt32, true),
        Field::new("new_version", DataType::UInt32, true),
        Field::new("old_template", DataType::Utf8, true),
        Field::new("new_template", DataType::Utf8, true),
        positions_widened_field(),
        slots_expanded_field(),
        Field::new("triggering_line_hash", DataType::FixedSizeBinary(16), true),
        Field::new("triggering_line_sample", DataType::Utf8, true),
        Field::new("reason", DataType::Utf8, true),
        // Compaction columns (RFC 0009 §3.6): OPTIONAL.
        Field::new("compaction_partition", DataType::Utf8, true),
        Field::new(
            "compaction_input_files",
            DataType::List(Arc::new(Field::new("element", DataType::Utf8, false))),
            true,
        ),
        Field::new("compaction_output_file", DataType::Utf8, true),
        Field::new("compaction_generation", DataType::UInt64, true),
        Field::new("compaction_rows", DataType::UInt64, true),
        // Alias columns (RFC 0001 §6.7 / RFC 0005 §3.7 amendment
        // 2026-06-12): OPTIONAL, NULL for all other kinds.
        Field::new("alias_representative_id", DataType::UInt64, true),
        Field::new(
            "alias_member_ids",
            DataType::List(Arc::new(Field::new("element", DataType::UInt64, false))),
            true,
        ),
        Field::new("alias_actor", DataType::Utf8, true),
    ];
    check_schema_against(&expected, &audit_schema());
}
