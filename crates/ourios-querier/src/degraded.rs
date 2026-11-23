//! RFC 0069 §3.2 on the query path: the materialization scan's rows are
//! decoded with every stored cell that does not decode degraded on its own,
//! and each degraded cell is reported once, naming the file and row it sits
//! in.

use std::collections::HashSet;

use datafusion::arrow::array::{Array, AsArray};
use datafusion::arrow::datatypes::Int64Type;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::dataframe::DataFrame;
use datafusion::functions::core::expr_fn::{file_row_index, input_file_name};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_miner::reconstruct::Reconstruction;
use ourios_parquet::{ShapeValidation, batch_to_degraded_records, columns};

use crate::api::UndecodableCells;
use crate::log_row::{LogBody, LogRow};
use crate::{QueryError, storage_err};

/// The scan's column holding each row's source object key.
const FILE: &str = "__ourios_file";
/// The scan's column holding each row's 0-based index within that file.
const FILE_ROW: &str = "__ourios_file_row";
const FILE_PATH: &str = "file.path";

/// `df` with each row's source file and its row index within it, so a
/// degraded cell can be located. Both are resolved per file by the scan, and
/// read only for a row that degrades.
pub(crate) fn located(df: DataFrame) -> Result<DataFrame, QueryError> {
    df.with_column(FILE, input_file_name())
        .and_then(|df| df.with_column(FILE_ROW, file_row_index()))
        .map_err(storage_err)
}

/// The materialization scan's rows, decoded.
pub(crate) struct Decoded<'a> {
    batches: &'a [RecordBatch],
    /// The record index each batch starts at, ascending.
    starts: Vec<usize>,
    /// Every returned row, in scan order.
    pub(crate) records: Vec<MinedRecord>,
    /// Each cell the reader degraded, as (record index, column).
    cells: Vec<(usize, &'static str)>,
}

/// Decode `batches`, degrading each stored cell that does not decode.
pub(crate) fn decode(batches: &[RecordBatch]) -> Result<Decoded<'_>, QueryError> {
    let mut records = Vec::new();
    let mut cells = Vec::new();
    for batch in batches {
        let offset = records.len();
        let decoded =
            batch_to_degraded_records(batch, offset, ShapeValidation::Skip).map_err(|e| {
                QueryError::Storage {
                    detail: format!("decode rows: {e}"),
                }
            })?;
        cells.extend(
            decoded
                .undecodable
                .iter()
                .map(|cell| (offset + cell.row, cell.column)),
        );
        records.extend(decoded.records);
    }
    Ok(Decoded::over(batches, records, cells))
}

impl<'a> Decoded<'a> {
    fn over(
        batches: &'a [RecordBatch],
        records: Vec<MinedRecord>,
        cells: Vec<(usize, &'static str)>,
    ) -> Self {
        let starts = batches
            .iter()
            .scan(0, |start, batch| {
                let this = *start;
                *start += batch.num_rows();
                Some(this)
            })
            .collect();
        Self {
            batches,
            starts,
            records,
            cells,
        }
    }

    /// Finish `rows`, rendered one per record: mark each body that is not
    /// valid UTF-8 as [`Reconstruction::RetainedLossy`], add each structured
    /// body that did not decode, then report every degraded cell and count
    /// them by column.
    pub(crate) fn settle(&self, rows: &mut [LogRow]) -> UndecodableCells {
        let not_utf8: HashSet<usize> = self
            .cells
            .iter()
            .filter(|&&(_, column)| column == columns::BODY)
            .map(|&(record, _)| record)
            .collect();
        let mut cells = self.cells.clone();
        for (index, (record, row)) in self.records.iter().zip(rows.iter_mut()).enumerate() {
            if not_utf8.contains(&index) {
                mark_lossy(record, row);
            } else if structured_body_undecoded(record, row) {
                cells.push((index, columns::BODY));
            }
        }
        cells.sort_unstable();
        let mut counts = UndecodableCells::default();
        for &(record, column) in &cells {
            self.report(record, column);
            counts.add(column);
        }
        counts
    }

    /// Log one `WARN` event naming the file, row and column of a degraded
    /// cell; never its content, which may hold masked values.
    fn report(&self, record: usize, column: &'static str) {
        let (file, row) = self.locate(record).unwrap_or_default();
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_READER_ROW_UNDECODABLE,
            {
                { FILE_PATH } = file.as_str(),
                { ourios_semconv::OURIOS_READER_ROW_INDEX } = row,
                { ourios_semconv::OURIOS_READER_COLUMN_NAME } = column,
            },
            "query: row {row} of {file} has an undecodable {column} cell; it is returned \
             degraded",
        );
    }

    /// The source file and row of record `record`, from the scan's own
    /// columns. `None` only for a batch that did not come from a file scan.
    fn locate(&self, record: usize) -> Option<(String, i64)> {
        let at = self
            .starts
            .partition_point(|&start| start <= record)
            .checked_sub(1)?;
        let batch = &self.batches[at];
        let row = record - self.starts[at];
        if row >= batch.num_rows() {
            return None;
        }
        let file = batch.column_by_name(FILE)?.as_string_opt::<i32>()?;
        let index = batch
            .column_by_name(FILE_ROW)?
            .as_primitive_opt::<Int64Type>()?;
        (file.is_valid(row) && index.is_valid(row))
            .then(|| (file.value(row).to_owned(), index.value(row)))
    }
}

/// Whether `row` renders a structured record's stored body as text, which
/// it does only when that body did not decode.
fn structured_body_undecoded(record: &MinedRecord, row: &LogRow) -> bool {
    record.body_kind == BodyKind::Structured
        && record.body.is_some()
        && matches!(row.body, LogBody::Rendered { .. })
}

/// Mark `row`'s body, whose stored bytes are not valid UTF-8, as their
/// lossy conversion: a text answer cannot carry the bytes themselves.
fn mark_lossy(record: &MinedRecord, row: &mut LogRow) {
    match &mut row.body {
        LogBody::Rendered { reconstruction, .. }
            if *reconstruction == Reconstruction::RetainedVerbatim =>
        {
            *reconstruction = Reconstruction::RetainedLossy;
        }
        // Canonical JSON is always UTF-8, so a structured body that is not
        // is corrupt even when its lossy conversion parses.
        LogBody::Structured(_) => {
            row.body = LogBody::Rendered {
                line: record.body.clone().unwrap_or_default().into_bytes(),
                reconstruction: Reconstruction::RetainedLossy,
            };
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::arrow::array::{ArrayRef, Int64Array, StringArray};
    use datafusion::arrow::record_batch::RecordBatch;

    use super::{Decoded, FILE, FILE_ROW};

    /// A scan batch whose rows came from `file`, starting at file row `from`.
    fn batch(file: &str, from: i64, rows: usize) -> RecordBatch {
        let files: ArrayRef = Arc::new(StringArray::from(vec![file; rows]));
        let indexes: ArrayRef = Arc::new(Int64Array::from_iter_values(
            (0..rows).map(|i| from + i64::try_from(i).expect("small")),
        ));
        RecordBatch::try_from_iter([(FILE, files), (FILE_ROW, indexes)]).expect("batch")
    }

    /// A record is located in the batch holding it, however many batches
    /// precede it, and a record past the last batch has no location.
    #[test]
    fn a_record_is_located_in_its_own_batch() {
        let batches = [batch("a", 0, 2), batch("a", 2, 1), batch("b", 0, 3)];
        let decoded = Decoded::over(&batches, Vec::new(), Vec::new());
        let located: Vec<_> = (0..7).map(|record| decoded.locate(record)).collect();
        assert_eq!(
            located,
            vec![
                Some(("a".to_owned(), 0)),
                Some(("a".to_owned(), 1)),
                Some(("a".to_owned(), 2)),
                Some(("b".to_owned(), 0)),
                Some(("b".to_owned(), 1)),
                Some(("b".to_owned(), 2)),
                None,
            ]
        );
    }
}
