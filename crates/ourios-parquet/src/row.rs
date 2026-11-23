//! A row as compaction carries it (RFC 0069 §3.2): decoded, or, when any
//! of its cells does not decode, in its stored form, every raw cell kept
//! as the bytes the file holds so the rewrite changes none of them.

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::DataType;
use ourios_core::record::MinedRecord;

use crate::columns;
use crate::promoted::{ATTR_PREFIX, RESOURCE_PREFIX, SERVICE_NAME_KEY, project_string_value};
use crate::reader::UndecodableCell;

/// A row compaction reads, sorts, spills and writes back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    /// The row as decoded, each undecodable cell degraded (RFC 0069 §3.2):
    /// what its sort key and the rewrite's hooks read.
    record: MinedRecord,
    /// The stored form, when a cell did not decode.
    stored: Option<Box<StoredRow>>,
}

/// What the rewrite writes back unchanged for a row with an undecodable
/// cell.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredRow {
    /// The body and attribute cells as stored.
    cells: StoredCells,
    /// The columns whose cell did not decode.
    undecodable: Vec<&'static str>,
    /// The stored promoted cells projected from an undecodable attribute
    /// cell, by column name; a `NULL` cell is absent.
    kept: Vec<(String, Kept)>,
}

#[derive(Debug, Clone, PartialEq)]
struct StoredCells {
    attributes: String,
    resource_attributes: String,
    scope_attributes: Option<String>,
    body: Option<Vec<u8>>,
}

/// A stored promoted cell, by the Arrow type of its column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kept {
    Str(String),
    I64(i64),
    F64(f64),
}

/// Which attribute cell a promoted column is projected from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Resource,
    Log,
}

impl Family {
    fn column(self) -> &'static str {
        match self {
            Self::Resource => columns::RESOURCE_ATTRIBUTES,
            Self::Log => columns::ATTRIBUTES,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Resource => RESOURCE_PREFIX,
            Self::Log => ATTR_PREFIX,
        }
    }
}

/// A row the batch builder writes: its decoded fields, and the stored
/// form of a row whose cells did not all decode.
pub(crate) trait Carried {
    fn record(&self) -> &MinedRecord;

    fn stored(&self) -> Option<&StoredRow> {
        None
    }
}

impl Carried for MinedRecord {
    fn record(&self) -> &MinedRecord {
        self
    }
}

impl Carried for Row {
    fn record(&self) -> &MinedRecord {
        &self.record
    }

    fn stored(&self) -> Option<&StoredRow> {
        self.stored.as_deref()
    }
}

impl Row {
    /// The decoded row, when both attribute cells that can name it decoded:
    /// a row whose log or resource attributes did not decode has no
    /// trustworthy identity (RFC 0069 §3.2).
    pub(crate) fn identity(&self) -> Option<&MinedRecord> {
        let named = self.stored().is_none_or(|stored| {
            !stored.is_undecodable(Family::Resource) && !stored.is_undecodable(Family::Log)
        });
        named.then_some(&self.record)
    }

    /// The columns whose cell did not decode; empty for a decoded row.
    pub(crate) fn undecodable(&self) -> &[&'static str] {
        self.stored().map_or(&[], |stored| &stored.undecodable)
    }
}

impl StoredRow {
    pub(crate) fn attributes(&self) -> &str {
        &self.cells.attributes
    }

    pub(crate) fn resource_attributes(&self) -> &str {
        &self.cells.resource_attributes
    }

    pub(crate) fn scope_attributes(&self) -> Option<&str> {
        self.cells.scope_attributes.as_deref()
    }

    pub(crate) fn body(&self) -> Option<&[u8]> {
        self.cells.body.as_deref()
    }

    pub(crate) fn is_undecodable(&self, family: Family) -> bool {
        self.undecodable.contains(&family.column())
    }

    /// Whether column `name` is promoted from an undecodable attribute cell.
    fn keeps(&self, name: &str) -> bool {
        [Family::Resource, Family::Log]
            .into_iter()
            .any(|family| name.starts_with(family.prefix()) && self.is_undecodable(family))
    }

    /// The stored cell of promoted column `key` of `family`.
    pub(crate) fn kept(&self, family: Family, key: &str) -> Option<&Kept> {
        let prefix = family.prefix();
        self.kept
            .iter()
            .find(|(name, _)| name.strip_prefix(prefix) == Some(key))
            .map(|(_, kept)| kept)
    }

    /// Heap bytes beyond the decoded record.
    fn stored_bytes(&self) -> usize {
        let cells = &self.cells;
        let kept: usize = self
            .kept
            .iter()
            .map(|(name, kept)| {
                name.len()
                    + match kept {
                        Kept::Str(s) => s.len(),
                        Kept::I64(_) | Kept::F64(_) => 0,
                    }
            })
            .sum();
        cells.attributes.len()
            + cells.resource_attributes.len()
            + cells.scope_attributes.as_ref().map_or(0, String::len)
            + cells.body.as_ref().map_or(0, Vec::len)
            + kept
    }
}

/// The promoted `service.name` a row sorts by (RFC 0036 §3.1): its stored
/// cell when the resource attributes did not decode.
pub(crate) fn service_name<R: Carried>(row: &R) -> Option<&str> {
    match row.stored() {
        Some(stored) if stored.is_undecodable(Family::Resource) => {
            match stored.kept(Family::Resource, SERVICE_NAME_KEY) {
                Some(Kept::Str(service)) => Some(service),
                _ => None,
            }
        }
        _ => project_string_value(&row.record().resource_attributes, SERVICE_NAME_KEY),
    }
}

/// Heap bytes the stored form adds to a row.
pub(crate) fn stored_footprint<R: Carried>(row: &R) -> usize {
    row.stored().map_or(0, StoredRow::stored_bytes)
}

/// `records`, decoded from `batch` with every undecodable cell named in
/// `undecodable`, as rows: each with a named cell in its stored form.
pub(crate) fn rows_of(
    batch: &RecordBatch,
    records: Vec<MinedRecord>,
    undecodable: &[UndecodableCell],
) -> Vec<Row> {
    let mut cells = undecodable.iter().peekable();
    records
        .into_iter()
        .enumerate()
        .map(|(i, record)| {
            let mut columns = Vec::new();
            while let Some(cell) = cells.next_if(|cell| cell.row == i) {
                columns.push(cell.column);
            }
            let stored = (!columns.is_empty()).then(|| Box::new(stored_row(batch, i, columns)));
            Row { record, stored }
        })
        .collect()
}

fn stored_row(batch: &RecordBatch, i: usize, undecodable: Vec<&'static str>) -> StoredRow {
    let text = |column| {
        batch
            .column_by_name(column)
            .and_then(|cells| text_cell(cells, i))
    };
    let cells = StoredCells {
        attributes: text(columns::ATTRIBUTES).unwrap_or_default(),
        resource_attributes: text(columns::RESOURCE_ATTRIBUTES).unwrap_or_default(),
        scope_attributes: text(columns::SCOPE_ATTRIBUTES),
        body: batch
            .column_by_name(columns::BODY)
            .and_then(|cells| binary_cell(cells, i)),
    };
    let mut row = StoredRow {
        cells,
        undecodable,
        kept: Vec::new(),
    };
    let schema = batch.schema();
    for (field, cells) in schema.fields().iter().zip(batch.columns()) {
        let name = field.name();
        if row.keeps(name)
            && let Some(kept) = kept_cell(cells, i)
        {
            row.kept.push((name.clone(), kept));
        }
    }
    row
}

fn text_cell(cells: &ArrayRef, i: usize) -> Option<String> {
    if cells.is_null(i) {
        return None;
    }
    match cells.data_type() {
        DataType::Utf8 => Some(cells.as_string::<i32>().value(i).to_owned()),
        DataType::LargeUtf8 => Some(cells.as_string::<i64>().value(i).to_owned()),
        DataType::Utf8View => Some(cells.as_string_view().value(i).to_owned()),
        _ => None,
    }
}

fn binary_cell(cells: &ArrayRef, i: usize) -> Option<Vec<u8>> {
    if cells.is_null(i) {
        return None;
    }
    match cells.data_type() {
        DataType::Binary => Some(cells.as_binary::<i32>().value(i).to_vec()),
        DataType::LargeBinary => Some(cells.as_binary::<i64>().value(i).to_vec()),
        DataType::BinaryView => Some(cells.as_binary_view().value(i).to_vec()),
        _ => None,
    }
}

fn kept_cell(cells: &ArrayRef, i: usize) -> Option<Kept> {
    if cells.is_null(i) {
        return None;
    }
    match cells.data_type() {
        DataType::Int64 => Some(Kept::I64(cells.as_primitive::<Int64Type>().value(i))),
        DataType::Float64 => Some(Kept::F64(cells.as_primitive::<Float64Type>().value(i))),
        _ => text_cell(cells, i).map(Kept::Str),
    }
}
