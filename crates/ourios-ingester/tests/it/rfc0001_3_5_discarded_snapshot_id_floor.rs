//! RFC 0001 §3.5 / §6.9 with RFC 0052 reclamation: a tenant whose
//! snapshot recovery discards is rebuilt from the frames that survived,
//! and the template ids it mints afterwards must never be ids the audit
//! stream already binds to another template (issue #898, `CLAUDE.md`
//! §3.1). Re-minting a shape first seen in a reclaimed frame under a new
//! id is hazard #5 drift; re-issuing an old id is a silent merge.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_core::audit::{AuditEvent, AuditPayload, TEMPLATE_INITIAL_VERSION, TemplateChange};
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::recovery::{self, RecoveryDriverError, RecoveryReport};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{AuditReader, Store};
use ourios_wal::{Wal, WalConfig};

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{BarrierRig, RigSpec, parquet_files, wal_config};

const TENANT: &str = "checkout";
const OTHER: &str = "search";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_discarded_snapshot_never_reissues_a_published_template_id() {
    // Given two templates minted, cut, published and their frames reclaimed.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    rig.ingest(TENANT, &["user alice logged in"]).await;
    rig.ingest(TENANT, &["disk sda1 is 91 percent full"]).await;
    seal_and_cut(&rig).await;
    assert_eq!(housekeeping_pass(&rig).removed_segments, 1);
    let node = Node::stop(rig);
    let issued = template_ids(&node.audit_events());
    assert_eq!(issued.len(), 2, "both templates are in the audit stream");

    // When the snapshot is discarded and the node restarts and mints.
    node.discard_snapshot(TENANT);
    let (report, after) = node.restart_and_mine(&[
        (TENANT, "order 7 shipped to berlin"),
        (TENANT, "user alice logged in"),
    ]);
    assert_eq!(report.accepted_horizons(), [], "the snapshot was discarded");
    assert_eq!(report.records_fed_to_miner, 0, "every frame was reclaimed");
    assert_eq!(report.issued_template_id_floor, issued.last().copied());

    // Then no new template reuses an id the audit stream already carries,
    // and no (template_id, version) names two templates.
    let minted = template_ids(&after);
    assert!(
        minted.is_disjoint(&issued),
        "re-issued ids {:?} (issued before the restart: {issued:?})",
        minted.intersection(&issued).collect::<Vec<_>>(),
    );
    assert_no_collisions(&node.audit_events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_tenant_does_not_mask_a_discarded_tenants_higher_ids() {
    // Given a tenant whose ids sit above every id a second tenant's
    // snapshot restores, all frames reclaimed.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    rig.ingest(OTHER, &["query 7 served"]).await;
    rig.ingest(TENANT, &["user alice logged in"]).await;
    rig.ingest(TENANT, &["disk sda1 is 91 percent full"]).await;
    seal_and_cut(&rig).await;
    assert_eq!(housekeeping_pass(&rig).removed_segments, 1);
    let node = Node::stop(rig);
    let issued = template_ids(&node.audit_events());

    // When only that tenant's snapshot is discarded.
    node.discard_snapshot(TENANT);
    let (report, after) = node.restart_and_mine(&[
        (TENANT, "order 7 shipped to berlin"),
        (OTHER, "cache evicted 5 keys"),
    ]);
    assert_eq!(
        report.accepted_horizons().len(),
        1,
        "the other tenant restores"
    );

    // Then the restored tenant's ids do not stand in for the floor.
    let minted = template_ids(&after);
    assert!(minted.is_disjoint(&issued), "{minted:?} vs {issued:?}");
    assert_no_collisions(&node.audit_events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaimed_frames_without_a_snapshot_over_them_seat_the_floor() {
    // Given a tenant's frames reclaimed under a snapshot that is then
    // removed from outside the node, so recovery sees no artefact at all.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    rig.ingest(TENANT, &["user alice logged in"]).await;
    seal_and_cut(&rig).await;
    assert_eq!(housekeeping_pass(&rig).removed_segments, 1);
    let node = Node::stop(rig);
    let issued = template_ids(&node.audit_events());
    std::fs::remove_file(node.snapshots.join(format!("{TENANT}.snap"))).expect("remove");

    // When the node restarts and mints a new shape.
    let (report, after) = node.restart_and_mine(&[(TENANT, "order 7 shipped to berlin")]);

    // Then the `RECLAIM` record alone is enough to read the floor.
    assert!(report.tenants.is_empty(), "no artefact was found");
    assert_eq!(report.issued_template_id_floor, issued.last().copied());
    assert!(template_ids(&after).is_disjoint(&issued));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_audit_stream_fails_recovery_only_when_the_floor_is_needed() {
    // Given a cut node whose audit stream holds a file that does not decode.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    rig.ingest(TENANT, &["user alice logged in"]).await;
    seal_and_cut(&rig).await;
    assert_eq!(housekeeping_pass(&rig).removed_segments, 1);
    let node = Node::stop(rig);
    let torn = node.audit.join("audit").join(format!("tenant_id={TENANT}"));
    std::fs::write(torn.join("torn.parquet"), b"not parquet").expect("torn audit file");

    // Then a restart whose every snapshot restores never reads the stream.
    let report = node.recover().expect("no floor is needed");
    assert_eq!(report.issued_template_id_floor, None);

    // And one that must seat the floor refuses to start rather than
    // guess it.
    node.discard_snapshot(TENANT);
    let err = node
        .recover()
        .expect_err("a floor over a partial read is no floor");
    assert!(matches!(err, RecoveryDriverError::IssuedIds(_)), "{err}");
}

/// A stopped node's roots.
struct Node {
    wal: PathBuf,
    audit: PathBuf,
    snapshots: PathBuf,
}

impl Node {
    /// Stop `rig` with no shutdown write — what a kill leaves.
    fn stop(rig: BarrierRig) -> Self {
        let node = Self {
            wal: rig.wal_root.clone(),
            audit: rig.audit_root.clone(),
            snapshots: rig.snapshots_root.clone(),
        };
        drop(rig);
        node
    }

    /// Replace `tenant`'s artefact with one recovery discards as corrupt.
    fn discard_snapshot(&self, tenant: &str) {
        let artefact = self.snapshots.join(format!("{tenant}.snap"));
        assert!(
            artefact.exists(),
            "the cut installed {}",
            artefact.display()
        );
        std::fs::write(artefact, b"not a snapshot").expect("discardable artefact");
    }

    /// Run startup recovery the way `serve` wires it, mine `lines`, and
    /// return the report plus the audit events the restarted miner emitted.
    fn restart_and_mine(&self, lines: &[(&str, &str)]) -> (RecoveryReport, Vec<AuditEvent>) {
        let before: HashSet<PathBuf> = parquet_files(&self.audit).into_iter().collect();
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&self.audit).expect("audit store"),
            100_000,
        ));
        let mut miner =
            MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()));
        let report = self.recover_into(&mut miner).expect("recover");
        for (tenant, body) in lines {
            let tenant = TenantId::new(*tenant);
            for record in assign(
                request(vec![resource_logs(tenant.as_str(), &[body])]),
                &tenant,
            ) {
                miner.ingest(&record);
            }
        }
        assert!(audit.flush(), "the restarted miner's events land");
        let after = parquet_files(&self.audit)
            .into_iter()
            .filter(|path| !before.contains(path))
            .flat_map(|path| read_audit(&path))
            .collect();
        (report, after)
    }

    /// Run startup recovery into a fresh miner.
    fn recover(&self) -> Result<RecoveryReport, RecoveryDriverError> {
        self.recover_into(&mut MinerCluster::new(MinerConfig::default()))
    }

    fn recover_into(
        &self,
        miner: &mut MinerCluster,
    ) -> Result<RecoveryReport, RecoveryDriverError> {
        let mut wal = Wal::open(WalConfig {
            segment_age_secs: 1,
            ..wal_config(&self.wal)
        })
        .expect("reopen");
        let store = Store::local(&self.audit).expect("audit store");
        recovery::recover(&mut wal, &self.snapshots, miner, &store)
    }

    fn audit_events(&self) -> Vec<AuditEvent> {
        parquet_files(&self.audit)
            .iter()
            .flat_map(|path| read_audit(path))
            .collect()
    }
}

fn read_audit(path: &Path) -> Vec<AuditEvent> {
    AuditReader::open_file(path)
        .expect("open audit file")
        .read_all()
        .expect("read audit file")
}

/// The `(version, template)` a template event binds.
fn binding(change: &TemplateChange) -> Option<(u32, &str)> {
    match change {
        TemplateChange::Created { new_template } => Some((TEMPLATE_INITIAL_VERSION, new_template)),
        TemplateChange::Widened {
            new_version,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            new_version,
            new_template,
            ..
        } => Some((*new_version, new_template)),
        TemplateChange::Adopted {
            template_version,
            new_template,
        } => Some((*template_version, new_template)),
        TemplateChange::RejectedDegenerate { .. } => None,
    }
}

/// Every template id a template event in `events` binds.
fn template_ids(events: &[AuditEvent]) -> BTreeSet<u64> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            AuditPayload::Template {
                template_id,
                change,
                ..
            } => binding(change).map(|_| *template_id),
            _ => None,
        })
        .collect()
}

/// No `(template_id, version)` is bound to two distinct templates.
fn assert_no_collisions(events: &[AuditEvent]) {
    let mut bound: BTreeMap<(u64, u32), BTreeSet<&str>> = BTreeMap::new();
    for event in events {
        if let AuditPayload::Template {
            template_id,
            change,
            ..
        } = &event.payload
            && let Some((version, template)) = binding(change)
        {
            bound
                .entry((*template_id, version))
                .or_default()
                .insert(template);
        }
    }
    let collisions: Vec<_> = bound.iter().filter(|(_, texts)| texts.len() > 1).collect();
    assert!(collisions.is_empty(), "colliding bindings: {collisions:?}");
}

fn housekeeping_pass(rig: &BarrierRig) -> ourios_wal::HousekeepingProgress {
    let housekeeper = Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
    );
    let HousekeepingTick::Completed(pass) = housekeeper.tick() else {
        panic!("the pass runs");
    };
    pass
}

/// Let the open segment age out, then take a cut whose idle rotation
/// seals it.
async fn seal_and_cut(rig: &BarrierRig) {
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
}

fn aging_wal(tmp: &Path) -> WalConfig {
    WalConfig {
        segment_age_secs: 1,
        ..wal_config(&tmp.join("wal"))
    }
}
