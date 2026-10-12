//! The panic boundary around every Parquet decode in this crate. The
//! `parquet`/`arrow` decoders assert, index and unwrap on malformed pages
//! and footers instead of returning an error, and every file read back
//! from the store is untrusted bytes: without this boundary a single
//! corrupt object aborts the process. A caught panic becomes the reader's
//! typed [`DecodeError::decode_panicked`], which names the object.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use arrow_array::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReader;

use crate::decode::DecodeError;

/// Run one decode of the untrusted Parquet object at `path`, turning a
/// panic inside the `parquet`/`arrow` decoders into
/// [`DecodeError::decode_panicked`].
pub(crate) fn guard_decode<T, E: DecodeError>(
    path: &Path,
    decode: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    catch_decode_panic(decode).unwrap_or_else(|message| Err(E::decode_panicked(path, message)))
}

fn catch_decode_panic<T>(decode: impl FnOnce() -> T) -> Result<T, String> {
    // Unwind-safe in practice: `decode` mutates only decoder state of the
    // one object being read, and every caller drops that state on the
    // error path (`Batches` retires its reader), so nothing a panic left
    // half-updated is observed afterwards.
    catch_unwind(AssertUnwindSafe(decode)).map_err(|payload| panic_message(payload.as_ref()))
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "a panic with a non-string payload".to_owned()
    }
}

/// A [`ParquetRecordBatchReader`] whose page decoding runs under
/// [`guard_decode`]. After a caught panic the reader is dropped and every
/// later call returns the same error, so a decoder left mid-page is never
/// read from again.
pub(crate) struct Batches(BatchesState);

enum BatchesState {
    Live(ParquetRecordBatchReader),
    Panicked(String),
}

impl Batches {
    pub(crate) fn new(reader: ParquetRecordBatchReader) -> Self {
        Self(BatchesState::Live(reader))
    }

    /// The next decoded batch of the object at `path`, or `None` at its
    /// end.
    pub(crate) fn next_batch<E: DecodeError>(
        &mut self,
        path: &Path,
    ) -> Option<Result<RecordBatch, E>> {
        let reader = match &mut self.0 {
            BatchesState::Live(reader) => reader,
            BatchesState::Panicked(message) => {
                return Some(Err(E::decode_panicked(path, message.clone())));
            }
        };
        match catch_decode_panic(|| reader.next()) {
            Ok(next) => next.map(|batch| batch.map_err(|e| E::parquet(e.into()))),
            Err(message) => {
                self.0 = BatchesState::Panicked(message.clone());
                Some(Err(E::decode_panicked(path, message)))
            }
        }
    }
}
