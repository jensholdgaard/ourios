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

/// Register `urls` as listing table `name` on `ctx` and return its
/// `DataFrame`. `DataFusion`'s default `Utf8View`/`BinaryView`
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
