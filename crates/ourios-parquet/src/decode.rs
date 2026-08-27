//! The shared column-accessor family both parquet readers decode
//! through (epic #745 wave 2): one implementation of every
//! `required_*` / `optional_*` accessor, the view-tolerant
//! [`StrCol`]/[`BinCol`] wrappers, and the §3.8/§3.9 baseline-column
//! check — generic over a [`DecodeError`] so `ReaderError` and
//! `AuditReaderError` keep their own variants and messages. Before
//! this module the two readers carried line-for-line twins, and a fix
//! landing on one twin only produced a live divergence (the audit
//! `Utf8View` gap, #746).

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, TimestampNanosecondType, UInt8Type, UInt32Type, UInt64Type};
use arrow_array::{Array, BinaryViewArray, RecordBatch, StringViewArray};

use crate::audit_reader::AuditReaderError;
use crate::reader::ReaderError;

/// A string column in either arrow representation. The parquet reader
/// yields plain `Utf8`; `DataFusion` (the querier's scan path) yields
/// `Utf8View` by default — one decoder serves both (RFC 0021 §3.1).
/// `pub(crate)` so the audit reader's accessors share it rather than
/// re-growing the plain-`Utf8`-only shape this replaced (epic #745
/// wave 0: the audit path had exactly that divergence).
pub(crate) enum StrCol<'a> {
    Plain(&'a arrow_array::StringArray),
    View(&'a StringViewArray),
}

impl<'a> StrCol<'a> {
    pub(crate) fn try_new(col: &'a dyn Array) -> Option<Self> {
        if let Some(a) = col.as_string_opt::<i32>() {
            return Some(Self::Plain(a));
        }
        col.as_string_view_opt().map(Self::View)
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Plain(a) => a.len(),
            Self::View(a) => a.len(),
        }
    }

    pub(crate) fn get(&self, i: usize) -> Option<&str> {
        match self {
            Self::Plain(a) => (!a.is_null(i)).then(|| a.value(i)),
            Self::View(a) => (!a.is_null(i)).then(|| a.value(i)),
        }
    }
}

/// [`StrCol`]'s binary counterpart (`Binary` | `BinaryView`).
pub(crate) enum BinCol<'a> {
    Plain(&'a arrow_array::BinaryArray),
    View(&'a BinaryViewArray),
}

impl<'a> BinCol<'a> {
    pub(crate) fn try_new(col: &'a dyn Array) -> Option<Self> {
        if let Some(a) = col.as_binary_opt::<i32>() {
            return Some(Self::Plain(a));
        }
        col.as_binary_view_opt().map(Self::View)
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Plain(a) => a.len(),
            Self::View(a) => a.len(),
        }
    }

    pub(crate) fn get(&self, i: usize) -> Option<&[u8]> {
        match self {
            Self::Plain(a) => (!a.is_null(i)).then(|| a.value(i)),
            Self::View(a) => (!a.is_null(i)).then(|| a.value(i)),
        }
    }
}

/// RFC 0005 §3.9 / §3.7: every REQUIRED (non-nullable) column of
/// `expected` must be present in the file's schema, else a hard
/// error. The one copy of the baseline-schema invariant — every
/// constructor of both readers (data and audit) enforces it here.
pub(crate) fn require_baseline_columns<E: DecodeError>(
    file_schema: &arrow_schema::Schema,
    expected: &arrow_schema::Schema,
) -> Result<(), E> {
    for expected_field in expected.fields() {
        if !expected_field.is_nullable()
            && file_schema
                .column_with_name(expected_field.name())
                .is_none()
        {
            return Err(E::missing_required(expected_field.name()));
        }
    }
    Ok(())
}

/// The constructor surface a reader error must offer the shared
/// accessors. Message text is built here once, so the two readers
/// cannot drift apart on diagnostics again.
pub(crate) trait DecodeError: Sized {
    /// A §3.8/§3.9 baseline REQUIRED column absent from the file.
    fn missing_required(name: &str) -> Self;
    /// A column present but shape-mismatched (type, null-on-required,
    /// list nesting).
    fn conversion(column: &'static str, detail: String) -> Self;
}

impl DecodeError for ReaderError {
    fn missing_required(name: &str) -> Self {
        Self::MissingRequiredColumn {
            name: name.to_string(),
        }
    }
    fn conversion(column: &'static str, detail: String) -> Self {
        Self::Conversion { column, detail }
    }
}

impl DecodeError for AuditReaderError {
    fn missing_required(name: &str) -> Self {
        Self::MissingRequiredColumn {
            name: name.to_string(),
        }
    }
    fn conversion(column: &'static str, detail: String) -> Self {
        Self::Conversion { column, detail }
    }
}

pub(crate) fn required_string<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<String>, E> {
    let col = required_column(batch, name)?;
    let arr = StrCol::try_new(col).ok_or_else(|| {
        E::conversion(
            name,
            format!(
                "expected Utf8/Utf8View string array, got {:?}",
                col.data_type()
            ),
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        let Some(v) = arr.get(i) else {
            return Err(E::conversion(
                name,
                format!("row {}: null on a REQUIRED column", row_offset + i),
            ));
        };
        out.push(v.to_string());
    }
    Ok(out)
}

pub(crate) fn required_u64<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u64>, E> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<UInt64Type>().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected UInt64Array, got {:?}", col.data_type()),
        )
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

pub(crate) fn required_u32<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u32>, E> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<UInt32Type>().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected UInt32Array, got {:?}", col.data_type()),
        )
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

pub(crate) fn required_u8<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u8>, E> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<UInt8Type>().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected UInt8Array, got {:?}", col.data_type()),
        )
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

pub(crate) fn required_f32<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<f32>, E> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<Float32Type>().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected Float32Array, got {:?}", col.data_type()),
        )
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

/// Materialise a primitive Arrow array into `Vec<T::Native>`,
/// erroring on any NULL slot. Plain `arr.values().to_vec()` would
/// silently turn NULL slots into zero (the underlying primitive
/// buffer's default fill), masking file corruption. Fast-paths
/// the null-free case so the common path is still a single
/// buffer copy.
pub(crate) fn materialize_required_primitive<
    T: arrow_array::types::ArrowPrimitiveType,
    E: DecodeError,
>(
    arr: &arrow_array::PrimitiveArray<T>,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<T::Native>, E> {
    if arr.null_count() == 0 {
        return Ok(arr.values().to_vec());
    }
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(E::conversion(
                name,
                format!("row {}: null on a REQUIRED column", row_offset + i),
            ));
        }
    }
    // Validity buffer reported nulls but no row matched —
    // unreachable in practice.
    Ok(arr.values().to_vec())
}

pub(crate) fn required_bool<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<bool>, E> {
    let col = required_column(batch, name)?;
    let arr = col.as_boolean_opt().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected BooleanArray, got {:?}", col.data_type()),
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(E::conversion(
                name,
                format!("row {}: null on a REQUIRED column", row_offset + i),
            ));
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

pub(crate) fn required_timestamp<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<i64>, E> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| {
            E::conversion(
                name,
                format!(
                    "expected TimestampNanosecondArray, got {:?}",
                    col.data_type()
                ),
            )
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(E::conversion(
                name,
                format!("row {}: null on a REQUIRED column", row_offset + i),
            ));
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

pub(crate) fn required_column<'a, E: DecodeError>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a dyn Array, E> {
    let idx = batch
        .schema()
        .index_of(name)
        .map_err(|_| E::missing_required(name))?;
    Ok(batch.column(idx).as_ref())
}

pub(crate) fn optional_string<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<String>>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = StrCol::try_new(col).ok_or_else(|| {
        E::conversion(
            name,
            format!(
                "expected Utf8/Utf8View string array, got {:?}",
                col.data_type()
            ),
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(arr.get(i).map(ToString::to_string));
    }
    Ok(Some(out))
}

pub(crate) fn optional_binary<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<Vec<u8>>>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = BinCol::try_new(col).ok_or_else(|| {
        E::conversion(
            name,
            format!(
                "expected Binary/BinaryView array, got {:?}",
                col.data_type()
            ),
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(arr.get(i).map(<[u8]>::to_vec));
    }
    Ok(Some(out))
}

pub(crate) fn optional_timestamp<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<i64>>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| {
            E::conversion(
                name,
                format!(
                    "expected TimestampNanosecondArray, got {:?}",
                    col.data_type()
                ),
            )
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i))
        });
    }
    Ok(Some(out))
}

pub(crate) fn optional_fixed_bytes16<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; 16]>>>, E> {
    optional_fixed_bytes::<16, E>(batch, name)
}

pub(crate) fn optional_fixed_bytes8<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; 8]>>>, E> {
    optional_fixed_bytes::<8, E>(batch, name)
}

pub(crate) fn optional_fixed_bytes<const N: usize, E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; N]>>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col.as_fixed_size_binary_opt().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected FixedSizeBinaryArray, got {:?}", col.data_type()),
        )
    })?;
    if usize::try_from(arr.value_length()).ok() != Some(N) {
        return Err(E::conversion(
            name,
            format!(
                "expected FixedSizeBinary({N}), got FixedSizeBinary({})",
                arr.value_length(),
            ),
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(if arr.is_null(i) {
            None
        } else {
            let slice = arr.value(i);
            let mut buf = [0u8; N];
            buf.copy_from_slice(slice);
            Some(buf)
        });
    }
    Ok(Some(out))
}

pub(crate) fn optional_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Option<&'a dyn Array> {
    let idx = batch.schema().index_of(name).ok()?;
    Some(batch.column(idx).as_ref())
}

/// Per-row `Option<u64>` for a nullable `UInt64` column (the §3.7
/// `template_id` / `compaction_generation` / `compaction_rows`
/// columns).
pub(crate) fn optional_u64<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<u64>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let arr = col.as_primitive_opt::<UInt64Type>().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected UInt64Array, got {:?}", col.data_type()),
        )
    })?;
    Ok((0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i)))
        .collect())
}

/// Per-row `Option<u32>` for a nullable `UInt32` column.
pub(crate) fn optional_u32<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<u32>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let arr = col.as_primitive_opt::<UInt32Type>().ok_or_else(|| {
        E::conversion(
            name,
            format!("expected UInt32Array, got {:?}", col.data_type()),
        )
    })?;
    Ok((0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i)))
        .collect())
}

/// Per-row `Option<Vec<String>>` for the nullable
/// `compaction_input_files` `LIST<STRING>` column. The element field
/// is non-nullable, so a NULL element is a corrupt row.
pub(crate) fn optional_string_list<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<Vec<String>>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let list = col.as_list_opt::<i32>().ok_or_else(|| {
        E::conversion(name, "column is not a LIST<STRING> as declared".to_string())
    })?;
    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        if list.is_null(row_idx) {
            out.push(None);
            continue;
        }
        let elements = list.value(row_idx);
        let strs = StrCol::try_new(elements.as_ref())
            .ok_or_else(|| E::conversion(name, "list element is not Utf8/Utf8View".to_string()))?;
        let mut row = Vec::with_capacity(strs.len());
        for i in 0..strs.len() {
            let Some(v) = strs.get(i) else {
                return Err(E::conversion(
                    name,
                    format!(
                        "batch row {row_idx} element {i}: NULL but the element field is non-nullable",
                    ),
                ));
            };
            row.push(v.to_string());
        }
        out.push(Some(row));
    }
    Ok(out)
}

/// Per-row `Option<Vec<u64>>` for the nullable `alias_member_ids`
/// `LIST<UInt64>` column. NULL list ⇒ `None` (not an alias row);
/// empty list ⇒ `Some(vec![])` — the §3.7 empty-vs-NULL distinction.
/// The element field is non-nullable, so a NULL element is a corrupt
/// row.
pub(crate) fn optional_u64_list<E: DecodeError>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<Vec<u64>>>, E> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let list = col.as_list_opt::<i32>().ok_or_else(|| {
        E::conversion(name, "column is not a LIST<UInt64> as declared".to_string())
    })?;
    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        if list.is_null(row_idx) {
            out.push(None);
            continue;
        }
        let elements = list.value(row_idx);
        let ids = elements
            .as_primitive_opt::<UInt64Type>()
            .ok_or_else(|| E::conversion(name, "list element is not UInt64".to_string()))?;
        let mut row = Vec::with_capacity(ids.len());
        for i in 0..ids.len() {
            if ids.is_null(i) {
                return Err(E::conversion(
                    name,
                    format!(
                        "batch row {row_idx} element {i}: NULL but the element field is non-nullable",
                    ),
                ));
            }
            row.push(ids.value(i));
        }
        out.push(Some(row));
    }
    Ok(out)
}
