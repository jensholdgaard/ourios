//! What a rig ingests, cuts and reclaims before it stops.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::BarrierRig;

/// A `ResourceLogs` for `service` with one structured record whose event
/// name is `event`: its template id has no audit event (RFC 0001 §6.2
/// step 0).
pub fn structured_logs(
    service: &str,
    event: &str,
) -> opentelemetry_proto::tonic::logs::v1::ResourceLogs {
    let mut logs = resource_logs(service, &["placeholder"]);
    let record = &mut logs.scope_logs[0].log_records[0];
    record.body = Some(AnyValue {
        value: Some(Value::IntValue(7)),
    });
    event.clone_into(&mut record.event_name);
    logs
}

/// Ingest `bodies` and one structured record per `events` for `tenant`.
pub async fn publish(rig: &BarrierRig, tenant: &str, bodies: &[&str], events: &[&str]) {
    for body in bodies {
        rig.ingest(tenant, &[body]).await;
    }
    for event in events {
        rig.pipeline
            .ingest(
                request(vec![structured_logs(tenant, event)]),
                TenantId::new(tenant),
            )
            .await
            .expect("the batch acks");
    }
}

/// Let the open segment age out and cut, so the idle rotation seals it
/// and the checkpoint stamps, reclaiming nothing.
pub async fn cut(rig: &BarrierRig) {
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
}

/// [`cut`], then one housekeeping pass that reclaims what the cut
/// covered.
pub async fn cut_and_reclaim(rig: &BarrierRig) {
    cut(rig).await;
    let housekeeper = Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
    );
    let HousekeepingTick::Completed(pass) = housekeeper.tick() else {
        panic!("the pass runs");
    };
    assert!(pass.removed_segments > 0, "the pass reclaimed: {pass:?}");
}
