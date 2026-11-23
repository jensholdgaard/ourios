//! How the reader treats a stored cell that does not decode (RFC 0069
//! §3.2): the strict decode fails the batch, the query path's degrading
//! decode replaces the cell and names it.

use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::canonical::decode_attributes;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::MinedRecord;

use super::ReaderError;
use crate::columns;

/// A stored cell the degrading decode could not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndecodableCell {
    /// The row's index within its batch.
    pub row: usize,
    /// The Parquet column holding the cell: `body`, `attributes`,
    /// `resource_attributes` or `scope_attributes`.
    pub column: &'static str,
}

/// A batch decoded by [`super::batch_to_degraded_records`].
#[derive(Debug, Clone, PartialEq)]
pub struct DegradedBatch {
    /// Every row of the batch, an undecodable cell replaced in place.
    pub records: Vec<MinedRecord>,
    /// The cells that did not decode, in row then column order.
    pub undecodable: Vec<UndecodableCell>,
}

/// The decode policy for one batch, and the cells it degraded.
pub(super) struct Cells {
    row_offset: usize,
    degrade: bool,
    pub(super) undecodable: Vec<UndecodableCell>,
}

impl Cells {
    /// Fail the batch on a cell that does not decode.
    pub(super) fn strict(row_offset: usize) -> Self {
        Self {
            row_offset,
            degrade: false,
            undecodable: Vec::new(),
        }
    }

    /// Degrade a cell that does not decode, and record it.
    pub(super) fn degrading(row_offset: usize) -> Self {
        Self {
            degrade: true,
            ..Self::strict(row_offset)
        }
    }

    /// The attributes stored in `column`'s cell of row `row`. Degrading, an
    /// undecodable cell becomes the reserved attribute holding its stored
    /// JSON, so the data stays visible and says why it is not structured.
    pub(super) fn attributes(
        &mut self,
        stored: &str,
        column: &'static str,
        row: usize,
    ) -> Result<Vec<KeyValue>, ReaderError> {
        if stored == "[]" {
            return Ok(Vec::new());
        }
        match decode_attributes(stored.as_bytes()) {
            Ok(attributes) => Ok(attributes),
            Err(_) if self.degrade => {
                self.undecodable.push(UndecodableCell { row, column });
                Ok(vec![undecodable_attribute(stored)])
            }
            Err(source) => Err(ReaderError::AttributeDecode {
                column,
                row_index: self.row_offset + row,
                source,
            }),
        }
    }

    /// The OPTIONAL `scope_attributes` cell: an absent column or a NULL
    /// cell means no scope attributes (RFC 0018 §3.1).
    pub(super) fn scope_attributes(
        &mut self,
        column: Option<&Vec<Option<String>>>,
        row: usize,
    ) -> Result<Vec<KeyValue>, ReaderError> {
        match column.and_then(|cells| cells[row].as_deref()) {
            None => Ok(Vec::new()),
            Some(stored) => self.attributes(stored, columns::SCOPE_ATTRIBUTES, row),
        }
    }

    /// The stored body as text. Bytes that are not valid UTF-8 convert
    /// lossily, as they always have; degrading, the cell is also recorded,
    /// so the row is marked as not byte-exact.
    pub(super) fn body(&mut self, stored: &[u8], row: usize) -> String {
        match std::str::from_utf8(stored) {
            Ok(text) => text.to_owned(),
            Err(_) if self.degrade => {
                self.undecodable.push(UndecodableCell {
                    row,
                    column: columns::BODY,
                });
                String::from_utf8_lossy(stored).into_owned()
            }
            Err(_) => String::from_utf8_lossy(stored).into_owned(),
        }
    }
}

/// The reserved `ourios.attribute.undecodable` attribute holding `stored`.
fn undecodable_attribute(stored: &str) -> KeyValue {
    KeyValue {
        key: ourios_semconv::OURIOS_ATTRIBUTE_UNDECODABLE.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(stored.to_owned())),
        }),
        key_strindex: 0,
    }
}
