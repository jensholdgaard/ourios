//! The RFC 0033 cached template-map artifact — format and derivation.
//!
//! One JSON object per tenant (`template_map.json`, RFC 0033 §3.2)
//! carrying **both** folds of the tenant's audit stream — the RFC 0017
//! §3.2 template registry and the RFC 0005 §3.7.1 alias map — plus the
//! `folded_files` frontier they were folded from. The audit stream
//! remains the source of truth; this artifact is a derived, discardable
//! acceleration, and every doubtful read resolves by folding the stream
//! (§3.3's dispositions, [`ArtifactRead`]).
//!
//! [`derive_template_map`] performs **one** [`crate::audit_scan`] pass
//! and folds both maps from that single capture — §3.5's no-partial
//! rule at the type level: a [`TemplateMap`] cannot be constructed
//! outside this module with only one fold populated, so a
//! registry-at-frontier-F1 / alias-map-at-F2 split is unrepresentable.
//! [`TemplateMap::publish`] commits the artifact atomically (tmp+rename
//! locally, conditional put on the store — §3.4, the RFC 0009 manifest
//! precedent); the freshness check (§3.3) and the write-through are the
//! follow-up slices.
//!
//! JSON follows the `manifest.json` precedent
//! (`ourios_parquet::Manifest`, RFC 0009 §3.4): small, human-
//! inspectable, `serde`-round-tripped, validated before use.

use std::path::Path;

use ourios_core::alias::AliasMap;
use ourios_core::audit::{AuditEvent, AuditPayload};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::{format_template, parse_template};
use ourios_parquet::percent_encode_tenant;
use serde::{Deserialize, Serialize};

use crate::template_registry::TemplateRegistry;
use crate::{QueryError, StoreRef, audit_scan, template_registry};

/// Canonical artifact filename at the root of a tenant's audit subtree
/// (`audit/tenant_id=<enc>/template_map.json`, RFC 0033 §3.2). Not a
/// `*.parquet` name, so every existing audit walk/listing ignores it by
/// construction.
pub const TEMPLATE_MAP_FILENAME: &str = "template_map.json";

/// The `format_version` this reader writes and understands. A reader
/// encountering any other version treats the artifact as absent
/// (forward compatibility, RFC 0033 §3.3) — no migration is ever
/// required because the artifact is derived and discardable.
pub const TEMPLATE_MAP_FORMAT_VERSION: u32 = 1;

/// The per-tenant cached fold of the audit stream (RFC 0033 §3.2):
/// both derived maps plus the exact audit-file frontier they folded.
///
/// Fields are private and the only constructors are
/// [`derive_template_map`] (one scan, both folds) and
/// [`TemplateMap::from_json`] (a validated read of a published
/// artifact) — so a partially populated artifact (§3.5) cannot exist.
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

/// Outcome of reading `template_map.json` bytes — the RFC 0033 §3.3
/// dispositions that are decidable from the bytes alone. Absence and
/// staleness are the caller's to decide (it owns the GET and the
/// listing); a tenant mismatch is not a variant because it fails the
/// read loudly ([`QueryError::Storage`]) rather than degrading to a
/// fresh fold.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArtifactRead {
    /// Well-formed, known version, tenant verified — usable as a cache
    /// hit once the caller's frontier check passes.
    Valid(TemplateMap),
    /// Torn: unparseable JSON or internally invalid content. Treated as
    /// absent (fresh fold; write-through overwrites, so the store
    /// self-heals); `detail` feeds the §3.7 `torn` telemetry outcome.
    Torn { detail: String },
    /// A future writer's `format_version`. Treated as absent (forward
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

/// Fold `tenant`'s [`TemplateMap`] from its audit stream — **one**
/// `audit_scan::read_all_events_captured` pass, both folds from the
/// captured events (RFC 0033 §3.5: the marginal cost of the second fold
/// is CPU over in-memory events, zero extra IO), and the frontier taken
/// from that same scan. Also returns the **bytes fetched** deriving it
/// (RFC 0031 §3.6 — on a cache miss this is exactly what template-map
/// acquisition cost).
///
/// Each fold is byte-for-byte the fresh derivation it caches:
/// the registry filter + `template_registry::fold_registry` matches
/// [`crate::derive_template_registry`], and the alias filter + stable
/// timestamp sort + [`AliasMap::from_events`] matches
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
    let scan = audit_scan::read_all_events_captured(backend, tenant)?;

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

/// The artifact's wire shape (RFC 0033 §3.2). Kept separate from
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

    /// Serialize to the canonical JSON bytes [`Self::from_json`]
    /// parses: registry entries sorted by `(template_id, version)`,
    /// alias classes sorted by representative, members sorted — so two
    /// derivations of the same fold serialize identically.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if serialization fails (not expected for
    /// these plain structs) or an alias class violates the non-empty
    /// invariant — corruption that must fail loudly rather than
    /// serialize a torn artifact.
    pub fn to_json(&self) -> Result<Vec<u8>, QueryError> {
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
    /// audit root, applying the RFC 0033 §3.3 dispositions: torn or
    /// internally invalid content and unknown `format_version` come
    /// back as their [`ArtifactRead`] variants (callers treat both as
    /// absent — the fresh fold is always a correct answer).
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] when the artifact's body `tenant_id`
    /// differs from the tenant whose path it was fetched from — a
    /// corrupt or foreign object under the tenant's root, failed
    /// loudly per the RFC 0005 §3.9 row-vs-path stance (exactly as
    /// the audit scan fails a foreign row, never serving or silently
    /// ignoring it).
    pub fn from_json(bytes: &[u8], tenant: &TenantId) -> Result<ArtifactRead, QueryError> {
        let torn = |detail: String| Ok(ArtifactRead::Torn { detail });
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

    /// Publish this artifact to `audit/tenant_id=<enc>/template_map.json`
    /// (RFC 0033 §3.2) following the RFC 0009 §3.4 manifest precedent,
    /// adapted to a derived, discardable object:
    ///
    /// - [`StoreRef::Local`]: write `template_map.json.tmp` in the
    ///   tenant's audit dir and `rename` it into place — the rename is
    ///   the only visibility point, so a reader observes the prior
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
    /// Publish failure is surfaceable but non-fatal by contract (§3.5):
    /// the write-through caller records it as telemetry and answers its
    /// query from the fold it already holds.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if serialization fails or on a
    /// non-precondition filesystem/backend failure.
    pub fn publish(
        &self,
        backend: StoreRef<'_>,
        expected: Option<&str>,
    ) -> Result<PublishOutcome, QueryError> {
        let bytes = self.to_json().map_err(|e| QueryError::Storage {
            detail: format!("serialize {TEMPLATE_MAP_FILENAME}: {e}"),
        })?;
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
                std::fs::rename(&tmp, &target).map_err(|e| io_err("rename", &tmp, &e))?;
                Ok(PublishOutcome::Published)
            }
            StoreRef::Remote(store) => {
                let key = format!("audit/tenant_id={enc}/{TEMPLATE_MAP_FILENAME}");
                let result = match expected {
                    None => store.put_if_absent_blocking(&key, bytes),
                    Some(e_tag) => store.put_if_match_blocking(&key, bytes, e_tag),
                };
                match result {
                    Ok(()) => Ok(PublishOutcome::Published),
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

    #[test]
    fn round_trips_through_json() {
        // Arrange
        let map = sample();

        // Act
        let restored = expect_valid(TemplateMap::from_json(
            &map.to_json().expect("serialize"),
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
        for bytes in [&b"not json"[..], &b"{\"format_version\":"[..], &[][..]] {
            let read = TemplateMap::from_json(bytes, &tenant()).expect("torn is not an error");
            assert!(
                matches!(read, ArtifactRead::Torn { .. }),
                "{bytes:?} must classify as torn",
            );
        }
    }

    #[test]
    fn unknown_format_version_is_treated_as_absent() {
        // A future writer's artifact: bump the version and change the
        // rest of the shape entirely — still UnknownVersion, not Torn.
        let future = br#"{"format_version": 2, "something_else": true}"#;
        let read = TemplateMap::from_json(future, &tenant()).expect("unknown version is no error");
        assert!(
            matches!(read, ArtifactRead::UnknownVersion { format_version: 2 }),
            "got {read:?}",
        );
    }

    #[test]
    fn tenant_mismatch_fails_loudly() {
        // The row-vs-path stance (RFC 0005 §3.9): a well-formed artifact
        // claiming another tenant under this tenant's root is a corrupt
        // or foreign object — an error, never absent-and-refolded.
        let bytes = sample().to_json().expect("serialize");
        let err = TemplateMap::from_json(&bytes, &TenantId::new("intruder"))
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
        let read = TemplateMap::from_json(&bytes, &tenant()).expect("read");
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
            let read = TemplateMap::from_json(&bytes, &tenant()).expect("read");
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
            let read = TemplateMap::from_json(&bytes, &tenant()).expect("read");
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
        let restored = expect_valid(TemplateMap::from_json(&bytes, &tenant()));
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
        let restored = expect_valid(TemplateMap::from_json(&bytes, &tenant()));
        assert_eq!(restored.folded_files(), second.folded_files());
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
        let held = expect_valid(TemplateMap::from_json(&bytes, &tenant()));
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
        let read = TemplateMap::from_json(&bytes, &tenant()).expect("read");
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
                    TemplateMap::from_json(&bytes, &tenant()),
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
            TemplateMap::from_json(&bytes, &tenant()),
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
