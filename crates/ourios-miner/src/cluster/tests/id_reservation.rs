//! RFC 0001 §6.9 (2026-10-05 amendment): template ids come only from
//! reserved blocks, and a mint with no reservable id fails parse with
//! its body retained instead of taking an id it cannot prove unique.

use std::sync::{Arc, Mutex};

use super::*;
use crate::upstream::LOG_RECORD_TEMPLATE_ATTR;

/// A reserver whose answers the test flips: a fixed block size above
/// the floor, or failure.
#[derive(Clone)]
struct Switch {
    up: Arc<Mutex<bool>>,
    floors: Arc<Mutex<Vec<u64>>>,
}

impl Switch {
    fn new() -> Self {
        Self {
            up: Arc::new(Mutex::new(true)),
            floors: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set_up(&self, up: bool) {
        *self.up.lock().expect("switch") = up;
    }

    fn floors(&self) -> Vec<u64> {
        self.floors.lock().expect("floors").clone()
    }
}

impl IdReserver for Switch {
    fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError> {
        self.floors.lock().expect("floors").push(floor);
        if !*self.up.lock().expect("switch") {
            return Err(IdReservationError::new("store unreachable"));
        }
        // Another node took the next 100 ids: this block skips them.
        IdBlock::new(floor + 100, floor + 102).ok_or_else(|| IdReservationError::new("empty"))
    }
}

fn reserving_cluster(switch: &Switch) -> (MinerCluster, SharedRecordSink) {
    let records = SharedRecordSink::new();
    let cluster = MinerCluster::new(
        MinerConfig::default().with_upstream_templates(ourios_config::UpstreamTemplates::Adopt),
    )
    .with_record_sink(Box::new(records.clone()))
    .with_id_reserver(Box::new(switch.clone()));
    (cluster, records)
}

#[test]
fn ids_come_from_reserved_blocks_only() {
    let switch = Switch::new();
    let (mut cluster, _) = reserving_cluster(&switch);
    let t = TenantId::new("t");
    let ids: Vec<u64> = [
        "alpha one two",
        "beta three four five",
        "gamma six",
        "delta seven eight nine ten",
    ]
    .iter()
    .map(|line| cluster.ingest(&string_record(&t, line)))
    .collect();
    assert_eq!(ids, [101, 102, 203, 204]);
    assert_eq!(switch.floors(), [0, 102]);
}

#[test]
fn a_string_mint_without_a_reservable_id_fails_parse_with_its_body() {
    let switch = Switch::new();
    let (mut cluster, records) = reserving_cluster(&switch);
    let t = TenantId::new("t");
    let known = cluster.ingest(&string_record(&t, "alpha one two"));
    cluster.ingest(&string_record(&t, "beta three four five"));
    drop(records.drain());
    switch.set_up(false);

    let fresh = cluster.ingest(&string_record(&t, "gamma six"));
    let attached = cluster.ingest(&string_record(&t, "alpha one two"));

    assert_eq!(fresh, NO_TEMPLATE, "no id it cannot prove unique");
    assert_eq!(attached, known, "a known shape still attaches");
    let rows = records.drain();
    assert_eq!(rows[0].body.as_deref(), Some("gamma six"), "body retained");
    assert_eq!(cluster.parse_failures_total(), 1);

    switch.set_up(true);
    assert_eq!(cluster.ingest(&string_record(&t, "gamma six")), 203);
}

#[test]
fn a_structured_first_sight_without_a_reservable_id_is_kept_verbatim() {
    let switch = Switch::new();
    let (mut cluster, records) = reserving_cluster(&switch);
    let t = TenantId::new("t");
    switch.set_up(false);

    let id = cluster.ingest(&structured_record(&t, 9, Some("scope")));

    assert_eq!(id, NO_TEMPLATE);
    let rows = records.drain();
    assert!(rows[0].body.is_some(), "the structured body is verbatim");
    assert_eq!(cluster.parse_failures_total(), 1);
    switch.set_up(true);
    assert_eq!(
        cluster.ingest(&structured_record(&t, 9, Some("scope"))),
        101
    );
}

#[test]
fn an_adoption_without_a_reservable_id_is_mined_instead() {
    let switch = Switch::new();
    let (mut cluster, _) = reserving_cluster(&switch);
    let t = TenantId::new("t");
    switch.set_up(false);
    let mut rec = string_record(&t, "user 7 logged in");
    rec.attributes.push(ourios_core::otlp::KeyValue {
        key: LOG_RECORD_TEMPLATE_ATTR.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::StringValue("user <*> logged in".to_string())),
        }),
        ..Default::default()
    });

    assert_eq!(cluster.ingest(&rec), NO_TEMPLATE);
    assert_eq!(cluster.parse_failures_total(), 1);
}

#[test]
fn restoring_an_id_at_u64_max_is_rejected() {
    let t = TenantId::new("t");
    let mut original = MinerCluster::new(MinerConfig::default());
    original.ingest(&structured_record(&t, 9, None));
    let mut state = original.snapshot_state(&t);
    state.structured_templates[0].template_id = u64::MAX;
    let err = MinerCluster::new(MinerConfig::default())
        .restore_tenant(&t, &state)
        .expect_err("no id is left above u64::MAX");
    assert!(err.to_string().contains("i64::MAX"), "{err}");
}

#[test]
fn restoring_an_id_past_the_i64_domain_is_rejected_and_at_it_accepted() {
    let t = TenantId::new("t");
    let mut original = MinerCluster::new(MinerConfig::default());
    original.ingest(&structured_record(&t, 9, None));
    let mut state = original.snapshot_state(&t);
    state.structured_templates[0].template_id = MAX_TEMPLATE_ID + 1;
    assert!(
        MinerCluster::new(MinerConfig::default())
            .restore_tenant(&t, &state)
            .is_err()
    );
    state.structured_templates[0].template_id = MAX_TEMPLATE_ID;
    let mut restored = MinerCluster::new(MinerConfig::default());
    restored
        .restore_tenant(&t, &state)
        .expect("the last id restores");
    assert_eq!(restored.highest_allocated(), MAX_TEMPLATE_ID);
    assert_eq!(
        restored.ingest(&structured_record(&t, 10, None)),
        NO_TEMPLATE,
        "the domain is exhausted"
    );
}
