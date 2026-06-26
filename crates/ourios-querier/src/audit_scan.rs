//! Shared scan over a tenant's RFC 0005 `audit/` partition subtree.
//!
//! Both audit-stream consumers — the RFC 0010 drift query
//! ([`crate::drift`]) and the RFC 0005 §3.7.1 alias-map derivation
//! ([`crate::alias_store`]) — resolve their file set through this one
//! listing so the tenancy guarantees stay in a single place:
//!
//! - **Tenant isolation is the prefix scope** (`CLAUDE.md` §3.7 /
//!   RFC0010.4 / RFC0019.5): the listing is scoped to
//!   `audit/tenant_id=<enc>`, and [`Store::list_blocking`] matches the
//!   prefix **segment-wise** — a string-prefix sibling such as
//!   `tenant_id=ab` is excluded when listing `tenant_id=a`, so no other
//!   tenant's events are reachable by construction. The object-storage
//!   key space has no symlinks, so (unlike the prior `std::fs` walk) no
//!   canonical-path escape backstop is needed (RFC 0019 §3.3 decision).
//!   The row-level tenant backstop in the consumers stays.
//! - **Optional day-granularity window prune** (RFC 0005 §3.4 — the
//!   audit layout has no `hour` segment): with a window, out-of-range
//!   `day=…` partitions are skipped before their files are read. The
//!   prune is conservative (an unparseable key is never pruned) and the
//!   row-level `timestamp` predicate stays the correctness authority.
//!   The alias derivation passes no window — it folds the tenant's
//!   whole alias history.

use ourios_core::tenant::TenantId;
use ourios_parquet::{Store, percent_encode_tenant};

use crate::QueryError;

/// Resolve the live audit `*.parquet` object keys for `tenant`,
/// optionally pruned to the day partitions that could hold an event in
/// the half-open `[start, end)` window. Keys are returned in
/// **lexicographic order** ([`Store::list_blocking`]'s contract) — the
/// file-path component of the RFC 0005 §3.7.1 total fold order, and
/// stable across re-scans. A tenant with no audit objects is an empty
/// set, not an error; any other listing failure surfaces as
/// [`QueryError::Storage`] rather than being masked as "no data".
pub(crate) fn audit_files(
    store: &Store,
    tenant: &TenantId,
    window: Option<(u64, u64)>,
) -> Result<Vec<String>, QueryError> {
    let enc = percent_encode_tenant(tenant.as_str());
    let prefix = format!("audit/tenant_id={enc}");
    // The segment-wise prefix scope is the tenant-isolation guarantee
    // (RFC0019.5): the store never returns another tenant's keys, and the
    // object key space has no symlinks to escape it.
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

/// Whether the day partition the object `key` lives under could hold an event
/// in the half-open window `[start, end)`. Returns `true` (do not prune)
/// whenever the `year/month/day` segments don't parse or aren't a real UTC
/// instant, so a query never drops in-window data on an unrecognised layout.
fn day_key_in_window(key: &str, start: u64, end: u64) -> bool {
    let Some((year, month, day)) = parse_day_partition(key) else {
        return true;
    };
    let Some((lo, hi)) = day_span_ns(year, month, day) else {
        return true;
    };
    lo < end && start < hi
}

/// Parse `(year, month, day)` from the `year=/month=/day=` Hive segments of an
/// audit object key. `None` if no contiguous `year=`, `month=`, `day=` run of
/// segments with parseable numbers is present (a foreign key shape).
fn parse_day_partition(key: &str) -> Option<(i32, u32, u32)> {
    let mut year = None;
    let mut month = None;
    let mut day = None;
    for segment in key.split('/') {
        if let Some(v) = segment.strip_prefix("year=") {
            year = v.parse().ok();
        } else if let Some(v) = segment.strip_prefix("month=") {
            month = v.parse().ok();
        } else if let Some(v) = segment.strip_prefix("day=") {
            day = v.parse().ok();
        }
    }
    Some((year?, month?, day?))
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

    fn day_key() -> String {
        "audit/tenant_id=t/year=2026/month=04/day=02/e.parquet".to_string()
    }

    #[test]
    fn day_partition_prune_overlap_cases() {
        let key = day_key();
        // A window inside the day overlaps → keep.
        assert!(day_key_in_window(
            &key,
            DAY_START + 3_600_000_000_000,
            DAY_START + 7_200_000_000_000,
        ));
        // A window touching the day's start (half-open, inclusive lo) → keep.
        assert!(day_key_in_window(&key, DAY_START, DAY_START + 1));
        // A window entirely before the day → prune.
        assert!(!day_key_in_window(
            &key,
            DAY_START - 7_200_000_000_000,
            DAY_START - 3_600_000_000_000,
        ));
        // A window starting exactly at the day's end is excluded (half-open
        // upper bound) → prune.
        assert!(!day_key_in_window(
            &key,
            DAY_START + DAY_NANOS,
            DAY_START + DAY_NANOS + 1,
        ));
    }

    #[test]
    fn day_partition_prune_is_conservative_on_unparseable_keys() {
        // A non-leaf / foreign key can't be proven out of range → never pruned.
        assert!(day_key_in_window("audit/tenant_id=t", 0, 1));
        assert!(day_key_in_window("some/other/key", 0, 1));
    }
}
