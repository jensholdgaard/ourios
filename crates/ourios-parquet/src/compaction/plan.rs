//! Candidate planning — sealed-partition selection over store listings
//! (read-only). Split from the flat `compaction.rs` (epic #745 wave 1);
//! pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path — types, siblings, external crates — resolving
// unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

/// Select the `tenant`'s sealed partitions that are worth compacting
/// (RFC 0009 §3.3), as of wall-clock `now_unix_nanos`. The result is the work
/// list a background compactor feeds to [`compact_partition`]; this function
/// makes only the *decision* — read-only [`Store`] listings and each candidate
/// partition's `manifest.json`, no mutation — so it is deterministic and
/// testable. The driving loop (timer + bounded concurrency) belongs in the
/// ingester role.
///
/// A partition is selected when it is **sealed** — its hour ended at least
/// `policy.grace_nanos` ago, so no writer is still appending — and a
/// **candidate**: it has at least two live files (fewer can't be consolidated)
/// and either more than `policy.min_files` of them or one below
/// `policy.small_file_bytes`. The list is ordered chronologically (oldest
/// partition first), deterministic across runs.
///
/// The decision is read from one recursive listing of the tenant's data
/// prefix (RFC 0009 §3.3, 2026-10-16 amendment). A partition's manifest is
/// read only when two or more committed files are listed there: fewer can't
/// hold two live files whatever the manifest names, so a settled partition
/// costs no request of its own and planning scales with the tenant's object
/// count, not its retained hours.
///
/// # Errors
///
/// [`CompactionError::Io`] if a [`Store`] listing fails, or
/// [`CompactionError::Manifest`] if a partition's manifest can't be read while
/// counting its live files.
pub fn plan_candidates(
    store: &Store,
    tenant: &str,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> Result<Vec<PartitionKey>, CompactionError> {
    plan_candidates_counted(store, tenant, now_unix_nanos, policy).0
}

/// [`plan_candidates`], with the store work it did: the RFC 0038 §3.8
/// counts, which are returned on the error path too (the work up to the
/// failure).
pub fn plan_candidates_counted(
    store: &Store,
    tenant: &str,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> (Result<Vec<PartitionKey>, CompactionError>, PlanCounts) {
    let mut lister = Lister::new(store);
    let selected = lister.candidates(tenant, now_unix_nanos, policy);
    (selected, lister.counts)
}

/// The store work one planning pass did (RFC 0038 §3.8).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlanCounts {
    /// Store listing calls, failed ones included.
    pub list_requests: usize,
    /// Objects those listings returned, before any filtering. A delimited
    /// roll-up returns only common prefixes, which are not objects.
    pub listed_objects: usize,
    /// Manifest reads issued, whether the manifest turned out readable or
    /// not: one per sealed partition whose listing holds two or more
    /// committed files and a manifest (RFC 0009 §3.3).
    pub manifest_reads: usize,
}

impl std::ops::AddAssign for PlanCounts {
    fn add_assign(&mut self, other: Self) {
        self.list_requests += other.list_requests;
        self.listed_objects += other.listed_objects;
        self.manifest_reads += other.manifest_reads;
    }
}

/// The planning walk's store calls, each counted as it is made.
struct Lister<'s> {
    store: &'s Store,
    counts: PlanCounts,
}

impl<'s> Lister<'s> {
    fn new(store: &'s Store) -> Self {
        Self {
            store,
            counts: PlanCounts::default(),
        }
    }

    fn candidates(
        &mut self,
        tenant: &str,
        now_unix_nanos: u64,
        policy: &CompactionPolicy,
    ) -> Result<Vec<PartitionKey>, CompactionError> {
        let mut selected = Vec::new();
        for (partition, listed) in self.listed_partitions(tenant)? {
            if is_sealed(&partition, now_unix_nanos, policy)
                && self.is_candidate(&partition, &listed, policy)?
            {
                selected.push(partition);
            }
        }
        Ok(selected)
    }

    /// Whether a partition is worth compacting per `policy` (see
    /// [`meets_policy`]). The live set is the listed files that are live
    /// under the RFC 0009 §3.4 rule ([`LiveSet`]): with no manifest listed,
    /// the flush files; otherwise also the named compacted files, less the
    /// consumed flush files. The manifest is read only when two or more files
    /// are listed, since fewer can't be two live files.
    fn is_candidate(
        &mut self,
        partition: &PartitionKey,
        listed: &ListedPartition,
        policy: &CompactionPolicy,
    ) -> Result<bool, CompactionError> {
        if listed.files.len() < 2 {
            return Ok(false);
        }
        let manifest = if listed.has_manifest {
            self.counts.manifest_reads += 1;
            read_manifest(self.store, partition)?
        } else {
            None
        };
        let live = LiveSet::new(manifest.as_ref());
        let sizes: Vec<u64> = listed
            .files
            .iter()
            .filter(|(name, _)| live.is_live(name))
            .map(|(_, size)| *size)
            .collect();
        Ok(meets_policy(&sizes, policy))
    }

    /// Group one recursive listing of the tenant's data prefix by hour
    /// partition, oldest first. Only objects directly inside a canonical
    /// `year=YYYY/month=MM/day=DD/hour=HH/` prefix count, as in
    /// [`hour_partitions`]; anything nested deeper or under a non-canonical
    /// segment is not a partition input.
    fn listed_partitions(
        &mut self,
        tenant: &str,
    ) -> Result<Vec<(PartitionKey, ListedPartition)>, CompactionError> {
        let root = format!("data/tenant_id={}", percent_encode_tenant(tenant));
        self.counts.list_requests += 1;
        let entries = self
            .store
            .list_with_sizes_blocking(Some(&root))
            .map_err(|e| store_io("list", &root, e))?;
        self.counts.listed_objects += entries.len();
        let mut partitions: BTreeMap<Hour, ListedPartition> = BTreeMap::new();
        for (key, size) in &entries {
            let Some((hour, name)) = key
                .strip_prefix(root.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(parse_partition_object)
            else {
                continue;
            };
            let listed = partitions.entry(hour).or_default();
            if name == MANIFEST_FILENAME {
                listed.has_manifest = true;
            } else if is_committed_parquet(name) {
                listed.files.push((name.to_owned(), *size));
            }
        }
        Ok(partitions
            .into_iter()
            .map(|((year, month, day, hour), listed)| {
                let partition = PartitionKey {
                    tenant_id: tenant.to_owned(),
                    year,
                    month,
                    day,
                    hour,
                };
                (partition, listed)
            })
            .collect())
    }

    /// [`hour_partitions`]' walk, counting each roll-up listing.
    fn hour_partitions(&mut self, tenant: &str) -> Result<Vec<PartitionKey>, CompactionError> {
        let root = format!("data/tenant_id={}", percent_encode_tenant(tenant));
        let mut partitions = Vec::new();
        for (year_prefix, year) in self.numbered_child_prefixes(&root, "year", 4)? {
            // `year` is a calendar year; skip the (unreachable for Ourios
            // output) value that wouldn't fit the `PartitionKey` `i32`.
            let Ok(year) = i32::try_from(year) else {
                continue;
            };
            for (month_prefix, month) in self.numbered_child_prefixes(&year_prefix, "month", 2)? {
                for (day_prefix, day) in self.numbered_child_prefixes(&month_prefix, "day", 2)? {
                    for (_hour_prefix, hour) in
                        self.numbered_child_prefixes(&day_prefix, "hour", 2)?
                    {
                        partitions.push(PartitionKey {
                            tenant_id: tenant.to_owned(),
                            year,
                            month,
                            day,
                            hour,
                        });
                    }
                }
            }
        }
        // Ascending tuple order is chronological (oldest sealed partition
        // first); dedup after the sort is a belt-and-braces guard (the
        // rollup yields each partition once).
        partitions.sort_by_key(|p| (p.year, p.month, p.day, p.hour));
        partitions.dedup();
        Ok(partitions)
    }

    /// Roll up the immediate child common-prefixes of `parent` (one
    /// delimiter level, via [`Store::list_common_prefixes_blocking`]) and
    /// parse each one's trailing `<name>=NN` segment in the canonical
    /// zero-padded form, returning `(child_prefix, value)` for the matches.
    /// A non-canonical child (`month=4`, `month=004`) parses to `None` and is
    /// dropped — the same way the pre-RFC-0019 `read_dir` walk skipped
    /// non-canonical directory names (RFC 0005 §3.4).
    fn numbered_child_prefixes(
        &mut self,
        parent: &str,
        name: &str,
        width: usize,
    ) -> Result<Vec<(String, u32)>, CompactionError> {
        self.counts.list_requests += 1;
        let children = self
            .store
            .list_common_prefixes_blocking(Some(parent))
            .map_err(|e| store_io("list", parent, e))?;
        Ok(children
            .into_iter()
            .filter_map(|child| {
                let value = parse_partition_segment(basename(&child), name, width)?;
                Some((child, value))
            })
            .collect())
    }
}

/// What a tenant listing shows directly inside one hour partition.
#[derive(Debug, Default)]
struct ListedPartition {
    /// Committed `*.parquet` objects as `(file name, size in bytes)`.
    files: Vec<(String, u64)>,
    has_manifest: bool,
}

/// The RFC 0009 §3.3 candidate test over a partition's live file sizes: at
/// least two files, and either more than `min_files` of them or one smaller
/// than `small_file_bytes`.
pub(super) fn meets_policy(sizes: &[u64], policy: &CompactionPolicy) -> bool {
    sizes.len() >= 2
        && (sizes.len() > policy.min_files
            || sizes.iter().any(|&len| len < policy.small_file_bytes))
}

/// An hour partition's `(year, month, day, hour)`, ordered chronologically.
type Hour = (i32, u32, u32, u32);

/// Split a tenant-relative key `year=YYYY/month=MM/day=DD/hour=HH/<name>` into
/// its hour and object name; `None` for any other shape.
fn parse_partition_object(relative: &str) -> Option<(Hour, &str)> {
    let mut segments = relative.split('/');
    let year = parse_partition_segment(segments.next()?, "year", 4)?;
    let month = parse_partition_segment(segments.next()?, "month", 2)?;
    let day = parse_partition_segment(segments.next()?, "day", 2)?;
    let hour = parse_partition_segment(segments.next()?, "hour", 2)?;
    let name = segments.next().filter(|name| !name.is_empty())?;
    if segments.next().is_some() {
        return None;
    }
    Some(((i32::try_from(year).ok()?, month, day, hour), name))
}

/// Whether the partition's hour ended at least `grace_nanos` before `now` (the
/// comparison is inclusive: sealed at exactly `hour_end + grace`). A partition
/// whose `(year, month, day, hour)` is not a real UTC instant (a corrupt key)
/// is treated as not sealed.
pub(super) fn is_sealed(
    partition: &PartitionKey,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> bool {
    let Some(hour_start) = NaiveDate::from_ymd_opt(partition.year, partition.month, partition.day)
        .and_then(|d| d.and_hms_opt(partition.hour, 0, 0))
        .map(|ndt| ndt.and_utc())
    else {
        return false;
    };
    let Some(start_nanos) = hour_start.timestamp_nanos_opt() else {
        return false;
    };
    let Ok(start) = u64::try_from(start_nanos) else {
        return false; // pre-1970; not a partition Ourios writes
    };
    now_unix_nanos
        >= start
            .saturating_add(HOUR_NANOS)
            .saturating_add(policy.grace_nanos)
}

/// Enumerate the tenant's `year=/month=/day=/hour=` leaf partitions that hold
/// objects, walking the Hive levels with a **delimiter rollup** at each step:
/// from `data/tenant_id=<enc>` roll up the `year=` child prefixes, then for each
/// the `month=`, then `day=`, then `hour=` children — every listing returns only
/// the immediate common-prefixes (cheap), never the full object set. This is the
/// object-store equivalent of the pre-RFC-0019 level-by-level `read_dir` walk,
/// not a recursive `O(N_objects)` scan. Each level's segment is parsed in the
/// canonical zero-padded form (`parse_partition_segment`); a non-canonical
/// child prefix is dropped exactly as the old walk dropped non-canonical dirs.
/// Returned sorted chronologically (oldest first) and deduplicated.
///
/// # Errors
///
/// [`CompactionError`] when the store's prefixes cannot be listed.
pub fn hour_partitions(store: &Store, tenant: &str) -> Result<Vec<PartitionKey>, CompactionError> {
    Lister::new(store).hour_partitions(tenant)
}

/// Parse one canonical Hive segment `<prefix>=<zero-padded number>` to its
/// value. Accepts only the exact zero-padded width [`PartitionKey::data_path`]
/// writes (`month=04`, not `month=4` or `month=004`), so the parsed key
/// round-trips to the scanned object (RFC 0005 §3.4); any other form is `None`.
pub(super) fn parse_partition_segment(segment: &str, prefix: &str, width: usize) -> Option<u32> {
    let digits = segment.strip_prefix(prefix)?.strip_prefix('=')?;
    let value: u32 = digits.parse().ok()?;
    (digits == format!("{value:0width$}")).then_some(value)
}

/// Visit every live row of `partition`, batch by batch, without rewriting
/// anything — the read half of the compaction path under the RFC 0009 §3.4
/// live-set rule, for the RFC 0048 §3.4 backfill.
///
/// The partition is listed **before** its manifest is read, as the querier
/// resolves (RFC 0009 §3.4, reader order). A commit that lands between the two
/// reads is then seen through its manifest: the flush files it consumed are
/// excluded and the compacted file it names is read by name. The reverse order
/// would pair an older manifest with a listing taken after the commit deleted
/// its inputs, and silently visit fewer rows. A commit that deletes a listed
/// file after both reads makes its `get` fail loudly.
///
/// # Errors
///
/// [`CompactionError`] on listing, get, or decode failures.
pub fn visit_partition_rows(
    store: &Store,
    partition: &PartitionKey,
    mut visit: impl FnMut(&[MinedRecord]),
) -> Result<(), CompactionError> {
    let listed = list_partition(store, partition)?;
    let manifest = read_manifest(store, partition)?;
    for key in live_keys_of(partition, manifest.as_ref(), listed) {
        let bytes = store
            .get_blocking(&key)
            .map_err(|e| store_io("get", &key, e))?;
        let mut reader =
            Reader::open_partition_bytes(bytes::Bytes::from(bytes), partition.clone(), &key)
                .map_err(CompactionError::Read)?;
        while let Some(batch) = reader.next_batch().map_err(CompactionError::Read)? {
            visit(&batch);
        }
    }
    Ok(())
}
