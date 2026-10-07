//! A receiver's roots, and its restart through startup recovery.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ourios_config::MinerConfig;
use ourios_core::audit::SharedAuditSink;
use ourios_core::record::SharedRecordSink;
use ourios_ingester::recovery::{self, RecoveryDriverError};
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig};

use super::{Restarted, audit_bindings, rows};
use crate::rfc0052_barrier_support::{BarrierRig, RigSpec, wal_config};

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
