//! The flusher's checkpoint cadence (RFC 0065 §6.5): RFC 0052's barrier on
//! the replica a group names flusher, once per interval, one checkpoint at
//! a time, and only once its miner has applied past the last cut written.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::FutureExt as _;
use ourios_parquet::Store;
use tokio::task::JoinHandle;

use super::checkpointing::{Checkpoint, Checkpoints};
use super::flushing::Flushing;
use super::kept::Kept;
use super::mining::{Captured, GroupMiner};
use super::proposer::Delivered;
use super::proposing::Proposing;
use super::publication::Publication;

/// Where one group's checkpoints are written and recorded.
#[derive(Clone)]
pub(super) struct Target {
    group: u32,
    store: Store,
    checkpoints: Checkpoints,
}

/// One group's checkpoint cadence on its flusher.
pub(super) struct Checkpointer {
    target: Target,
    every: Duration,
    /// The cut of the last checkpoint written, which an idle group does
    /// not cut again.
    written: Option<u64>,
    /// The checkpoint still writing, which yields its cut once written.
    running: Option<JoinHandle<Option<u64>>>,
    /// How a written checkpoint is proposed; none in tests that only write.
    proposing: Option<Proposing>,
}

/// A cut the miner captured, and the publication it publishes through.
pub(super) struct Cut {
    captured: Captured,
    publication: Arc<Publication>,
    at: Instant,
    /// The publication's generation at the cut.
    generation: u64,
}

impl Checkpointer {
    pub(super) fn new(group: u32, every: Duration, store: Store, checkpoints: Checkpoints) -> Self {
        Self {
            target: Target {
                group,
                store,
                checkpoints,
            },
            every,
            written: None,
            running: None,
            proposing: None,
        }
    }

    /// Propose each checkpoint it writes with `proposing`.
    pub(super) fn proposing(self, proposing: Proposing) -> Self {
        Self {
            proposing: Some(proposing),
            ..self
        }
    }

    /// Where its checkpoints go, for a capture off the runtime.
    pub(super) fn target(&self) -> Target {
        self.target.clone()
    }

    /// How often it cuts.
    pub(super) fn every(&self) -> Duration {
        self.every
    }

    /// Whether no checkpoint is still writing; the cut of one that has
    /// just finished becomes [`Self::written`].
    pub(super) fn ready(&mut self) -> bool {
        match self.running.take() {
            Some(running) if !running.is_finished() => {
                self.running = Some(running);
                false
            }
            Some(finished) => {
                if let Some(Ok(Some(cut))) = finished.now_or_never() {
                    self.written = Some(cut);
                }
                true
            }
            None => true,
        }
    }

    /// The cut of the last checkpoint written, which the next must pass.
    pub(super) fn written(&self) -> Option<u64> {
        self.written
    }

    /// Publish and write `cut` off the runtime.
    pub(super) fn start(&mut self, cut: Cut) {
        let (target, proposing) = (self.target.clone(), self.proposing.clone());
        self.running = Some(tokio::spawn(async move {
            let writing = tokio::task::spawn_blocking(move || {
                let written = target
                    .checkpoint(&cut.publication)
                    .run(&cut.captured, (cut.at, cut.generation))
                    .is_ok();
                (written, cut)
            });
            let (written, cut) = writing.await.ok()?;
            if !written {
                return None;
            }
            // Once its bundle is durable, whatever `checkpoint.json` says
            // (§6.5); a proposal not taken is dropped, and the next barrier
            // proposes a later cut.
            if let Some(proposing) = proposing {
                // A defect fails the checkpoint as a failed write does, so
                // the next barrier tries again (§6.8).
                if let Some(Delivered::Malformed(_)) = proposing.propose(&cut.captured).await {
                    return None;
                }
            }
            Some(cut.captured.bundle.cut().seq)
        }));
    }
}

impl Target {
    /// The cut at `miner`'s last entry, if that is past `written` and the
    /// flusher publishes everything kept; a capture that fails is
    /// recorded. Blocking: it runs on the miner's step.
    pub(super) fn capture(
        &self,
        miner: &GroupMiner,
        (kept, flushing): (&Mutex<Kept>, &Mutex<Flushing>),
        written: Option<u64>,
    ) -> Option<Cut> {
        // A checkpoint entry moves the miner on but changes nothing a
        // bundle holds: an idle group cuts nothing new.
        let through = miner.changed_through()?;
        if written.is_some_and(|written| through <= written) {
            return None;
        }
        let (publication, generation) = {
            // The order `Keeping::keep` takes them in.
            let kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
            let flushing = flushing.lock().unwrap_or_else(PoisonError::into_inner);
            let publication = flushing.checkpointable(&kept)?;
            let generation = publication.generation();
            (publication, generation)
        };
        let at = Instant::now();
        match miner.capture(publication.log())? {
            Ok(captured) => Some(Cut {
                captured,
                publication,
                at,
                generation,
            }),
            Err(error) => {
                self.checkpoint(&publication).failed(at, "_OTHER", &error);
                None
            }
        }
    }

    fn checkpoint<'a>(&'a self, publication: &'a Publication) -> Checkpoint<'a> {
        Checkpoint {
            group: self.group,
            publication,
            store: &self.store,
            checkpoints: &self.checkpoints,
        }
    }
}
