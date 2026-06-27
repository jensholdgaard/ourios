//! Shared scan over a tenant's RFC 0005 `audit/` partition subtree.
//!
//! Both audit-stream consumers — the RFC 0010 drift query
//! ([`crate::drift`]) and the RFC 0005 §3.7.1 alias-map derivation
//! ([`crate::alias_store`]) — resolve their file set through this one
//! [`audit_files`] so the tenancy guarantees stay in a single place.
//!
//! Like the bulk log/data scan, this is a **hybrid** keyed on whether the
//! querier is local or S3 (RFC 0019 §3.3):
//!
//! - **Local backend** (`local_root == Some`): the original `std::fs` walk,
//!   rooted at `audit/tenant_id=<enc>/`, **unchanged** — including the
//!   canonical-path tenant-isolation backstops: the tenant root must not be a
//!   symlink into another tenant's subtree (anchored to the canonical bucket
//!   root), every resolved `*.parquet` must canonicalize *under* the canonical
//!   tenant root, and canonical paths are de-duplicated (an in-tenant symlink
//!   can't double-count a file). This matters because drift has no row-level
//!   tenant check, so the walk's backstop is its only local tenant guard, and
//!   the dedup prevents a symlinked same-tenant file from being counted twice.
//! - **S3 backend** (`local_root == None`): [`Store::list_blocking`] over the
//!   `audit/tenant_id=<enc>` prefix. Tenant isolation is the **segment-wise**
//!   prefix scope (RFC0019.5) — a string-prefix sibling such as `tenant_id=ab`
//!   is excluded when listing `tenant_id=a` — and the object key space has no
//!   symlinks, so the canonicalize backstops are moot there. The keys come back
//!   lexicographically sorted (the §3.7.1 fold order) and unique.
//!
//! **Optional day-granularity window prune** (RFC 0005 §3.4 — the audit layout
//! has no `hour` segment): with a window, out-of-range `day=…` partitions are
//! skipped before their files are read. The prune is conservative (an
//! unparseable path/key is never pruned) and the row-level `timestamp`
//! predicate stays the correctness authority. The alias derivation passes no
//! window — it folds the tenant's whole alias history.

use std::path::{Path, PathBuf};

use ourios_core::audit::AuditEvent;
use ourios_core::tenant::TenantId;
use ourios_parquet::{AuditReader, Store, percent_encode_tenant};

use crate::QueryError;

/// The resolved audit file set, addressed per backend (RFC 0019 §3.3). The
/// consumers branch on this: the alias / registry folds read local files
/// directly and S3 keys through the [`Store`]; drift builds local-path or
/// object-store table URLs.
pub(crate) enum AuditFiles {
    /// Absolute, canonical local file paths (lexicographically sorted, unique).
    Local(Vec<PathBuf>),
    /// Store-relative object keys (lexicographically sorted, unique).
    Remote(Vec<String>),
}

impl AuditFiles {
    /// True when the tenant has no live audit files in the resolved set.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Local(paths) => paths.is_empty(),
            Self::Remote(keys) => keys.is_empty(),
        }
    }
}

/// Resolve `tenant`'s live audit `*.parquet` file set, optionally pruned to the
/// day partitions that could hold an event in the half-open `[start, end)`
/// window. Files come back in **lexicographic order** — the file-path component
/// of the RFC 0005 §3.7.1 total fold order, stable across re-scans — and unique.
/// A tenant with no audit files is an empty set, not an error.
///
/// `local_root` is `Some` for the local backend (the `std::fs` walk with the
/// canonical-path backstops) and `None` for S3 (the prefix-scoped
/// [`Store::list_blocking`]).
pub(crate) fn audit_files(
    store: &Store,
    local_root: Option<&Path>,
    tenant: &TenantId,
    window: Option<(u64, u64)>,
) -> Result<AuditFiles, QueryError> {
    match local_root {
        Some(root) => Ok(AuditFiles::Local(local_audit_files(root, tenant, window)?)),
        None => Ok(AuditFiles::Remote(remote_audit_files(
            store, tenant, window,
        )?)),
    }
}

/// Read every [`AuditEvent`] from `tenant`'s resolved audit file set (the
/// `None`-window full history), in the §3.7.1 fold order, applying the
/// **row-level tenant backstop** (`CLAUDE.md` §3.7 / RFC 0005 §3.9 row-vs-path):
/// the listing/walk is already tenant-scoped, so a row claiming another tenant
/// is a corrupt or foreign file — fail loudly rather than fold (or silently
/// drop) it. The shared reader for the alias-map and template-registry folds; a
/// local file is read with [`AuditReader::open_file`], an S3 key via
/// [`Store::get_blocking`] → [`AuditReader::open_bytes`].
pub(crate) fn read_all_events(
    store: &Store,
    local_root: Option<&Path>,
    tenant: &TenantId,
) -> Result<Vec<AuditEvent>, QueryError> {
    let files = audit_files(store, local_root, tenant, None)?;
    let mut events: Vec<AuditEvent> = Vec::new();
    let mut push_validated = |label: &str, read: Vec<AuditEvent>| -> Result<(), QueryError> {
        for event in read {
            if event.tenant_id != *tenant {
                return Err(QueryError::Storage {
                    detail: format!(
                        "audit file {label} carries a row for tenant {} under tenant {}'s \
                         partition root",
                        event.tenant_id.as_str(),
                        tenant.as_str(),
                    ),
                });
            }
            events.push(event);
        }
        Ok(())
    };
    match files {
        AuditFiles::Local(paths) => {
            for path in &paths {
                let read = AuditReader::open_file(path)
                    .and_then(AuditReader::read_all)
                    .map_err(|e| QueryError::Storage {
                        detail: format!("audit file {}: {e}", path.display()),
                    })?;
                push_validated(&path.display().to_string(), read)?;
            }
        }
        AuditFiles::Remote(keys) => {
            for key in &keys {
                let bytes = store.get_blocking(key).map_err(|e| QueryError::Storage {
                    detail: format!("audit file {key}: {e}"),
                })?;
                let read = AuditReader::open_bytes(bytes::Bytes::from(bytes))
                    .and_then(AuditReader::read_all)
                    .map_err(|e| QueryError::Storage {
                        detail: format!("audit file {key}: {e}"),
                    })?;
                push_validated(key, read)?;
            }
        }
    }
    Ok(events)
}

/// The **local** `std::fs` audit walk (RFC 0019 §3.3 local branch) — the
/// pre-RFC-0019 behaviour, byte-for-byte: rooted at `audit/tenant_id=<enc>/`,
/// with the day-window prune, the symlinked-tenant-root rejection, the per-file
/// canonical-path escape backstop, canonical-path dedup, and lexicographic sort.
fn local_audit_files(
    bucket_root: &Path,
    tenant: &TenantId,
    window: Option<(u64, u64)>,
) -> Result<Vec<PathBuf>, QueryError> {
    let io_err = |op: &str, p: &Path, e: &std::io::Error| QueryError::Storage {
        detail: format!("{op} {}: {e}", p.display()),
    };
    let enc = percent_encode_tenant(tenant.as_str());
    let tenant_dir = bucket_root.join("audit").join(format!("tenant_id={enc}"));

    let mut files = Vec::new();
    let mut stack = vec![tenant_dir.clone()];
    while let Some(dir) = stack.pop() {
        // Day-granularity partition prune (RFC 0005 §3.4 / RFC 0010
        // §6.5): an out-of-window `day=…` leaf is skipped *before* it
        // is listed, so its footers are never opened.
        // `day_partition_in_window` is conservative — a non-leaf or
        // unparseable dir (`year=`, `month=`, `tenant_id=`) is never
        // pruned, so the walk still descends to the leaves; only a
        // `day=` leaf whose `[day_start, day_start + 1d)` UTC span
        // misses `[start, end)` is dropped.
        if let Some((start, end)) = window
            && !day_partition_in_window(&dir, start, end)
        {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read_dir", &dir, &e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read_dir entry", &dir, &e))?;
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                // `*.parquet.tmp` has extension `tmp`, so an uncommitted /
                // crashed writer's temp file contributes nothing.
                Ok(_) if path.extension().is_some_and(|x| x == "parquet") => files.push(path),
                Ok(_) => {}
                Err(e) => return Err(io_err("file_type", &path, &e)),
            }
        }
    }
    if files.is_empty() {
        return Ok(files);
    }
    // Tenant-isolation backstop (RFC0010.4 / §3.7), mirroring the log
    // path: every resolved file must canonicalize *under* the tenant's
    // canonical `audit/tenant_id=…` root. The directory walk is already
    // partition-local, but a symlinked `*.parquet` could resolve into
    // another tenant's tree — this `starts_with` check fails such a path
    // loudly rather than reading another tenant's audit events.
    let tenant_root = tenant_dir
        .canonicalize()
        .map_err(|e| io_err("canonicalize", &tenant_dir, &e))?;
    // The trust anchor is the bucket root, not the tenant dir itself:
    // if `audit/tenant_id=…` (or `audit/`) were a symlink into another
    // tenant's subtree, canonicalizing it as the root would make every
    // foreign file pass `starts_with`. Requiring the canonical tenant
    // dir to equal the path constructed under the canonical bucket
    // root rejects a symlinked tenant root outright.
    let bucket_canonical = bucket_root
        .canonicalize()
        .map_err(|e| io_err("canonicalize", bucket_root, &e))?;
    let expected_root = bucket_canonical
        .join("audit")
        .join(format!("tenant_id={enc}"));
    if tenant_root != expected_root {
        return Err(QueryError::Storage {
            detail: format!(
                "audit tenant root {} resolves outside its expected partition path {}",
                tenant_root.display(),
                expected_root.display(),
            ),
        });
    }
    // De-duplicate the canonical paths (mirroring the log path in
    // `lib.rs`): two names resolving to the same file — e.g. an
    // in-tenant symlink — must not be read or counted twice.
    let mut seen = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(files.len());
    for file in files {
        let abs = file
            .canonicalize()
            .map_err(|e| io_err("canonicalize", &file, &e))?;
        if !abs.starts_with(&tenant_root) {
            return Err(QueryError::Storage {
                detail: format!(
                    "resolved audit file {} escapes tenant partition root {}",
                    abs.display(),
                    tenant_root.display(),
                ),
            });
        }
        if seen.insert(abs.clone()) {
            validated.push(abs);
        }
    }
    // Lexicographic path order: deterministic regardless of the walk's
    // stack order, and the §3.7.1 same-timestamp tiebreak across files.
    validated.sort();
    Ok(validated)
}

/// The **S3** audit listing (RFC 0019 §3.3 S3 branch): the live audit
/// `*.parquet` object keys under `audit/tenant_id=<enc>`. Tenant isolation is
/// the segment-wise prefix scope of [`Store::list_blocking`] (RFC0019.5); the
/// object key space has no symlinks, so the local canonicalize backstops are
/// moot. Keys come back sorted (the §3.7.1 fold order) and unique.
fn remote_audit_files(
    store: &Store,
    tenant: &TenantId,
    window: Option<(u64, u64)>,
) -> Result<Vec<String>, QueryError> {
    let enc = percent_encode_tenant(tenant.as_str());
    let prefix = format!("audit/tenant_id={enc}");
    let keys = store
        .list_blocking(Some(&prefix))
        .map_err(|e| QueryError::Storage {
            detail: format!("list audit prefix {prefix}: {e}"),
        })?;
    let files = keys
        .into_iter()
        // `*.parquet.tmp` does not end in `.parquet`, so an uncommitted /
        // crashed writer's temp object contributes nothing.
        .filter(|key| key.ends_with(".parquet"))
        // Day-granularity partition prune (RFC 0005 §3.4 / RFC 0010
        // §6.5): a key whose `year/month/day` segments fall out of the
        // window is dropped before it is read. `day_key_in_window` is
        // conservative — a key whose segments don't parse is never
        // pruned — so an unrecognised layout never drops in-window data.
        .filter(|key| window.is_none_or(|(start, end)| day_key_in_window(key, start, end)))
        .collect();
    Ok(files)
}

/// One day in nanoseconds — the span a `…/day=DD/` audit partition covers.
const DAY_NANOS: u64 = 86_400_000_000_000;

/// Whether the day partition at `dir` could hold an event in the half-open
/// window `[start, end)`. Returns `true` (do not prune) whenever the trailing
/// `year/month/day` segments don't parse or aren't a real UTC instant, so a
/// query never drops in-window data on an unrecognised layout.
fn day_partition_in_window(dir: &Path, start: u64, end: u64) -> bool {
    let segments: Vec<&str> = dir
        .components()
        .rev()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    day_segments_in_window(&segments, start, end)
}

/// The key-based sibling of [`day_partition_in_window`] for the S3 branch:
/// whether the day partition the object `key` lives under could hold an event
/// in `[start, end)`.
fn day_key_in_window(key: &str, start: u64, end: u64) -> bool {
    // The key's leaf is the file name; the day partition is its parent
    // directory, so the partition segments are the key's path segments
    // (deepest first) after dropping that leaf.
    let segments: Vec<&str> = key.rsplit('/').skip(1).collect();
    day_segments_in_window(&segments, start, end)
}

/// Decide the day-window prune from `segments` ordered **deepest-first** (the
/// reversed path/key components, leaf already dropped for a key). Requires the
/// deepest three to be exactly `day=`, `month=`, `year=` **adjacent** with
/// parseable numbers — a non-contiguous or foreign layout (or a non-leaf dir)
/// fails to parse and is never pruned (the conservative guarantee).
fn day_segments_in_window(segments: &[&str], start: u64, end: u64) -> bool {
    let Some((year, month, day)) = parse_day_partition(segments) else {
        return true;
    };
    let Some((lo, hi)) = day_span_ns(year, month, day) else {
        return true;
    };
    lo < end && start < hi
}

/// Parse `(year, month, day)` from the trailing three Hive segments (passed
/// **deepest-first**). `None` unless the deepest three are exactly `day=`,
/// `month=`, `year=` **adjacent** with parseable numbers — a non-contiguous run
/// (e.g. a stray segment between them) or a non-leaf / foreign path mis-parses
/// to `None`, which the caller treats as non-prunable.
fn parse_day_partition(segments: &[&str]) -> Option<(i32, u32, u32)> {
    let mut it = segments.iter();
    let day = it.next()?.strip_prefix("day=")?.parse().ok()?;
    let month = it.next()?.strip_prefix("month=")?.parse().ok()?;
    let year = it.next()?.strip_prefix("year=")?.parse().ok()?;
    Some((year, month, day))
}

/// The `[start, end)` UTC-nanosecond span of the day partition. `None` if it
/// isn't a real UTC instant or predates the 1970 epoch.
fn day_span_ns(year: i32, month: u32, day: u32) -> Option<(u64, u64)> {
    let start = chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp_nanos_opt()?;
    let lo = u64::try_from(start).ok()?;
    Some((lo, lo.saturating_add(DAY_NANOS)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `day=02` on 2026-04 covers [00:00, next-00:00) UTC.
    const DAY_START: u64 = 1_775_088_000_000_000_000; // 2026-04-02T00:00:00Z

    fn day_dir() -> PathBuf {
        [
            "bucket",
            "audit",
            "tenant_id=t",
            "year=2026",
            "month=04",
            "day=02",
        ]
        .iter()
        .collect()
    }

    /// An S3 object key under the same `day=02` partition.
    const DAY_KEY: &str = "audit/tenant_id=t/year=2026/month=04/day=02/e.parquet";

    #[test]
    fn day_partition_prune_overlap_cases() {
        let dir = day_dir();
        // A window inside the day overlaps → keep.
        assert!(day_partition_in_window(
            &dir,
            DAY_START + 3_600_000_000_000,
            DAY_START + 7_200_000_000_000,
        ));
        // A window touching the day's start (half-open, inclusive lo) → keep.
        assert!(day_partition_in_window(&dir, DAY_START, DAY_START + 1));
        // A window entirely before the day → prune.
        assert!(!day_partition_in_window(
            &dir,
            DAY_START - 7_200_000_000_000,
            DAY_START - 3_600_000_000_000,
        ));
        // A window starting exactly at the day's end is excluded (half-open
        // upper bound) → prune.
        assert!(!day_partition_in_window(
            &dir,
            DAY_START + DAY_NANOS,
            DAY_START + DAY_NANOS + 1,
        ));
    }

    #[test]
    fn day_key_prune_matches_the_path_prune() {
        // The key-based prune (S3 branch) agrees with the path-based prune.
        assert!(day_key_in_window(
            DAY_KEY,
            DAY_START + 3_600_000_000_000,
            DAY_START + 7_200_000_000_000,
        ));
        assert!(!day_key_in_window(
            DAY_KEY,
            DAY_START - 7_200_000_000_000,
            DAY_START - 3_600_000_000_000,
        ));
    }

    #[test]
    fn day_partition_prune_is_conservative_on_unparseable_paths() {
        // A non-leaf / foreign path can't be proven out of range → never pruned.
        let tenant_dir: PathBuf = ["bucket", "audit", "tenant_id=t"].iter().collect();
        assert!(day_partition_in_window(&tenant_dir, 0, 1));
        let foreign: PathBuf = ["some", "other", "dir"].iter().collect();
        assert!(day_partition_in_window(&foreign, 0, 1));
        // And the key sibling.
        assert!(day_key_in_window("audit/tenant_id=t/x.parquet", 0, 1));
        assert!(day_key_in_window("some/other/key", 0, 1));
    }

    #[test]
    fn day_prune_requires_a_contiguous_trailing_run() {
        // A stray segment between `month=` and `day=` breaks the contiguous
        // `year=/month=/day=` run, so the deepest three aren't the partition
        // triple — the prune must NOT fire (conservative: never drop an
        // in-window object on an unrecognised layout). Before the fix the
        // anywhere-scan grabbed the three regardless and could prune a valid
        // object whose window is far from the (mis-parsed) day.
        let non_contiguous = "audit/tenant_id=t/year=2026/month=04/stray=x/day=02/e.parquet";
        // A window entirely before `day=02` would prune if the triple parsed;
        // because the run is non-contiguous, it parses to None → keep.
        assert!(day_key_in_window(
            non_contiguous,
            DAY_START - 7_200_000_000_000,
            DAY_START - 3_600_000_000_000,
        ));
        // The path form too.
        let dir: PathBuf = [
            "bucket",
            "audit",
            "tenant_id=t",
            "year=2026",
            "month=04",
            "stray=x",
            "day=02",
        ]
        .iter()
        .collect();
        assert!(day_partition_in_window(
            &dir,
            DAY_START - 7_200_000_000_000,
            DAY_START - 3_600_000_000_000,
        ));
    }
}
