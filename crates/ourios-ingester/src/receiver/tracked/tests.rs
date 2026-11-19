//! A shutdown waits for every ingest in flight, a detached one included,
//! and refuses those that start after it closes.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::tenant::TenantId;
use tokio::sync::Notify;

use super::super::ingest::{Ingest, SharedIngest};
use super::super::pipeline::{Export, IngestFailure, ReceiveError, SpanError};
use super::Tracked;

/// An ingest that runs until it is released.
#[derive(Default)]
struct Held {
    entered: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl Ingest for Held {
    async fn ingest_reported(
        &self,
        _export: Export<'_>,
        _credential: Option<&str>,
        _span_error: SpanError,
    ) -> Result<usize, ReceiveError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(1)
    }
}

fn no_span(_: &ReceiveError) -> Option<String> {
    None
}

async fn ingest(tracked: &Tracked) -> Result<usize, ReceiveError> {
    let tenant = TenantId::new("acme");
    let export = Export {
        request: ExportLogsServiceRequest::default(),
        tenant,
        binding: None,
        lenient_json: false,
    };
    tracked.ingest_reported(export, None, no_span).await
}

/// An ingest whose client has gone, still running on its own task, holds
/// the drain until it ends.
#[tokio::test]
async fn a_drain_waits_for_an_ingest_still_running() {
    let held = Arc::new(Held::default());
    let (tracked, drain) = Tracked::new(Arc::clone(&held) as SharedIngest);
    let tracked = Arc::new(tracked);
    let running = tokio::spawn({
        let tracked = Arc::clone(&tracked);
        async move { ingest(&tracked).await }
    });
    held.entered.notified().await;

    drain.close();
    let waited = tokio::time::timeout(Duration::from_millis(100), drain.drained()).await;
    assert!(waited.is_err(), "the drain resolved with an ingest running");

    held.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), drain.drained())
        .await
        .expect("drained once it ends");
    assert_eq!(running.await.expect("joined").expect("ingested"), 1);
}

/// Once closed, an ingest that starts is refused as retryable and never
/// reaches the inner ingest, so it can emit nothing after the flush.
#[tokio::test]
async fn an_ingest_started_after_close_is_refused_retryably() {
    let held = Arc::new(Held::default());
    let (tracked, drain) = Tracked::new(Arc::clone(&held) as SharedIngest);
    drain.close();

    let refused = tokio::time::timeout(Duration::from_secs(5), ingest(&tracked))
        .await
        .expect("refused at once, not run")
        .expect_err("refused");
    assert_eq!(
        IngestFailure::classify(&refused),
        IngestFailure::Unavailable
    );
    drain.drained().await;
}
