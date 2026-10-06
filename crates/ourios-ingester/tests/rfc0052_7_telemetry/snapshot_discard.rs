//! Startup recovery names each snapshot it discards (#884): one
//! `ourios.receiver.snapshot.discarded` per discarded tenant, carrying
//! the tenant and why, and none for a tenant it restored.

use std::path::Path;

use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::recovery::{self, DiscardReason, SnapshotFate};
use ourios_ingester::snapshot_store;
use ourios_miner::cluster::{MinerCluster, RestoreError};
use ourios_miner::snapshot::{SNAPSHOT_VERSION, SnapshotState};
use ourios_semconv as semconv;
use ourios_telemetry::live_check::{self, Checked, Event, EventSpec};
use ourios_wal::{Wal, WalOffset};

use crate::harness::{harness, serial};
use crate::ingest_support::{request, resource_logs, template_ids, wal_config};

const DISCARDED: EventSpec = EventSpec {
    name: semconv::EVENT_OURIOS_RECEIVER_SNAPSHOT_DISCARDED,
    required: &[semconv::OURIOS_TENANT, "error.type"],
    optional: &[],
};

const SEGMENT: &str = "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0";

/// The discarded tenants, each with the `error.type` its artefact earns.
const DISCARDS: [(&str, &str); 5] = [
    ("legacy", "unknown_version"),
    ("garbled", "corrupt"),
    ("blank", "empty"),
    ("unmarked", "no_horizon"),
    ("rejected", "restore_failed"),
];

fn horizon() -> WalOffset {
    WalOffset {
        segment: uuid::Uuid::parse_str(SEGMENT).expect("test uuid"),
        byte: 64,
    }
}

fn artefact(root: &Path, tenant: &str, bytes: &[u8]) {
    std::fs::create_dir_all(root).expect("snapshots root");
    std::fs::write(root.join(format!("{tenant}.snap")), bytes).expect("artefact");
}

/// A tenant's state holding one mined template.
fn mined_state(tenant: &TenantId) -> SnapshotState {
    let mut miner = MinerCluster::new(MinerConfig::default());
    let lines = request(vec![resource_logs("checkout", &["user 1 logged in"])]);
    for record in assign(lines, tenant) {
        miner.ingest(&record);
    }
    miner.snapshot_state(tenant)
}

/// One artefact per discard class, plus one that restores.
fn write_artefacts(root: &Path) {
    let v1 = format!(r#"{{"wal_high_water":{{"segment":"{SEGMENT}","byte":64}}}}"#);
    artefact(root, "legacy", &[&[1u8][..], v1.as_bytes()].concat());
    artefact(root, "garbled", &[SNAPSHOT_VERSION, b'{', b'!']);
    artefact(root, "blank", &[]);

    let unmarked = TenantId::new("unmarked");
    let mut state = mined_state(&unmarked);
    state.wal_high_water = None;
    snapshot_store::write(root, &unmarked, &state).expect("unmarked");

    let rejected = TenantId::new("rejected");
    let mut state = mined_state(&rejected);
    let leaf = state.leaves.first().cloned().expect("a mined leaf");
    state.leaves.push(leaf);
    state.wal_high_water = Some(snapshot_store::high_water(horizon()));
    snapshot_store::write(root, &rejected, &state).expect("rejected");

    let restored = TenantId::new("restored");
    let mut state = mined_state(&restored);
    state.wal_high_water = Some(snapshot_store::high_water(horizon()));
    snapshot_store::write(root, &restored, &state).expect("restored");
}

fn discards_of<'a>(events: &'a [Event], tenant: &str) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|e| {
            e.name == DISCARDED.name
                && e.attributes.get(semconv::OURIOS_TENANT).map(String::as_str) == Some(tenant)
        })
        .collect()
}

/// Every discard class is named once for its tenant, the restored tenant
/// not at all, and each emission is registry-conformant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_names_each_discarded_snapshot_once() {
    let _serial = serial().await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let snapshots = tmp.path().join("snapshots");
    write_artefacts(&snapshots);

    let mut wal = Wal::open(wal_config(&tmp.path().join("wal"))).expect("open");
    let mut miner = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(&mut wal, &snapshots, &mut miner, &template_ids(tmp.path()))
        .expect("recover");
    let events = harness().events();

    for (tenant, class) in DISCARDS {
        let named = discards_of(&events, tenant);
        assert_eq!(named.len(), 1, "{tenant}: {events:#?}");
        assert_eq!(named[0].error_type(), Some(class), "{tenant}");
        let fate = report
            .tenants
            .iter()
            .find(|t| t.tenant_id.as_str() == tenant)
            .map(|t| &t.fate);
        assert!(
            matches!(fate, Some(SnapshotFate::Discarded(reason)) if reason.error_type() == class),
            "{tenant}: {fate:?}"
        );
    }
    assert_eq!(
        report.accepted_horizons(),
        vec![(TenantId::new("restored"), horizon())],
        "only the control restored"
    );
    let legacy = discards_of(&events, "legacy");
    let body = legacy[0].body.as_deref().unwrap_or_default();
    assert!(
        body.contains("snapshot format version 1"),
        "the v1 discard names its version byte: {body:?}"
    );
    assert!(
        body.contains("will be rebuilt next") && body.contains("if the restart succeeds"),
        "replay and the legacy check still follow, and either can fail startup: {body:?}"
    );
    let rejected = discards_of(&events, "rejected");
    let body = rejected[0].body.as_deref().unwrap_or_default();
    assert!(
        body.contains("restore_failed: inconsistent snapshot: template_id")
            && body.contains("appears more than once"),
        "the restore_failed discard names why the miner rejected it: {body:?}"
    );
    assert!(
        discards_of(&events, "restored").is_empty(),
        "a restored tenant is not announced: {events:#?}"
    );
    assert_eq!(
        events.iter().filter(|e| e.name == DISCARDED.name).count(),
        DISCARDS.len(),
        "one event per discarded tenant"
    );

    let checked = live_check::live_check(&events, &[DISCARDED])
        .expect("the discard event is registry-conformant");
    if checked == Checked::SpecOnly {
        eprintln!("#884: weaver is not configured here; checked the event against its spec only");
    }
}

#[test]
fn every_discard_reason_has_its_registry_error_type() {
    for (reason, class) in [
        (DiscardReason::UnknownVersion(1), "unknown_version"),
        (DiscardReason::Corrupt, "corrupt"),
        (DiscardReason::Empty, "empty"),
        (DiscardReason::NoHorizon, "no_horizon"),
        (
            DiscardReason::RestoreFailed(RestoreError::TenantAlreadyLive),
            "restore_failed",
        ),
        (DiscardReason::Other, "_OTHER"),
    ] {
        assert_eq!(reason.error_type(), class);
    }
    assert_eq!(
        DiscardReason::UnknownVersion(1).to_string(),
        "snapshot format version 1",
        "the message names the version byte; error.type does not"
    );
}
