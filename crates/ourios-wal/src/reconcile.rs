//! RFC 0052 §3.2's open-time reconciliation: what the `CHECKPOINT`
//! version, the `RECLAIM` record and the segment headers together say
//! about a root, and what `Wal::open` must write before it goes on.
//!
//! Missing means "never reclaimed" and damaged means "reclaimed,
//! extent unknown", so the two are never collapsed: only the first is
//! safe to proceed from. Every row that cannot be told apart from a
//! loss fails closed, naming the files it read.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::{CheckpointError, OpenError, WalConfig, checkpoint, reclaim, reclaim_store, segment};

/// The geometry every `RECLAIM` file this build creates is sized for.
/// `max_tenants` and `max_unlinks_per_pass` become [`WalConfig`] knobs
/// with the housekeeping pass that reads the cap (RFC 0052 §3.8);
/// until then the RFC's own defaults are the only values, and
/// `Wal::open` still rebuilds a file built at a smaller geometry.
pub(crate) fn configured_geometry() -> Result<reclaim::Geometry, reclaim::GeometryError> {
    reclaim::Geometry::new(
        reclaim::DEFAULT_MAX_TENANTS,
        reclaim::DEFAULT_MAX_UNLINKS_PER_PASS,
    )
}

/// What RFC 0052 §3.2's open-time matrix decided about this root.
pub(crate) struct RootWitness {
    pub(crate) store: Option<reclaim_store::ReclaimStore>,
    /// A version-2 `CHECKPOINT` beside a record — the only shape a
    /// pass may plan segments under.
    pub(crate) reclaimable: bool,
}

impl RootWitness {
    /// A pre-RFC root: no record is created, and the first checkpoint
    /// makes one on the upgrade path.
    fn legacy() -> Self {
        Self {
            store: None,
            reclaimable: false,
        }
    }

    /// A record whose root still owes the version-2 upgrade.
    fn with_record(store: reclaim_store::ReclaimStore) -> Self {
        Self {
            store: Some(store),
            reclaimable: false,
        }
    }

    fn witnessed(store: reclaim_store::ReclaimStore) -> Self {
        Self {
            store: Some(store),
            reclaimable: true,
        }
    }
}

/// RFC 0052 §3.2's open-time matrix over the `CHECKPOINT` version, the
/// `RECLAIM` record and the segment headers. Missing means "never
/// reclaimed" and damaged means "reclaimed, extent unknown", so the
/// two are never collapsed: only the first is safe to proceed from.
pub(crate) fn root(
    config: &WalConfig,
    sidecar: Option<checkpoint::Sidecar>,
    segments: &[PathBuf],
) -> Result<RootWitness, OpenError> {
    let version = sidecar.map(|s| s.version);
    if reclaim_store::present(&config.root)? {
        return recorded_root(config, version, segments);
    }
    match version {
        // Every unlink is gated on a checkpoint, and on a post-RFC
        // root the record is created at open, before the first
        // checkpoint can exist — so a version-2 sidecar beside no
        // record is a loss, not a pre-RFC layout.
        Some(checkpoint::SidecarVersion::Current) => Err(lost_record(&config.root)),
        Some(checkpoint::SidecarVersion::Legacy) => Ok(RootWitness::legacy()),
        None => open_without_sidecars(config, segments),
    }
}

/// The record is there: promote or retain its checkpoint witness by
/// the `CHECKPOINT` beside it, and reconcile its `planned` entries
/// against the directory before anything reads it as proof of loss.
fn recorded_root(
    config: &WalConfig,
    version: Option<checkpoint::SidecarVersion>,
    segments: &[PathBuf],
) -> Result<RootWitness, OpenError> {
    let geometry = configured_geometry().map_err(|e| OpenError::InvalidConfig {
        field: "max_tenants",
        detail: e.to_string(),
    })?;
    let mut store =
        reclaim_store::ReclaimStore::open(&config.root, geometry, config.macos_full_fsync)?;
    match (version, store.record().witness.checkpoint) {
        (Some(checkpoint::SidecarVersion::Current), witness) => {
            promote_checkpoint_witness(&mut store, witness)?;
            planned(&mut store, &config.root)?;
            Ok(RootWitness::witnessed(store))
        }
        // The migration's own retry state: armed, no version-2
        // checkpoint, and nothing reclaimed — housekeeping is gated
        // until the witness exists — so the next checkpoint retries
        // the upgrade against the arming already on disk.
        (Some(checkpoint::SidecarVersion::Legacy), _) => {
            planned(&mut store, &config.root)?;
            Ok(RootWitness::with_record(store))
        }
        (None, reclaim::Witness::Terminal) => Err(lost_checkpoint(&config.root)),
        // Not fresh at all: the first rotation on a legacy root writes
        // the record before it creates its version-2 segment, so this
        // is that root mid migration. Emptying the record here would
        // discard a mode the first pass may already have adopted.
        (None, _) if !segments.is_empty() => {
            planned(&mut store, &config.root)?;
            Ok(RootWitness::with_record(store))
        }
        (None, _) => {
            reset_record(&mut store)?;
            Ok(RootWitness::with_record(store))
        }
    }
}

/// An armed record beside a present version-2 `CHECKPOINT` is the
/// ordinary post-upgrade state — the crash simply landed before the
/// record's next write — so open promotes it durably before anything
/// reads the matrix, and it is never a fault.
fn promote_checkpoint_witness(
    store: &mut reclaim_store::ReclaimStore,
    witness: reclaim::Witness,
) -> Result<(), OpenError> {
    if witness == reclaim::Witness::Terminal {
        return Ok(());
    }
    let record = reclaim::ReclaimRecord {
        witness: reclaim::WitnessFlags {
            checkpoint: reclaim::Witness::Terminal,
            ..store.record().witness
        },
        ..store.record().clone()
    };
    store.commit(&record).map_err(OpenError::from)
}

/// A record beside a missing `CHECKPOINT` on a root with **no
/// segments** is a genuinely fresh root whose arming preceded a
/// checkpoint that never landed: it is opened empty and re-armed by
/// the next attempt.
/// "Empty" is the *checkpoint* arming being cleared; the
/// `published_seeding` pair shares this header and is monotone by
/// §3.2, so it survives a reset that has nothing to do with it.
fn reset_record(store: &mut reclaim_store::ReclaimStore) -> Result<(), OpenError> {
    let empty = reclaim::ReclaimRecord {
        witness: reclaim::WitnessFlags {
            checkpoint: reclaim::Witness::Unarmed,
            ..store.record().witness
        },
        ..reclaim::ReclaimRecord::default()
    };
    if *store.record() == empty {
        return Ok(());
    }
    store.commit(&empty).map_err(OpenError::from)
}

/// §3.2's open-time reconciliation of the `planned` list. A planned
/// segment still present is retained — the ledger rebuilds it and a
/// later pass re-plans it — and its entry is dropped; an absent one is
/// treated exactly like a completed reclamation, since a planned
/// segment held only covered frames by construction. The reconciled
/// record is made durable before the first pass.
pub(crate) fn planned(
    store: &mut reclaim_store::ReclaimStore,
    root: &Path,
) -> Result<(), OpenError> {
    if store.record().planned.is_empty() {
        return Ok(());
    }
    let mode = match store.record().consumer_mode {
        reclaim::RecordedMode::Known => reclaim::EntryMode::Known,
        reclaim::RecordedMode::NoConsumer => reclaim::EntryMode::NoConsumer,
        // The codec refuses planned records beside an unrecorded mode,
        // so a decoded record with entries always has one.
        reclaim::RecordedMode::Unrecorded => return Ok(()),
    };
    let mut record = store.record().clone();
    let planned = std::mem::take(&mut record.planned);
    for entry in &planned {
        let present = root
            .join(format!("{}.wal", entry.segment))
            .try_exists()
            .map_err(|source| OpenError::Io {
                op: "stat(planned segment)",
                source,
            })?;
        if !present {
            raise_reclaimed(&mut record, entry, mode)?;
        }
    }
    store.commit(&record).map_err(OpenError::from)
}

/// An absent planned segment is a reclamation that finished: each
/// tenant's last offset in it raises that tenant's `reclaimed_through`
/// exactly as the commit would have.
fn raise_reclaimed(
    record: &mut reclaim::ReclaimRecord,
    planned: &reclaim::PlannedUnlink,
    mode: reclaim::EntryMode,
) -> Result<(), OpenError> {
    for (&id, &offset) in &planned.last_offsets {
        record
            .dictionary
            .raise_reclaimed(id, reclaim::Entry { mode, offset })
            .map_err(|e| OpenError::Corrupt {
                detail: format!(
                    "RECLAIM sidecar: planned segment {} names {e}",
                    planned.segment
                ),
            })?;
    }
    Ok(())
}

/// Neither sidecar. RFC 0052 §3.2's witness is then the segment
/// header's format version, which every root with any segment
/// necessarily carries: a version-2 segment means the root has run
/// under this RFC and its missing sidecars are a loss, while
/// all-version-1 segments are the shape every live pre-RFC root has.
fn open_without_sidecars(
    config: &WalConfig,
    segments: &[PathBuf],
) -> Result<RootWitness, OpenError> {
    match post_rfc_segment(segments) {
        Some(path) => Err(lost_sidecars(&config.root, path)),
        None if segments.is_empty() => seed_fresh_record(config),
        None => Ok(RootWitness::legacy()),
    }
}

/// The first segment whose header names RFC 0052's version. A header
/// that cannot be read is not a witness either way — `replay` halts on
/// it as it always has, and deciding here would turn that halt into a
/// different one at `open`.
fn post_rfc_segment(segments: &[PathBuf]) -> Option<&PathBuf> {
    segments.iter().find(|path| {
        File::open(path)
            .ok()
            .and_then(|mut handle| segment::read_header(&mut handle).ok())
            .is_some_and(|header| header.version == segment::SEGMENT_VERSION)
    })
}

/// A fresh root: the empty record is written and its parent fsynced
/// **before** `create_fresh_segment`, so every directory that has ever
/// held a segment has held a record too and the fail-closed row can
/// never fire on a node's own first start.
fn seed_fresh_record(config: &WalConfig) -> Result<RootWitness, OpenError> {
    let geometry = configured_geometry().map_err(|e| OpenError::InvalidConfig {
        field: "max_tenants",
        detail: e.to_string(),
    })?;
    let store = reclaim_store::ReclaimStore::create(
        &config.root,
        geometry,
        &reclaim::ReclaimRecord::default(),
        config.macos_full_fsync,
    )?;
    Ok(RootWitness::with_record(store))
}

fn lost_record(root: &Path) -> OpenError {
    OpenError::Corrupt {
        detail: format!(
            "{} is missing beside a version-2 {} (RFC 0052 §3.2): every unlink is gated on a checkpoint and the record is created at open, so this root has run under RFC 0052 and its reclaim record is a loss, not an absence",
            root.join(reclaim::SIDECAR_NAME).display(),
            root.join(checkpoint::SIDECAR_NAME).display(),
        ),
    }
}

fn lost_checkpoint(root: &Path) -> OpenError {
    OpenError::Corrupt {
        detail: format!(
            "{} carries checkpoint_seen but {} is missing (RFC 0052 §3.2): a version-2 checkpoint succeeded on this root, so the sidecar is a loss — without it the Parquet-side suppression horizon cannot be rebuilt and replay would republish",
            root.join(reclaim::SIDECAR_NAME).display(),
            root.join(checkpoint::SIDECAR_NAME).display(),
        ),
    }
}

fn lost_sidecars(root: &Path, segment: &Path) -> OpenError {
    OpenError::Corrupt {
        detail: format!(
            "{} and {} are both missing beside the version-2 segment {} (RFC 0052 §3.2): the segment header is the witness that this root has run under RFC 0052, so reading it as a fresh root would replay frames whose Parquet rows may already exist",
            root.join(reclaim::SIDECAR_NAME).display(),
            root.join(checkpoint::SIDECAR_NAME).display(),
            segment.display(),
        ),
    }
}

/// Write `checkpoint_armed` durably before the first version-2
/// `CHECKPOINT` attempt, creating the record when a legacy root has
/// none (RFC 0052 §3.2). Arming before the attempt is what keeps a
/// crash in the window between the two an ordinary state rather than a
/// lost checkpoint.
pub(crate) fn arm(
    store: &mut Option<reclaim_store::ReclaimStore>,
    config: &WalConfig,
) -> Result<(), CheckpointError> {
    match store.as_mut() {
        Some(held) if held.record().witness.checkpoint == reclaim::Witness::Unarmed => {
            // Only the checkpoint witness moves. The
            // `published_seeding` pair shares this header, is monotone
            // by §3.2 ("once confirmed it never clears"), and belongs
            // to a writer this slice does not have — replacing the
            // whole `WitnessFlags` would silently clear it.
            let record = reclaim::ReclaimRecord {
                witness: reclaim::WitnessFlags {
                    checkpoint: reclaim::Witness::Armed,
                    ..held.record().witness
                },
                ..held.record().clone()
            };
            held.commit(&record).map_err(record_failed)
        }
        Some(_) => Ok(()),
        None => ensure_record(store, config).map_err(record_failed),
    }
}

/// RFC 0052 §3.2's ordering rule, which reaches rotation and not only
/// open: **no version-2 segment is ever created before the record is
/// durable.** A legacy root rotates before it ever checkpoints —
/// rotation is append-driven and the first barrier may be minutes
/// away — so a rotation that installed a version-2 segment while the
/// root still had no sidecar would leave exactly the shape the
/// open-time matrix fails closed on, and the node would refuse to
/// open after a restart: a live root bricked by rotating.
///
/// The record is created armed and with its mode unrecorded, which
/// the matrix reads as a legacy root mid migration and **retains**.
/// A root that already has one is left alone; arming it is the
/// checkpoint's job.
pub(crate) fn ensure_record(
    store: &mut Option<reclaim_store::ReclaimStore>,
    config: &WalConfig,
) -> Result<(), reclaim_store::StoreError> {
    if store.is_some() {
        return Ok(());
    }
    let record = reclaim::ReclaimRecord {
        witness: reclaim::WitnessFlags {
            checkpoint: reclaim::Witness::Armed,
            ..reclaim::WitnessFlags::default()
        },
        ..reclaim::ReclaimRecord::default()
    };
    let geometry = configured_geometry().map_err(|e| reclaim_store::StoreError::Corrupt {
        detail: format!("RECLAIM sidecar: sizing the file: {e}"),
    })?;
    *store = Some(reclaim_store::ReclaimStore::create(
        &config.root,
        geometry,
        &record,
        config.macos_full_fsync,
    )?);
    Ok(())
}

/// Write `checkpoint_seen` at the next record write after the
/// checkpoint succeeded — immediately, since no pass is pending
/// (§3.2). Once seen the flag never clears, which is what lets the
/// open-time matrix read a later missing `CHECKPOINT` as a loss rather
/// than as a fresh root.
pub(crate) fn witness(
    store: &mut Option<reclaim_store::ReclaimStore>,
) -> Result<(), CheckpointError> {
    match store.as_mut() {
        Some(held) if held.record().witness.checkpoint != reclaim::Witness::Terminal => {
            let record = reclaim::ReclaimRecord {
                witness: reclaim::WitnessFlags {
                    checkpoint: reclaim::Witness::Terminal,
                    ..held.record().witness
                },
                ..held.record().clone()
            };
            held.commit(&record).map_err(record_failed)
        }
        Some(_) | None => Ok(()),
    }
}

/// Map a sidecar failure onto the checkpoint's own error surface.
/// Damaged bytes travel as an `InvalidData` source rather than a new
/// public variant, the way the segment-header read already does.
fn record_failed(e: reclaim_store::StoreError) -> CheckpointError {
    match e {
        reclaim_store::StoreError::Io { op, source } => CheckpointError::Io { op, source },
        reclaim_store::StoreError::Corrupt { detail } => CheckpointError::Io {
            op: "write(RECLAIM)",
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, detail),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ourios_core::tenant::TenantId;

    use super::planned;
    use crate::WalOffset;
    use crate::reclaim::{
        Dictionary, Entry, EntryMode, Geometry, PlannedUnlink, ReclaimRecord, RecordedMode,
        SlotState, WitnessFlags,
    };
    use crate::reclaim_store::ReclaimStore;

    /// RFC 0052 §3.2's open-time reconciliation of the `planned` list.
    /// A planned segment still present is retained and its entry
    /// dropped — the next pass re-plans it — while an absent one is a
    /// reclamation that finished, raising each tenant's
    /// `reclaimed_through` as the commit would have. Both outcomes
    /// leave the list empty and the record durable.
    #[test]
    fn reconcile_planned_drops_present_segments_and_raises_absent_ones() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let geometry = Geometry::new(2, 2).expect("geometry");
        let mut dictionary = Dictionary::default();
        let tenant = TenantId::try_new("checkout").expect("tenant");
        let id = dictionary.assign(&tenant, geometry).expect("slot id");

        let present = uuid::Uuid::now_v7();
        let absent = uuid::Uuid::now_v7();
        std::fs::write(tmp.path().join(format!("{present}.wal")), b"").expect("present segment");
        let plan = |segment, byte| PlannedUnlink {
            segment,
            uncertain: false,
            last_offsets: BTreeMap::from([(id, WalOffset { segment, byte })]),
        };
        let record = ReclaimRecord {
            witness: WitnessFlags::default(),
            consumer_mode: RecordedMode::Known,
            dictionary,
            planned: vec![plan(present, 100), plan(absent, 200)],
        };
        let mut store = ReclaimStore::create(tmp.path(), geometry, &record, false).expect("create");

        planned(&mut store, tmp.path()).expect("reconcile");

        let reconciled = store.record();
        assert!(
            reconciled.planned.is_empty(),
            "every planned entry is resolved, not carried forward",
        );
        let state = reconciled.dictionary.get(id).map(|r| r.state.clone());
        assert_eq!(
            state,
            Some(SlotState::Live {
                reclaimed_through: Some(Entry {
                    mode: EntryMode::Known,
                    offset: WalOffset {
                        segment: absent,
                        byte: 200,
                    },
                }),
            }),
            "only the absent segment's offsets become proof of loss",
        );
        assert!(
            tmp.path().join(format!("{present}.wal")).exists(),
            "reconciliation never unlinks; the retained segment stays for the next pass",
        );
    }
}
