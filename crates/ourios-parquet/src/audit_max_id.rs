//! The highest `template_id` one audit file binds, read from its footer.
//!
//! Bootstrapping the durable template-id high-water (RFC 0001 §6.9) reads
//! this over the whole audit history, which can run to gigabytes, so it
//! never decodes an [`AuditEvent`]: each row group's `template_id` chunk
//! statistics answer it, and only a row group without usable statistics
//! decodes, and then that one column alone.
//!
//! [`AuditEvent`]: ourios_core::audit::AuditEvent

use arrow_array::{Array, UInt64Array};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ColumnChunkMetaData;
use parquet::file::statistics::Statistics;

use crate::audit_columns;
use crate::audit_reader::AuditReaderError;

/// What one row group's `template_id` chunk statistics say.
enum ChunkMax {
    /// The exact maximum, or `None` when every value is null.
    Known(Option<u64>),
    /// No usable statistics: the column must be decoded.
    Unknown,
}

/// The highest `template_id` any row of the audit file in `bytes` carries,
/// or `None` when no row carries one (non-template events, or a file
/// without the column).
///
/// # Errors
///
/// [`AuditReaderError::Parquet`] when the footer or a decoded column page
/// does not parse; [`AuditReaderError::Conversion`] when `template_id` is
/// not the `UInt64` column RFC 0005 §3.7 specifies.
pub fn max_template_id(bytes: bytes::Bytes) -> Result<Option<u64>, AuditReaderError> {
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(bytes).map_err(AuditReaderError::Parquet)?;
    let schema = builder.metadata().file_metadata().schema_descr_ptr();
    let Some(leaf) = schema
        .columns()
        .iter()
        .position(|column| column.path().parts() == [audit_columns::TEMPLATE_ID])
    else {
        return Ok(None);
    };
    let mut highest = None;
    let mut undescribed = Vec::new();
    for (index, row_group) in builder.metadata().row_groups().iter().enumerate() {
        match chunk_max(row_group.column(leaf)) {
            ChunkMax::Known(max) => highest = highest.max(max),
            ChunkMax::Unknown => undescribed.push(index),
        }
    }
    if undescribed.is_empty() {
        return Ok(highest);
    }
    let decoded = builder
        .with_projection(ProjectionMask::leaves(&schema, [leaf]))
        .with_row_groups(undescribed)
        .build()
        .map_err(AuditReaderError::Parquet)?;
    for batch in decoded {
        let batch = batch.map_err(|e| AuditReaderError::Parquet(e.into()))?;
        highest = highest.max(column_max(batch.column(0).as_ref())?);
    }
    Ok(highest)
}

fn chunk_max(chunk: &ColumnChunkMetaData) -> ChunkMax {
    let Some(Statistics::Int64(stats)) = chunk.statistics() else {
        return ChunkMax::Unknown;
    };
    let all_null = u64::try_from(chunk.num_values()).ok() == stats.null_count_opt();
    match stats.max_opt() {
        Some(max) if stats.max_is_exact() => ChunkMax::Known(Some(max.cast_unsigned())),
        None if all_null => ChunkMax::Known(None),
        _ => ChunkMax::Unknown,
    }
}

fn column_max(column: &dyn Array) -> Result<Option<u64>, AuditReaderError> {
    let ids = column
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: audit_columns::TEMPLATE_ID,
            detail: format!("expected UInt64, found {}", column.data_type()),
        })?;
    Ok(ids.iter().flatten().max())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use ourios_core::alias::ActorId;
    use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange};
    use ourios_core::tenant::TenantId;
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::{EnabledStatistics, WriterProperties};

    use super::*;
    use crate::audit_events_to_batch;

    fn tenant() -> TenantId {
        TenantId::new("checkout")
    }

    fn created(template_id: u64) -> AuditEvent {
        AuditEvent {
            tenant_id: tenant(),
            timestamp: SystemTime::UNIX_EPOCH,
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: [0; 16],
                triggering_line_sample: None,
                change: TemplateChange::Created {
                    new_template: format!("line {template_id}"),
                },
            },
        }
    }

    fn alias(representative_id: u64) -> AuditEvent {
        AuditEvent {
            tenant_id: tenant(),
            timestamp: SystemTime::UNIX_EPOCH,
            payload: AuditPayload::AliasAsserted {
                representative_id,
                member_ids: vec![representative_id + 1],
                actor: ActorId::new("operator").expect("actor"),
                reason: String::new(),
            },
        }
    }

    /// `events` as one Parquet file, one row group per `rows_per_group`.
    fn file(events: &[AuditEvent], statistics: EnabledStatistics, rows: usize) -> bytes::Bytes {
        let batch = audit_events_to_batch(events).expect("batch");
        let props = WriterProperties::builder()
            .set_statistics_enabled(statistics)
            .set_max_row_group_row_count(Some(rows))
            .build();
        let mut out = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut out, batch.schema(), Some(props)).expect("w");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        bytes::Bytes::from(out)
    }

    #[test]
    fn the_footer_answers_across_row_groups_and_skips_null_ids() {
        let events = [created(4), alias(900), created(17), created(9)];
        let bytes = file(&events, EnabledStatistics::Chunk, 2);
        assert_eq!(max_template_id(bytes).expect("read"), Some(17));
    }

    #[test]
    fn a_file_without_statistics_decodes_the_column_to_the_same_answer() {
        let events = [created(4), alias(900), created(17), created(9)];
        let bytes = file(&events, EnabledStatistics::None, 2);
        assert_eq!(max_template_id(bytes).expect("read"), Some(17));
    }

    #[test]
    fn a_file_of_non_template_events_binds_no_id() {
        for statistics in [EnabledStatistics::Chunk, EnabledStatistics::None] {
            let bytes = file(&[alias(900)], statistics, 2);
            assert_eq!(max_template_id(bytes).expect("read"), None);
        }
    }

    #[test]
    fn the_production_writer_records_template_id_statistics() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = crate::Store::local(tmp.path()).expect("store");
        let events = [created(4), created(17)];
        let partition = crate::derive_audit_partition(&events[0]).expect("partition");
        let mut writer = crate::AuditWriter::open_in(&store, partition).expect("writer");
        writer.append_events(&events).expect("append");
        let written = writer.close().expect("close");
        let bytes =
            bytes::Bytes::from(std::fs::read(tmp.path().join(&written.path)).expect("read file"));
        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes).expect("footer");
        let schema = builder.metadata().file_metadata().schema_descr_ptr();
        let leaf = schema
            .columns()
            .iter()
            .position(|c| c.path().parts() == [audit_columns::TEMPLATE_ID])
            .expect("template_id column");
        for row_group in builder.metadata().row_groups() {
            assert!(
                matches!(chunk_max(row_group.column(leaf)), ChunkMax::Known(Some(17))),
                "the footer alone answers for a production-written file",
            );
        }
    }
}
