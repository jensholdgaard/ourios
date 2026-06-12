//! Shared scan over a tenant's RFC 0005 `audit/` partition subtree.
//!
//! Both audit-stream consumers — the RFC 0010 drift query
//! ([`crate::drift`]) and the RFC 0005 §3.7.1 alias-map derivation
//! ([`crate::alias_store`]) — resolve their file set through this one
//! walk so the tenancy guarantees stay in a single place:
//!
//! - **Tenant isolation is the partition root** (`CLAUDE.md` §3.7 /
//!   RFC0010.4): the walk is rooted at `audit/tenant_id=<enc>/`, so no
//!   other tenant's events are reachable by construction.
//! - **Canonical-path escape backstop**: every resolved `*.parquet`
//!   must canonicalize *under* the tenant's canonical root — a
//!   symlinked file resolving into another tenant's tree fails loudly
//!   rather than being read.
//! - **Optional day-granularity window prune** (RFC 0005 §3.4 — the
//!   audit layout has no `hour` segment): with a window, out-of-range
//!   `day=…` leaves are skipped before they are listed. The prune is
//!   conservative (an unparseable dir is never pruned) and the
//!   row-level `timestamp` predicate stays the correctness authority.
//!   The alias derivation passes no window — it folds the tenant's
//!   whole alias history.

use std::path::{Path, PathBuf};

use ourios_core::tenant::TenantId;
use ourios_parquet::percent_encode_tenant;

use crate::QueryError;

/// Resolve the live audit `*.parquet` files for `tenant`, optionally
/// pruned to the day partitions that could hold an event in the
/// half-open `[start, end)` window. Canonical paths are de-duplicated
/// (an in-tenant symlink can't double-count a file) and returned in
/// **lexicographic path order** — the file-path component of the
/// RFC 0005 §3.7.1 total fold order, and stable across re-scans. A
/// missing tenant directory is an empty set, not an error; any other
/// I/O failure surfaces as [`QueryError::Storage`] rather than being
/// masked as "no data".
pub(crate) fn audit_files(
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

/// One day in nanoseconds — the span a `…/day=DD/` audit partition covers.
const DAY_NANOS: u64 = 86_400_000_000_000;

/// Whether the day partition at `dir` could hold an event in the half-open
/// window `[start, end)`. Returns `true` (do not prune) whenever the trailing
/// `year/month/day` segments don't parse or aren't a real UTC instant, so a
/// query never drops in-window data on an unrecognised layout.
fn day_partition_in_window(dir: &Path, start: u64, end: u64) -> bool {
    let Some((year, month, day)) = parse_day_partition(dir) else {
        return true;
    };
    let Some((lo, hi)) = day_span_ns(year, month, day) else {
        return true;
    };
    lo < end && start < hi
}

/// Parse `(year, month, day)` from the trailing three Hive segments of an audit
/// partition directory. `None` if the deepest three components aren't `day=`,
/// `month=`, `year=` with parseable numbers (a non-leaf dir or a foreign path).
fn parse_day_partition(dir: &Path) -> Option<(i32, u32, u32)> {
    let mut segments = dir.components().rev().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    });
    let day = segments.next()?.strip_prefix("day=")?.parse().ok()?;
    let month = segments.next()?.strip_prefix("month=")?.parse().ok()?;
    let year = segments.next()?.strip_prefix("year=")?.parse().ok()?;
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
    fn day_partition_prune_is_conservative_on_unparseable_paths() {
        // A non-leaf / foreign path can't be proven out of range → never pruned.
        let tenant_dir: PathBuf = ["bucket", "audit", "tenant_id=t"].iter().collect();
        assert!(day_partition_in_window(&tenant_dir, 0, 1));
        let foreign: PathBuf = ["some", "other", "dir"].iter().collect();
        assert!(day_partition_in_window(&foreign, 0, 1));
    }
}
