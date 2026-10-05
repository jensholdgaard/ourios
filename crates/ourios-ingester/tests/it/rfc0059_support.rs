//! Shared rig for the RFC 0059 scenarios: a receiver that mints, cuts,
//! publishes and reclaims through the production barrier and
//! housekeeping, then restarts through startup recovery against the
//! store's template-id high-water.

// The shared-`tests/` module shape: each scenario uses part of the rig.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_config::MinerConfig;
use ourios_core::audit::{AuditEvent, AuditPayload, TEMPLATE_INITIAL_VERSION, TemplateChange};
use ourios_core::record::{MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::recovery::{self, RecoveryDriverError, RecoveryReport};
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{AuditReader, Reader, Store};
use ourios_wal::{Wal, WalConfig};

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{BarrierRig, RigSpec, parquet_files, wal_config};

/// A receiver's roots: its WAL (and snapshots beneath it), and the one
/// store its data and audit streams publish to.
pub struct Node {
    pub tmp: PathBuf,
    pub wal: PathBuf,
    pub snapshots: PathBuf,
    pub store: PathBuf,
}

impl Node {
    /// A rig whose segments age out after a second, so a cut can seal
    /// them for housekeeping.
    pub fn rig(tmp: &Path) -> BarrierRig {
        BarrierRig::build(
            tmp,
            RigSpec::new(WalConfig {
                segment_age_secs: 1,
                ..wal_config(&tmp.join("wal"))
            }),
        )
    }

    /// Stop `rig` with no shutdown write, and gather its data and audit
    /// objects into one store, the production layout.
    pub fn stop(rig: BarrierRig, tmp: &Path) -> Self {
        Self::stop_into(rig, tmp, &tmp.join("store"))
    }

    /// [`Self::stop`], gathering into `store`, which another node may
    /// share.
    pub fn stop_into(rig: BarrierRig, tmp: &Path, store: &Path) -> Self {
        let node = Self {
            tmp: tmp.to_path_buf(),
            wal: rig.wal_root.clone(),
            snapshots: rig.snapshots_root.clone(),
            store: store.to_path_buf(),
        };
        let (data, audit) = (rig.data_root.clone(), rig.audit_root.clone());
        drop(rig);
        copy_tree(&data, &node.store);
        copy_tree(&audit, &node.store);
        node
    }

    /// A receiver that never ran: an empty WAL root and an empty store.
    pub fn empty(tmp: &Path) -> Self {
        let node = Self {
            tmp: tmp.to_path_buf(),
            wal: tmp.join("wal"),
            snapshots: tmp.join("wal").join("snapshots"),
            store: tmp.join("store"),
        };
        std::fs::create_dir_all(&node.wal).expect("wal root");
        std::fs::create_dir_all(&node.store).expect("store root");
        node
    }

    /// The high-water object's bytes, if any.
    pub fn high_water_bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(
            self.store
                .join(ourios_ingester::template_ids::HIGH_WATER_KEY),
        )
        .ok()
    }

    /// Write `bytes` as the object at `key` in the store.
    pub fn put(&self, key: &str, bytes: &[u8]) {
        self.store().put_blocking(key, bytes.to_vec()).expect("put");
    }

    pub fn store(&self) -> Store {
        Store::local(&self.store).expect("store")
    }

    /// Run startup recovery into a fresh miner wired as `serve` wires it,
    /// with its record sink observable.
    pub fn restart(&self) -> Result<Restarted, RecoveryDriverError> {
        self.restart_over(self.store())
    }

    /// [`Self::restart`] over `store`, e.g. the node's store behind
    /// [`Hooks`].
    pub fn restart_over(&self, store: Store) -> Result<Restarted, RecoveryDriverError> {
        let ids = TemplateIds::new(store);
        let records = SharedRecordSink::new();
        let mut miner = MinerCluster::new(MinerConfig::default())
            .with_record_sink(Box::new(records.clone()))
            .with_id_reserver(ids.reserver());
        let mut wal = Wal::open(WalConfig {
            segment_age_secs: 1,
            ..wal_config(&self.wal)
        })
        .expect("reopen the WAL");
        let report = recovery::recover(&mut wal, &self.snapshots, &mut miner, &ids)?;
        Ok(Restarted {
            report,
            miner,
            records,
            _ids: ids,
        })
    }

    /// Replace `tenant`'s snapshot artefact with `bytes`.
    pub fn overwrite_snapshot(&self, tenant: &str, bytes: &[u8]) {
        let artefact = self.snapshots.join(format!("{tenant}.snap"));
        assert!(
            artefact.exists(),
            "the cut installed {}",
            artefact.display()
        );
        std::fs::write(artefact, bytes).expect("overwrite artefact");
    }

    /// Every id a data row or audit event in the store carries.
    pub fn issued(&self) -> BTreeSet<u64> {
        let mut ids: BTreeSet<u64> = rows(&self.store.join("data"))
            .iter()
            .map(|row| row.template_id)
            .filter(|id| *id != 0)
            .collect();
        ids.extend(
            audit_bindings(&self.store.join("audit"))
                .keys()
                .map(|(id, _)| *id),
        );
        ids
    }
}

/// A restarted receiver.
pub struct Restarted {
    pub report: RecoveryReport,
    pub miner: MinerCluster,
    pub records: SharedRecordSink,
    _ids: TemplateIds,
}

impl Restarted {
    /// Mine one string line for `tenant`, returning its template id.
    pub fn mine(&mut self, tenant: &str, body: &str) -> u64 {
        let tenant = TenantId::new(tenant);
        let records = assign(
            request(vec![resource_logs(tenant.as_str(), &[body])]),
            &tenant,
        );
        records
            .iter()
            .map(|record| self.miner.ingest(record))
            .last()
            .expect("one record")
    }

    /// Mine one structured record for `tenant`, keyed by `event`.
    pub fn mine_structured(&mut self, tenant: &str, event: &str) -> u64 {
        let tenant = TenantId::new(tenant);
        let records = assign(
            request(vec![structured_logs(tenant.as_str(), event)]),
            &tenant,
        );
        records
            .iter()
            .map(|record| self.miner.ingest(record))
            .last()
            .expect("one record")
    }

    /// Every id a record emitted since the restart carries.
    pub fn emitted(&self) -> BTreeSet<u64> {
        self.records
            .drain()
            .iter()
            .map(|r| r.template_id)
            .filter(|id| *id != 0)
            .collect()
    }
}

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

/// Let the open segment age out, cut so the idle rotation seals it, and
/// run one housekeeping pass that reclaims what the cut covered.
pub async fn cut_and_reclaim(rig: &BarrierRig) {
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
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

/// Every mined row in the Parquet files under `root`.
pub fn rows(root: &Path) -> Vec<MinedRecord> {
    parquet_files(root)
        .iter()
        .flat_map(|path| {
            Reader::open_file(path)
                .expect("open_file")
                .read_all()
                .expect("read_all")
        })
        .collect()
}

/// Every audit event in the Parquet files under `root`.
pub fn audit_events(root: &Path) -> Vec<AuditEvent> {
    parquet_files(root)
        .iter()
        .flat_map(|path| {
            AuditReader::open_file(path)
                .expect("open audit file")
                .read_all()
                .expect("read audit file")
        })
        .collect()
}

/// Every `(template_id, version)` the audit events under `root` bind, with
/// the template texts bound to it.
pub fn audit_bindings(root: &Path) -> BTreeMap<(u64, u32), BTreeSet<String>> {
    let mut bound: BTreeMap<(u64, u32), BTreeSet<String>> = BTreeMap::new();
    for event in audit_events(root) {
        if let AuditPayload::Template {
            template_id,
            change,
            ..
        } = event.payload
            && let Some((version, template)) = binding(change)
        {
            bound
                .entry((template_id, version))
                .or_default()
                .insert(template);
        }
    }
    bound
}

fn binding(change: TemplateChange) -> Option<(u32, String)> {
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
        } => Some((new_version, new_template)),
        TemplateChange::Adopted {
            template_version,
            new_template,
        } => Some((template_version, new_template)),
        TemplateChange::RejectedDegenerate { .. } => None,
    }
}

/// Copy every file under `from` into `to`, keeping relative paths.
fn copy_tree(from: &Path, to: &Path) {
    let mut pending = vec![from.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let target = to.join(path.strip_prefix(from).expect("under from"));
            if path.is_dir() {
                std::fs::create_dir_all(&target).expect("mkdir");
                pending.push(path);
            } else {
                std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
                std::fs::copy(&path, &target).expect("copy");
            }
        }
    }
}

/// RFC0059.9's contract: `recovered` equals `control` under an injective
/// renaming of ids that touches only ids first minted above `high_water`
/// (the tail replay's fresh block). Every other id, and every other field,
/// is exactly equal. Returns the ids the renaming moved.
pub fn assert_equivalent_up_to_renaming(
    recovered: &MinerCluster,
    control: &MinerCluster,
    high_water: u64,
) -> BTreeSet<u64> {
    assert_eq!(recovered.tenant_ids(), control.tenant_ids());
    let mut renamed = BTreeSet::new();
    for tenant in control.tenant_ids() {
        let (got, want) = (
            recovered.snapshot_state(&tenant),
            control.snapshot_state(&tenant),
        );
        let renaming = renaming(&want, &got);
        let mut targets = BTreeSet::new();
        for (from, to) in &renaming {
            assert!(
                targets.insert(*to),
                "{tenant:?}: the renaming is not injective at {to}"
            );
            if from != to {
                assert!(
                    *to > high_water,
                    "{tenant:?}: only ids minted above {high_water} may move ({from} -> {to})"
                );
                renamed.insert(*to);
            }
        }
        assert_eq!(
            got,
            renamed_state(&want, &renaming),
            "{tenant:?}: equal up to the renaming of tail-minted ids",
        );
    }
    renamed
}

type State = ourios_miner::snapshot::SnapshotState;

/// Each template of `state`, keyed by its content.
fn ids_by_content(state: &State) -> BTreeMap<String, u64> {
    let leaves = state.leaves.iter().map(|l| {
        let key = (&l.template, l.severity_number, &l.scope_name);
        (format!("leaf {key:?}"), l.template_id)
    });
    let structured = state.structured_templates.iter().map(|s| {
        let key = (s.severity_number, &s.scope_name, &s.event_name);
        (format!("structured {key:?}"), s.template_id)
    });
    let adopted = state.adopted_templates.iter().map(|a| {
        let key = (&a.canonical, a.severity_number, &a.scope_name);
        (format!("adopted {key:?}"), a.template_id)
    });
    leaves.chain(structured).chain(adopted).collect()
}

/// Pair each template of `from` with the template of `to` that has the
/// same content, id to id.
fn renaming(from: &State, to: &State) -> BTreeMap<u64, u64> {
    let (from, to) = (ids_by_content(from), ids_by_content(to));
    assert_eq!(
        from.keys().collect::<Vec<_>>(),
        to.keys().collect::<Vec<_>>(),
        "the same templates on both sides"
    );
    from.into_iter().map(|(key, id)| (id, to[&key])).collect()
}

fn renamed_state(state: &State, renaming: &BTreeMap<u64, u64>) -> State {
    let mut state = state.clone();
    for leaf in &mut state.leaves {
        leaf.template_id = renaming[&leaf.template_id];
    }
    for structured in &mut state.structured_templates {
        structured.template_id = renaming[&structured.template_id];
    }
    for adopted in &mut state.adopted_templates {
        adopted.template_id = renaming[&adopted.template_id];
    }
    state.leaves.sort_by_key(|l| l.template_id);
    state.structured_templates.sort_by_key(|s| s.template_id);
    state
        .adopted_templates
        .sort_by(|a, b| (a.template_id, &a.canonical).cmp(&(b.template_id, &b.canonical)));
    state
}

/// A store that can be switched off, and can let another writer win the
/// high-water's create.
#[derive(Clone, Default)]
pub struct Hooks {
    pub down: Arc<std::sync::atomic::AtomicBool>,
    pub race_the_create: Arc<std::sync::atomic::AtomicBool>,
}

impl Hooks {
    pub fn wrap(&self, store: Store) -> Store {
        let hooks = self.clone();
        store.wrap_backend(move |inner| Arc::new(HookedStore { inner, hooks }))
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, std::sync::atomic::Ordering::Release);
    }

    fn enter(&self) -> object_store::Result<()> {
        if self.down.load(std::sync::atomic::Ordering::Acquire) {
            return Err(object_store::Error::Generic {
                store: "hooked",
                source: "the store is down".into(),
            });
        }
        Ok(())
    }
}

struct HookedStore {
    inner: Arc<dyn object_store::ObjectStore>,
    hooks: Hooks,
}

impl std::fmt::Debug for HookedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HookedStore({})", self.inner)
    }
}

impl std::fmt::Display for HookedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HookedStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for HookedStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.hooks.enter()?;
        let creating = matches!(opts.mode, object_store::PutMode::Create);
        if creating
            && location.as_ref() == ourios_ingester::template_ids::HIGH_WATER_KEY
            && self
                .hooks
                .race_the_create
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            let winner = br#"{"reserved_through": 1}"#.to_vec();
            self.inner
                .put_opts(location, winner.into(), object_store::PutOptions::default())
                .await?;
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.hooks.enter()?;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.hooks.enter()?;
        self.inner.get_opts(location, options).await
    }

    async fn get_ranges(
        &self,
        location: &object_store::path::Path,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<bytes::Bytes>> {
        self.hooks.enter()?;
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.hooks.enter()?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.hooks.enter()?;
        self.inner.copy_opts(from, to, options).await
    }
}
