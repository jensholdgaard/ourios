//! The DSL → plan seam for the `fuzz/` cargo-fuzz `dsl_parse` target
//! (RFC 0015, RFC0015.15). Behind the `fuzzing` feature; not part of the
//! stable public API.
//!
//! [`lower`] runs what [`Querier::run_query`](crate::Querier::run_query)
//! does before its first IO: `plan::compile`, `plan::apply` and, for an
//! aggregation stage, the grouped `Aggregate` node, over an empty in-memory
//! table with a scan's schema, then `DataFusion`'s analyzer and optimizer.
//! Nothing is executed. The alias map and template registry are empty (the
//! store-derived inputs a real query would fold first), and the clock is
//! fixed so the window resolves the same way on every call.

use std::sync::{Arc, LazyLock};

use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::file_format::parquet::transform_schema_to_view;
use datafusion::logical_expr::LogicalPlan;
use datafusion::prelude::SessionContext;
use ourios_core::alias::AliasMap;
use ourios_core::tenant::TenantId;
use ourios_parquet::{PromotedAttributes, PromotedClass, PromotedKey};

use crate::dsl::Query;
use crate::{QueryError, TemplateRegistry, plan, schema_adapt, storage_err};

/// The fixed query clock: 2026-10-11T00:00:00Z.
pub const NOW_UNIX_NANO: u64 = 1_791_676_800_000_000_000;

/// The default window a query without `range(...)` gets: one hour.
pub const DEFAULT_WINDOW_NANOS: u64 = 3_600_000_000_000;

/// Which scanned files the union schema stands for (RFC 0005 §3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanSchema {
    /// Files from the current writer under a promoted set with one key of
    /// each RFC 0042 class: `resource.deployment.environment` and
    /// `attr.user.id` (string), `attr.http.response.status_code` (`I64`)
    /// and `attr.duration` (`F64`), besides `resource.service.name`.
    Current,
    /// Files carrying only the required columns: every OPTIONAL column, the
    /// promoted ones included, is absent from the scan (RFC0007.4).
    RequiredOnly,
}

/// A lowered query's logical plan, opaque so no `DataFusion` type crosses
/// the crate boundary (hazard `CLAUDE.md` §4.6).
#[derive(Debug, PartialEq)]
pub struct LoweredPlan(LogicalPlan);

/// The window, limit and aggregation checks `run_query` runs before any IO.
///
/// # Errors
///
/// The [`QueryError`] those checks reject the query with.
pub fn validate(query: &Query) -> Result<(), QueryError> {
    plan::validate(query, NOW_UNIX_NANO, DEFAULT_WINDOW_NANOS).map(|_| ())
}

/// Lower `query` to its optimized logical plan against `schema`. `None` is a
/// query the compiler proves empty (a predicate that matches nothing).
///
/// # Errors
///
/// The [`QueryError`] compilation, lowering or `DataFusion`'s planning
/// rejects the query with.
pub fn lower(query: &Query, schema: ScanSchema) -> Result<Option<LoweredPlan>, QueryError> {
    let tenant = TenantId::new("fuzz-tenant");
    let compiled = plan::compile(
        query,
        &tenant,
        NOW_UNIX_NANO,
        DEFAULT_WINDOW_NANOS,
        &AliasMap::new(),
        &TemplateRegistry::new(),
        None,
    )?;
    let aggregate = compiled.aggregate.clone();
    let scan = SESSION
        .read_batch(RecordBatch::new_empty(scan_schema(schema)))
        .map_err(storage_err)?;
    let Some(filtered) = plan::apply(scan, compiled)? else {
        return Ok(None);
    };
    let frame = match &aggregate {
        Some(agg) => crate::aggregate_frame(filtered, agg, &tenant)?,
        None => filtered,
    };
    let optimized = frame.into_optimized_plan().map_err(storage_err)?;
    Ok(Some(LoweredPlan(optimized)))
}

static SESSION: LazyLock<SessionContext> = LazyLock::new(crate::session);

/// The union schema a listing-table scan of such files presents: the
/// writer's schema with `DataFusion`'s view types, then the querier's own
/// promoted-column merge.
fn scan_schema(schema: ScanSchema) -> Arc<Schema> {
    let promoted = PromotedAttributes::new_typed(
        [PromotedKey::string("deployment.environment")],
        [
            PromotedKey::string("user.id"),
            PromotedKey {
                key: "http.response.status_code".to_string(),
                class: PromotedClass::I64,
            },
            PromotedKey {
                key: "duration".to_string(),
                class: PromotedClass::F64,
            },
        ],
    );
    let written = ourios_parquet::data_schema_with_promoted(&promoted);
    let file = match schema {
        ScanSchema::Current => written.as_ref().clone(),
        ScanSchema::RequiredOnly => Schema::new(
            written
                .fields()
                .iter()
                .filter(|f| !f.is_nullable())
                .cloned()
                .collect::<Vec<_>>(),
        ),
    };
    let viewed = transform_schema_to_view(&file);
    match schema_adapt::merge_scanned_schemas(vec![viewed.clone()], &promoted) {
        Ok(merged) => Arc::new(merged),
        // One schema never conflicts with itself.
        Err(_) => Arc::new(viewed),
    }
}
