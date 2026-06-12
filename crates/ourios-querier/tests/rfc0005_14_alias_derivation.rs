//! Scenario RFC0005.14 — the v1 reader-side alias-map derivation
//! (RFC 0005 §3.7.1, amendment 2026-06-12; the storage round-trip half
//! lives in `ourios-parquet/tests/audit_round_trip.rs`).
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Each test writes alias events to the real RFC 0005 `audit/` stream,
//! then runs `resolves_to` through the public [`Querier::run_query`]
//! surface with **no injected map** — so the asserted row counts can
//! only come from the §3.7.1 storage-derived fold. The cross-file
//! same-timestamp tests pin the total fold order's file-path tiebreak
//! by writing one event per file and renaming the files into a crafted
//! lexicographic order (the scan orders by path, not by write time).

mod common;

use std::path::{Path, PathBuf};

use common::{DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, at, simple, write_all};
use ourios_core::alias::ActorId;
use ourios_core::audit::{AuditEvent, AuditPayload};
use ourios_core::tenant::TenantId;
use ourios_parquet::{AuditWriter, PartitionKey};
use ourios_querier::Querier;

const A: u64 = 10;
const B: u64 = 20;

fn alias_asserted(
    tenant: &str,
    representative_id: u64,
    member_ids: Vec<u64>,
    ts: u64,
) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts),
        payload: AuditPayload::AliasAsserted {
            representative_id,
            member_ids,
            actor: ActorId::new("op-test").expect("actor"),
            reason: String::new(),
        },
    }
}

fn alias_retracted(
    tenant: &str,
    representative_id: u64,
    member_ids: Vec<u64>,
    ts: u64,
) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts),
        payload: AuditPayload::AliasRetracted {
            representative_id,
            member_ids,
            actor: ActorId::new("op-test").expect("actor"),
            reason: String::new(),
        },
    }
}

/// Write `event` as a single-event audit file and rename it to
/// `final_name` inside its partition directory, so the test controls
/// the lexicographic file order the §3.7.1 tiebreak folds in.
fn write_audit_file_named(bucket: &Path, event: &AuditEvent, final_name: &str) -> PathBuf {
    // TS0 is 2026-04-02T10:58Z; the audit partition is tenant + UTC day.
    let partition = PartitionKey {
        tenant_id: event.tenant_id.as_str().to_owned(),
        year: 2026,
        month: 4,
        day: 2,
        hour: 0,
    };
    let mut writer = AuditWriter::open(bucket, partition).expect("open audit writer");
    writer
        .append_events(std::slice::from_ref(event))
        .expect("append");
    let written = writer.close().expect("close");
    let target = written
        .path
        .parent()
        .expect("partition dir")
        .join(final_name);
    std::fs::rename(&written.path, &target).expect("rename to crafted order");
    target
}

/// Three data rows under tenant `T`: two for leaf A, one for leaf B —
/// so `resolves_to(A)` counts 2 without an active alias and 3 with one.
fn write_data_rows(bucket: &Path) {
    write_all(
        bucket,
        &[
            simple("T", A, TS0),
            simple("T", A, TS0 + 1_000),
            simple("T", B, TS0 + HOUR_NS),
        ],
    );
}

async fn resolves_to_a_rows(bucket: &Path, tenant: &str) -> u64 {
    let query = ourios_querier::dsl::parse(&format!("resolves_to({A})")).expect("parse");
    Querier::new(bucket)
        .run_query(&query, &TenantId::new(tenant), NOW, DEFAULT_WINDOW_NS, None)
        .await
        .expect("run_query")
        .rows
}

/// RFC0005.14 — the derived map folds assert→retract in event-time
/// order: an assertion at `t` retracted at `t + 1` leaves no class, so
/// `resolves_to(A)` is back to exactly A's rows.
#[tokio::test]
async fn derived_map_folds_assert_then_retract_by_timestamp() {
    // Arrange — data rows plus an assert/retract pair a nanosecond
    // apart, in a single audit file (in-file row order is the fold
    // order within one file).
    let bucket = tempfile::TempDir::new().expect("temp");
    write_data_rows(bucket.path());
    let partition = PartitionKey {
        tenant_id: "T".to_owned(),
        year: 2026,
        month: 4,
        day: 2,
        hour: 0,
    };
    let mut writer = AuditWriter::open(bucket.path(), partition).expect("open");
    writer
        .append_events(&[
            alias_asserted("T", A, vec![B], TS0),
            alias_retracted("T", A, vec![B], TS0 + 1),
        ])
        .expect("append");
    writer.close().expect("close");

    // Act / Assert — assert-then-retract folds to no class: only A's
    // two rows match (RFC 0001 §6.7 via RFC 0005 §3.7.1).
    assert_eq!(resolves_to_a_rows(bucket.path(), "T").await, 2);
}

/// RFC0005.14 — the §3.7.1 cross-file same-timestamp tiebreak: two
/// single-event files carry an assertion and its retraction at the
/// SAME nanosecond, so event time cannot order them — the
/// lexicographic file path must. With the assert in `a.parquet` and
/// the retract in `b.parquet`, the fold ends retracted.
#[tokio::test]
async fn cross_file_same_timestamp_tiebreak_assert_first() {
    // Arrange.
    let bucket = tempfile::TempDir::new().expect("temp");
    write_data_rows(bucket.path());
    write_audit_file_named(
        bucket.path(),
        &alias_asserted("T", A, vec![B], TS0),
        "a.parquet",
    );
    write_audit_file_named(
        bucket.path(),
        &alias_retracted("T", A, vec![B], TS0),
        "b.parquet",
    );

    // Act / Assert — assert folds first (path "a" < "b"), then the
    // retraction dissolves the class: only A's rows.
    assert_eq!(resolves_to_a_rows(bucket.path(), "T").await, 2);
}

/// RFC0005.14 — the mirror case: the SAME two events at the SAME
/// nanosecond, but with the retraction in the lexicographically
/// *earlier* file. The fold order flips — retract (a no-op on an
/// empty map) then assert — so the class is active and
/// `resolves_to(A)` expands to A ∪ B. Together with the test above
/// this pins that the outcome is decided by the file-path tiebreak
/// and nothing else.
#[tokio::test]
async fn cross_file_same_timestamp_tiebreak_retract_first() {
    // Arrange — identical events, reversed crafted file order.
    let bucket = tempfile::TempDir::new().expect("temp");
    write_data_rows(bucket.path());
    write_audit_file_named(
        bucket.path(),
        &alias_retracted("T", A, vec![B], TS0),
        "a.parquet",
    );
    write_audit_file_named(
        bucket.path(),
        &alias_asserted("T", A, vec![B], TS0),
        "b.parquet",
    );

    // Act / Assert — the assertion folds last: the {A, B} class is
    // active, so A's two rows plus B's one row match.
    assert_eq!(resolves_to_a_rows(bucket.path(), "T").await, 3);
}

/// RFC0005.14 — tenant isolation at the storage layer (`CLAUDE.md`
/// §3.7; RFC0001.14): tenant T2's alias events on disk contribute
/// nothing to T's derived map — the derivation scans only T's
/// `audit/tenant_id=T/` partition root.
#[tokio::test]
async fn second_tenants_alias_events_never_fold_into_the_derived_map() {
    // Arrange — T has data rows but NO alias events; T2 asserts the
    // very same {A, B} class under its own partition root (plus a
    // data row so the fixture tenant is real).
    let bucket = tempfile::TempDir::new().expect("temp");
    write_data_rows(bucket.path());
    write_all(bucket.path(), &[simple("T2", A, TS0)]);
    write_audit_file_named(
        bucket.path(),
        &alias_asserted("T2", A, vec![B], TS0),
        "a.parquet",
    );

    // Act / Assert — T's derived map is empty: resolves_to(A) is the
    // singleton {A}, matching only A's two rows. T2's own map *does*
    // hold the class, but T2 has only one A row to match.
    assert_eq!(resolves_to_a_rows(bucket.path(), "T").await, 2);
    assert_eq!(resolves_to_a_rows(bucket.path(), "T2").await, 1);
}
