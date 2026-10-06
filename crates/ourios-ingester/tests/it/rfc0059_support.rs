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
use ourios_core::audit::{AuditPayload, SharedAuditSink, TEMPLATE_INITIAL_VERSION, TemplateChange};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::recovery::{self, RecoveryDriverError, RecoveryReport};
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig};

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{BarrierRig, RigSpec, wal_config};
pub use crate::rfc0052_barrier_support::{audit_events, rows};

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
        self.restart_with(store, true)
    }

    /// [`Self::restart_over`], with the RFC 0059 §3.5 bootstrap
    /// authorisation set as given.
    pub fn restart_with(
        &self,
        store: Store,
        allow_bootstrap: bool,
    ) -> Result<Restarted, RecoveryDriverError> {
        let ids = TemplateIds::new(store).with_bootstrap_allowed(allow_bootstrap);
        let records = SharedRecordSink::new();
        let audit = SharedAuditSink::new();
        let mut miner = MinerCluster::new(MinerConfig::default())
            .with_record_sink(Box::new(records.clone()))
            .with_id_reserver(ids.reserver());
        drop(miner.replace_audit_sink(Box::new(audit.clone())));
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
            audit,
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

/// How long the startup crash fixture has to reach its kill point.
const FIXTURE_WATCHDOG: Duration = Duration::from_secs(60);

/// The startup crash fixture as a real OS process over a node's roots,
/// killed and reaped however the test ends, since it never exits on its
/// own.
pub struct Fixture {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Fixture {
    /// Spawn the fixture in `mode` (`reserve` or `scan`) over `node`, with
    /// a thread forwarding its stdout lines. The thread keeps the pipe open
    /// until the child dies, so the kill, not a failed write, ends it.
    pub fn spawn(mode: &str, node: &Node) -> Self {
        use std::io::BufRead;

        let mut child =
            std::process::Command::new(env!("CARGO_BIN_EXE_template_ids_crash_fixture"))
                .arg(mode)
                .arg(&node.wal)
                .arg(&node.store)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn the template-id crash fixture");
        let stdout = child.stdout.take().expect("fixture stdout piped");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    /// Wait for the fixture's first line, which must start with `prefix`.
    pub fn reached(&self, prefix: &str) -> String {
        match self.lines.recv_timeout(FIXTURE_WATCHDOG) {
            Ok(line) if line.starts_with(prefix) => line,
            Ok(line) => panic!("the fixture printed {line:?} before {prefix}"),
            Err(e) => panic!("the fixture never printed {prefix}: {e}"),
        }
    }

    /// `SIGKILL` the child and reap it.
    pub fn kill(&mut self) {
        self.child.kill().expect("SIGKILL the fixture");
        self.child.wait().expect("reap the fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Already reaped on the passing path; these only matter on a failing one.
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

/// A restarted receiver.
pub struct Restarted {
    pub report: RecoveryReport,
    pub miner: MinerCluster,
    pub records: SharedRecordSink,
    /// What recovery and the miner published to the audit sink.
    pub audit: SharedAuditSink,
    _ids: TemplateIds,
}

impl Restarted {
    /// Mine one string line for `tenant`, returning its template id.
    pub fn mine(&mut self, tenant: &str, body: &str) -> u64 {
        self.mine_logs(tenant, resource_logs(tenant, &[body]))
    }

    /// Mine one structured record for `tenant`, keyed by `event`.
    pub fn mine_structured(&mut self, tenant: &str, event: &str) -> u64 {
        self.mine_logs(tenant, structured_logs(tenant, event))
    }

    /// Mine `logs` for `tenant`, returning its last record's template id.
    fn mine_logs(
        &mut self,
        tenant: &str,
        logs: opentelemetry_proto::tonic::logs::v1::ResourceLogs,
    ) -> u64 {
        let tenant = TenantId::new(tenant);
        assign(request(vec![logs]), &tenant)
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

/// Every template id `cluster` holds, per tenant.
pub fn ids_per_tenant(cluster: &MinerCluster) -> BTreeMap<String, BTreeSet<u64>> {
    cluster
        .tenant_ids()
        .into_iter()
        .map(|tenant| {
            let ids = ids_by_content(&cluster.snapshot_state(&tenant))
                .into_values()
                .collect();
            (tenant.as_str().to_owned(), ids)
        })
        .collect()
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
    /// Let another writer create the high-water right after the next read
    /// that finds it absent: the window between a start's trust decision
    /// and its seat.
    pub create_after_absent_read: Arc<std::sync::atomic::AtomicBool>,
    /// Refuse every call as an S3 `403` would.
    pub denied: Arc<std::sync::atomic::AtomicBool>,
    /// Reads of the high-water attempted, whether or not they succeed.
    pub high_water_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// When set to `n`, the high-water's `n`th write from now fails, and
    /// every later one with it; 0 (the default) never fails one.
    pub high_water_puts_until_failure: Arc<std::sync::atomic::AtomicUsize>,
}

/// What another writer leaves in the high-water when it wins a race.
const WINNER: &[u8] = br#"{"reserved_through": 1}"#;

fn is_high_water(location: &object_store::path::Path) -> bool {
    location.as_ref() == ourios_ingester::template_ids::HIGH_WATER_KEY
}

fn take(flag: &std::sync::atomic::AtomicBool) -> bool {
    flag.swap(false, std::sync::atomic::Ordering::AcqRel)
}

impl Hooks {
    pub fn wrap(&self, store: Store) -> Store {
        let hooks = self.clone();
        store.wrap_backend(move |inner| Arc::new(HookedStore { inner, hooks }))
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, std::sync::atomic::Ordering::Release);
    }

    /// Fail the high-water write `high_water_puts_until_failure` counts
    /// down to.
    fn count_down_put(&self) -> object_store::Result<()> {
        let left = &self.high_water_puts_until_failure;
        match left.load(std::sync::atomic::Ordering::Acquire) {
            0 => Ok(()),
            1 => Err(object_store::Error::Generic {
                store: "hooked",
                source: "the high-water write fails".into(),
            }),
            n => {
                left.store(n - 1, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        }
    }

    fn enter(&self) -> object_store::Result<()> {
        if self.denied.load(std::sync::atomic::Ordering::Acquire) {
            return Err(object_store::Error::PermissionDenied {
                path: "hooked".to_owned(),
                source: "AccessDenied".into(),
            });
        }
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

impl HookedStore {
    /// Another writer creates the high-water, once, if `flag` is armed.
    async fn win_if(
        &self,
        flag: &std::sync::atomic::AtomicBool,
        location: &object_store::path::Path,
    ) -> object_store::Result<()> {
        if !take(flag) {
            return Ok(());
        }
        self.inner
            .put_opts(
                location,
                WINNER.to_vec().into(),
                object_store::PutOptions::default(),
            )
            .await
            .map(|_| ())
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
        if is_high_water(location) {
            self.hooks.count_down_put()?;
        }
        let creating = matches!(opts.mode, object_store::PutMode::Create);
        if creating && is_high_water(location) {
            self.win_if(&self.hooks.race_the_create, location).await?;
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
        if is_high_water(location) {
            self.hooks
                .high_water_reads
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.hooks.enter()?;
        let got = self.inner.get_opts(location, options).await;
        let absent = matches!(got, Err(object_store::Error::NotFound { .. }));
        if absent && is_high_water(location) {
            self.win_if(&self.hooks.create_after_absent_read, location)
                .await?;
        }
        got
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
