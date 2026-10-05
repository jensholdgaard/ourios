//! The highest template id a data or audit file carries, read from its
//! footer (RFC 0059 §3.5).
//!
//! The template-id bootstrap reads this over every data and audit file in
//! a store, hundreds of thousands of them on a long-lived node, so a file
//! is answered from its footer's column statistics, fetched with one
//! ranged read of its tail. Only a row group without usable statistics
//! makes the whole file come down, and then only its id columns decode.
//! No row is ever materialised.

use arrow_array::{Array, ListArray, UInt64Array};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::errors::ParquetError;
use parquet::file::metadata::{ColumnChunkMetaData, ParquetMetaData, ParquetMetaDataReader};
use parquet::file::statistics::Statistics;

use crate::audit_reader::AuditReaderError;
use crate::store::{Store, StoreError};
use crate::{audit_columns, columns};

/// How much of a file's tail the first read fetches: enough for the
/// footer of every file the writers produce.
pub const FOOTER_PREFETCH_BYTES: u64 = 64 * 1024;

/// Which id-carrying columns a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdColumns {
    /// A data file's `template_id`.
    Data,
    /// An audit file's `template_id` and the alias events' ids.
    Audit,
}

impl IdColumns {
    fn leaves(self) -> &'static [&'static [&'static str]] {
        match self {
            Self::Data => &[&[columns::TEMPLATE_ID]],
            Self::Audit => &[
                &[audit_columns::TEMPLATE_ID],
                &[audit_columns::ALIAS_REPRESENTATIVE_ID],
                &[audit_columns::ALIAS_MEMBER_IDS, "list", "element"],
            ],
        }
    }
}

/// What a footer alone says about a file's highest id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterMax {
    /// The exact answer: `None` when no row carries an id.
    Known(Option<u64>),
    /// Some row group has no usable statistics; the id columns must be
    /// decoded.
    NeedsDecode,
    /// The tail held too little of the footer; read this many bytes.
    NeedsTail(u64),
}

/// Why a file's highest id could not be read.
#[derive(Debug)]
pub enum IdMaxError {
    /// Fetching the file, or its tail, failed.
    Store {
        key: String,
        source: Box<StoreError>,
    },
    /// The file does not parse, or its id columns have the wrong type.
    Decode {
        key: String,
        source: Box<AuditReaderError>,
    },
}

impl std::fmt::Display for IdMaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store { key, source } => write!(f, "read {key}: {source}"),
            Self::Decode { key, source } => write!(f, "decode {key}: {source}"),
        }
    }
}

impl IdMaxError {
    /// True if the store refused a read for want of a permission.
    #[must_use]
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::Store { source, .. } if source.is_permission_denied())
    }
}

impl std::error::Error for IdMaxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store { source, .. } => Some(source.as_ref()),
            Self::Decode { source, .. } => Some(source.as_ref()),
        }
    }
}

/// The highest id the object at `key` carries in `columns`, from its
/// footer where the statistics allow.
///
/// # Errors
///
/// [`IdMaxError`] when a read fails or the file does not parse.
pub fn object_max_id(
    store: &Store,
    key: &str,
    columns: IdColumns,
) -> Result<Option<u64>, IdMaxError> {
    let store_err = |source| IdMaxError::Store {
        key: key.to_owned(),
        source: Box::new(source),
    };
    let decode_err = |source| IdMaxError::Decode {
        key: key.to_owned(),
        source: Box::new(source),
    };
    let mut want = FOOTER_PREFETCH_BYTES;
    loop {
        let tail = store.get_suffix_blocking(key, want).map_err(store_err)?;
        match footer_max(&tail.bytes, tail.object_size, columns).map_err(decode_err)? {
            FooterMax::Known(max) => return Ok(max),
            FooterMax::NeedsTail(needed) if needed > want => want = needed,
            FooterMax::NeedsTail(_) | FooterMax::NeedsDecode => {
                let bytes = store.get_blocking(key).map_err(store_err)?;
                return decoded_max(bytes::Bytes::from(bytes), columns).map_err(decode_err);
            }
        }
    }
}

/// What the footer in `tail`, the last bytes of a file of `object_size`
/// bytes, says about its highest id.
///
/// # Errors
///
/// [`AuditReaderError::Parquet`] when the footer does not parse.
pub fn footer_max(
    tail: &bytes::Bytes,
    object_size: u64,
    columns: IdColumns,
) -> Result<FooterMax, AuditReaderError> {
    let mut reader = ParquetMetaDataReader::new();
    match reader.try_parse_sized(tail, object_size) {
        Ok(()) => {}
        Err(ParquetError::NeedMoreData(needed)) => {
            return Ok(FooterMax::NeedsTail(needed as u64));
        }
        Err(e) => return Err(AuditReaderError::Parquet(e)),
    }
    let metadata = reader.finish().map_err(AuditReaderError::Parquet)?;
    Ok(metadata_max(&metadata, columns))
}

fn metadata_max(metadata: &ParquetMetaData, columns: IdColumns) -> FooterMax {
    let leaves = id_leaves(metadata, columns);
    let mut highest = None;
    for row_group in metadata.row_groups() {
        for &leaf in &leaves {
            match chunk_max(row_group.column(leaf)) {
                ChunkMax::Known(max) => highest = highest.max(max),
                ChunkMax::Unknown => return FooterMax::NeedsDecode,
            }
        }
    }
    FooterMax::Known(highest)
}

/// The leaf indices of `columns` present in the file; an absent column
/// (an older file) carries no id.
fn id_leaves(metadata: &ParquetMetaData, columns: IdColumns) -> Vec<usize> {
    let schema = metadata.file_metadata().schema_descr();
    columns
        .leaves()
        .iter()
        .filter_map(|path| {
            schema
                .columns()
                .iter()
                .position(|column| column.path().parts() == *path)
        })
        .collect()
}

/// What one column chunk's statistics say.
enum ChunkMax {
    /// The exact maximum, or `None` when every value is null.
    Known(Option<u64>),
    /// No usable statistics: the column must be decoded.
    Unknown,
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

/// The highest id in the whole file `bytes`, decoding only its id
/// columns.
///
/// # Errors
///
/// [`AuditReaderError::Parquet`] when the file does not parse;
/// [`AuditReaderError::Conversion`] when an id column is neither `UInt64`
/// nor a list of `UInt64`.
pub fn decoded_max(
    bytes: bytes::Bytes,
    columns: IdColumns,
) -> Result<Option<u64>, AuditReaderError> {
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(bytes).map_err(AuditReaderError::Parquet)?;
    let leaves = id_leaves(builder.metadata(), columns);
    if leaves.is_empty() {
        return Ok(None);
    }
    let schema = builder.metadata().file_metadata().schema_descr_ptr();
    let reader = builder
        .with_projection(ProjectionMask::leaves(&schema, leaves))
        .build()
        .map_err(AuditReaderError::Parquet)?;
    let mut highest = None;
    for batch in reader {
        let batch = batch.map_err(|e| AuditReaderError::Parquet(e.into()))?;
        for column in batch.columns() {
            highest = highest.max(column_max(column.as_ref())?);
        }
    }
    Ok(highest)
}

fn column_max(column: &dyn Array) -> Result<Option<u64>, AuditReaderError> {
    let any = column.as_any();
    if let Some(ids) = any.downcast_ref::<UInt64Array>() {
        return Ok(ids.iter().flatten().max());
    }
    if let Some(lists) = any.downcast_ref::<ListArray>() {
        return column_max(lists.values().as_ref());
    }
    Err(AuditReaderError::Conversion {
        column: audit_columns::TEMPLATE_ID,
        detail: format!(
            "expected UInt64 or a list of it, found {}",
            column.data_type()
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use ourios_core::alias::ActorId;
    use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange};
    use ourios_core::record::{BodyKind, MinedRecord};
    use ourios_core::tenant::TenantId;
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::{EnabledStatistics, WriterProperties};

    use super::*;
    use crate::{audit_events_to_batch, mined_records_to_batch};

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

    fn alias(representative_id: u64, member: u64) -> AuditEvent {
        AuditEvent {
            tenant_id: tenant(),
            timestamp: SystemTime::UNIX_EPOCH,
            payload: AuditPayload::AliasAsserted {
                representative_id,
                member_ids: vec![member],
                actor: ActorId::new("operator").expect("actor"),
                reason: String::new(),
            },
        }
    }

    fn row(template_id: u64) -> MinedRecord {
        MinedRecord {
            tenant_id: tenant(),
            template_id,
            template_version: 1,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 1,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: Vec::new(),
            separators: vec![String::new()],
            body: Some(format!("line {template_id}")),
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    /// `batch` as one Parquet file, one row group per `rows`.
    fn file(
        batch: &arrow_array::RecordBatch,
        statistics: EnabledStatistics,
        rows: usize,
    ) -> bytes::Bytes {
        let props = WriterProperties::builder()
            .set_statistics_enabled(statistics)
            .set_max_row_group_row_count(Some(rows))
            .build();
        let mut out = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut out, batch.schema(), Some(props)).expect("w");
        writer.write(batch).expect("write");
        writer.close().expect("close");
        bytes::Bytes::from(out)
    }

    fn audit_file(events: &[AuditEvent], statistics: EnabledStatistics) -> bytes::Bytes {
        file(
            &audit_events_to_batch(events).expect("batch"),
            statistics,
            2,
        )
    }

    fn whole(bytes: &bytes::Bytes, columns: IdColumns) -> FooterMax {
        footer_max(bytes, bytes.len() as u64, columns).expect("footer")
    }

    #[test]
    fn the_footer_answers_across_row_groups_and_alias_ids() {
        let events = [created(4), alias(900, 950), created(17), created(9)];
        let bytes = audit_file(&events, EnabledStatistics::Chunk);
        assert_eq!(whole(&bytes, IdColumns::Audit), FooterMax::Known(Some(950)));
    }

    #[test]
    fn a_footer_only_buffer_is_enough() {
        let events = [created(4), created(17)];
        let bytes = audit_file(&events, EnabledStatistics::Chunk);
        let size = bytes.len() as u64;
        let FooterMax::NeedsTail(needed) =
            footer_max(&bytes.slice(bytes.len() - 8..), size, IdColumns::Audit)
                .expect("the tail parses")
        else {
            panic!("eight bytes hold only the footer length");
        };
        let tail = bytes.slice(bytes.len() - usize::try_from(needed).expect("fits")..);
        assert!(tail.len() < bytes.len(), "the footer is less than the file");
        assert_eq!(
            footer_max(&tail, size, IdColumns::Audit).expect("footer"),
            FooterMax::Known(Some(17)),
        );
    }

    #[test]
    fn a_file_without_statistics_decodes_its_id_columns_to_the_same_answer() {
        let events = [created(4), alias(900, 950), created(17)];
        let bytes = audit_file(&events, EnabledStatistics::None);
        assert_eq!(whole(&bytes, IdColumns::Audit), FooterMax::NeedsDecode);
        assert_eq!(
            decoded_max(bytes, IdColumns::Audit).expect("decode"),
            Some(950)
        );
    }

    /// RFC 0059 §3.5: the footer answers an audit file only when every id
    /// column has usable statistics; one alias column without them makes
    /// that column's data be read.
    #[test]
    fn an_alias_column_without_statistics_forces_a_decode() {
        let events = [created(4), alias(900, 950), created(17)];
        let batch = audit_events_to_batch(&events).expect("batch");
        for leaf in [
            vec![audit_columns::ALIAS_REPRESENTATIVE_ID.to_owned()],
            vec![
                audit_columns::ALIAS_MEMBER_IDS.to_owned(),
                "list".to_owned(),
                "element".to_owned(),
            ],
        ] {
            let props = WriterProperties::builder()
                .set_statistics_enabled(EnabledStatistics::Chunk)
                .set_column_statistics_enabled(
                    parquet::schema::types::ColumnPath::new(leaf.clone()),
                    EnabledStatistics::None,
                )
                .build();
            let mut out = Vec::new();
            let mut writer =
                ArrowWriter::try_new(&mut out, batch.schema(), Some(props)).expect("w");
            writer.write(&batch).expect("write");
            writer.close().expect("close");
            let bytes = bytes::Bytes::from(out);
            assert_eq!(
                whole(&bytes, IdColumns::Audit),
                FooterMax::NeedsDecode,
                "{leaf:?} has no statistics, so the footer cannot answer",
            );
            assert_eq!(
                decoded_max(bytes, IdColumns::Audit).expect("decode"),
                Some(950)
            );
        }
    }

    #[test]
    fn data_files_answer_from_template_id() {
        let batch = mined_records_to_batch(&[row(3), row(41), row(0)]).expect("batch");
        let with = file(&batch, EnabledStatistics::Chunk, 2);
        assert_eq!(whole(&with, IdColumns::Data), FooterMax::Known(Some(41)));
        let without = file(&batch, EnabledStatistics::None, 2);
        assert_eq!(
            decoded_max(without, IdColumns::Data).expect("decode"),
            Some(41)
        );
    }

    #[test]
    fn a_file_of_events_without_ids_binds_none() {
        let quarantined = AuditEvent {
            tenant_id: tenant(),
            timestamp: SystemTime::UNIX_EPOCH,
            payload: AuditPayload::RecordQuarantined {
                partition: "year=2026/month=10/day=05/hour=00".to_owned(),
                error: "test".to_owned(),
            },
        };
        let bytes = audit_file(&[quarantined], EnabledStatistics::Chunk);
        assert_eq!(whole(&bytes, IdColumns::Audit), FooterMax::Known(None));
    }

    #[test]
    fn the_production_writers_record_id_statistics() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::local(tmp.path()).expect("store");
        let events = [created(4), created(17)];
        let partition = crate::derive_audit_partition(&events[0]).expect("partition");
        let mut writer = crate::AuditWriter::open_in(&store, partition).expect("writer");
        writer.append_events(&events).expect("append");
        let written = writer.close().expect("close");
        let key = written.path.to_str().expect("utf-8 key");
        let tail = store
            .get_suffix_blocking(key, FOOTER_PREFETCH_BYTES)
            .expect("tail");
        assert!(
            tail.bytes.len() as u64 <= tail.object_size,
            "a suffix read never exceeds the object",
        );
        assert_eq!(
            footer_max(&tail.bytes, tail.object_size, IdColumns::Audit).expect("footer"),
            FooterMax::Known(Some(17)),
            "a production audit file is answered from its footer alone",
        );
        assert_eq!(
            object_max_id(&store, key, IdColumns::Audit).expect("read"),
            Some(17)
        );

        let rows = [row(3), row(41)];
        let partition = crate::PartitionKey::derive(&rows[0]).expect("partition");
        let mut writer = crate::Writer::open_in(&store, partition).expect("writer");
        writer.append_records(&rows).expect("append");
        let written = writer.close().expect("close");
        let tail = store
            .get_suffix_blocking(&written.key, FOOTER_PREFETCH_BYTES)
            .expect("tail");
        assert_eq!(
            footer_max(&tail.bytes, tail.object_size, IdColumns::Data).expect("footer"),
            FooterMax::Known(Some(41)),
            "a production data file is answered from its footer alone",
        );
    }
}
