//! The RFC 0033 cached template-map artifact — format and derivation.
//!
//! One JSON object per tenant, shipped as a single zstd frame
//! (`template_map.v2.json.zst`, RFC 0033 §3.2 + the 2026-07-13
//! compressed-encoding amendment), carrying **both** folds of the
//! tenant's audit stream — the RFC 0017 §3.2 template registry and the
//! RFC 0005 §3.7.1 alias map — plus the `folded_files` frontier they
//! were folded from. The audit stream remains the source of truth; this
//! artifact is a derived, discardable acceleration, and every doubtful
//! read resolves by folding the stream (§3.3's dispositions,
//! [`ArtifactRead`]).
//!
//! [`derive_template_map`] performs **one** [`crate::audit_scan`] pass
//! and folds both maps from that single capture — §3.5's no-partial
//! rule at the type level: a [`TemplateMap`] cannot be constructed
//! outside this module with only one fold populated, so a
//! registry-at-frontier-F1 / alias-map-at-F2 split is unrepresentable.
//! [`TemplateMap::publish`] commits the artifact atomically (tmp+rename
//! locally, conditional put on the store — §3.4, the RFC 0009 manifest
//! precedent). [`load_or_derive`] is the cached read path the query
//! layer consumes: one listing, the §3.3 frontier check, fallback to
//! the fresh fold on every non-hit disposition, and the §3.5
//! best-effort write-through.
//!
//! JSON follows the `manifest.json` precedent
//! (`ourios_parquet::Manifest`, RFC 0009 §3.4): small, human-
//! inspectable, `serde`-round-tripped, validated before use.
//!
//! Every lookup and publish records its RFC 0033 §3.7 outcome on the
//! `ourios.template_map.*` instruments — resolved through the
//! process-global meter, names from the weaver-generated
//! [`ourios_semconv`] constants.

use std::path::Path;
use std::sync::LazyLock;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};
use ourios_core::alias::AliasMap;
use ourios_core::audit::{AuditEvent, AuditPayload};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::{format_template, parse_template};
use ourios_parquet::percent_encode_tenant;
use ourios_semconv as semconv;
use serde::{Deserialize, Serialize};

use crate::template_registry::TemplateRegistry;
use crate::{QueryError, StoreRef, audit_scan, template_registry};

/// Canonical artifact key at the root of a tenant's audit subtree
/// (`audit/tenant_id=<enc>/template_map.v2.json.zst`, RFC 0033 §3.2
/// amendment 2026-07-13): the encoding version lives in the key, so a
/// pre-amendment reader sees literal absence — the cleanest realization
/// of the unknown-version-is-absent rule. Not a `*.parquet` name, so
/// every existing audit walk/listing ignores it by construction.
pub const TEMPLATE_MAP_FILENAME: &str = "template_map.v2.json.zst";

/// The superseded v1 key (uncompressed JSON, `format_version` 1).
/// Post-amendment readers never GET it; a successful v2 publish
/// best-effort deletes it (§3.4 amendment — unconditional, any v1
/// artifact is derived and discardable, and a failed delete is
/// swallowed like every other best-effort publish IO).
pub const TEMPLATE_MAP_V1_FILENAME: &str = "template_map.json";

/// The `format_version` this reader writes and understands — it names
/// the whole artifact contract *including* the zstd transport encoding
/// (§3.2 amendment). A reader encountering any other version treats the
/// artifact as absent (forward compatibility, RFC 0033 §3.3) — no
/// migration is ever required because the artifact is derived and
/// discardable.
pub const TEMPLATE_MAP_FORMAT_VERSION: u32 = 2;

/// The artifact frame's zstd level — the crate default (3), an
/// implementation constant, not configuration (§3.2 amendment: the
/// object is kilobyte-scale and written once per miss; raise here if
/// run #21 measures the ratio marginal).
const ARTIFACT_ZSTD_LEVEL: i32 = zstd::DEFAULT_COMPRESSION_LEVEL;

/// Ceiling on the decompressed JSON body, enforced symmetrically: the
/// reader stops a crafted zstd bomb from allocating past it (oversize
/// classifies `Torn` — the artifact is untrusted input), and the
/// publish side refuses to serialize a larger body, so a legitimate
/// artifact can never trip the reader's bound and cause a
/// torn/republish churn loop. 64 MiB is far above any observed
/// registry (otel-demo-v8's whole fold is ~0.5 MB compressed Parquet).
const ARTIFACT_MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// Decompress one artifact frame, refusing to allocate beyond
/// [`ARTIFACT_MAX_DECOMPRESSED_BYTES`].
fn decode_bounded(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut json = Vec::new();
    let decoder = zstd::Decoder::new(bytes)?;
    decoder
        .take(ARTIFACT_MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut json)?;
    if json.len() as u64 > ARTIFACT_MAX_DECOMPRESSED_BYTES {
        return Err(std::io::Error::other(format!(
            "decompressed body exceeds the {ARTIFACT_MAX_DECOMPRESSED_BYTES}-byte bound",
        )));
    }
    Ok(json)
}

/// `ourios.template_map.lookup.outcome` attribute values (RFC 0033 §3.7):
/// the five §3.3 lookup dispositions, folded flat onto one counter (the
/// error.type convention — one instrument, the dimension on an
/// attribute).
const LOOKUP_OUTCOME_HIT: &str = "hit";
const LOOKUP_OUTCOME_MISS: &str = "miss";
const LOOKUP_OUTCOME_STALE: &str = "stale";
const LOOKUP_OUTCOME_TORN: &str = "torn";
const LOOKUP_OUTCOME_UNKNOWN_VERSION: &str = "unknown_version";
/// `ourios.template_map.publish.outcome` attribute values (RFC 0033 §3.7).
const PUBLISH_OUTCOME_PUBLISHED: &str = "published";
const PUBLISH_OUTCOME_LOST_RACE: &str = "lost_race";
const PUBLISH_OUTCOME_ERROR: &str = "error";

/// The RFC 0033 §3.7 instruments: lookups by outcome, publishes by
/// outcome, and the artifact byte size at publish. Names come from the
/// weaver-generated [`ourios_semconv`] constants.
struct TemplateMapMetrics {
    lookups: Counter<u64>,
    publishes: Counter<u64>,
    artifact_size: Histogram<u64>,
}

/// Resolved once, on the first lookup or publish, through the
/// process-global meter (the RFC 0001 §6.8 API/SDK split) — every binary
/// installs its `MeterProvider` at startup, before serving a query, so
/// the lazy init binds to the real provider; with none installed the
/// instruments are cheap no-ops. Both counters carry a *required*
/// outcome attribute, so neither is zero-seeded (the
/// `CompactionMetrics` stance): each series surfaces on its first real
/// measurement.
static METRICS: LazyLock<TemplateMapMetrics> = LazyLock::new(|| {
    let meter = global::meter("ourios.template_map");
    TemplateMapMetrics {
        lookups: meter
            .u64_counter(semconv::OURIOS_TEMPLATE_MAP_LOOKUPS)
            .with_unit("{lookup}")
            .build(),
        publishes: meter
            .u64_counter(semconv::OURIOS_TEMPLATE_MAP_PUBLISHES)
            .with_unit("{publish}")
            .build(),
        artifact_size: meter
            .u64_histogram(semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE)
            .with_unit("By")
            .build(),
    }
});

impl TemplateMapMetrics {
    fn record_lookup(&self, outcome: CacheOutcome) {
        let value = match outcome {
            CacheOutcome::Hit => LOOKUP_OUTCOME_HIT,
            CacheOutcome::Miss {
                reason: MissReason::Absent,
            } => LOOKUP_OUTCOME_MISS,
            CacheOutcome::Miss {
                reason: MissReason::Torn,
            } => LOOKUP_OUTCOME_TORN,
            CacheOutcome::Miss {
                reason: MissReason::UnknownVersion,
            } => LOOKUP_OUTCOME_UNKNOWN_VERSION,
            CacheOutcome::StaleRefreshed => LOOKUP_OUTCOME_STALE,
        };
        self.lookups.add(
            1,
            &[KeyValue::new(
                semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME,
                value,
            )],
        );
    }

    fn record_publish(&self, outcome: &'static str) {
        self.publishes.add(
            1,
            &[KeyValue::new(
                semconv::OURIOS_TEMPLATE_MAP_PUBLISH_OUTCOME,
                outcome,
            )],
        );
    }
}

/// The per-tenant cached fold of the audit stream (RFC 0033 §3.2):
/// both derived maps plus the exact audit-file frontier they folded.
///
/// Fields are private and the only constructors are
/// [`derive_template_map`] (one scan, both folds) and
/// [`TemplateMap::from_artifact_bytes`] (a validated read of a
/// published artifact) — so a partially populated artifact (§3.5)
/// cannot exist.
#[derive(Debug)]
pub struct TemplateMap {
    tenant: TenantId,
    /// The audit `*.parquet` set the folds consumed, as store-relative
    /// keys under the tenant's audit root, sorted lexicographically —
    /// the §3.3 set-equality validity condition's left-hand side.
    folded_files: Vec<String>,
    registry: TemplateRegistry,
    aliases: AliasMap,
}

/// Outcome of reading `template_map.v2.json.zst` bytes — the RFC 0033
/// §3.3 dispositions that are decidable from the bytes alone. Absence
/// and staleness are the caller's to decide (it owns the GET and the
/// listing); a tenant mismatch is not a variant because it fails the
/// read loudly ([`QueryError::Storage`]) rather than degrading to a
/// fresh fold.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArtifactRead {
    /// Well-formed, known version, tenant verified — usable as a cache
    /// hit once the caller's frontier check passes.
    Valid(TemplateMap),
    /// Torn: not a zstd frame / failed decompression (§3.3 amendment),
    /// unparseable JSON, or internally invalid content. Treated as
    /// absent (fresh fold; write-through overwrites, so the store
    /// self-heals); `detail` feeds the §3.7 `torn` telemetry outcome.
    Torn { detail: String },
    /// A different writer's `format_version`, probed on the
    /// *decompressed* bytes (§3.2 amendment — defense-in-depth behind
    /// the version-in-key rule). Treated as absent (forward
    /// compatibility) — distinct from [`Self::Torn`] because it is not
    /// corruption and carries its own §3.7 outcome.
    UnknownVersion { format_version: u32 },
}

/// Outcome of a [`TemplateMap::publish`] (RFC 0033 §3.4). A lost race
/// is a **non-error** outcome, unlike the manifest's authoritative
/// generation swap: every writer publishes a correct fold of *some*
/// frontier and the reader verifies the frontier independently at every
/// read (§3.3), so the loser discards its write and moves on — no retry
/// loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// This writer's artifact is now the published one.
    Published,
    /// A concurrent writer published first (the create /
    /// compare-and-swap precondition failed). Whatever it published is
    /// a correct fold; a stale one is detected and rewritten on the
    /// next query.
    LostRace,
}

/// Why a [`load_or_derive`] lookup missed — the RFC 0033 §3.3
/// dispositions that resolve to the fresh fold, distinguished for the
/// §3.7 lookup-outcome telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MissReason {
    /// No artifact under the tenant's audit root.
    Absent,
    /// Torn: unreadable, unparseable, or internally invalid artifact —
    /// treated as absent. A parsed-but-invalid artifact is overwritten
    /// by the write-through at its observed `ETag` (the store
    /// self-heals); a failed GET observes no `ETag`, so on the remote
    /// backend its publish is create-only and the heal waits for a
    /// readable fetch — the fold answers the query either way.
    Torn,
    /// A future writer's `format_version` — treated as absent (forward
    /// compatibility), republished at this reader's version.
    UnknownVersion,
}

/// Outcome of one [`load_or_derive`] lookup (RFC 0033 §3.3/§3.7). Every
/// variant's answer is correct — the outcome distinguishes what IO paid
/// for it, never what was served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheOutcome {
    /// The artifact's frontier equals the live listing: served from the
    /// artifact, zero audit GETs.
    Hit,
    /// No usable artifact (§3.3 dispositions): fresh fold,
    /// write-through.
    Miss {
        /// The disposition that voided the artifact.
        reason: MissReason,
    },
    /// A valid artifact at a different frontier — never served (§3.3:
    /// re-derive, never serve stale): fresh fold over the live listing,
    /// write-through republish at the new frontier.
    StaleRefreshed,
}

/// The cached read path (RFC 0033 §3.3/§3.5): resolve `tenant`'s
/// [`TemplateMap`] through the published artifact when it is exactly
/// fresh, and by the fresh fold — today's behaviour, then a best-effort
/// write-through publish — on **every** other disposition.
///
/// IO per outcome (the freshness LIST is never byte-counted, matching
/// both backends' existing accounting):
///
/// - [`CacheOutcome::Hit`] — one LIST (the live frontier) + one GET
///   (the artifact); **zero** audit GETs. Acquisition bytes = the
///   artifact GET's bytes.
/// - [`CacheOutcome::Miss`] — one LIST + one artifact GET that found
///   nothing usable + the full audit fold over that same listing + one
///   best-effort publish. Acquisition bytes = the fold's audit bytes,
///   plus the failed GET's bytes if it returned any (torn / unknown
///   version).
/// - [`CacheOutcome::StaleRefreshed`] — as a miss, with the stale
///   artifact's GET bytes counted (they were fetched and discarded).
///
/// The listing is taken **once**, before the artifact GET is compared,
/// and the fallback fold consumes that same listing (§3.3's
/// LIST-before-GET-is-compared rule), so a file appearing mid-query
/// affects a cached and an uncached query identically.
///
/// The write-through publishes the fold it already holds at the frontier
/// it already listed — both folds, never a partial artifact (§3.5) —
/// with the CAS expectation observed by this read (the artifact's `ETag`,
/// or create-if-absent). A publish failure or lost race is never a query
/// failure; it abstains entirely when the **compressed** artifact would
/// not be smaller than the audit bytes just folded (§3.2 amendment —
/// nothing to win, and the no-artifact path is today's behaviour).
///
/// # Errors
///
/// [`QueryError::Storage`] as [`derive_template_map`], and when a
/// fetched artifact's body `tenant_id` differs from `tenant` (the
/// row-vs-path stance — a loud failure, not a fresh-fold fallback).
pub fn load_or_derive(
    backend: StoreRef<'_>,
    tenant: &TenantId,
) -> Result<(TemplateMap, u64, CacheOutcome), QueryError> {
    let resolved = audit_scan::resolve_audit_set(backend, tenant)?;
    let (fetched_bytes, expected, outcome) = match fetch_artifact(backend, tenant) {
        FetchedArtifact::Absent => (
            0,
            None,
            CacheOutcome::Miss {
                reason: MissReason::Absent,
            },
        ),
        FetchedArtifact::Unreadable => (
            0,
            None,
            CacheOutcome::Miss {
                reason: MissReason::Torn,
            },
        ),
        FetchedArtifact::Present { bytes, e_tag } => {
            let len = bytes.len() as u64;
            match TemplateMap::from_artifact_bytes(&bytes, tenant)? {
                ArtifactRead::Valid(map) if map.folded_files() == resolved.frontier() => {
                    METRICS.record_lookup(CacheOutcome::Hit);
                    return Ok((map, len, CacheOutcome::Hit));
                }
                ArtifactRead::Valid(_) => (len, e_tag, CacheOutcome::StaleRefreshed),
                ArtifactRead::Torn { .. } => (
                    len,
                    e_tag,
                    CacheOutcome::Miss {
                        reason: MissReason::Torn,
                    },
                ),
                ArtifactRead::UnknownVersion { .. } => (
                    len,
                    e_tag,
                    CacheOutcome::Miss {
                        reason: MissReason::UnknownVersion,
                    },
                ),
            }
        }
    };
    let (map, fold_bytes) = fold_template_map(resolved, tenant)?;
    let acquisition_bytes =
        fold_bytes
            .checked_add(fetched_bytes)
            .ok_or_else(|| QueryError::Storage {
                detail: format!(
                    "template-map acquisition bytes overflow u64 (fold={fold_bytes}, \
                     artifact={fetched_bytes})"
                ),
            })?;
    // Recorded only after every fallible step — a counted outcome is
    // always one that answered, mirroring the hit arm's record-at-return.
    METRICS.record_lookup(outcome);
    map.write_through(backend, expected.as_deref(), fold_bytes);
    Ok((map, acquisition_bytes, outcome))
}

/// One GET of `tenant`'s artifact, classified. Infallible by design: a
/// failed (non-not-found) GET is `Unreadable` — RFC 0033 §1 pins that an
/// unreadable artifact is *bypassed* (the fresh fold is always a correct
/// answer), never a query failure the audit fold wouldn't itself hit.
enum FetchedArtifact {
    Absent,
    Unreadable,
    Present {
        bytes: Vec<u8>,
        /// The `ETag` observed (S3 backend) — the CAS expectation a
        /// write-through publish carries. `None` on the local backend
        /// (its publish is last-writer-wins, §3.4).
        e_tag: Option<String>,
    },
}

fn fetch_artifact(backend: StoreRef<'_>, tenant: &TenantId) -> FetchedArtifact {
    let enc = percent_encode_tenant(tenant.as_str());
    match backend {
        StoreRef::Local(root) => {
            let path = root
                .join("audit")
                .join(format!("tenant_id={enc}"))
                .join(TEMPLATE_MAP_FILENAME);
            match std::fs::read(&path) {
                Ok(bytes) => FetchedArtifact::Present { bytes, e_tag: None },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => FetchedArtifact::Absent,
                Err(_) => FetchedArtifact::Unreadable,
            }
        }
        StoreRef::Remote(store) => {
            let key = format!("audit/tenant_id={enc}/{TEMPLATE_MAP_FILENAME}");
            match store.get_with_etag_blocking_opt(&key) {
                Ok(Some((bytes, e_tag))) => FetchedArtifact::Present { bytes, e_tag },
                Ok(None) => FetchedArtifact::Absent,
                Err(_) => FetchedArtifact::Unreadable,
            }
        }
    }
}

/// Fold `tenant`'s [`TemplateMap`] from its audit stream — **one**
/// `audit_scan` listing + read pass, both folds from the captured events
/// (RFC 0033 §3.5: the marginal cost of the second fold is CPU over
/// in-memory events, zero extra IO), and the frontier taken from that
/// same scan. Also returns the **bytes fetched** deriving it (RFC 0031
/// §3.6 — on a cache miss this is exactly what template-map acquisition
/// cost).
///
/// Each fold is byte-for-byte the fresh derivation it caches:
/// the registry filter + `fold_registry` matches
/// [`crate::derive_template_registry`], and the alias filter + stable
/// timestamp sort + `AliasMap::from_events` matches
/// [`crate::derive_alias_map`] (RFC0033.1 pins this by property test).
///
/// # Errors
///
/// [`QueryError::Storage`] if the audit subtree cannot be listed, an
/// audit file cannot be read, or a row claims a tenant other than the
/// one whose partition root it lives under (the RFC 0005 §3.9
/// row-vs-path backstop).
pub fn derive_template_map(
    backend: StoreRef<'_>,
    tenant: &TenantId,
) -> Result<(TemplateMap, u64), QueryError> {
    fold_template_map(audit_scan::resolve_audit_set(backend, tenant)?, tenant)
}

/// [`derive_template_map`] from a pre-resolved audit set — the fallback
/// arm of [`load_or_derive`], which must fold the **same** listing its
/// freshness comparison used (RFC 0033 §3.3: one listing, taken once,
/// for both).
fn fold_template_map(
    resolved: audit_scan::ResolvedAuditSet<'_>,
    tenant: &TenantId,
) -> Result<(TemplateMap, u64), QueryError> {
    let scan = resolved.read_events(tenant)?;

    let mut alias_events: Vec<&AuditEvent> = scan
        .events
        .iter()
        .filter(|e| {
            matches!(
                &e.payload,
                AuditPayload::AliasAsserted { .. } | AuditPayload::AliasRetracted { .. }
            )
        })
        .collect();
    alias_events.sort_by_key(|e| e.timestamp);
    let aliases = AliasMap::from_events(alias_events.iter().copied());
    drop(alias_events);

    let template_events: Vec<AuditEvent> = scan
        .events
        .into_iter()
        .filter(|e| matches!(&e.payload, AuditPayload::Template { .. }))
        .collect();
    let registry = template_registry::fold_registry(template_events);

    Ok((
        TemplateMap {
            tenant: tenant.clone(),
            folded_files: scan.frontier,
            registry,
            aliases,
        },
        scan.bytes_read,
    ))
}

/// The artifact's JSON body — the decompressed content of the zstd
/// frame (RFC 0033 §3.2; the amendment changed only the transport
/// encoding, this shape is unchanged). Kept separate from
/// [`TemplateMap`] so the semantic type never holds unvalidated
/// content and the wire field names are pinned independently of the
/// in-memory representation.
#[derive(Serialize, Deserialize)]
struct TemplateMapJson {
    format_version: u32,
    tenant_id: String,
    folded_files: Vec<String>,
    registry: Vec<RegistryEntry>,
    alias_map: Vec<AliasClass>,
}

/// One `(template_id, version)` registry key with its template in the
/// canonical space-joined `format_template` form the audit stream
/// itself stores — no second token encoding (RFC 0033 §3.2).
#[derive(Serialize, Deserialize)]
struct RegistryEntry {
    template_id: u64,
    version: u32,
    template: String,
}

/// One folded alias equivalence class: `members` sorted ascending,
/// `representative = min(members)` (RFC 0001 §6.7).
#[derive(Serialize, Deserialize)]
struct AliasClass {
    representative: u64,
    members: Vec<u64>,
}

/// First-pass probe: only `format_version`, so an unknown-version
/// artifact whose other fields have evolved is classified
/// [`ArtifactRead::UnknownVersion`], not torn.
#[derive(Deserialize)]
struct VersionProbe {
    format_version: u32,
}

impl TemplateMap {
    /// The tenant this artifact was derived for — matches the audit
    /// row encoding ([`TenantId::as_str`], byte-for-byte).
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The frontier: the audit `*.parquet` set the folds consumed, as
    /// store-relative keys under the tenant's audit root, sorted
    /// lexicographically (RFC 0033 §3.2/§3.3).
    #[must_use]
    pub fn folded_files(&self) -> &[String] {
        &self.folded_files
    }

    /// The cached RFC 0017 §3.2 template registry.
    #[must_use]
    pub fn registry(&self) -> &TemplateRegistry {
        &self.registry
    }

    /// The cached RFC 0005 §3.7.1 alias map.
    #[must_use]
    pub fn alias_map(&self) -> &AliasMap {
        &self.aliases
    }

    /// The published-object bytes: the canonical JSON body
    /// zstd-compressed as a single frame at the crate-default level
    /// (§3.2 amendment — an implementation constant, not
    /// configuration). This is exactly what a warm GET pays, what the
    /// §3.5 abstention compares against the folded audit bytes, and
    /// what the §3.7 `artifact.size` histogram records.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if JSON serialization fails, an alias
    /// class violates the non-empty invariant, or the zstd encode
    /// fails.
    pub fn to_artifact_bytes(&self) -> Result<Vec<u8>, QueryError> {
        let json = self.to_json()?;
        if json.len() as u64 > ARTIFACT_MAX_DECOMPRESSED_BYTES {
            return Err(QueryError::Storage {
                detail: format!(
                    "{TEMPLATE_MAP_FILENAME} body would be {} bytes, past the \
                     {ARTIFACT_MAX_DECOMPRESSED_BYTES}-byte reader bound",
                    json.len(),
                ),
            });
        }
        zstd::encode_all(json.as_slice(), ARTIFACT_ZSTD_LEVEL).map_err(|e| QueryError::Storage {
            detail: format!("compress {TEMPLATE_MAP_FILENAME}: {e}"),
        })
    }

    /// Serialize to the canonical JSON body the artifact frame carries:
    /// registry entries sorted by `(template_id, version)`, alias
    /// classes sorted by representative, members sorted — so two
    /// derivations of the same fold serialize identically.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if serialization fails (not expected for
    /// these plain structs) or an alias class violates the non-empty
    /// invariant — corruption that must fail loudly rather than
    /// serialize a torn artifact.
    fn to_json(&self) -> Result<Vec<u8>, QueryError> {
        let mut registry: Vec<RegistryEntry> = self
            .registry
            .iter()
            .map(|(&(template_id, version), tokens)| RegistryEntry {
                template_id,
                version,
                template: format_template(tokens),
            })
            .collect();
        registry.sort_unstable_by_key(|e| (e.template_id, e.version));
        let alias_map: Vec<AliasClass> = self
            .aliases
            .classes(&self.tenant)
            .into_iter()
            .map(|class| match class.first().copied() {
                Some(representative) => Ok(AliasClass {
                    representative,
                    members: class.into_iter().collect(),
                }),
                // A stored class always has ≥ 2 members (the AliasMap
                // invariant); an empty one is corruption and must fail
                // LOUDLY rather than serialize representative 0 into a
                // torn artifact.
                None => Err(QueryError::Storage {
                    detail: "alias class with no members — AliasMap invariant violation"
                        .to_string(),
                }),
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        serde_json::to_vec(&TemplateMapJson {
            format_version: TEMPLATE_MAP_FORMAT_VERSION,
            tenant_id: self.tenant.as_str().to_owned(),
            folded_files: self.folded_files.clone(),
            registry,
            alias_map,
        })
        .map_err(|e| QueryError::Storage {
            detail: format!("template_map serialization: {e}"),
        })
    }

    /// Parse and validate artifact `bytes` fetched from `tenant`'s
    /// audit root, applying the RFC 0033 §3.3 dispositions (as amended
    /// 2026-07-13): a missing zstd frame or failed decompression is
    /// torn, the `format_version` probe runs on the decompressed bytes,
    /// and torn / internally invalid content and unknown
    /// `format_version` come back as their [`ArtifactRead`] variants
    /// (callers treat both as absent — the fresh fold is always a
    /// correct answer).
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] when the artifact's body `tenant_id`
    /// differs from the tenant whose path it was fetched from — a
    /// corrupt or foreign object under the tenant's root, failed
    /// loudly per the RFC 0005 §3.9 row-vs-path stance (exactly as
    /// the audit scan fails a foreign row, never serving or silently
    /// ignoring it).
    pub fn from_artifact_bytes(
        bytes: &[u8],
        tenant: &TenantId,
    ) -> Result<ArtifactRead, QueryError> {
        let torn = |detail: String| Ok(ArtifactRead::Torn { detail });
        let json = match decode_bounded(bytes) {
            Ok(json) => json,
            Err(e) => return torn(format!("decompress {TEMPLATE_MAP_FILENAME}: {e}")),
        };
        let bytes = json.as_slice();
        let probe: VersionProbe = match serde_json::from_slice(bytes) {
            Ok(probe) => probe,
            Err(e) => return torn(format!("parse {TEMPLATE_MAP_FILENAME}: {e}")),
        };
        if probe.format_version != TEMPLATE_MAP_FORMAT_VERSION {
            return Ok(ArtifactRead::UnknownVersion {
                format_version: probe.format_version,
            });
        }
        let raw: TemplateMapJson = match serde_json::from_slice(bytes) {
            Ok(raw) => raw,
            Err(e) => return torn(format!("parse {TEMPLATE_MAP_FILENAME}: {e}")),
        };
        if raw.tenant_id != tenant.as_str() {
            return Err(QueryError::Storage {
                detail: format!(
                    "{TEMPLATE_MAP_FILENAME} claims tenant {} under tenant {}'s audit root",
                    raw.tenant_id,
                    tenant.as_str(),
                ),
            });
        }
        if let Err(detail) = validate(&raw) {
            return torn(detail);
        }
        let registry: TemplateRegistry = raw
            .registry
            .iter()
            .map(|e| ((e.template_id, e.version), parse_template(&e.template)))
            .collect();
        let aliases = AliasMap::from_classes(
            tenant,
            raw.alias_map
                .into_iter()
                .map(|class| class.members.into_iter().collect()),
        );
        Ok(ArtifactRead::Valid(Self {
            tenant: tenant.clone(),
            folded_files: raw.folded_files,
            registry,
            aliases,
        }))
    }

    /// Publish this artifact to
    /// `audit/tenant_id=<enc>/template_map.v2.json.zst` (RFC 0033 §3.2
    /// amendment) following the RFC 0009 §3.4 manifest precedent,
    /// adapted to a derived, discardable object:
    ///
    /// - [`StoreRef::Local`]: write `template_map.v2.json.zst.tmp` in
    ///   the tenant's audit dir and `rename` it into place — the rename
    ///   is the only visibility point, so a reader observes the prior
    ///   artifact (or its absence) or the new one, never a partial
    ///   write; a crash leaves a stray `.tmp` no reader opens. Like
    ///   `Manifest::write_atomic` this is atomic but not crash-durable
    ///   (no fsync): losing the artifact in a crash costs one
    ///   re-derivation. `expected` cannot be enforced on the filesystem
    ///   (no conditional rename), so this branch is last-writer-wins —
    ///   safe because any published artifact is a correct fold of some
    ///   frontier, verified at every read (§3.3/§3.4).
    /// - [`StoreRef::Remote`]: single-object conditional put (the
    ///   `Manifest::publish_cas` shape). `expected` is the `ETag`
    ///   observed when the artifact was last read, or `None` when it
    ///   was observed absent (create-if-absent). A failed precondition
    ///   is [`PublishOutcome::LostRace`], never an error.
    ///
    /// A successful publish then best-effort **deletes** the stale v1
    /// key ([`TEMPLATE_MAP_V1_FILENAME`], §3.4 amendment) —
    /// unconditional, never a query failure; a crash or failure between
    /// publish and delete leaves both keys, which is harmless (each
    /// reader population GETs only its own key), and the next
    /// successful publish retries the delete implicitly.
    ///
    /// Publish failure is surfaceable but non-fatal by contract (§3.5):
    /// the write-through caller records it as telemetry and answers its
    /// query from the fold it already holds.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if serialization/compression fails or on
    /// a non-precondition filesystem/backend failure.
    pub fn publish(
        &self,
        backend: StoreRef<'_>,
        expected: Option<&str>,
    ) -> Result<PublishOutcome, QueryError> {
        let bytes = self.to_artifact_bytes().map_err(|e| {
            METRICS.record_publish(PUBLISH_OUTCOME_ERROR);
            QueryError::Storage {
                detail: format!("serialize {TEMPLATE_MAP_FILENAME}: {e}"),
            }
        })?;
        self.publish_bytes(backend, expected, bytes)
    }

    /// The RFC 0033 §3.5 write-through: publish this fresh fold
    /// best-effort after a cache miss. Never fails the caller — a
    /// serialization or backend failure and a lost race are all
    /// telemetry-only outcomes (the §3.7 publish-outcome counter, via
    /// [`publish_bytes`](Self::publish_bytes)) — and **abstains** when
    /// the **compressed** artifact would not be smaller than
    /// `folded_audit_bytes`, the audit bytes the fold just read (§3.2
    /// amendment: the comparison is between the bytes a warm GET would
    /// pay and the bytes the fold just paid; on a tiny or empty tenant
    /// even the compressed envelope can exceed the stream it caches,
    /// and the no-artifact path is exactly today's behaviour). An
    /// abstention records no publish outcome — §3.7 pins the values to
    /// `published` / `lost_race` / `error`, and a publish that never
    /// starts is none of them.
    fn write_through(
        &self,
        backend: StoreRef<'_>,
        expected: Option<&str>,
        folded_audit_bytes: u64,
    ) {
        let Ok(bytes) = self.to_artifact_bytes() else {
            METRICS.record_publish(PUBLISH_OUTCOME_ERROR);
            return;
        };
        if bytes.len() as u64 >= folded_audit_bytes {
            return;
        }
        // Best-effort by contract: Published and LostRace are both fine,
        // and an error must not fail the query answered from the fold in
        // hand (publish_bytes already recorded it).
        let _ = self.publish_bytes(backend, expected, bytes);
    }

    /// Commit `bytes` per the §3.4 backend ladder, recording the §3.7
    /// publish outcome — and, on [`PublishOutcome::Published`], the
    /// artifact byte size (compressed: the published-object bytes a
    /// warm GET pays, the number RFC0033.6 gates on) — for every
    /// caller, so the write-through and a direct [`Self::publish`]
    /// count identically.
    fn publish_bytes(
        &self,
        backend: StoreRef<'_>,
        expected: Option<&str>,
        bytes: Vec<u8>,
    ) -> Result<PublishOutcome, QueryError> {
        let size = bytes.len() as u64;
        let result = self.publish_bytes_inner(backend, expected, bytes);
        match &result {
            Ok(PublishOutcome::Published) => {
                METRICS.record_publish(PUBLISH_OUTCOME_PUBLISHED);
                METRICS.artifact_size.record(size, &[]);
            }
            Ok(PublishOutcome::LostRace) => METRICS.record_publish(PUBLISH_OUTCOME_LOST_RACE),
            Err(_) => METRICS.record_publish(PUBLISH_OUTCOME_ERROR),
        }
        result
    }

    fn publish_bytes_inner(
        &self,
        backend: StoreRef<'_>,
        expected: Option<&str>,
        bytes: Vec<u8>,
    ) -> Result<PublishOutcome, QueryError> {
        let enc = percent_encode_tenant(self.tenant.as_str());
        match backend {
            StoreRef::Local(root) => {
                let io_err = |op: &str, p: &Path, e: &std::io::Error| QueryError::Storage {
                    detail: format!("{op} {}: {e}", p.display()),
                };
                let dir = root.join("audit").join(format!("tenant_id={enc}"));
                std::fs::create_dir_all(&dir).map_err(|e| io_err("create_dir_all", &dir, &e))?;
                let tmp = dir.join(format!("{TEMPLATE_MAP_FILENAME}.tmp"));
                std::fs::write(&tmp, &bytes).map_err(|e| io_err("write", &tmp, &e))?;
                let target = dir.join(TEMPLATE_MAP_FILENAME);
                std::fs::rename(&tmp, &target).map_err(|e| QueryError::Storage {
                    detail: format!("rename {} -> {}: {e}", tmp.display(), target.display()),
                })?;
                // §3.4 amendment: best-effort unconditional delete of the
                // stale v1 key — failure is harmless (both keys may
                // coexist; the next publish retries implicitly).
                let _ = std::fs::remove_file(dir.join(TEMPLATE_MAP_V1_FILENAME));
                Ok(PublishOutcome::Published)
            }
            StoreRef::Remote(store) => {
                let key = format!("audit/tenant_id={enc}/{TEMPLATE_MAP_FILENAME}");
                let result = match expected {
                    None => store.put_if_absent_blocking(&key, bytes),
                    Some(e_tag) => store.put_if_match_blocking(&key, bytes, e_tag),
                };
                match result {
                    Ok(()) => {
                        // §3.4 amendment: best-effort unconditional delete
                        // of the stale v1 key (no CAS — any v1 artifact is
                        // derived and discardable); failure is harmless.
                        let _ = store.delete_blocking(&format!(
                            "audit/tenant_id={enc}/{TEMPLATE_MAP_V1_FILENAME}"
                        ));
                        Ok(PublishOutcome::Published)
                    }
                    Err(e) if e.is_precondition() || e.is_already_exists() => {
                        Ok(PublishOutcome::LostRace)
                    }
                    Err(e) => Err(QueryError::Storage {
                        detail: format!("publish {key}: {e}"),
                    }),
                }
            }
        }
    }
}

/// Validate the canonical form [`TemplateMap::to_json`] writes; any
/// violation is a torn artifact (treated as absent, self-healed by the
/// next write-through). The frontier check mirrors the `Manifest`
/// filename validation: entries must be tenant-root-relative `*.parquet`
/// keys, so a hostile artifact cannot name paths outside the tenant's
/// audit subtree (`CLAUDE.md` §3.7).
fn validate(raw: &TemplateMapJson) -> Result<(), String> {
    // Canonical-form check on every template string: the artifact is
    // untrusted input. parse_template/format_template round-trip alone
    // is NOT sufficient — empty segments (leading/trailing/doubled
    // spaces) parse to Fixed("") and round-trip unchanged — so enforce
    // what mine-time tokenization actually guarantees: a non-empty
    // string splits into non-empty, whitespace-free tokens.
    for entry in &raw.registry {
        let canonical = entry.template.is_empty()
            || entry
                .template
                .split(' ')
                .all(|tok| !tok.is_empty() && !tok.chars().any(char::is_whitespace));
        if !canonical {
            return Err(format!(
                "registry template for id {} v{} is not in canonical format_template form",
                entry.template_id, entry.version,
            ));
        }
    }
    for name in &raw.folded_files {
        if !is_tenant_relative_parquet(name) {
            return Err(format!(
                "folded_files entry is not a tenant-relative *.parquet key: {name:?}"
            ));
        }
    }
    for pair in raw.folded_files.windows(2) {
        if pair[0] >= pair[1] {
            return Err(format!(
                "folded_files is not strictly sorted: {:?} >= {:?}",
                pair[0], pair[1],
            ));
        }
    }
    for pair in raw.registry.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if (a.template_id, a.version) >= (b.template_id, b.version) {
            return Err(format!(
                "registry is not strictly sorted by (template_id, version) at \
                 ({}, {}) / ({}, {})",
                a.template_id, a.version, b.template_id, b.version,
            ));
        }
    }
    let mut seen_members = std::collections::HashSet::new();
    for class in &raw.alias_map {
        if class.members.len() < 2 {
            return Err(format!(
                "alias class of {} member(s) is not an alias set",
                class.members.len()
            ));
        }
        for pair in class.members.windows(2) {
            if pair[0] >= pair[1] {
                return Err(format!(
                    "alias class members are not strictly sorted: {} >= {}",
                    pair[0], pair[1],
                ));
            }
        }
        if class.members.first() != Some(&class.representative) {
            return Err(format!(
                "alias class representative {} is not min(members)",
                class.representative
            ));
        }
        for member in &class.members {
            if !seen_members.insert(*member) {
                return Err(format!("alias classes overlap on template_id {member}"));
            }
        }
    }
    for pair in raw.alias_map.windows(2) {
        if pair[0].representative >= pair[1].representative {
            return Err(format!(
                "alias classes are not sorted by representative: {} >= {}",
                pair[0].representative, pair[1].representative,
            ));
        }
    }
    Ok(())
}

/// Whether `name` is a tenant-root-relative `*.parquet` key: one or
/// more ordinary path segments (no `..`, no root, no prefix) with a
/// lowercase `.parquet` extension — multi-segment because audit keys
/// carry their `year=…/month=…/day=…` partition dirs, otherwise the
/// same stance as the manifest's `is_partition_local_parquet`.
fn is_tenant_relative_parquet(name: &str) -> bool {
    use std::path::Component;
    // Path::components() normalizes away empty segments, so a doubled
    // slash would slip past the component check and misclassify a
    // malformed artifact Valid (failing only later, at frontier
    // equality) — reject non-canonical separators explicitly first.
    if name.is_empty() || name.contains('\\') || name.split('/').any(str::is_empty) {
        return false;
    }
    // Tenant-root-relative means the audit writer's Hive layout: the
    // first segment is `year=…` and no segment re-introduces the
    // `tenant_id=`/`audit` prefix — a foreign-tree name can't escape
    // (the frontier is compared, never dereferenced) but would
    // misclassify a hostile artifact stale instead of torn.
    let mut segments = name.split('/');
    if !segments.next().is_some_and(|s| s.starts_with("year=")) {
        return false;
    }
    if name
        .split('/')
        .any(|s| s.starts_with("tenant_id=") || s == "audit")
    {
        return false;
    }
    let path = Path::new(name);
    let mut components = path.components();
    let all_normal = components
        .by_ref()
        .all(|c| matches!(c, Component::Normal(_)));
    all_normal && path.extension().is_some_and(|ext| ext == "parquet")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ourios_core::audit::TEMPLATE_INITIAL_VERSION;
    use ourios_miner::tree::OwnedToken;

    use super::*;

    fn tenant() -> TenantId {
        TenantId::new("acme")
    }

    /// A hand-built artifact with both folds populated and an
    /// out-of-order-on-purpose in-memory shape, so the tests observe
    /// the canonicalisation `to_json` applies.
    fn sample() -> TemplateMap {
        let mut registry = TemplateRegistry::new();
        registry.insert(
            (7, 2),
            vec![OwnedToken::Fixed("user".into()), OwnedToken::Wildcard],
        );
        registry.insert(
            (7, TEMPLATE_INITIAL_VERSION),
            vec![OwnedToken::Fixed("user".into())],
        );
        registry.insert((3, TEMPLATE_INITIAL_VERSION), vec![OwnedToken::Wildcard]);
        let aliases = AliasMap::from_classes(
            &tenant(),
            vec![
                [9, 12].into_iter().collect::<BTreeSet<u64>>(),
                [3, 7].into_iter().collect(),
            ],
        );
        TemplateMap {
            tenant: tenant(),
            folded_files: vec![
                "year=2026/month=07/day=11/a.parquet".to_string(),
                "year=2026/month=07/day=12/b.parquet".to_string(),
            ],
            registry,
            aliases,
        }
    }

    fn expect_valid(read: Result<ArtifactRead, QueryError>) -> TemplateMap {
        match read.expect("read") {
            ArtifactRead::Valid(map) => map,
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    /// Compress a mutated JSON body into the v2 transport frame, so the
    /// torn/invalid fixtures exercise the same decompress-then-parse
    /// path a real GET takes.
    fn compress(json: &[u8]) -> Vec<u8> {
        zstd::encode_all(json, ARTIFACT_ZSTD_LEVEL).expect("compress fixture")
    }

    #[test]
    fn round_trips_through_artifact_bytes() {
        // Arrange
        let map = sample();

        // Act
        let restored = expect_valid(TemplateMap::from_artifact_bytes(
            &map.to_artifact_bytes().expect("serialize"),
            &tenant(),
        ));

        // Assert — both folds and the frontier survive byte-identically.
        assert_eq!(restored.registry(), map.registry());
        assert_eq!(
            restored.alias_map().classes(&tenant()),
            map.alias_map().classes(&tenant()),
        );
        assert_eq!(restored.folded_files(), map.folded_files());
        assert_eq!(restored.tenant(), &tenant());
    }

    #[test]
    fn serialization_is_canonical() {
        // The wire form sorts registry entries by (template_id, version)
        // and alias classes by representative, independent of in-memory
        // iteration order.
        let json: serde_json::Value =
            serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
        let keys: Vec<(u64, u64)> = json["registry"]
            .as_array()
            .expect("registry array")
            .iter()
            .map(|e| {
                (
                    e["template_id"].as_u64().expect("id"),
                    e["version"].as_u64().expect("version"),
                )
            })
            .collect();
        assert_eq!(keys, vec![(3, 1), (7, 1), (7, 2)]);
        let reps: Vec<u64> = json["alias_map"]
            .as_array()
            .expect("alias array")
            .iter()
            .map(|c| c["representative"].as_u64().expect("rep"))
            .collect();
        assert_eq!(reps, vec![3, 9]);
        assert_eq!(json["format_version"], TEMPLATE_MAP_FORMAT_VERSION);
    }

    #[test]
    fn torn_bytes_are_treated_as_absent() {
        // Non-frames (a v1-style plain-JSON body planted at the v2 key
        // included — the §3.3 amendment's "not a zstd frame" arm), a
        // truncated frame, and frames of unparseable JSON all classify
        // torn, never an error.
        let good = sample().to_artifact_bytes().expect("serialize");
        let bad_json = compress(b"not json");
        let truncated_json = compress(b"{\"format_version\":");
        for bytes in [
            &b"not a zstd frame"[..],
            &br#"{"format_version":1,"tenant_id":"acme"}"#[..],
            &good[..good.len() / 2],
            &bad_json[..],
            &truncated_json[..],
        ] {
            let read =
                TemplateMap::from_artifact_bytes(bytes, &tenant()).expect("torn is not an error");
            assert!(
                matches!(read, ArtifactRead::Torn { .. }),
                "{bytes:?} must classify as torn",
            );
        }
    }

    #[test]
    fn decompression_bomb_classifies_torn_within_the_bound() {
        // The artifact is untrusted input: a crafted frame whose tiny
        // compressed body expands past the reader bound must classify
        // torn without ever allocating the full expansion.
        let expansion = usize::try_from(ARTIFACT_MAX_DECOMPRESSED_BYTES + 1024).expect("test host");
        let bomb = compress(&vec![0u8; expansion]);
        assert!(
            bomb.len() < 1024 * 1024,
            "the bomb must be small compressed ({} bytes) for the test to mean anything",
            bomb.len(),
        );
        let read =
            TemplateMap::from_artifact_bytes(&bomb, &tenant()).expect("oversize is not an error");
        match read {
            ArtifactRead::Torn { detail } => assert!(
                detail.contains("bound"),
                "the torn detail names the bound: {detail}",
            ),
            other => panic!("expected Torn, got {other:?}"),
        }
    }

    #[test]
    fn unknown_format_version_is_treated_as_absent() {
        // The probe runs on the decompressed bytes (§3.2 amendment):
        // a future writer's body — the version bumped and the rest of
        // the shape changed entirely — and a v1 body shipped in the v2
        // transport both classify UnknownVersion, not Torn.
        for (json, version) in [
            (&br#"{"format_version": 3, "something_else": true}"#[..], 3),
            (&br#"{"format_version": 1, "tenant_id": "acme"}"#[..], 1),
        ] {
            let read = TemplateMap::from_artifact_bytes(&compress(json), &tenant())
                .expect("unknown version is no error");
            assert!(
                matches!(
                    read,
                    ArtifactRead::UnknownVersion { format_version } if format_version == version
                ),
                "got {read:?}",
            );
        }
    }

    #[test]
    fn tenant_mismatch_fails_loudly() {
        // The row-vs-path stance (RFC 0005 §3.9): a well-formed artifact
        // claiming another tenant under this tenant's root is a corrupt
        // or foreign object — an error, never absent-and-refolded.
        let bytes = sample().to_artifact_bytes().expect("serialize");
        let err = TemplateMap::from_artifact_bytes(&bytes, &TenantId::new("intruder"))
            .expect_err("foreign artifact must fail the read");
        match err {
            QueryError::Storage { detail } => assert!(
                detail.contains("claims tenant acme under tenant intruder"),
                "unexpected detail: {detail}",
            ),
            other => panic!("expected Storage, got {other:?}"),
        }
    }

    #[test]
    fn unsorted_frontier_is_torn() {
        let mut json: serde_json::Value =
            serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
        json["folded_files"] = serde_json::json!([
            "year=2026/month=07/day=12/b.parquet",
            "year=2026/month=07/day=11/a.parquet",
        ]);
        let bytes = serde_json::to_vec(&json).expect("serialize");
        let read = TemplateMap::from_artifact_bytes(&compress(&bytes), &tenant()).expect("read");
        assert!(matches!(read, ArtifactRead::Torn { .. }), "got {read:?}");
    }

    #[test]
    fn escaping_or_foreign_frontier_entries_are_torn() {
        for entry in [
            "../other-tenant/a.parquet",
            "/abs/a.parquet",
            "a.parquet.tmp",
            "template_map.json",
            "",
        ] {
            let mut json: serde_json::Value =
                serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
            json["folded_files"] = serde_json::json!([entry]);
            let bytes = serde_json::to_vec(&json).expect("serialize");
            let read =
                TemplateMap::from_artifact_bytes(&compress(&bytes), &tenant()).expect("read");
            assert!(
                matches!(read, ArtifactRead::Torn { .. }),
                "{entry:?} must classify as torn, got {read:?}",
            );
        }
    }

    #[test]
    fn invalid_alias_classes_are_torn() {
        for (label, classes) in [
            (
                "degenerate",
                serde_json::json!([{ "representative": 3, "members": [3] }]),
            ),
            (
                "unsorted members",
                serde_json::json!([{ "representative": 3, "members": [7, 3] }]),
            ),
            (
                "representative not min",
                serde_json::json!([{ "representative": 7, "members": [3, 7] }]),
            ),
            (
                "overlapping classes",
                serde_json::json!([
                    { "representative": 3, "members": [3, 7] },
                    { "representative": 5, "members": [5, 7] },
                ]),
            ),
        ] {
            let mut json: serde_json::Value =
                serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
            json["alias_map"] = classes;
            let bytes = serde_json::to_vec(&json).expect("serialize");
            let read =
                TemplateMap::from_artifact_bytes(&compress(&bytes), &tenant()).expect("read");
            assert!(
                matches!(read, ArtifactRead::Torn { .. }),
                "{label} must classify as torn, got {read:?}",
            );
        }
    }

    /// A second artifact distinguishable from [`sample`] (one more
    /// frontier entry), for asserting which of two publishes a reader
    /// observes.
    fn sample_at_wider_frontier() -> TemplateMap {
        let mut map = sample();
        map.folded_files
            .push("year=2026/month=07/day=13/c.parquet".to_string());
        map
    }

    #[test]
    fn publish_local_commits_atomically_and_reads_back() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        let map = sample();

        // Act
        let outcome = map
            .publish(crate::StoreRef::Local(bucket.path()), None)
            .expect("publish");

        // Assert — committed under the tenant's audit root, no `.tmp`
        // left behind, and the bytes read back Valid.
        assert_eq!(outcome, PublishOutcome::Published);
        let dir = bucket.path().join("audit").join("tenant_id=acme");
        assert!(!dir.join(format!("{TEMPLATE_MAP_FILENAME}.tmp")).exists());
        let bytes = std::fs::read(dir.join(TEMPLATE_MAP_FILENAME)).expect("read");
        let restored = expect_valid(TemplateMap::from_artifact_bytes(&bytes, &tenant()));
        assert_eq!(restored.folded_files(), map.folded_files());
        assert_eq!(restored.registry(), map.registry());
    }

    #[test]
    fn publish_local_republish_overwrites() {
        // The local branch is last-writer-wins (no filesystem CAS) —
        // safe because any published artifact is a correct fold of some
        // frontier, verified at every read (RFC 0033 §3.4).
        let bucket = tempfile::tempdir().expect("temp");
        let backend = crate::StoreRef::Local(bucket.path());
        let first = sample();
        let second = sample_at_wider_frontier();
        first.publish(backend, None).expect("first publish");

        let outcome = second.publish(backend, None).expect("second publish");

        assert_eq!(outcome, PublishOutcome::Published);
        let bytes = std::fs::read(
            bucket
                .path()
                .join("audit")
                .join("tenant_id=acme")
                .join(TEMPLATE_MAP_FILENAME),
        )
        .expect("read");
        let restored = expect_valid(TemplateMap::from_artifact_bytes(&bytes, &tenant()));
        assert_eq!(restored.folded_files(), second.folded_files());
    }

    #[test]
    fn publish_deletes_stale_v1_key() {
        // §3.4 amendment: a successful v2 publish best-effort-deletes
        // the superseded v1 key on both backends — and its absence
        // (nothing to delete) never fails a publish.
        let map = sample();

        let bucket = tempfile::tempdir().expect("temp");
        let dir = bucket.path().join("audit").join("tenant_id=acme");
        std::fs::create_dir_all(&dir).expect("create audit dir");
        std::fs::write(
            dir.join(TEMPLATE_MAP_V1_FILENAME),
            br#"{"format_version":1}"#,
        )
        .expect("plant stale v1 artifact");
        assert_eq!(
            map.publish(crate::StoreRef::Local(bucket.path()), None)
                .expect("publish"),
            PublishOutcome::Published,
        );
        assert!(dir.join(TEMPLATE_MAP_FILENAME).exists());
        assert!(
            !dir.join(TEMPLATE_MAP_V1_FILENAME).exists(),
            "the stale v1 key must be deleted after a v2 publish",
        );

        let root = tempfile::tempdir().expect("temp");
        let store = ourios_parquet::Store::local(root.path()).expect("store");
        let v1_key = format!("audit/tenant_id=acme/{TEMPLATE_MAP_V1_FILENAME}");
        store
            .put_blocking(&v1_key, br#"{"format_version":1}"#.to_vec())
            .expect("plant stale v1 artifact");
        assert_eq!(
            map.publish(crate::StoreRef::Remote(&store), None)
                .expect("publish"),
            PublishOutcome::Published,
        );
        assert!(
            store
                .get_blocking_opt(&v1_key)
                .expect("probe v1 key")
                .is_none(),
            "the stale v1 key must be deleted after a v2 publish",
        );
    }

    #[test]
    fn publish_store_create_if_absent_and_lost_race() {
        // The store branch through the `Store` seam. `LocalFileSystem`
        // supports the create-if-absent half of the conditional put, so
        // the None-expectation ladder — create wins, a concurrent
        // create loses harmlessly — runs without an S3 backend (the
        // `If-Match` swap half is the localstack integration arm).
        let root = tempfile::tempdir().expect("temp");
        let store = ourios_parquet::Store::local(root.path()).expect("store");
        let backend = crate::StoreRef::Remote(&store);
        let winner = sample();
        let loser = sample_at_wider_frontier();

        assert_eq!(
            winner.publish(backend, None).expect("create"),
            PublishOutcome::Published,
        );
        // The concurrent writer also observed absence; its stale
        // expectation loses the race as an outcome, not an error.
        assert_eq!(
            loser.publish(backend, None).expect("lost race is Ok"),
            PublishOutcome::LostRace,
        );

        // The store still holds the winner's valid, readable artifact.
        let bytes = store
            .get_blocking(&format!("audit/tenant_id=acme/{TEMPLATE_MAP_FILENAME}"))
            .expect("get");
        let held = expect_valid(TemplateMap::from_artifact_bytes(&bytes, &tenant()));
        assert_eq!(held.folded_files(), winner.folded_files());
    }

    #[test]
    fn duplicate_registry_keys_are_torn() {
        let mut json: serde_json::Value =
            serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
        json["registry"] = serde_json::json!([
            { "template_id": 7, "version": 1, "template": "user <*>" },
            { "template_id": 7, "version": 1, "template": "user <*> <*>" },
        ]);
        let bytes = serde_json::to_vec(&json).expect("serialize");
        let read = TemplateMap::from_artifact_bytes(&compress(&bytes), &tenant()).expect("read");
        assert!(matches!(read, ArtifactRead::Torn { .. }), "got {read:?}");
    }

    #[test]
    fn non_canonical_templates_classify_torn() {
        // Empty segments round-trip through parse/format unchanged, so
        // the validator must reject them explicitly.
        for bad in [" a", "a ", "a  b", "a\tb", " "] {
            let mut json: TemplateMapJson =
                serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
            json.registry[0].template = bad.to_string();
            let bytes = serde_json::to_vec(&json).expect("serialize");
            assert!(
                matches!(
                    TemplateMap::from_artifact_bytes(&compress(&bytes), &tenant()),
                    Ok(ArtifactRead::Torn { .. })
                ),
                "template {bad:?} must classify torn",
            );
        }
        // The empty template (zero-token) stays valid — it is canonical.
        let mut json: TemplateMapJson =
            serde_json::from_slice(&sample().to_json().expect("serialize")).expect("parse");
        json.registry[0].template = String::new();
        let bytes = serde_json::to_vec(&json).expect("serialize");
        assert!(matches!(
            TemplateMap::from_artifact_bytes(&compress(&bytes), &tenant()),
            Ok(ArtifactRead::Valid(_))
        ));
    }

    #[test]
    fn non_canonical_frontier_paths_rejected() {
        for bad in [
            "year=2026//month=07/a.parquet",
            "/year=2026/a.parquet",
            "year=2026/a.parquet/",
            "a\\b.parquet",
            "",
            "audit/tenant_id=other/year=2026/a.parquet",
            "tenant_id=other/year=2026/a.parquet",
            "month=07/a.parquet",
        ] {
            assert!(!is_tenant_relative_parquet(bad), "{bad:?} must be rejected");
        }
        assert!(is_tenant_relative_parquet("year=2026/month=07/a.parquet"));
    }
}
