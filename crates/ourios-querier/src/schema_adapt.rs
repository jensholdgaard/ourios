//! RFC 0042 §3.3 — cross-file promoted-column type handling.
//!
//! Two seams keep a scan correct when files written under different
//! promoted declarations coexist:
//!
//! 1. [`merge_scanned_schemas`] builds the scan's union schema. A
//!    promoted column (`resource.<key>` / `attr.<key>`) whose key is in
//!    the **declared** set takes the declared class's type regardless of
//!    what any file carries; an undeclared promoted column that
//!    conflicts across files resolves to `Utf8` (the string class is
//!    the pre-RFC-0042 universal, and the §3.3 rule below makes the
//!    numeric files' cells read `NULL` under it). Non-promoted columns
//!    keep Arrow's own merge — a conflict there is RFC 0005 schema
//!    corruption and stays an error.
//! 2. [`PromotedNoCoercionFactory`] is the per-file physical-expression
//!    adapter: a promoted column whose file type differs from the scan
//!    schema's type is read as if the column were **absent** (a typed
//!    `NULL` literal). `DataFusion`'s default adapter would insert a cast
//!    instead — and Arrow's safe `Utf8 → Int64` cast *parses* string
//!    content, the exact coercion RFC 0042 §3.1/§3.3 forbids
//!    (projection must depend on the variant, never the value).
//!
//! Everything that is not a type-mismatched promoted column delegates to
//! the default adapter, so missing-column `NULL` filling and
//! non-promoted casts behave exactly as stock `DataFusion`.

use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::error::Result as DfResult;
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_expr::expressions::{Column, Literal};
use datafusion::physical_expr_adapter::{
    DefaultPhysicalExprAdapterFactory, PhysicalExprAdapter, PhysicalExprAdapterFactory,
};
use datafusion::scalar::ScalarValue;
use ourios_parquet::{PromotedAttributes, promoted};

/// Is this column name a promoted attribute column (RFC 0022 §3.1
/// naming: the DSL path, literally)?
fn is_promoted_name(name: &str) -> bool {
    name.starts_with(promoted::RESOURCE_PREFIX) || name.starts_with(promoted::ATTR_PREFIX)
}

/// The declared class's Arrow type for a promoted column name, if the
/// name is in the declared set.
fn declared_type(declared: &PromotedAttributes, name: &str) -> Option<DataType> {
    let key_class = |keys: &[ourios_parquet::PromotedKey], prefix: &str| {
        let key = name.strip_prefix(prefix)?;
        keys.iter()
            .find(|k| k.key == key)
            .map(|k| k.class.data_type())
    };
    key_class(declared.resource_keys(), promoted::RESOURCE_PREFIX)
        .or_else(|| key_class(declared.log_keys(), promoted::ATTR_PREFIX))
}

/// Build the scan's union schema across every scanned file's schema
/// (RFC0007.4 / RFC 0005 §3.9), applying the RFC 0042 §3.3 rules to
/// promoted columns. Field order is first appearance across the inputs,
/// matching `Schema::try_merge`.
pub(crate) fn merge_scanned_schemas(
    schemas: Vec<Schema>,
    declared: &PromotedAttributes,
) -> Result<Schema, String> {
    let mut order: Vec<String> = Vec::new();
    let mut fields: std::collections::HashMap<String, Vec<Field>> =
        std::collections::HashMap::new();
    for schema in schemas {
        for field in schema.fields() {
            let entry = fields.entry(field.name().clone()).or_insert_with(|| {
                order.push(field.name().clone());
                Vec::new()
            });
            entry.push(field.as_ref().clone());
        }
    }

    let mut merged = Vec::with_capacity(order.len());
    for name in order {
        let seen = fields.remove(&name).expect("field recorded in order");
        if is_promoted_name(&name) {
            let target = declared_type(declared, &name).unwrap_or_else(|| {
                let mut types = seen.iter().map(Field::data_type);
                let first = types.next().expect("at least one occurrence").clone();
                if types.all(|t| *t == first) {
                    first
                } else {
                    // Undeclared conflict: resolve to the string class
                    // (the pre-RFC-0042 universal); the §3.3 rule reads
                    // the non-matching files' cells as NULL.
                    DataType::Utf8
                }
            });
            // Promoted columns are OPTIONAL by construction (RFC 0022
            // §3.1); nullable also covers the mismatched-file NULLs.
            merged.push(Field::new(name, target, true));
        } else {
            // Non-promoted columns: Arrow's own merge semantics. A type
            // conflict here is RFC 0005 schema corruption — error, as
            // `try_merge` always did.
            let unioned = Schema::try_merge(seen.into_iter().map(|f| Schema::new(vec![f])))
                .map_err(|e| format!("merging scanned file schemas: {e}"))?;
            merged.push(unioned.fields()[0].as_ref().clone());
        }
    }
    Ok(Schema::new(merged))
}

/// The RFC 0042 §3.3 per-file expression adapter factory (installed via
/// `ListingTableConfig::with_expr_adapter_factory`).
#[derive(Debug)]
pub(crate) struct PromotedNoCoercionFactory;

impl PhysicalExprAdapterFactory for PromotedNoCoercionFactory {
    fn create(
        &self,
        logical_file_schema: SchemaRef,
        physical_file_schema: SchemaRef,
    ) -> DfResult<Arc<dyn PhysicalExprAdapter>> {
        let inner = DefaultPhysicalExprAdapterFactory.create(
            Arc::clone(&logical_file_schema),
            Arc::clone(&physical_file_schema),
        )?;
        Ok(Arc::new(PromotedNoCoercion {
            logical: logical_file_schema,
            physical: physical_file_schema,
            inner,
        }))
    }
}

/// See [`PromotedNoCoercionFactory`]. `logical` is the scan schema the
/// query addresses; `physical` is one file's own schema.
#[derive(Debug)]
struct PromotedNoCoercion {
    logical: SchemaRef,
    physical: SchemaRef,
    inner: Arc<dyn PhysicalExprAdapter>,
}

impl PhysicalExprAdapter for PromotedNoCoercion {
    fn rewrite(&self, expr: Arc<dyn PhysicalExpr>) -> DfResult<Arc<dyn PhysicalExpr>> {
        // First pass: replace references to type-mismatched promoted
        // columns with a typed NULL literal (the §3.3 "read as absent"
        // rule). Second pass: the default adapter handles everything
        // else (missing columns, benign casts, index remapping).
        let expr = expr
            .transform(|e| {
                let Some(column) = e.downcast_ref::<Column>() else {
                    return Ok(Transformed::no(e));
                };
                if !is_promoted_name(column.name()) {
                    return Ok(Transformed::no(e));
                }
                let (Ok(logical), Ok(physical)) = (
                    self.logical.field_with_name(column.name()),
                    self.physical.field_with_name(column.name()),
                ) else {
                    // Absent in the file: the default adapter's NULL
                    // fill is already the §3.3 behaviour.
                    return Ok(Transformed::no(e));
                };
                if logical.data_type() == physical.data_type() {
                    return Ok(Transformed::no(e));
                }
                let null = ScalarValue::Null.cast_to(logical.data_type())?;
                Ok(Transformed::yes(
                    Arc::new(Literal::new(null)) as Arc<dyn PhysicalExpr>
                ))
            })?
            .data;
        self.inner.rewrite(expr)
    }
}
