//! RFC0069.3: `ourios.reader.row.undecodable` is emitted under its registry
//! name with only registry attributes, once per degraded cell, and
//! live-checked where weaver is configured.
//!
//! Its own test binary: `ourios_telemetry::live_check` installs the
//! process-global `tracing` subscriber it reads the events from.

#[path = "support/rfc0069_degraded.rs"]
#[allow(dead_code)]
mod fixture;

use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryRequest};
use ourios_semconv as semconv;
use ourios_telemetry::live_check::{self, Checked, EventSpec};

const ROW_UNDECODABLE: EventSpec = EventSpec {
    name: semconv::EVENT_OURIOS_READER_ROW_UNDECODABLE,
    required: &[
        "file.path",
        semconv::OURIOS_READER_ROW_INDEX,
        semconv::OURIOS_READER_COLUMN_NAME,
    ],
    optional: &[],
};

#[tokio::test]
async fn every_degraded_cell_is_named_and_live_checked() {
    let capture = live_check::event_capture().expect("the only subscriber this binary installs");
    capture.reset();
    let bucket = tempfile::TempDir::new().expect("temp");
    fixture::write_fixture(bucket.path());

    Querier::new(bucket.path())
        .run(QueryRequest {
            tenant: TenantId::new("a"),
            time_range: None,
            template_id: None,
            severity_text: None,
            limit: Some(10),
        })
        .await
        .expect("the query does not fail");

    let events = capture.events();
    let named = events
        .iter()
        .filter(|e| e.name == ROW_UNDECODABLE.name)
        .count();
    assert_eq!(named, 6, "one event per degraded cell");
    let checked = live_check::live_check(&events, &[ROW_UNDECODABLE])
        .expect("every degraded cell's event is registry-conformant");
    if checked == Checked::SpecOnly {
        eprintln!("RFC0069.3: weaver is not configured here; checked against the spec only");
    }
}
