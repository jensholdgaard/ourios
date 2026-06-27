//! Compile + execute the RFC 0010 `drift` query over the RFC 0005 `audit/`
//! Parquet stream.
//!
//! This is the audit-stream sibling of [`crate::compile`]: it lowers a
//! [`DriftQuery`](crate::dsl::DriftQuery) IR to a `DataFusion` plan
//! (`Filter` → `Aggregate` → `Sort`, RFC 0010 §6.3/§6.6) and collects the
//! result into [`DriftRow`]s. Like the log path, no `datafusion`/`arrow`/SQL
//! type crosses a public boundary (hazard `CLAUDE.md` §4.6 / RFC0010.8) — the
//! SQL in RFC 0001 §6.7 / RFC 0010 §6.3 is anchored programmatically, never
//! exposed.
//!
//! Tenancy (RFC0010.4 / `CLAUDE.md` §3.7) is a prefix scope: the scan is scoped
//! to the executing tenant's `audit/tenant_id=<enc>/` prefix, so no other
//! tenant's events are reachable (RFC0019.5 — the listing is segment-wise
//! prefix-scoped). The window drives a day-granularity `year/month/day`
//! partition prune (RFC 0005 §3.4 — the audit layout has no `hour` segment),
//! then an exact `timestamp` predicate trims the boundary days to the half-open
//! `[from, to)` window (RFC 0010 §6.5). The listing — tenant prefix, day prune
//! — is the shared [`crate::audit_scan`], also used by the §3.7.1 alias-map
//! derivation.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use datafusion::arrow::array::{
    Array, Int64Array, TimestampNanosecondArray, UInt32Array, UInt64Array,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig};
use datafusion::functions_aggregate::expr_fn::{count, max, min};
use datafusion::prelude::{SessionContext, col, lit};

use ourios_core::audit::{EVENT_TYPE_TEMPLATE_TYPE_EXPANDED, EVENT_TYPE_TEMPLATE_WIDENED};
use ourios_core::tenant::TenantId;
use ourios_parquet::{Store, audit_columns};

use crate::dsl::DriftQuery;
use crate::{
    QueryError, QueryStats, audit_scan, audit_table_urls, scan_stats, storage_err,
    time_bound_scalar,
};

/// One drift row: a template that gained at least one version in the queried
/// window, with the §6.3 aggregates. The columns map one-to-one onto RFC 0010
/// §6.4 / RFC 0001 §6.7. Plain owned scalars — no `datafusion`/`arrow` type
/// (RFC0010.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftRow {
    /// The drifting leaf.
    pub template_id: u64,
    /// `COUNT(*)` of qualifying events for this template in the window.
    pub widening_count: u64,
    /// `MIN(old_version)` across the template's qualifying events.
    pub min_old_version: u32,
    /// `MAX(new_version)` across the template's qualifying events.
    pub max_new_version: u32,
    /// `MIN(timestamp)` — first qualifying event in the window.
    pub first_seen: SystemTime,
    /// `MAX(timestamp)` — last qualifying event in the window.
    pub last_seen: SystemTime,
}

/// Result of a drift query: the per-template [`DriftRow`]s (ordered by
/// `widening_count` descending, then `template_id` ascending — RFC 0010 §6.6)
/// plus the scan's pruning / IO accounting. An empty result is an empty `rows`
/// vector, never an error (RFC0010.5). Distinct from the log-query
/// `QueryResult` so the two result shapes can't be confused (RFC 0010 §6.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftResult {
    pub rows: Vec<DriftRow>,
    pub stats: QueryStats,
}

/// Column aliases for the §6.3 projection. Internal to this module — they name
/// the aggregate outputs the result decoder reads back.
const WIDENING_COUNT: &str = "widening_count";
const MIN_OLD_VERSION: &str = "min_old_version";
const MAX_NEW_VERSION: &str = "max_new_version";
const FIRST_SEEN: &str = "first_seen";
const LAST_SEEN: &str = "last_seen";

/// Execute the drift query against the tenant's audit stream in `store`,
/// resolving the window against `now_unix_nano`.
///
/// `local_root` is `Some` for the local backend (drift then addresses the audit
/// files by absolute local path) and `None` for S3 (it registers the store on
/// the `SessionContext` and addresses by object-store URL) — the hybrid scan of
/// RFC 0019 §3.3, mirroring the bulk log path.
///
/// `now_unix_nano` is the wall-clock reference the relative `from`/`to` bounds
/// (`-7d`, `now`) resolve against; the caller supplies it so execution is
/// deterministic and testable (mirroring [`crate::Querier::run_query`]).
pub(crate) async fn run_drift(
    store: &Store,
    local_root: Option<&Path>,
    query: &DriftQuery,
    tenant: &TenantId,
    now_unix_nano: u64,
) -> Result<DriftResult, QueryError> {
    let (start, end) = resolve_window(query, now_unix_nano)?;
    if start == end {
        // An empty half-open `[from, to)` window can hold no events, so the
        // result is empty without any audit-tree IO or DataFusion planning
        // (RFC0010.5).
        return Ok(DriftResult::default());
    }
    // Resolve the audit file set off the async runtime: the S3 branch's
    // `Store::list_blocking` (and the local `std::fs` walk) are blocking, so
    // offload them via `spawn_blocking` rather than tie up a runtime worker.
    // `Store` / `TenantId` / `PathBuf` are cheap to clone into the task.
    let files = {
        let store = store.clone();
        let tenant = tenant.clone();
        let local_root = local_root.map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || {
            audit_scan::audit_files(&store, local_root.as_deref(), &tenant, Some((start, end)))
        })
        .await
        .map_err(|e| QueryError::Storage {
            detail: format!("audit listing task: {e}"),
        })??
    };
    if files.is_empty() {
        // No audit files for the window ⇒ empty drift result, not an error
        // (RFC0010.5).
        return Ok(DriftResult::default());
    }

    let ctx = SessionContext::new();
    let urls = audit_table_urls(&ctx, store, &files)?;
    let options =
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");
    let config = ListingTableConfig::new_with_multi_paths(urls)
        .with_listing_options(options)
        .infer_schema(&ctx.state())
        .await
        .map_err(storage_err)?;
    let table = ListingTable::try_new(config).map_err(storage_err)?;
    ctx.register_table("audit", Arc::new(table))
        .map_err(storage_err)?;

    let base = ctx.table("audit").await.map_err(storage_err)?;

    // §6.3 step 1 — event_type IN ('template_widened', 'template_type_expanded')
    // AND timestamp in the half-open [start, end) window (RFC 0010 §6.5).
    let event_type = col(audit_columns::EVENT_TYPE);
    let widened = event_type.clone().eq(lit(EVENT_TYPE_TEMPLATE_WIDENED));
    let type_expanded = event_type.eq(lit(EVENT_TYPE_TEMPLATE_TYPE_EXPANDED));
    let ts = col(audit_columns::TIMESTAMP);
    let in_window = ts
        .clone()
        .gt_eq(lit(time_bound_scalar(start)?))
        .and(ts.lt(lit(time_bound_scalar(end)?)));
    let filtered = base
        .filter(widened.or(type_expanded).and(in_window))
        .map_err(storage_err)?;

    // §6.3 steps 2–3 — GROUP BY template_id, the five aggregates.
    let aggregated = filtered
        .aggregate(
            vec![col(audit_columns::TEMPLATE_ID)],
            vec![
                count(lit(1_i64)).alias(WIDENING_COUNT),
                min(col(audit_columns::OLD_VERSION)).alias(MIN_OLD_VERSION),
                max(col(audit_columns::NEW_VERSION)).alias(MAX_NEW_VERSION),
                min(col(audit_columns::TIMESTAMP)).alias(FIRST_SEEN),
                max(col(audit_columns::TIMESTAMP)).alias(LAST_SEEN),
            ],
        )
        .map_err(storage_err)?;

    // §6.3 step 4 — ORDER BY widening_count DESC, template_id ASC (RFC0010.6).
    let sorted = aggregated
        .sort(vec![
            col(WIDENING_COUNT).sort(false, false),
            col(audit_columns::TEMPLATE_ID).sort(true, false),
        ])
        .map_err(storage_err)?;

    let plan = sorted.create_physical_plan().await.map_err(storage_err)?;
    let batches = datafusion::physical_plan::collect(Arc::clone(&plan), ctx.task_ctx())
        .await
        .map_err(storage_err)?;
    let rows = decode_drift_rows(&batches)?;
    let stats = scan_stats(plan.as_ref());
    Ok(DriftResult { rows, stats })
}

/// Resolve the drift window's `[from, to)` bounds to nanoseconds, normalising
/// so the lower bound is the smaller of the two (an operator who writes the
/// bounds reversed still gets the window they meant).
fn resolve_window(query: &DriftQuery, now: u64) -> Result<(u64, u64), QueryError> {
    let from = crate::compile::resolve_time(&query.from, now)?;
    let to = crate::compile::resolve_time(&query.to, now)?;
    Ok((from.min(to), from.max(to)))
}

/// Decode the grouped+sorted aggregate batches into [`DriftRow`]s, preserving
/// the engine's row order (the §6.3 `Sort`). The schema is fixed by the
/// projection above (`template_id`, then the five named aggregates), so a
/// shape mismatch is a surfaced error, not a silent wrong answer.
fn decode_drift_rows(batches: &[RecordBatch]) -> Result<Vec<DriftRow>, QueryError> {
    let mut rows = Vec::new();
    for batch in batches {
        let template_id = u64_column(batch, audit_columns::TEMPLATE_ID)?;
        // `COUNT(*)` is `Int64` and non-negative; convert to the `u64` the
        // result row carries.
        let widening_count = i64_column(batch, WIDENING_COUNT)?;
        let min_old_version = u32_column(batch, MIN_OLD_VERSION)?;
        let max_new_version = u32_column(batch, MAX_NEW_VERSION)?;
        let first_seen = ts_column(batch, FIRST_SEEN)?;
        let last_seen = ts_column(batch, LAST_SEEN)?;
        for i in 0..batch.num_rows() {
            // The audit schema marks `template_id` / `old_version` /
            // `new_version` (and, defensively, the timestamp aggregates)
            // nullable. The drift filter selects only template events, which
            // carry these by convention, so a NULL here means a corrupted or
            // foreign audit file reached the aggregate — surface it rather
            // than decode NULL silently as `0`.
            if template_id.is_null(i)
                || min_old_version.is_null(i)
                || max_new_version.is_null(i)
                || first_seen.is_null(i)
                || last_seen.is_null(i)
            {
                return Err(QueryError::Storage {
                    detail: concat!(
                        "drift aggregate: NULL in a drift group's ",
                        "template_id / version / timestamp ",
                        "(corrupt or foreign audit file)"
                    )
                    .to_string(),
                });
            }
            let count =
                u64::try_from(widening_count.value(i)).map_err(|_| QueryError::Storage {
                    detail: "drift aggregate: widening_count is negative".to_string(),
                })?;
            rows.push(DriftRow {
                template_id: template_id.value(i),
                widening_count: count,
                min_old_version: min_old_version.value(i),
                max_new_version: max_new_version.value(i),
                first_seen: decode_ts(first_seen.value(i))?,
                last_seen: decode_ts(last_seen.value(i))?,
            });
        }
    }
    Ok(rows)
}

/// `template_id` is the `UInt64` group key; decode through this after a type
/// check — a non-`UInt64` column is a plan/schema drift, surfaced rather than
/// panicked.
fn u64_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array, QueryError> {
    typed_column(batch, name)
}

/// `COUNT(*)` returns an `Int64` array.
fn i64_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Int64Array, QueryError> {
    typed_column(batch, name)
}

fn u32_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt32Array, QueryError> {
    typed_column(batch, name)
}

fn ts_column<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a TimestampNanosecondArray, QueryError> {
    typed_column(batch, name)
}

/// Downcast a result column to a concrete array type, surfacing a `Storage`
/// error (not a panic) if the column is missing or the wrong type — a plan or
/// schema contract drift, the same shape the log path's `count_value` guards.
fn typed_column<'a, A: Array + 'static>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a A, QueryError> {
    let idx = batch
        .schema()
        .index_of(name)
        .map_err(|_| QueryError::Storage {
            detail: format!("drift aggregate: result is missing column `{name}`"),
        })?;
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| QueryError::Storage {
            detail: format!("drift aggregate: column `{name}` has an unexpected type"),
        })
}

/// Convert a non-negative nanos-since-epoch (the audit `timestamp` is
/// REQUIRED and non-negative on every Ourios-written row) to a [`SystemTime`].
fn decode_ts(nanos: i64) -> Result<SystemTime, QueryError> {
    let ns = u64::try_from(nanos).map_err(|_| QueryError::Storage {
        detail: "drift aggregate: a timestamp bound is negative (pre-epoch)".to_string(),
    })?;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_nanos(ns))
        .ok_or_else(|| QueryError::Storage {
            detail: "drift aggregate: a timestamp bound is out of range".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ir::Time;

    #[test]
    fn resolve_window_normalises_reversed_bounds() {
        // Arrange — `from` after `to`; the resolved window is still ordered.
        let now = 10_000 * 1_000_000_000;
        let q = DriftQuery {
            from: Time::Now,
            to: Time::Duration {
                neg: true,
                literal: "1h".into(),
            },
        };
        // Act
        let (lo, hi) = resolve_window(&q, now).unwrap();
        // Assert — lo is the earlier instant, hi the later (now).
        assert_eq!(hi, now);
        assert_eq!(lo, now - 3_600 * 1_000_000_000);
    }

    #[test]
    fn decode_drift_rows_rejects_a_null_group_key() {
        use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};

        // A one-row aggregate batch whose `template_id` group key is NULL —
        // the shape a corrupt/foreign audit file could yield. decode must
        // surface it, not silently decode NULL as `template_id = 0`.
        let schema = Arc::new(Schema::new(vec![
            Field::new(audit_columns::TEMPLATE_ID, DataType::UInt64, true),
            Field::new(WIDENING_COUNT, DataType::Int64, false),
            Field::new(MIN_OLD_VERSION, DataType::UInt32, true),
            Field::new(MAX_NEW_VERSION, DataType::UInt32, true),
            Field::new(
                FIRST_SEEN,
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                true,
            ),
            Field::new(
                LAST_SEEN,
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                true,
            ),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(vec![None::<u64>])),
                Arc::new(Int64Array::from(vec![1_i64])),
                Arc::new(UInt32Array::from(vec![Some(1_u32)])),
                Arc::new(UInt32Array::from(vec![Some(2_u32)])),
                Arc::new(TimestampNanosecondArray::from(vec![Some(0_i64)])),
                Arc::new(TimestampNanosecondArray::from(vec![Some(0_i64)])),
            ],
        )
        .expect("aggregate batch");

        match decode_drift_rows(&[batch]) {
            Err(QueryError::Storage { detail }) => {
                assert!(detail.contains("NULL"), "unexpected detail: {detail}");
            }
            other => panic!("expected a Storage NULL error, got {other:?}"),
        }
    }
}
