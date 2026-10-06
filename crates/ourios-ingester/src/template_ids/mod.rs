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

use ourios_miner::cluster::{
    IdBlock, IdReservationError, IdSpaceExhausted, MAX_TEMPLATE_ID, MinerCluster,
};
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
        EVENT_OURIOS_RECEIVER_TEMPLATE_IDS_REFILL_FAILED as REFILL_FAILED,
        EVENT_OURIOS_RECEIVER_TEMPLATE_IDS_REFILL_STOPPED as REFILL_STOPPED,
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
    /// Every compare-and-swap attempt lost a race.
    Contended,
    /// The bootstrap scan could not read a data or audit file.
    Scan(Box<IdMaxError>),
    /// No id is left in the `u64` space.
    Exhausted(IdSpaceExhausted),
    /// The background refiller thread could not start.
    Refiller(String),
    /// The miner could not take its first block from the ones reserved.
    FirstBlock(IdReservationError),
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
    /// The seated marker is malformed, or claims more than the high-water
    /// holds: it vouches for nothing (RFC 0059 §3.5).
    MarkerInvalid { detail: String },
    /// A seated root, at startup or reserving live, found no high-water:
    /// it was deleted, and re-creating it could seat below a block another
    /// receiver still holds.
    HighWaterDeleted,
    /// No high-water exists, the store already holds data, and this start
    /// was not authorised to bootstrap (RFC 0059 §3.5).
    BootstrapNotAuthorized,
    /// The high-water reads below a reservation this root already made
    /// usable: an older copy was restored (RFC 0059 §3.1).
    HighWaterRolledBack { seen: u64, found: u64 },
}

impl TemplateIdsError {
    /// The `error.type` a failure is logged under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Store { .. } => "store",
            Self::Malformed { .. } => "malformed",
            Self::LaterVersion { .. } => "later_version",
            Self::Contended => "contended",
            Self::Scan(_) => "scan",
            Self::Exhausted(_) => "exhausted",
            Self::BootstrapRaceLost => "bootstrap_race_lost",
            Self::Marker { .. } | Self::Snapshots(_) | Self::MarkerInvalid { .. } => "marker",
            Self::HighWaterDeleted => "deleted",
            Self::BootstrapNotAuthorized => "bootstrap_not_authorized",
            Self::HighWaterRolledBack { .. } => "rolled_back",
            Self::Refiller(_) | Self::FirstBlock(_) => "_OTHER",
        }
    }
}

impl std::fmt::Display for TemplateIdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store { op, key, source } if source.is_permission_denied() => write!(
                f,
                "{op} {key}: permission denied; the receiver needs {} there \
                 (RFC 0059 §3.6): {source}",
                s3_action(op)
            ),
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
            Self::Contended => write!(
                f,
                "{HIGH_WATER_KEY}: {MAX_CAS_ATTEMPTS} compare-and-swap attempts all lost"
            ),
            Self::Scan(e) if e.is_permission_denied() => write!(
                f,
                "template-id bootstrap scan: permission denied; the receiver needs \
                 s3:ListBucket and s3:GetObject on data/ and audit/ for the bootstrap \
                 (RFC 0059 §3.6): {e}"
            ),
            Self::Scan(e) => write!(f, "template-id bootstrap scan: {e}"),
            Self::Exhausted(e) => write!(f, "template-id high-water: {e}"),
            Self::Refiller(e) => write!(f, "start the template-id refiller: {e}"),
            Self::FirstBlock(e) => write!(f, "take the first template-id block: {e}"),
            Self::BootstrapRaceLost => write!(
                f,
                "another start created {HIGH_WATER_KEY} while this one had restored \
                 snapshots it never seated; restart to discard them"
            ),
            Self::Marker { op, source } => write!(f, "{op}: {source}"),
            Self::Snapshots(e) => write!(f, "seated marker: {e}"),
            Self::MarkerInvalid { detail } => {
                write!(f, "{SEATED_MARKER} is not a usable seated marker: {detail}")
            }
            Self::BootstrapNotAuthorized => write!(
                f,
                "{HIGH_WATER_KEY} is absent but the store already holds data: bootstrapping \
                 the template-id high-water needs explicit authorisation. For the upgrade to \
                 RFC 0059, stop every older receiver, start one upgraded replica with \
                 receiver.template_ids_allow_bootstrap (OURIOS_TEMPLATE_IDS_ALLOW_BOOTSTRAP) \
                 set to true, and remove the setting once it has seated"
            ),
            Self::HighWaterRolledBack { seen, found } => write!(
                f,
                "{HIGH_WATER_KEY} reads {found}, below the {seen} this root already reserved: \
                 the object was rolled back to an older copy, which is unsupported. Recover \
                 by stopping every receiver, removing the object and every root's \
                 {SEATED_MARKER}, and starting one replica authorised to bootstrap (RFC 0059 \
                 §3.1)"
            ),
            Self::HighWaterDeleted => write!(
                f,
                "{HIGH_WATER_KEY} is gone though this root has seated against it; it must \
                 never be deleted. Recover by stopping every receiver, removing every root's \
                 {SEATED_MARKER}, and starting one replica authorised to bootstrap (RFC 0059 \
                 §3.1)"
            ),
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
            | Self::Contended
            | Self::BootstrapRaceLost
            | Self::MarkerInvalid { .. }
            | Self::HighWaterDeleted
            | Self::BootstrapNotAuthorized
            | Self::HighWaterRolledBack { .. }
            | Self::Refiller(_) => None,
            Self::FirstBlock(e) => Some(e),
        }
    }
}

/// The S3 action a store operation of this module needs.
fn s3_action(op: &str) -> &'static str {
    match op {
        "list" => "s3:ListBucket",
        "write" => "s3:PutObject",
        _ => "s3:GetObject",
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
    match value.get(FIELD).map(|n| (n, n.as_u64())) {
        Some((_, Some(id))) if id <= MAX_TEMPLATE_ID => Ok(id),
        Some((n, Some(_))) => Err(malformed(format!(
            "`{FIELD}` is {n}, above the template-id domain's i64::MAX"
        ))),
        Some((n, None)) => Err(malformed(format!("`{FIELD}` is {n}, not a u64"))),
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

/// Create the high-water at `reserved_through`. Only the startup
/// bootstrap calls this (RFC 0059 §3.5): a live reservation never creates
/// an absent object.
fn create(store: &Store, reserved_through: u64) -> Result<Written, TemplateIdsError> {
    landed(
        store,
        store.put_if_absent_blocking(HIGH_WATER_KEY, encode(reserved_through)),
    )
}

/// Replace the high-water read as `prior` with `reserved_through`: a
/// compare-and-swap where the backend supports one, an overwrite where it
/// does not (RFC 0059 §3.6). A compare-and-swap against a deleted object
/// fails rather than re-creating it.
fn update(
    store: &Store,
    prior: &HighWater,
    reserved_through: u64,
) -> Result<Written, TemplateIdsError> {
    let bytes = encode(reserved_through);
    let outcome = match (&prior.e_tag, store.supports_conditional_update()) {
        (Some(tag), true) => store.put_if_match_blocking(HIGH_WATER_KEY, bytes, tag),
        (None, true) => {
            return Err(TemplateIdsError::Malformed {
                key: HIGH_WATER_KEY.to_owned(),
                detail: "read without an ETag, so no compare-and-swap can guard the write"
                    .to_owned(),
            });
        }
        (_, false) => store.put_blocking(HIGH_WATER_KEY, bytes),
    };
    landed(store, outcome)
}

/// A write counts as landed only once it is durable: the local backend's
/// rename is fsynced first (RFC 0059 §3.2's write-before-allocate).
fn landed(store: &Store, outcome: Result<(), StoreError>) -> Result<Written, TemplateIdsError> {
    match outcome {
        Ok(()) => store
            .sync_local_blocking(HIGH_WATER_KEY)
            .map(|()| Written::Landed)
            .map_err(store_err("write", HIGH_WATER_KEY)),
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
/// [`TemplateIdsError::HighWaterDeleted`] when the object is gone, which
/// a reservation never repairs; otherwise [`TemplateIdsError`] when the
/// object is unreadable, a write fails, every attempt loses, or no id is
/// left at or below [`MAX_TEMPLATE_ID`]. A block that would pass it is
/// shortened to end there.
pub fn reserve(store: &Store, floor: u64) -> Result<IdBlock, TemplateIdsError> {
    for _ in 0..MAX_CAS_ATTEMPTS {
        let prior = read(store)?.ok_or(TemplateIdsError::HighWaterDeleted)?;
        let after = prior.reserved_through.max(floor);
        // The final block is shortened at the domain's top, so
        // `MAX_TEMPLATE_ID` itself stays issuable.
        let through = after.saturating_add(BLOCK).min(MAX_TEMPLATE_ID);
        let block =
            IdBlock::new(after, through).ok_or(TemplateIdsError::Exhausted(IdSpaceExhausted))?;
        if let Written::Landed = update(store, &prior, block.through())? {
            return Ok(block);
        }
    }
    Err(TemplateIdsError::Contended)
}

/// What a start expects of the high-water it read when deciding whether
/// to trust its snapshots (RFC 0059 §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapPolicy {
    /// The start saw the object (a seated root, or a markerless one that
    /// discards its snapshots): absence now means it was deleted.
    Refuse,
    /// The start saw no object and restored its snapshots on that basis,
    /// so it must create the object itself. Only a store with no data or
    /// audit file yet, a genuinely new one, bootstraps unauthorised.
    IfStoreEmpty,
    /// As [`Self::IfStoreEmpty`], authorised by the operator for the
    /// upgrade.
    Authorized,
}

/// Whether the store holds any data or audit object: one delimited
/// listing of each prefix's first level.
fn store_has_data(store: &Store) -> Result<bool, TemplateIdsError> {
    for prefix in ["data", "audit"] {
        let listing = store
            .list_delimited_blocking(Some(prefix))
            .map_err(store_err("list", prefix))?;
        if !listing.objects.is_empty() || !listing.common_prefixes.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
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
pub fn seat(
    store: &Store,
    miner: &mut MinerCluster,
    policy: BootstrapPolicy,
) -> Result<Seated, TemplateIdsError> {
    let restored = miner.highest_allocated();
    let seated = match (read(store)?, policy) {
        (Some(high_water), BootstrapPolicy::Refuse) => Seated {
            high_water: high_water.reserved_through,
            bootstrapped: false,
        },
        // Another start created the object after this one saw it absent
        // and restored its snapshots: their ids may lie above its floor.
        (Some(_), BootstrapPolicy::IfStoreEmpty | BootstrapPolicy::Authorized) => {
            return Err(TemplateIdsError::BootstrapRaceLost);
        }
        (None, BootstrapPolicy::Refuse) => return Err(TemplateIdsError::HighWaterDeleted),
        (None, BootstrapPolicy::IfStoreEmpty) if store_has_data(store)? => {
            return Err(TemplateIdsError::BootstrapNotAuthorized);
        }
        (None, BootstrapPolicy::IfStoreEmpty | BootstrapPolicy::Authorized) => {
            bootstrap(store, restored)?
        }
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
            br#"{"reserved_through": 9223372036854775808}"#,
            br#"{"reserved_through": 18446744073709551615}"#,
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
    fn a_reservation_never_creates_an_absent_object() {
        let (_tmp, local) = local();
        for store in [Store::in_memory(), local] {
            assert!(matches!(
                reserve(&store, 0),
                Err(TemplateIdsError::HighWaterDeleted)
            ));
            assert!(read(&store).expect("read").is_none(), "nothing was created");
        }
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
    fn the_high_water_reads_up_to_i64_max() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(MAX_TEMPLATE_ID))
            .expect("put");
        let read = read(&store).expect("read").expect("present");
        assert_eq!(read.reserved_through, MAX_TEMPLATE_ID);
    }

    #[test]
    fn the_final_block_is_shortened_to_end_at_i64_max() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(MAX_TEMPLATE_ID - 234))
            .expect("put");
        let block = reserve(&store, 0).expect("the shortened final block");
        assert_eq!(
            (block.after(), block.through()),
            (MAX_TEMPLATE_ID - 234, MAX_TEMPLATE_ID)
        );
        assert!(matches!(
            reserve(&store, 0),
            Err(TemplateIdsError::Exhausted(_))
        ));
        let read = read(&store).expect("read").expect("present");
        assert_eq!(
            read.reserved_through, MAX_TEMPLATE_ID,
            "exhaustion writes nothing"
        );
    }

    #[test]
    fn a_block_ending_at_i64_max_is_reserved() {
        let store = Store::in_memory();
        store
            .put_blocking(HIGH_WATER_KEY, encode(MAX_TEMPLATE_ID - BLOCK))
            .expect("put");
        let block = reserve(&store, 0).expect("the last block");
        assert_eq!(block.through(), MAX_TEMPLATE_ID);
    }
}
