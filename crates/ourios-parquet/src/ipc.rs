//! Mined rows as an Arrow IPC stream in the record files' schema (RFC 0065
//! §6.7): how a tail replica hands a querier the rows it has not
//! published, so the querier reads them with the columns it reads from
//! Parquet.

use std::fmt;
use std::io::Cursor;

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::ArrowError;
use ourios_core::record::MinedRecord;

use crate::record_batch::{BatchError, mined_records_to_batch};

/// Why rows did not encode or a stream did not decode.
#[derive(Debug)]
pub enum IpcError {
    /// A record does not fit the record files' schema.
    Batch(BatchError),
    Arrow(ArrowError),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Batch(e) => write!(f, "a row does not fit the record schema: {e}"),
            Self::Arrow(e) => write!(f, "arrow ipc: {e}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Batch(e) => Some(e),
            Self::Arrow(e) => Some(e),
        }
    }
}

/// `records` as one record batch in an Arrow IPC stream.
///
/// # Errors
///
/// [`IpcError`] when a record does not fit the record schema.
pub fn encode(records: &[MinedRecord]) -> Result<Vec<u8>, IpcError> {
    let batch = mined_records_to_batch(records).map_err(IpcError::Batch)?;
    let mut writer = StreamWriter::try_new(Vec::new(), &batch.schema()).map_err(IpcError::Arrow)?;
    writer.write(&batch).map_err(IpcError::Arrow)?;
    writer.into_inner().map_err(IpcError::Arrow)
}

/// The record batches of the Arrow IPC stream `bytes`.
///
/// # Errors
///
/// [`IpcError::Arrow`] when `bytes` is not a stream.
pub fn decode(bytes: &[u8]) -> Result<Vec<RecordBatch>, IpcError> {
    let reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(IpcError::Arrow)?;
    reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(IpcError::Arrow)
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, IngestPosition, MinedRecord, Param};
    use ourios_core::tenant::TenantId;

    use super::{decode, encode};
    use crate::record_batch::mined_records_to_batch;

    fn record(seq: u64) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("acme"),
            template_id: 7,
            template_version: 2,
            severity_number: 9,
            severity_text: None,
            scope_name: Some("lib.auth".to_owned()),
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 1_700_000_000_000_000_000 + seq,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: seq.to_string(),
            }],
            separators: vec![String::new(), " ".to_owned(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
            ingest_position: Some(IngestPosition {
                group: 3,
                seq,
                index: 0,
            }),
        }
    }

    #[test]
    fn rows_read_back_as_the_record_batch_they_encode() {
        let records: Vec<_> = (1..=3).map(record).collect();
        assert!(!records.is_empty());
        let batches = decode(&encode(&records).expect("encodes")).expect("decodes");
        assert_eq!(
            batches,
            vec![mined_records_to_batch(&records).expect("batch")]
        );
    }

    #[test]
    fn bytes_that_are_not_a_stream_are_refused() {
        assert!(decode(b"not an arrow stream").is_err());
    }
}
