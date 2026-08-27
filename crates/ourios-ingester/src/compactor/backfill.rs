//! RFC 0048 §3.4 graph backfill — the store-held lock and the
//! read→derive→emit pass. Split from the flat `compactor.rs`
//! (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path resolving unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

/// The backfill lock key for `tenant` (RFC 0048 §3.4, the RFC 0005 §3.4
/// path encoding — like the erasure marker).
#[must_use]
pub fn backfill_lock_key(tenant: &str) -> String {
    format!(
        "{BACKFILL_PREFIX}tenant_id={}",
        percent_encode_tenant(tenant)
    )
}

/// Acquire the backfill lock for `tenant`: `Ok(true)` when this call
/// created it, `Ok(false)` when it already existed (another backfill —
/// or a crashed one; `release_backfill_lock` / `--unlock` clears it).
///
/// # Errors
///
/// [`IngestError::Io`] when the marker cannot be written.
pub fn acquire_backfill_lock(store: &Store, tenant: &str) -> Result<bool, IngestError> {
    let key = backfill_lock_key(tenant);
    match store.put_if_absent_blocking(&key, b"{}".to_vec()) {
        Ok(()) => Ok(true),
        Err(e) if e.is_already_exists() => Ok(false),
        Err(e) => Err(store_error("put backfill lock", &key, &e)),
    }
}

/// Release the backfill lock for `tenant` (idempotent — an absent lock is
/// already released).
///
/// # Errors
///
/// [`IngestError::Io`] when the delete fails for a reason other than
/// absence.
pub fn release_backfill_lock(store: &Store, tenant: &str) -> Result<(), IngestError> {
    let key = backfill_lock_key(tenant);
    match store.delete_blocking(&key) {
        Ok(()) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(store_error("remove backfill lock", &key, &e)),
    }
}

/// An acquired backfill lock, released on drop — so a panic, an early
/// return or a cancelled task cannot leave the tenant's erasures deferred
/// (RFC 0048 §3.4; an OS kill still needs `graph backfill --unlock`, which
/// is why that verb exists). Release failures are logged, never panicked:
/// the sweep must not die on a store hiccup.
#[cfg(feature = "openfga")]
pub(super) struct BackfillLock<'a> {
    store: &'a Store,
    tenant: &'a str,
}

#[cfg(feature = "openfga")]
impl Drop for BackfillLock<'_> {
    fn drop(&mut self) {
        if let Err(e) = release_backfill_lock(self.store, self.tenant) {
            tracing::warn!(
                "backfill lock for tenant {:?} could not be released ({e}); \
                 the tenant's erasures stay deferred until `graph backfill --unlock`",
                self.tenant,
            );
        }
    }
}

/// Whether `tenant` holds a backfill lock right now (RFC 0048 §3.4) —
/// read per erasure request rather than from a snapshot, so a lock taken
/// while the sweep runs is still observed.
///
/// # Errors
///
/// [`IngestError::Io`] when the marker cannot be read.
pub fn backfill_lock_held(store: &Store, tenant: &str) -> Result<bool, IngestError> {
    let key = backfill_lock_key(tenant);
    store
        .get_blocking_opt(&key)
        .map(|body| body.is_some())
        .map_err(|e| store_error("read backfill lock", &key, &e))
}

/// The tenants holding a backfill lock, in deterministic order.
///
/// # Errors
///
/// [`IngestError::Io`] when the lock prefix cannot be listed.
pub fn backfill_locks(store: &Store) -> Result<Vec<String>, IngestError> {
    let mut keys = store
        .list_blocking(Some(BACKFILL_PREFIX))
        .map_err(|e| store_error("list backfill locks", BACKFILL_PREFIX, &e))?;
    keys.sort();
    Ok(keys
        .into_iter()
        .filter_map(|key| {
            key.strip_prefix(BACKFILL_PREFIX)?
                .strip_prefix("tenant_id=")
                .and_then(percent_decode_tenant)
        })
        .collect())
}

/// What one `graph backfill` run did (RFC 0048 §3.4).
#[cfg(feature = "openfga")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Partitions read (after the `--from` filter).
    pub partitions: usize,
    /// Rows offered to the emitter.
    pub rows: u64,
    /// Tuples written (idempotent; an existing tuple still counts as sent).
    pub tuples: usize,
}

/// Why a backfill refused to start (RFC 0048 §3.4 / RFC0048.8).
#[cfg(feature = "openfga")]
#[derive(Debug)]
pub enum BackfillRefusal {
    /// Erasures pending for the tenant — run again after the next sweep.
    ErasuresPending(usize),
    /// Another backfill holds the lock (`graph backfill --unlock` clears a
    /// crashed run's).
    Locked,
}

#[cfg(feature = "openfga")]
impl std::fmt::Display for BackfillRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ErasuresPending(count) => write!(
                f,
                "{count} erasure(s) pending for the tenant; run again after the next sweep \
                 (RFC 0048 §3.4)"
            ),
            Self::Locked => f.write_str(
                "a backfill lock exists for the tenant (another run, or a crashed one — \
                 `graph backfill --unlock` clears it)",
            ),
        }
    }
}

/// Feed the graph from `tenant`'s stored history (RFC 0048 §3.4): every
/// partition whose hour start is ≥ `from` (when given) is read — never
/// rewritten — its rows offered to the emitter, and the derived tuples
/// written in ≤ 100-tuple idempotent batches, one emit per partition,
/// with one structured progress event each. Holds the backfill lock for
/// the duration; refuses (leaving **no** lock) while any erasure marker
/// for the tenant is pending — checked before the lock and re-checked
/// under it (RFC0048.8).
///
/// # Errors
///
/// `Ok(Err(refusal))` when the fence refuses; `Err` on store or graph
/// failures (the lock is released on the store paths that reach it).
#[cfg(feature = "openfga")]
pub async fn backfill_tenant(
    store: &Store,
    emitter: &Arc<GraphEmitter>,
    tenant: &str,
    from_unix_nanos: Option<u64>,
) -> Result<Result<BackfillReport, BackfillRefusal>, IngestError> {
    let pending = pending_erasures_for(store, tenant)?.len();
    if pending > 0 {
        return Ok(Err(BackfillRefusal::ErasuresPending(pending)));
    }
    if !acquire_backfill_lock(store, tenant)? {
        return Ok(Err(BackfillRefusal::Locked));
    }
    // From here the lock is owned: every exit — refusal, error, panic,
    // cancellation — releases it (RFC0048.8).
    let _lock = BackfillLock { store, tenant };
    // Re-check under the lock: a marker written between the check and the
    // acquire must win — refuse (RFC0048.8).
    let pending = pending_erasures_for(store, tenant)?.len();
    if pending > 0 {
        return Ok(Err(BackfillRefusal::ErasuresPending(pending)));
    }
    backfill_locked(store, emitter, tenant, from_unix_nanos)
        .await
        .map(Ok)
}

/// The read → derive → emit loop of [`backfill_tenant`], run under the
/// lock.
#[cfg(feature = "openfga")]
async fn backfill_locked(
    store: &Store,
    emitter: &Arc<GraphEmitter>,
    tenant: &str,
    from_unix_nanos: Option<u64>,
) -> Result<BackfillReport, IngestError> {
    use ourios_parquet::compaction::visit_partition_rows;
    let mut report = BackfillReport::default();
    let partitions = hour_partitions(store, tenant).map_err(IngestError::Compaction)?;
    for partition in partitions {
        if let Some(from) = from_unix_nanos
            && partition
                .hour_start_unix_nanos()
                .is_none_or(|start| start < from)
        {
            continue;
        }
        let blocking_store = store.clone();
        let blocking_partition = partition.clone();
        let blocking_tenant = tenant.to_string();
        let blocking_emitter = Arc::clone(emitter);
        // The read + derivation is blocking Parquet I/O; the emit below is
        // the async HTTP path, per partition, exactly like the sweep's.
        let (rows, tuples) = tokio::task::spawn_blocking(move || {
            let mut rows: u64 = 0;
            let mut tuples = GraphTuples::default();
            visit_partition_rows(&blocking_store, &blocking_partition, |batch| {
                rows += to_u64(batch.len());
                tuples.extend(blocking_emitter.derive(&blocking_tenant, batch));
            })
            .map(|()| (rows, tuples))
        })
        .await
        // A panic or cancellation here must not unwind past
        // `backfill_tenant`'s `release_backfill_lock` — a leaked lock
        // would defer the tenant's erasures until an operator ran
        // `--unlock` (RFC 0048 §3.4).
        .map_err(|e| IngestError::Io {
            op: "backfill read task",
            path: PathBuf::from(tenant),
            source: std::io::Error::other(e.to_string()),
        })?
        .map_err(IngestError::Compaction)?;
        let written = emitter.emit(&tuples).await.map_err(|e| IngestError::Io {
            op: "backfill emit",
            path: PathBuf::from(tenant),
            source: std::io::Error::other(e.to_string()),
        })?;
        report.partitions += 1;
        report.rows += rows;
        report.tuples += written.tuples;
        tracing::info!(
            name: ourios_semconv::EVENT_OURIOS_GRAPH_BACKFILL_PROGRESS,
            "backfill progress: tenant {:?} partition {}-{:02}-{:02}T{:02}, {} rows offered, {} tuples written",
            tenant,
            partition.year,
            partition.month,
            partition.day,
            partition.hour,
            rows,
            written.tuples,
        );
    }
    Ok(report)
}
