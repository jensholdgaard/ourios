//! Durable template-id allocation (RFC 0059).
//!
//! One object per store, `miner/template_ids.v1.json`, records the
//! highest id any allocator may issue. A block is reserved there, by a
//! compare-and-swap where the backend has one, before any id in it is
//! used ([`reserve`]); every start reads it and allocates above it
//! ([`seat`]); the first start computes it from the store's own footers
//! ([`bootstrap`]).

mod bootstrap;
mod marker;
mod reserver;

pub use bootstrap::{BootstrapScan, bootstrap};
pub use marker::{SEATED_MARKER, SnapshotTrust, mark_seated};
pub use reserver::TemplateIds;

use ourios_miner::cluster::{IdBlock, IdSpaceExhausted, MinerCluster};
use ourios_parquet::{IdMaxError, Store, StoreError};

/// The high-water object's key (RFC 0059 §3.1).
pub const HIGH_WATER_KEY: &str = "miner/template_ids.v1.json";
const HIGH_WATER_PREFIX: &str = "miner";
const HIGH_WATER_STEM: &str = "miner/template_ids.v";
const FIELD: &str = "reserved_through";

/// Ids per reservation (RFC 0059 §3.2).
pub const BLOCK: u64 = 1_000;
/// Compare-and-swap attempts per reservation before it counts as failed.
const MAX_CAS_ATTEMPTS: usize = 16;

/// The telemetry names RFC 0059 §3.9 registers, in one place.
pub(crate) mod names {
    /// The `error.type` of a snapshot discarded because its root never
    /// seated (RFC 0059 §3.5); registered in ourios-semconv#8.
    pub(crate) const PREDATES_HIGH_WATER: &str = "predates_high_water";

    pub(super) use ourios_semconv::{
        EVENT_OURIOS_RECEIVER_TEMPLATE_IDS_BOOTSTRAP_PROGRESS as BOOTSTRAP_PROGRESS,
        EVENT_OURIOS_RECEIVER_TEMPLATE_IDS_BOOTSTRAPPED as BOOTSTRAPPED,
        OURIOS_RECEIVER_TEMPLATE_IDS_AUDIT_MAX as AUDIT_MAX,
        OURIOS_RECEIVER_TEMPLATE_IDS_DATA_MAX as DATA_MAX,
        OURIOS_RECEIVER_TEMPLATE_IDS_FILES_SCANNED as FILES_SCANNED,
        OURIOS_RECEIVER_TEMPLATE_IDS_FLOOR as FLOOR,
    };
}

/// Why the high-water could not be read, written or seated.
#[derive(Debug)]
pub enum TemplateIdsError {
    /// A store call failed.
    Store {
        op: &'static str,
        key: String,
        source: Box<StoreError>,
    },
    /// The object exists but does not hold a version-1 high-water.
    Malformed { key: String, detail: String },
    /// Only a later format version's object exists.
    LaterVersion { key: String },
    /// A reservation needs the object, and it is gone.
    Missing,
    /// Every compare-and-swap attempt lost a race.
    Contended,
    /// The bootstrap scan could not read a data or audit file.
    Scan(Box<IdMaxError>),
    /// No id is left in the `u64` space.
    Exhausted(IdSpaceExhausted),
    /// The background refiller thread could not start.
    Refiller(String),
    /// Another start created the high-water while this one, without a
    /// seated marker, had restored its own snapshots: their ids may lie in
    /// the winner's blocks, so this start must not go on (RFC 0059 §3.5).
    BootstrapRaceLost,
    /// The seated marker could not be checked or written.
    Marker {
        op: &'static str,
        source: std::io::Error,
    },
    /// The snapshots directory could not be made durable.
    Snapshots(crate::snapshot_store::SnapshotStoreError),
}

impl TemplateIdsError {
    /// The `error.type` a failure is logged under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Store { .. } => "store",
            Self::Malformed { .. } => "malformed",
            Self::LaterVersion { .. } => "later_version",
            Self::Missing => "missing",
            Self::Contended => "contended",
            Self::Scan(_) => "scan",
            Self::Exhausted(_) => "exhausted",
            Self::BootstrapRaceLost => "bootstrap_race_lost",
            Self::Marker { .. } | Self::Snapshots(_) => "marker",
            Self::Refiller(_) => "_OTHER",
        }
    }
}

impl std::fmt::Display for TemplateIdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store { op, key, source } => write!(f, "{op} {key}: {source}"),
            Self::Malformed { key, detail } => {
                write!(f, "{key} is not a template-id high-water: {detail}")
            }
            Self::LaterVersion { key } => {
                write!(
                    f,
                    "{key} is a later template-id high-water format than this build reads"
                )
            }
            Self::Missing => write!(f, "{HIGH_WATER_KEY} is gone; a reservation needs it"),
            Self::Contended => write!(
                f,
                "{HIGH_WATER_KEY}: {MAX_CAS_ATTEMPTS} compare-and-swap attempts all lost"
            ),
            Self::Scan(e) => write!(f, "template-id bootstrap scan: {e}"),
            Self::Exhausted(e) => write!(f, "template-id high-water: {e}"),
            Self::Refiller(e) => write!(f, "start the template-id refiller: {e}"),
            Self::BootstrapRaceLost => write!(
                f,
                "another start created {HIGH_WATER_KEY} while this one had restored \
                 snapshots it never seated; restart to discard them"
            ),
            Self::Marker { op, source } => write!(f, "{op}: {source}"),
            Self::Snapshots(e) => write!(f, "seated marker: {e}"),
        }
    }
}

impl std::error::Error for TemplateIdsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store { source, .. } => Some(source.as_ref()),
            Self::Scan(e) => Some(e.as_ref()),
            Self::Exhausted(e) => Some(e),
            Self::Marker { source, .. } => Some(source),
            Self::Snapshots(e) => Some(e),
            Self::Malformed { .. }
            | Self::LaterVersion { .. }
            | Self::Missing
            | Self::Contended
            | Self::BootstrapRaceLost
            | Self::Refiller(_) => None,
        }
    }
}

fn store_err(op: &'static str, key: &str) -> impl FnOnce(StoreError) -> TemplateIdsError {
    let key = key.to_owned();
    move |source| TemplateIdsError::Store {
        op,
        key,
        source: Box::new(source),
    }
}

/// The high-water as read, with the tag a compare-and-swap writes against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighWater {
    pub reserved_through: u64,
    e_tag: Option<String>,
}

/// Read the high-water, or `None` when the store has none of any version.
///
/// # Errors
///
/// [`TemplateIdsError`] when the object cannot be fetched or parsed, or a
/// later format version's object exists beside it or alone (RFC 0059
/// §3.1): a newer binary may have moved the authoritative high-water
/// there, so a stale v1 is never trusted.
pub fn read(store: &Store) -> Result<Option<HighWater>, TemplateIdsError> {
    refuse_later_version(store)?;
    let Some((bytes, e_tag)) = store
        .get_with_etag_blocking_opt(HIGH_WATER_KEY)
        .map_err(store_err("read", HIGH_WATER_KEY))?
    else {
        return Ok(None);
    };
    let reserved_through = parse(&bytes)?;
    Ok(Some(HighWater {
        reserved_through,
        e_tag,
    }))
}

fn parse(bytes: &[u8]) -> Result<u64, TemplateIdsError> {
    let malformed = |detail: String| TemplateIdsError::Malformed {
        key: HIGH_WATER_KEY.to_owned(),
        detail,
    };
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| malformed(e.to_string()))?;
    match value.get(FIELD) {
        Some(n) => n
            .as_u64()
            .ok_or_else(|| malformed(format!("`{FIELD}` is {n}, not a u64"))),
        None => Err(malformed(format!("no `{FIELD}`"))),
    }
}

fn refuse_later_version(store: &Store) -> Result<(), TemplateIdsError> {
    let keys = store
        .list_blocking(Some(HIGH_WATER_PREFIX))
        .map_err(store_err("list", HIGH_WATER_PREFIX))?;
    let later = keys
        .into_iter()
        .find(|key| key.starts_with(HIGH_WATER_STEM) && key != HIGH_WATER_KEY);
    match later {
        Some(key) => Err(TemplateIdsError::LaterVersion { key }),
        None => Ok(()),
    }
}

fn encode(reserved_through: u64) -> Vec<u8> {
    serde_json::json!({ FIELD: reserved_through })
        .to_string()
        .into_bytes()
}

/// Whether a write landed or lost a race.
enum Written {
    Landed,
    Lost,
}

/// Write `reserved_through`: create-if-absent when there is no object,
/// compare-and-swap against `prior` where the backend supports it, and
/// overwrite on a backend that does not (RFC 0059 §3.6).
fn write(
    store: &Store,
    prior: Option<&HighWater>,
    reserved_through: u64,
) -> Result<Written, TemplateIdsError> {
    let bytes = encode(reserved_through);
    let outcome = match prior {
        None => store.put_if_absent_blocking(HIGH_WATER_KEY, bytes),
        Some(HighWater {
            e_tag: Some(tag), ..
        }) if store.supports_conditional_update() => {
            store.put_if_match_blocking(HIGH_WATER_KEY, bytes, tag)
        }
        Some(_) => store.put_blocking(HIGH_WATER_KEY, bytes),
    };
    match outcome {
        Ok(()) => Ok(Written::Landed),
        Err(e) if e.is_already_exists() || e.is_precondition() => Ok(Written::Lost),
        Err(e) => Err(store_err("write", HIGH_WATER_KEY)(e)),
    }
}

/// Durably reserve a block above `floor` (RFC 0059 §3.2): read `N`, write
/// `max(N, floor) + BLOCK`, and return the block only once that write has
/// landed. A lost compare-and-swap re-reads and retries.
///
/// # Errors
///
/// [`TemplateIdsError`] when the object is unreadable or gone, a write
/// fails, every attempt loses, or the block would reach `u64::MAX`.
pub fn reserve(store: &Store, floor: u64) -> Result<IdBlock, TemplateIdsError> {
    for _ in 0..MAX_CAS_ATTEMPTS {
        let prior = read(store)?.ok_or(TemplateIdsError::Missing)?;
        let after = prior.reserved_through.max(floor);
        let block = after
            .checked_add(BLOCK)
            .filter(|through| *through < u64::MAX)
            .and_then(|through| IdBlock::new(after, through))
            .ok_or(TemplateIdsError::Exhausted(IdSpaceExhausted))?;
        if let Written::Landed = write(store, Some(&prior), block.through())? {
            return Ok(block);
        }
    }
    Err(TemplateIdsError::Contended)
}

/// What a start found and seated.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Seated {
    /// The high-water read at start, or written by its bootstrap.
    pub high_water: u64,
    /// Whether this start wrote the object for the first time.
    pub bootstrapped: bool,
}

/// Seat `miner` above the store's high-water before replay mints
/// anything (RFC 0059 §3.4), bootstrapping the object when it is absent.
///
/// # Errors
///
/// [`TemplateIdsError`] when the object cannot be read or bootstrapped;
/// startup fails closed on any of them.
pub fn seat(store: &Store, miner: &mut MinerCluster) -> Result<Seated, TemplateIdsError> {
    let restored = miner.highest_allocated();
    let seated = match read(store)? {
        Some(high_water) => Seated {
            high_water: high_water.reserved_through,
            bootstrapped: false,
        },
        None => bootstrap(store, restored)?,
    };
    miner
        .allocate_past_issued(seated.high_water.max(restored))
        .map_err(TemplateIdsError::Exhausted)?;
    Ok(seated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::local(tmp.path()).expect("store");
        (tmp, store)
    }

    #[test]
    fn a_store_without_the_object_has_no_high_water() {
        let (_tmp, store) = local();
        assert_eq!(read(&store).expect("read"), None);
    }

    #[test]
    fn each_unreadable_object_is_refused() {
        for body in [
            &b"not json"[..],
            br#"{"other": 1}"#,
            br#"{"reserved_through": -1}"#,
            br#"{"reserved_through": "7"}"#,
        ] {
            let (_tmp, store) = local();
            store
                .put_blocking(HIGH_WATER_KEY, body.to_vec())
                .expect("put");
            let err = read(&store).expect_err("unreadable");
            assert!(matches!(err, TemplateIdsError::Malformed { .. }), "{err}");
        }
    }

    #[test]
    fn a_later_version_is_refused_alone_and_beside_v1() {
        for beside_v1 in [false, true] {
            let (_tmp, store) = local();
            if beside_v1 {
                store
                    .put_blocking(HIGH_WATER_KEY, encode(7))
                    .expect("put v1");
            }
            store
                .put_blocking("miner/template_ids.v2.json", b"{}".to_vec())
                .expect("put v2");
            let err = read(&store).expect_err("later version");
            assert!(
                matches!(err, TemplateIdsError::LaterVersion { .. }),
                "{err}"
            );
        }
    }

    #[test]
    fn a_reservation_needs_the_object() {
        let (_tmp, store) = local();
        assert!(matches!(reserve(&store, 0), Err(TemplateIdsError::Missing)));
    }

    #[test]
    fn reservations_raise_the_high_water_above_the_floor() {
        let (_tmp, local) = local();
        for store in [Store::in_memory(), local] {
            store.put_blocking(HIGH_WATER_KEY, encode(10)).expect("put");
            let first = reserve(&store, 0).expect("first");
            assert_eq!((first.after(), first.through()), (10, 10 + BLOCK));
            let second = reserve(&store, 5_000).expect("second");
            assert_eq!((second.after(), second.through()), (5_000, 5_000 + BLOCK));
            let read = read(&store).expect("read").expect("present");
            assert_eq!(read.reserved_through, 5_000 + BLOCK);
        }
    }

    #[test]
    fn a_block_reaching_u64_max_is_refused() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(u64::MAX - BLOCK))
            .expect("put");
        assert!(matches!(
            reserve(&store, 0),
            Err(TemplateIdsError::Exhausted(_))
        ));
    }
}
