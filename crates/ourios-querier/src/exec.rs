//! Shared `DataFusion` execution seams (epic #745 wave 2): the one
//! plan→collect→stats→spans path every query takes, and the one
//! listing-table registration whose schema handling is an explicit,
//! reviewable argument instead of a per-call-site accident.

// Split from the crate root's call sites; the parent scope is the
// import surface so every referenced name resolves unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;
use ourios_parquet::PromotedAttributes;

/// How [`register_listing_table`] derives the table schema.
pub(crate) enum SchemaMode<'a> {
    /// Per-file inference merged to the **union** schema with the
    /// promoted no-coercion expression adapter — the data path.
    /// The union is load-bearing (RFC0007.4 / RFC 0005 §3.9, and
    /// since RFC 0022 for predicate compilation: `attr_match` gates
    /// the promoted-column arms on the post-union schema). A bare
    /// `ListingTableConfig::infer_schema` infers from the *first*
    /// table path only — with per-file URLs that is one arbitrary
    /// file, not the union — so this arm infers per file and merges.
    /// The extra footer reads are already paid: the Parquet format
    /// fetches every listed file's footer for statistics at plan
    /// time. RFC 0042 §3.3: a declared promoted key's class fixes
    /// its union-schema type; an undeclared promoted-column conflict
    /// resolves to Utf8; the per-file expression adapter reads a
    /// type-mismatched promoted column as absent (typed NULL) —
    /// `DataFusion`'s default adapter would cast, and Arrow's safe
    /// Utf8→Int64 cast *parses* string content, the coercion §3.3
    /// forbids.
    Union(&'a PromotedAttributes),
    /// Bare first-file inference — the audit path, whose files share
    /// one schema by construction (the RFC 0005 §3.7 writer). The
    /// first-file-only caveat above is why this arm is an explicit
    /// choice, not a default.
    Infer,
}

/// The session every query path runs on.
///
/// `execution.collect_statistics` is **off** deliberately (RFC 0021
/// §3.2a). With it on, `DataFusion` substitutes columns that per-file
/// statistics prove constant — and folding an all-NULL column to a
/// NULL literal collapses predicates like `body == "…"` to a bare
/// constant, which leaves no column reference for a pruning predicate
/// to be built over, so the row groups are scanned instead of skipped
/// (upstream apache/datafusion#24769, fix proposed in #24770). That
/// defeats pillar #1 on exactly the queries RFC 0044 exists to make
/// fast. Collection also costs a per-file footer read at plan time,
/// which a many-file log store pays on every query.
///
/// Remove this override once the upstream fix ships in a release we
/// depend on; the RFC0044.7/.8 and RFC0007.1 pruning tests are the
/// gate that will catch it either way.
pub(crate) fn session() -> SessionContext {
    let mut config = datafusion::prelude::SessionConfig::new();
    config.options_mut().execution.collect_statistics = false;
    SessionContext::new_with_config(config)
}

#[cfg(test)]
mod session_tests {
    #[test]
    fn the_shared_session_disables_statistics_collection() {
        let ctx = super::session();
        assert!(
            !ctx.state().config().options().execution.collect_statistics,
            "collect_statistics must stay off until apache/datafusion#24769              is fixed in a release we depend on (see session())"
        );
    }
}

/// Register `urls` as listing table `name` on `ctx` and return its
/// `DataFrame`. **Tenancy** is not a parameter here by design: the
/// §3.7 scope is enforced where URLs are *resolved* (the tenant-
/// prefixed `resolve_data_urls` / `audit_table_urls`), plus the
/// audit path's row-level guard — this helper receives
/// already-scoped URLs, and a `TenantId` parameter it never used
/// would be false reassurance. `DataFusion`'s default `Utf8View`/`BinaryView`
/// representations are fine on either path: the shared RFC 0005
/// decoder handles both view and plain string/binary arrays
/// (RFC 0021 / RFC0021.4), so no `schema_force_view_types` override
/// is needed.
pub(crate) async fn register_listing_table(
    ctx: &SessionContext,
    name: &str,
    urls: Vec<ListingTableUrl>,
    mode: SchemaMode<'_>,
) -> Result<datafusion::dataframe::DataFrame, QueryError> {
    let options =
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");
    let config = match mode {
        SchemaMode::Union(promoted) => {
            let mut schemas = Vec::with_capacity(urls.len());
            for url in &urls {
                let schema = options
                    .infer_schema(&ctx.state(), url)
                    .await
                    .map_err(storage_err)?;
                schemas.push(schema.as_ref().clone());
            }
            let file_schema = schema_adapt::merge_scanned_schemas(schemas, promoted)
                .map_err(|detail| QueryError::Storage { detail })?;
            ListingTableConfig::new_with_multi_paths(urls)
                .with_listing_options(options)
                .with_schema(Arc::new(file_schema))
                .with_expr_adapter_factory(Arc::new(schema_adapt::PromotedNoCoercionFactory))
        }
        SchemaMode::Infer => ListingTableConfig::new_with_multi_paths(urls)
            .with_listing_options(options)
            .infer_schema(&ctx.state())
            .await
            .map_err(storage_err)?,
    };
    let table = ListingTable::try_new(config).map_err(storage_err)?;
    ctx.register_table(name, Arc::new(table))
        .map_err(storage_err)?;
    ctx.table(name).await.map_err(storage_err)
}

/// The one execution path: physical-plan the frame, collect it, and
/// account for it — [`scan_stats`] plus the RFC 0040 operator spans,
/// which every call site MUST record (this helper replaced five
/// copy-paste sites whose only guard was discipline).
pub(crate) async fn execute_plan(
    df: datafusion::dataframe::DataFrame,
    task_ctx: Arc<datafusion::execution::TaskContext>,
) -> Result<(Vec<RecordBatch>, QueryStats), QueryError> {
    let plan = df.create_physical_plan().await.map_err(storage_err)?;
    let batches = datafusion::physical_plan::collect(Arc::clone(&plan), task_ctx)
        .await
        .map_err(storage_err)?;
    let stats = scan_stats(plan.as_ref());
    record_operator_spans(plan.as_ref());
    Ok((batches, stats))
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;

    /// Write a one-batch parquet file at `path` with the given fields.
    fn write_file(path: &std::path::Path, fields: Vec<Field>, batch_cols: Vec<ArrayRef>) {
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), batch_cols).expect("batch");
        let file = std::fs::File::create(path).expect("create");
        let mut w = ArrowWriter::try_new(file, schema, None).expect("writer");
        w.write(&batch).expect("write");
        w.close().expect("close");
    }

    fn two_divergent_files(dir: &std::path::Path) -> Vec<ListingTableUrl> {
        // File A: shared + only_a. File B: shared + only_b — the shape
        // the union mode exists for and the infer mode is blind to.
        let a = dir.join("a.parquet");
        let b = dir.join("b.parquet");
        write_file(
            &a,
            vec![
                Field::new("shared", DataType::Int64, false),
                Field::new("only_a", DataType::Utf8, true),
            ],
            vec![
                Arc::new(Int64Array::from(vec![1_i64])),
                Arc::new(StringArray::from(vec![Some("x")])),
            ],
        );
        write_file(
            &b,
            vec![
                Field::new("shared", DataType::Int64, false),
                Field::new("only_b", DataType::Utf8, true),
            ],
            vec![
                Arc::new(Int64Array::from(vec![2_i64])),
                Arc::new(StringArray::from(vec![Some("y")])),
            ],
        );
        vec![
            ListingTableUrl::parse(a.display().to_string()).expect("url a"),
            ListingTableUrl::parse(b.display().to_string()).expect("url b"),
        ]
    }

    /// `Union` sees every scanned file's columns; `Infer` is the
    /// documented first-file-only shape — the divergence this enum
    /// makes an explicit argument.
    #[tokio::test]
    async fn union_merges_where_infer_takes_the_first_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let promoted = PromotedAttributes::default();

        let ctx = SessionContext::new();
        let df = register_listing_table(
            &ctx,
            "union_t",
            two_divergent_files(dir.path()),
            SchemaMode::Union(&promoted),
        )
        .await
        .expect("union registers");
        let names: Vec<_> = df
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        assert!(names.contains(&"only_a".to_string()) && names.contains(&"only_b".to_string()));

        let ctx = SessionContext::new();
        let df = register_listing_table(
            &ctx,
            "infer_t",
            two_divergent_files(dir.path()),
            SchemaMode::Infer,
        )
        .await
        .expect("infer registers");
        let names: Vec<_> = df
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        assert!(
            names.contains(&"only_a".to_string()) && !names.contains(&"only_b".to_string()),
            "infer is first-file-only by contract: {names:?}",
        );
    }

    /// The one execution path returns the collected batches AND the
    /// scan accounting — a call site cannot take one without the
    /// other (the RFC 0040 property this helper exists to enforce).
    #[tokio::test]
    async fn execute_plan_returns_batches_with_scan_stats() {
        let dir = tempfile::tempdir().expect("tmp");
        let promoted = PromotedAttributes::default();
        let ctx = SessionContext::new();
        let df = register_listing_table(
            &ctx,
            "t",
            two_divergent_files(dir.path()),
            SchemaMode::Union(&promoted),
        )
        .await
        .expect("registers");
        let (batches, stats) = execute_plan(df, ctx.task_ctx()).await.expect("executes");
        let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
        assert_eq!(rows, 2, "both files' rows collected");
        assert!(
            stats.row_groups_scanned > 0,
            "the scan is accounted: {stats:?}",
        );
    }
}
