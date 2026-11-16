//! A flusher's checkpoint (RFC 0065 §6.5): once its miner has captured
//! the cut, publish everything buffered through it, then write the cut's
//! bundle and `checkpoint.json`. Nothing is purged until a checkpoint
//! entry applies, so a failure loses nothing and the next cut retries.

use std::fmt;
use std::time::Instant;

use ourios_parquet::Store;
use ourios_semconv as semconv;

use super::bundle::write::{Published, WriteError, write};
use super::mining::Captured;
use super::publication::Publication;
pub use super::telemetry::Checkpoints;

/// Why a checkpoint failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Failed {
    /// What was buffered through the cut did not all become durable.
    Publish,
    /// The bundle or `checkpoint.json` was not written.
    Write(WriteError),
}

impl Failed {
    /// Its `error.type`, as `ourios.wal.replication.checkpoint.duration`
    /// documents them.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Publish | Self::Write(WriteError::Store { .. }) => "store",
            Self::Write(WriteError::Differs { .. }) => "object_differs",
            Self::Write(WriteError::Unreadable(_)) => "pointer_unreadable",
            Self::Write(WriteError::NoCompareAndSwap) => "no_compare_and_swap",
            Self::Write(_) => "_OTHER",
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Publish => {
                f.write_str("the rows buffered through the cut were not all published")
            }
            Self::Write(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Failed {}

/// What a checkpoint of `group` writes and where.
pub struct Checkpoint<'a> {
    pub group: u32,
    pub publication: &'a Publication,
    pub store: &'a Store,
    pub checkpoints: &'a Checkpoints,
}

impl Checkpoint<'_> {
    /// Publish, then write `captured`, timed from `cut_at`, when the miner
    /// captured it. Blocking: the caller runs it off the async runtime.
    ///
    /// # Errors
    ///
    /// The [`Failed`] step; it is recorded and reported.
    pub fn run(&self, captured: &Captured, cut_at: Instant) -> Result<Published, Failed> {
        let outcome = if self.publication.flush_all() {
            write(self.store, &captured.bundle, &captured.snapshots).map_err(Failed::Write)
        } else {
            Err(Failed::Publish)
        };
        match &outcome {
            Ok(_) => self.checkpoints.ended(self.group, cut_at.elapsed(), None),
            Err(failed) => self.failed(cut_at, failed.error_type(), failed),
        }
        outcome
    }

    /// Record and report a checkpoint cut at `cut_at` that failed with
    /// `error_type`, for `why`.
    pub fn failed(&self, cut_at: Instant, error_type: &'static str, why: &dyn fmt::Display) {
        self.checkpoints
            .ended(self.group, cut_at.elapsed(), Some(error_type));
        tracing::warn!(
            name: semconv::EVENT_OURIOS_WAL_REPLICATION_CHECKPOINT_FAILED,
            {
                { semconv::OURIOS_WAL_GROUP } = i64::from(self.group),
                { crate::metrics::ERROR_TYPE } = error_type,
            },
            "the flusher of group {} wrote no checkpoint, and the next cut retries: {why}",
            self.group,
        );
    }
}

#[cfg(test)]
mod tests;
