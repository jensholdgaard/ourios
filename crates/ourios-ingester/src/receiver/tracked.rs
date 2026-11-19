//! The exports a node is still ingesting, so a shutdown can wait for them.
//!
//! The OTLP transports run each ingest on its own task, and a client that
//! disconnects leaves that task running on its own. A shutdown that only
//! joins the listeners could then flush the audit sink before such an
//! ingest emits its tenant denial (RFC 0026 §3.4), losing it. [`Tracked`]
//! counts the ingests in flight: [`Drain::close`] refuses any that start
//! after it, and [`Drain::drained`] resolves once those already running
//! have ended.

use std::sync::Arc;

use tokio::sync::watch;

use super::ingest::{Ingest, SharedIngest};
use super::pipeline::{Export, ReceiveError, SpanError};

/// An [`Ingest`] whose calls in flight a shutdown can wait for.
pub struct Tracked {
    inner: SharedIngest,
    flight: Arc<Flight>,
}

/// The calls in flight, and whether new ones are refused.
#[derive(Default)]
struct Flight {
    state: watch::Sender<State>,
}

#[derive(Clone, Copy, Default)]
struct State {
    running: usize,
    closed: bool,
}

/// Where a shutdown closes a [`Tracked`] ingest and waits for it.
#[derive(Clone)]
pub struct Drain(Arc<Flight>);

impl Tracked {
    /// `inner`, its calls counted, and where to drain them.
    #[must_use]
    pub fn new(inner: SharedIngest) -> (Self, Drain) {
        let flight = Arc::new(Flight::default());
        let drain = Drain(Arc::clone(&flight));
        (Self { inner, flight }, drain)
    }
}

impl Drain {
    /// Refuse every ingest that starts from now on.
    pub fn close(&self) {
        self.0.state.send_modify(|state| state.closed = true);
    }

    /// Resolves once no ingest is running.
    pub async fn drained(&self) {
        let mut state = self.0.state.subscribe();
        // The sender lives as long as `self`, so the wait cannot fail.
        let _ = state.wait_for(|state| state.running == 0).await;
    }
}

impl Flight {
    /// Count a call in, unless closed.
    fn enter(self: &Arc<Self>) -> Option<Running> {
        let mut entered = false;
        self.state.send_modify(|state| {
            if !state.closed {
                state.running += 1;
                entered = true;
            }
        });
        entered.then(|| Running(Arc::clone(self)))
    }
}

/// A call in flight, counted out as it ends, however it ends.
struct Running(Arc<Flight>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.state.send_modify(|state| state.running -= 1);
    }
}

#[async_trait::async_trait]
impl Ingest for Tracked {
    async fn ingest_reported(
        &self,
        export: Export<'_>,
        credential: Option<&str>,
        span_error: SpanError,
    ) -> Result<usize, ReceiveError> {
        let Some(_running) = self.flight.enter() else {
            return Err(ReceiveError::ShuttingDown);
        };
        self.inner
            .ingest_reported(export, credential, span_error)
            .await
    }
}

#[cfg(test)]
mod tests;
