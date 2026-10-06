//! RFC0059.10 — Ids increase per allocator across restarts.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::sync::{Arc, Mutex};

use ourios_config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{
    IdBlock, IdReservationError, IdReserver, IdSpaceExhausted, MAX_TEMPLATE_ID, MinerCluster,
};

/// A stand-in for the store's high-water: blocks of four above whatever
/// it last granted.
#[derive(Clone, Default)]
struct HighWater(Arc<Mutex<u64>>);

impl IdReserver for HighWater {
    fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError> {
        let mut granted = self.0.lock().expect("high-water");
        let after = (*granted).max(floor);
        *granted = after + 4;
        IdBlock::new(after, *granted).ok_or_else(|| IdReservationError::new("empty"))
    }
}

fn line(tenant: &TenantId, text: &str) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(text.to_owned())),
        ..Default::default()
    }
}

/// Scenario RFC0059.10 — an allocator's ids strictly increase in issue
/// order across a restart.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_10_ids_strictly_increase_across_restarts() {
    let tenant = TenantId::new("checkout");
    let high_water = HighWater::default();
    let mut before =
        MinerCluster::new(MinerConfig::default()).with_id_reserver(Box::new(high_water.clone()));
    let mut issued: Vec<u64> = ["alpha one", "beta two three", "gamma four"]
        .iter()
        .map(|text| before.ingest(&line(&tenant, text)))
        .collect();
    let state = before.snapshot_state(&tenant);
    let reserved = *high_water.0.lock().expect("high-water");

    // The restart: restore, seat above the high-water, mint more.
    let mut after =
        MinerCluster::new(MinerConfig::default()).with_id_reserver(Box::new(high_water.clone()));
    after.restore_tenant(&tenant, &state).expect("restore");
    after.allocate_past_issued(reserved).expect("seat");
    issued.extend(
        ["delta five six seven", "epsilon eight"]
            .iter()
            .map(|text| after.ingest(&line(&tenant, text))),
    );

    assert!(
        issued.windows(2).all(|pair| pair[0] < pair[1]),
        "issue order is id order: {issued:?}"
    );
    assert!(issued[3] > reserved, "the restart skips the unused block");
}

/// Scenario RFC0059.10 — no id above `i64::MAX` is ever issued.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_10_no_id_above_i64_max_is_issued() {
    let tenant = TenantId::new("checkout");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    assert_eq!(
        cluster.allocate_past_issued(u64::MAX),
        Err(IdSpaceExhausted)
    );
    assert_eq!(
        cluster.allocate_past_issued(MAX_TEMPLATE_ID + 1),
        Err(IdSpaceExhausted)
    );

    cluster
        .allocate_past_issued(MAX_TEMPLATE_ID - 1)
        .expect("seat below the top");
    assert_eq!(
        cluster.ingest(&line(&tenant, "alpha one")),
        MAX_TEMPLATE_ID,
        "the last id in the domain is issued"
    );
    assert_eq!(
        cluster.ingest(&line(&tenant, "beta two three")),
        0,
        "no id is left to issue, so the mint fails parse"
    );
}
