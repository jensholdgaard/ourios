//! Live-file resolution (local walk + manifest, S3 twin) and the
//! `DataFusion` listing-URL construction — one job: which bytes may this
//! query read (epic #745 wave 1; moved verbatim from the crate root).

// Split from the crate root (epic #745 wave 1); the parent scope is
// the import surface so every pre-split `crate::X` path resolves
// unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::audit_fetch::{FetchPool, fetch_in_order};
use crate::fetch_count::FetchCounter;
use ourios_parquet::{ManifestError, compacted_generation};
use tracing::Instrument as _;

/// Resolve the live data files a query must read under `dir` (a
/// tenant's partition root), honouring the RFC 0009 §3.4
/// per-partition manifest. Recursive because the data is nested
/// `year=/month=/day=/hour=/`. With a window, nothing below a parsed
/// `year=`/`month=`/`day=` ancestor whose whole span misses it is live, even a
/// stray file whose own directory does not parse: the walk does not descend
/// there. This is the contract [`resolve_live_keys`] follows too, through the
/// shared [`visit`], so both backends resolve the same set (RFC 0019 §3.3).
///
/// For each partition directory: if it holds a `manifest.json`, the
/// manifest is authoritative and contributes exactly the files it
/// names (files present on disk but not listed — orphans awaiting GC,
/// or a writer's uncommitted `*.parquet.tmp` — are ignored). With no
/// manifest (every partition today, pre-compaction) it falls back to
/// all committed `*.parquet` in that directory; `*.parquet.tmp` has
/// extension `tmp`, so the poisoned-writer case contributes nothing.
///
/// An empty result means the tenant has nothing queryable. A missing
/// directory (`NotFound`) is empty; any *other* I/O error (permission
/// denied, transient failure) is propagated as [`QueryError::Storage`]
/// rather than silently masked as "no data" — a wrong zero-row answer
/// is worse than a surfaced error.
///
/// Every manifest byte read is added to the tally's `fetched`, and the
/// directory reads and manifest reads to its counts.
pub(super) fn resolve_live_files(
    dir: &std::path::Path,
    window: Option<(u64, u64)>,
    tally: &mut Tally<'_>,
) -> Result<Vec<PathBuf>, QueryError> {
    let mut files = Vec::new();
    // `Some(level)` while the walk is still cutting through the window's Hive
    // ancestors; `None` below a subtree the window covers whole (or cannot be
    // parsed), where only the per-partition prune applies.
    let mut stack = vec![(dir.to_path_buf(), window.map(|_| HiveLevel::Year))];
    while let Some((d, level)) = stack.pop() {
        let Some(DirListing {
            subdirs,
            mut parquets,
        }) = read_local_dir(&d, &mut tally.counts)?
        else {
            continue;
        };
        // Partition-level time pruning (RFC 0007): when the query has a
        // time range, skip a leaf partition whose `hour=HH` span can't
        // overlap it — so DataFusion never opens those footers. This is
        // a pure optimisation layered on the row-level time column
        // predicate (which stays the correctness authority);
        // `hour_partition_in_window` is conservative, never pruning a
        // path it can't prove out of range, so no in-window data is lost.
        let keep = window.is_none_or(|(start, end)| hour_partition_in_window(&d, start, end));
        if keep {
            let manifest = read_local_manifest(&d, tally).map_err(|e| QueryError::Storage {
                detail: format!("manifest in {}: {e}", d.display()),
            })?;
            let listed = std::mem::take(&mut parquets);
            files.extend(live_in_partition(manifest.as_ref(), listed, |name| {
                d.join(name)
            }));
        }
        for subdir in subdirs {
            let (Some(level), Some(window)) = (level, window) else {
                stack.push((subdir, None));
                continue;
            };
            let segment = subdir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            match visit(level, segment, &subdir, window) {
                Visit::Skip => {}
                Visit::Descend(next) => stack.push((subdir, Some(next))),
                Visit::List => stack.push((subdir, None)),
            }
        }
    }
    Ok(files)
}

/// One local directory's entries: its subdirectories and its committed
/// `*.parquet` files.
struct DirListing {
    subdirs: Vec<PathBuf>,
    parquets: Vec<PathBuf>,
}

/// Read directory `d` as one listing call, counting it and the non-directory
/// entries it returns into `counts`. `Ok(None)` when `d` is not there.
fn read_local_dir(
    d: &std::path::Path,
    counts: &mut ResolutionCounts,
) -> Result<Option<DirListing>, QueryError> {
    let io_err = |op: &str, p: &std::path::Path, e: &std::io::Error| QueryError::Storage {
        detail: format!("{op} {}: {e}", p.display()),
    };
    counts.list_requests += 1;
    let entries = match std::fs::read_dir(d) {
        Ok(entries) => entries,
        // The dir (or a subdir, lost to a concurrent housekeeping
        // unlink) simply isn't there → not data, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("read_dir", d, &e)),
    };
    let mut listing = DirListing {
        subdirs: Vec::new(),
        parquets: Vec::new(),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err("read_dir entry", d, &e))?;
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => listing.subdirs.push(path),
            Ok(_) if path.extension().is_some_and(|x| x == "parquet") => {
                counts.listed_objects += 1;
                listing.parquets.push(path);
            }
            Ok(_) => counts.listed_objects += 1,
            Err(e) => return Err(io_err("file_type", &path, &e)),
        }
    }
    Ok(Some(listing))
}

/// `<partition_dir>/manifest.json`, counting the bytes read into the tally's
/// `fetched`; `Ok(None)` when absent (the glob fallback), as
/// [`Manifest::read`]. An absent manifest is not a manifest read (the S3
/// twin never issues that GET), but a failed one is.
fn read_local_manifest(
    partition_dir: &std::path::Path,
    tally: &mut Tally<'_>,
) -> Result<Option<Manifest>, ManifestError> {
    match std::fs::read(partition_dir.join(MANIFEST_FILENAME)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            tally.counts.manifest_reads += 1;
            Err(ManifestError::Io(e))
        }
        Ok(bytes) => {
            tally.counts.manifest_reads += 1;
            tally.fetched.add(bytes.len() as u64);
            Manifest::from_json(&bytes).map(Some)
        }
    }
}

/// The RFC 0038 §3.7.2 work counts of one file-set resolution, kept on the
/// error path too.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResolutionCounts {
    /// Store listing calls, or directory reads on the local walk.
    pub(super) list_requests: usize,
    /// Objects those listings returned, before window and manifest filtering.
    pub(super) listed_objects: usize,
    /// Manifest reads.
    pub(super) manifest_reads: usize,
}

impl ResolutionCounts {
    /// Set the counts on the `resolve files` span. `i64`, not `usize`/`u64`:
    /// tracing-opentelemetry renders a `u64` field as a string, and §3.7.2
    /// wants a count.
    fn record(self, span: &tracing::Span) {
        for (key, count) in [
            (
                ourios_semconv::OURIOS_FILE_SET_LIST_REQUEST_COUNT,
                self.list_requests,
            ),
            (
                ourios_semconv::OURIOS_FILE_SET_LISTED_OBJECTS,
                self.listed_objects,
            ),
            (
                ourios_semconv::OURIOS_FILE_SET_MANIFEST_READ_COUNT,
                self.manifest_reads,
            ),
        ] {
            span.record(key, i64::try_from(count).unwrap_or(i64::MAX));
        }
    }
}

/// What one resolution accounts for as it runs: the session's fetched
/// manifest bytes and the resolution's own work counts.
pub(super) struct Tally<'a> {
    fetched: &'a FetchCounter,
    pub(super) counts: ResolutionCounts,
}

impl<'a> Tally<'a> {
    pub(super) fn new(fetched: &'a FetchCounter) -> Self {
        Self {
            fetched,
            counts: ResolutionCounts::default(),
        }
    }
}

/// What one file-set resolution covers: the tenant, its `data/` prefix and
/// the partition window that prunes it.
#[derive(Clone, Copy)]
pub(super) struct DataScope<'a> {
    pub(super) tenant: &'a TenantId,
    pub(super) prefix: &'a str,
    pub(super) window: Option<(u64, u64)>,
}

/// Resolve the live data files under the tenant's `data/` prefix and turn
/// them into the `DataFusion` table URLs for the hybrid scan (RFC 0019 §3.3),
/// with the listing and manifest reads on the blocking pool:
///
/// - **Local backend** ([`Backend::Local`]): walk `std::fs` under
///   `<root>/<prefix>` honouring the RFC 0009 §3.4 manifest, then address
///   each file by its absolute local path — byte-for-byte the pre-RFC-0019
///   read path, with the canonical-path tenant-isolation backstop intact.
/// - **S3 backend** ([`Backend::Remote`]): list the window's keys under
///   `prefix` through the [`Store`] (segment-wise prefix-scoped, the
///   RFC0019.5 tenant guarantee), resolve the per-partition manifest through
///   the [`Store`], register the store on `ctx`, and address each key by the
///   `ourios://store/<key>` object-store URL.
///
/// The whole resolution is the RFC 0038 §3.7.1 `resolve files` phase, open
/// from before the first listing to the finished URLs — on both backends,
/// and when nothing is live, since that work ran either way.
pub(super) async fn resolve_data_urls(
    backend: &Backend,
    ctx: &SessionContext,
    scope: DataScope<'_>,
) -> Result<Vec<ListingTableUrl>, QueryError> {
    let span = tracing::info_span!(
        "resolve files",
        otel.kind = "internal",
        ourios.tenant = scope.tenant.as_str(),
        ourios.file_set.list_request_count = tracing::field::Empty,
        ourios.file_set.listed_objects = tracing::field::Empty,
        ourios.file_set.manifest_read_count = tracing::field::Empty,
        ourios.file_set.live_files = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    resolve_in_phase(backend, ctx, scope, &span)
        .instrument(span.clone())
        .await
        // `i64`, not `usize`/`u64`: tracing-opentelemetry renders a `u64`
        // field as a string, and §3.7.2 wants a count.
        .inspect(|urls| {
            span.record(
                ourios_semconv::OURIOS_FILE_SET_LIVE_FILES,
                i64::try_from(urls.len()).unwrap_or(i64::MAX),
            );
        })
        .inspect_err(|e| {
            ourios_core::telemetry::record_span_error(&span, crate::template_map::error_type(e));
        })
}

/// Resolve `scope` again only to compare it with an earlier resolution, after
/// a scan over that one failed (RFC0009.18). Not a `resolve files` phase of
/// its own: the query's phase count stays one per scan attempt.
pub(super) async fn recheck_data_urls(
    backend: &Backend,
    ctx: &SessionContext,
    scope: DataScope<'_>,
) -> Result<Vec<ListingTableUrl>, QueryError> {
    resolve_in_phase(backend, ctx, scope, &tracing::Span::none()).await
}

async fn resolve_in_phase(
    backend: &Backend,
    ctx: &SessionContext,
    scope: DataScope<'_>,
    span: &tracing::Span,
) -> Result<Vec<ListingTableUrl>, QueryError> {
    // The manifest reads count toward the session's fetched bytes.
    let fetched = fetch_count::counter(ctx).unwrap_or_default();
    let (resolved, counts) = crate::phase::spawn_blocking(span, {
        let backend = backend.clone();
        let prefix = scope.prefix.to_owned();
        let window = scope.window;
        move || {
            let mut tally = Tally::new(&fetched);
            let resolved = resolve_live_set(&backend, &prefix, window, &mut tally);
            Ok((resolved, tally.counts))
        }
    })
    .await?;
    counts.record(span);
    match (resolved?, backend) {
        (LiveSet::Urls(urls), _) => Ok(urls),
        (LiveSet::Keys(keys), Backend::Remote(store)) => object_store_urls(ctx, store, &keys),
        // Keys come only from the remote branch of `resolve_live_set`,
        // which read this same backend; surfaced rather than unwrapped.
        (LiveSet::Keys(_), Backend::Local(_)) => Err(QueryError::Storage {
            detail: "internal: S3 data keys reached with a local backend".to_string(),
        }),
    }
}

/// The blocking half of [`resolve_data_urls`]: finished table URLs on
/// the local backend, store keys on S3 (their URLs need the query's
/// `SessionContext`, which stays on the async side).
pub(super) enum LiveSet {
    Urls(Vec<ListingTableUrl>),
    Keys(Vec<String>),
}

/// Resolve the tenant's live data files for [`resolve_data_urls`] —
/// blocking IO, run on the blocking pool.
pub(super) fn resolve_live_set(
    backend: &Backend,
    prefix: &str,
    window: Option<(u64, u64)>,
    tally: &mut Tally<'_>,
) -> Result<LiveSet, QueryError> {
    match backend {
        Backend::Local(root) => {
            let tenant_dir = root.join(prefix);
            let live_files = resolve_live_files(&tenant_dir, window, tally)?;
            local_file_urls(&tenant_dir, &live_files).map(LiveSet::Urls)
        }
        Backend::Remote(store) => {
            resolve_live_keys(store, prefix, window, tally).map(LiveSet::Keys)
        }
    }
}

/// The S3 analog of [`resolve_live_files`]: resolve the live data-file **keys**
/// under the tenant's `prefix` through the [`Store`] seam (RFC 0019 §3.3),
/// honouring partition-level time pruning + the RFC 0009 §3.4 per-partition
/// manifest. Returns store-relative keys (the same key space `Store::get`/`put`
/// take), addressed as object-store URLs by the caller.
///
/// Every listing is segment-wise prefix-scoped to this tenant (RFC0019.5) and
/// comes back in lexicographic order. With no window the whole prefix is listed
/// recursively. With a window, [`window_listing`] lists only the subtrees the
/// window can reach, so the cost follows the window rather than the tenant's
/// history (#853). The contract, shared with [`resolve_live_files`] through
/// [`visit`]: nothing below a parsed `year=`/`month=`/`day=` ancestor whose
/// whole span misses the window is live, even a stray key whose own directory
/// does not parse; anything else meets the per-partition rule below. The keys
/// are then grouped by their partition directory
/// (everything up to the last `/`); for each partition: skip it when an
/// `hour=HH` window prune proves it out of range, then if it carries a
/// `manifest.json` the manifest is authoritative (only its named files are
/// live, joined onto the partition key), otherwise fall back to the
/// partition's committed `*.parquet` keys (`*.parquet.tmp` is excluded — it
/// does not end in `.parquet`).
pub(super) fn resolve_live_keys(
    store: &Store,
    prefix: &str,
    window: Option<(u64, u64)>,
    tally: &mut Tally<'_>,
) -> Result<Vec<String>, QueryError> {
    let keys = match window {
        Some(window) => window_listing(store, prefix, window, &mut tally.counts)?,
        None => list_recursive(store, prefix, &mut tally.counts)?,
    };
    live_keys_from_listing(store, &keys, window, tally)
}

/// Every `(key, size)` under `prefix`, recursively, as one listing call
/// counted into `counts`.
fn list_recursive(
    store: &Store,
    prefix: &str,
    counts: &mut ResolutionCounts,
) -> Result<Vec<(String, u64)>, QueryError> {
    counts.list_requests += 1;
    let keys = store
        .list_with_sizes_blocking(Some(prefix))
        .map_err(|e| QueryError::Storage {
            detail: format!("list data prefix {prefix}: {e}"),
        })?;
    counts.listed_objects += keys.len();
    Ok(keys)
}

/// The Hive time level a delimited listing's children sit at, carrying the
/// ancestors already parsed on the way down (RFC 0005 §3.4 layout
/// `year=/month=/day=/hour=`).
#[derive(Clone, Copy)]
enum HiveLevel {
    Year,
    Month(i32),
    Day(i32, u32),
    Hour,
}

/// Every `(key, size)` under the tenant `prefix` that the window
/// `[start, end)` can reach, sorted by key: a walk down the Hive time levels with one delimited listing
/// per visited level. At each level a child whose segment parses to a UTC span
/// is dropped when the span misses the window, listed recursively in one call
/// when the window covers it whole (and at the `hour=` leaves), and descended
/// into otherwise. A child that does not parse as the expected level (a foreign
/// name, an impossible date, a pre-epoch span) is listed recursively, so it
/// meets the same per-partition rule as a full listing would give it. Objects
/// sitting directly at a visited level are kept.
///
/// Relative to listing everything and pruning afterwards, the one difference
/// is that nothing below a parsed ancestor whose whole span misses the window
/// is listed — the Hive path is the proof those keys are out of range.
///
/// Every listing call, and the objects it returns, is counted into `counts`.
fn window_listing(
    store: &Store,
    prefix: &str,
    (start, end): (u64, u64),
    counts: &mut ResolutionCounts,
) -> Result<Vec<(String, u64)>, QueryError> {
    let mut keys = Vec::new();
    let mut pending = vec![(prefix.to_owned(), HiveLevel::Year)];
    while let Some((dir, level)) = pending.pop() {
        counts.list_requests += 1;
        let listing =
            store
                .list_delimited_blocking(Some(&dir))
                .map_err(|e| QueryError::Storage {
                    detail: format!("list data prefix {dir}: {e}"),
                })?;
        counts.listed_objects += listing.objects.len();
        keys.extend(listing.objects);
        for child in listing.common_prefixes {
            let segment = child.rsplit('/').next().unwrap_or(&child);
            match visit(level, segment, &PathBuf::from(&child), (start, end)) {
                Visit::Skip => {}
                Visit::Descend(next) => pending.push((child, next)),
                Visit::List => keys.extend(list_recursive(store, &child, counts)?),
            }
        }
    }
    keys.sort_unstable();
    Ok(keys)
}

/// What the window walk does with one child prefix.
enum Visit {
    /// Provably out of the window: not listed.
    Skip,
    /// Listed recursively in one call.
    List,
    /// Partly in the window: walked one level further down.
    Descend(HiveLevel),
}

/// Decide how a window walk — the S3 listing or the local directory walk —
/// treats `child` (last segment `segment`), found under a directory at `level`:
/// `hour=` leaves go through the same conservative prune as a full listing; a
/// parsed ancestor is skipped when its span misses the window, descended into
/// when the window cuts it, and taken whole when the window covers it; an
/// unparseable one is taken whole. Both backends share this so they resolve
/// the same live set (RFC 0019 §3.3).
fn visit(
    level: HiveLevel,
    segment: &str,
    child: &std::path::Path,
    (start, end): (u64, u64),
) -> Visit {
    if matches!(level, HiveLevel::Hour) {
        return if hour_partition_in_window(child, start, end) {
            Visit::List
        } else {
            Visit::Skip
        };
    }
    match child_span(level, segment) {
        Some((_, lo, hi)) if hi <= start || end <= lo => Visit::Skip,
        Some((next, lo, hi)) if lo < start || end < hi => Visit::Descend(next),
        _ => Visit::List,
    }
}

/// Parse `segment` as the child of a directory at `level`, returning the level
/// below it and the child's `[lo, hi)` UTC-nanosecond span. `None` when the
/// segment is not the expected `<name>=<n>`, names an impossible date, or
/// starts before the epoch — the cases [`hour_partition_in_window`] refuses to
/// prune. Numbers parse without a width check, as the leaf prune parses them.
fn child_span(level: HiveLevel, segment: &str) -> Option<(HiveLevel, u64, u64)> {
    fn value<T: std::str::FromStr>(segment: &str, name: &str) -> Option<T> {
        segment.strip_prefix(name)?.strip_prefix('=')?.parse().ok()
    }
    let (next, from, to) = match level {
        HiveLevel::Year => {
            let year = value(segment, "year")?;
            let from = chrono::NaiveDate::from_ymd_opt(year, 1, 1)?;
            (
                HiveLevel::Month(year),
                from,
                chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1),
            )
        }
        HiveLevel::Month(year) => {
            let month = value(segment, "month")?;
            let from = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
            (
                HiveLevel::Day(year, month),
                from,
                from.checked_add_months(chrono::Months::new(1)),
            )
        }
        HiveLevel::Day(year, month) => {
            let from = chrono::NaiveDate::from_ymd_opt(year, month, value(segment, "day")?)?;
            (HiveLevel::Hour, from, from.succ_opt())
        }
        HiveLevel::Hour => return None,
    };
    let midnight_ns = |date: chrono::NaiveDate| {
        date.and_hms_opt(0, 0, 0)?
            .and_utc()
            .timestamp_nanos_opt()
            .and_then(|ns| u64::try_from(ns).ok())
    };
    let lo = midnight_ns(from)?;
    // A span running past the last representable instant still starts in
    // range; it just never ends before the window does.
    let hi = to.and_then(midnight_ns).unwrap_or(u64::MAX);
    Some((next, lo, hi))
}

/// Read the listed partition `manifests` (`(key, size)`, in listing
/// order) through the process-wide [`FetchPool`], so a query spanning many
/// partitions keeps several GETs in flight instead of paying one round trip
/// per partition. Returns each manifest under its partition directory;
/// every manifest byte read is added to the tally's `fetched`, and every
/// GET issued, failed or not, to its manifest reads.
///
/// A listed manifest that cannot be read is an error, not "no manifest": the
/// glob fallback would bring back the inputs the manifest retired, counting
/// compacted rows twice.
fn read_store_manifests(
    store: &Store,
    manifests: Vec<(String, u64)>,
    tally: &mut Tally<'_>,
) -> Result<HashMap<String, Manifest>, QueryError> {
    let objects: Arc<[(String, u64)]> = manifests.into();
    let mut read = HashMap::with_capacity(objects.len());
    // The GETs run on the pool's threads, so they count into a shared cell.
    let gets = Arc::new(AtomicUsize::new(0));
    let store = store.clone();
    let fetched = tally.fetched;
    let result = fetch_in_order(
        FetchPool::shared(),
        Arc::clone(&objects),
        Arc::new({
            let gets = Arc::clone(&gets);
            move |key: &str| {
                gets.fetch_add(1, Ordering::Relaxed);
                store
                    .get_bytes_blocking(key)
                    .map_err(|e| manifest_error(key, &ManifestError::Io(std::io::Error::other(e))))
            }
        }),
        |index, bytes| {
            let key = &objects[index].0;
            fetched.add(bytes.len() as u64);
            let manifest = Manifest::from_json(&bytes).map_err(|e| manifest_error(key, &e))?;
            let (dir, _) = key.rsplit_once('/').unwrap_or(("", key));
            read.insert(dir.to_owned(), manifest);
            Ok(())
        },
    );
    // On a failure the pool lets GETs its workers already claimed run to
    // completion, detached from this resolution, so a claimed GET can still
    // be issued after this read. The error-path count is the GETs issued up
    // to the failure, not those detached workers' late ones.
    tally.counts.manifest_reads += gets.load(Ordering::Relaxed);
    result.map(|()| read)
}

fn manifest_error(key: &str, e: &ManifestError) -> QueryError {
    QueryError::Storage {
        detail: format!("manifest {key}: {e}"),
    }
}

/// The per-partition half of [`resolve_live_keys`]: group `keys` by partition
/// directory, drop the partitions an `hour=HH` window prune proves out of
/// range, and resolve each remaining one through its manifest or the glob
/// fallback. `keys` must hold every key of each partition it touches — a
/// partition's manifest counts only when it is in the same listing. The
/// manifest reads are accounted into `tally`.
fn live_keys_from_listing(
    store: &Store,
    keys: &[(String, u64)],
    window: Option<(u64, u64)>,
    tally: &mut Tally<'_>,
) -> Result<Vec<String>, QueryError> {
    // Group keys by partition directory (the key up to its last `/`).
    let mut by_partition: std::collections::BTreeMap<&str, Vec<(&str, u64)>> =
        std::collections::BTreeMap::new();
    for (key, size) in keys {
        let (dir, _) = key.rsplit_once('/').unwrap_or(("", key.as_str()));
        by_partition.entry(dir).or_default().push((key, *size));
    }
    // Partition-level time pruning (RFC 0007), conservative — never prunes a
    // partition it can't prove out of range. `hour_partition_in_window`
    // parses the trailing Hive segments off a path, so build one from the
    // partition-dir key.
    if let Some((start, end)) = window {
        by_partition.retain(|dir, _| hour_partition_in_window(&PathBuf::from(dir), start, end));
    }

    // Only read a manifest whose key is actually in the listing: the
    // partition is already enumerated, so a GET for an absent manifest is a
    // wasted (404) request per un-compacted partition on S3. Absent ⇒ no
    // manifest ⇒ all committed files live. Listings return store-relative
    // keys, so this compares like-for-like.
    let listed_manifests = by_partition
        .iter()
        .filter_map(|(dir, partition_keys)| {
            let manifest_key = format!("{dir}/{MANIFEST_FILENAME}");
            partition_keys
                .iter()
                .find(|(key, _)| *key == manifest_key)
                .map(|(key, size)| ((*key).to_owned(), *size))
        })
        .collect();
    let mut manifests = read_store_manifests(store, listed_manifests, tally)?;

    let mut live = Vec::new();
    for (dir, partition_keys) in by_partition {
        let manifest = manifests.remove(dir);
        let listed = partition_keys
            .into_iter()
            .filter(|(key, _)| key.ends_with(".parquet"))
            .map(|(key, _)| key.to_owned());
        live.extend(live_in_partition(manifest.as_ref(), listed, |name| {
            format!("{dir}/{name}")
        }));
    }
    Ok(live)
}

/// One partition's live files under the RFC 0009 §3.4 live-set rule, as the
/// compactor computes them: the compacted files `manifest` names, by name
/// (`locate` joins a name onto the partition), plus every `listed` flush
/// file it does not consume. With no manifest that is every listed flush
/// file; a compacted file is never live unless named.
fn live_in_partition<T: AsRef<std::path::Path>>(
    manifest: Option<&Manifest>,
    listed: impl IntoIterator<Item = T>,
    locate: impl Fn(&str) -> T,
) -> Vec<T> {
    let live = ourios_parquet::LiveSet::new(manifest);
    let named = manifest
        .into_iter()
        .flat_map(|m| &m.files)
        .map(|name| locate(name));
    let flush = listed.into_iter().filter(|item| {
        item.as_ref()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| compacted_generation(name).is_none() && live.is_live(name))
    });
    named.chain(flush).collect()
}

/// Build the `DataFusion` table URLs for the **local** backend: every resolved
/// file must canonicalize *under* the tenant's canonical partition root before
/// it is addressed, the tenant-isolation backstop (RFC0007.5 / §3.7). The
/// manifest's entries are already validated as partition-local names
/// (`Manifest::validate`), but a symlinked `*.parquet` could still resolve
/// outside — this `starts_with` check fails such a path loudly rather than
/// reading another tenant's data. Canonical paths are de-duplicated so a
/// manifest naming the same file twice can't double-count its rows.
///
/// Each URL is the canonical absolute path: `DataFusion` 53 treats an absolute
/// filesystem path as local and URI-encodes it internally, so spaces / reserved
/// characters are handled without a hand-built `file://…` string.
/// `year/month/day/hour` stay path-only (not file columns) and the query
/// filters only data columns, so no table partition columns are declared.
pub(super) fn local_file_urls(
    tenant_dir: &std::path::Path,
    live_files: &[PathBuf],
) -> Result<Vec<ListingTableUrl>, QueryError> {
    if live_files.is_empty() {
        return Ok(Vec::new());
    }
    let tenant_root = tenant_dir.canonicalize().map_err(|e| QueryError::Storage {
        detail: format!("canonicalize {}: {e}", tenant_dir.display()),
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::with_capacity(live_files.len());
    for file in live_files {
        let abs = file.canonicalize().map_err(|e| QueryError::Storage {
            detail: format!("canonicalize {}: {e}", file.display()),
        })?;
        if !abs.starts_with(&tenant_root) {
            return Err(QueryError::Storage {
                detail: format!(
                    "resolved file {} escapes tenant partition root {}",
                    abs.display(),
                    tenant_root.display(),
                ),
            });
        }
        if seen.insert(abs.clone()) {
            urls.push(ListingTableUrl::parse(abs.display().to_string()).map_err(storage_err)?);
        }
    }
    Ok(urls)
}

/// Build the `DataFusion` table URLs for the **S3** backend: register the
/// [`Store`]'s `object_store` on `ctx` under the [`STORE_URL`] scheme/authority
/// and address each store-relative key by an `ourios://store/<key>` URL
/// (RFC 0019 §3.3). Tenant isolation is the segment-wise prefix scope of the
/// listing that produced `keys` (RFC0019.5) — the object key space has no
/// symlinks, so there is no canonical-path escape to backstop here (the §3.7
/// row-level backstop in the consumers stays). De-duplicates keys so a manifest
/// naming the same file twice can't double-count its rows.
pub(super) fn object_store_urls(
    ctx: &SessionContext,
    store: &Store,
    keys: &[String],
) -> Result<Vec<ListingTableUrl>, QueryError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let store_url = datafusion::execution::object_store::ObjectStoreUrl::parse(STORE_URL)
        .map_err(storage_err)?;
    ctx.register_object_store(
        store_url.as_ref(),
        crate::fetch_count::counted(ctx, store.object_store()),
    );
    // `Store::object_store()` is the RAW backend (prefix NOT applied), whereas
    // `list_blocking`/`get_blocking` operate in the store-relative key space
    // under `Store::prefix()` (the `OURIOS_S3_PREFIX` root). So the URLs handed
    // to DataFusion — which reads the raw backend directly — must carry the FULL
    // key: the store prefix segments followed by the relative key. With no
    // prefix (the local default) this is just the key.
    let prefix: Vec<String> = store
        .prefix()
        .parts()
        .map(|p| p.as_ref().to_owned())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::with_capacity(keys.len());
    for key in keys {
        if seen.insert(key.clone()) {
            urls.push(
                ListingTableUrl::parse(object_store_url_for_key(&prefix, key))
                    .map_err(storage_err)?,
            );
        }
    }
    Ok(urls)
}

/// Build the `ourios://store/<prefix>/<key>` URL for a store-relative `key`
/// under the store's `prefix` segments, percent-encoding each path segment.
///
/// Two reasons the full path matters:
/// - **Prefix** — `Store::object_store()` is the un-scoped raw backend, so the
///   URL must carry the store's `OURIOS_S3_PREFIX` root (`prefix`) ahead of the
///   relative key, or `DataFusion` would address an un-prefixed (not-found) path.
/// - **Encoding** — `ListingTableUrl::parse` URL-**decodes** the path, and a
///   key carries literal `%` (the partition dir is `tenant_id=<percent-encoded>`,
///   e.g. `tenant_id=tenant%20ABC`), so an un-encoded segment would be
///   double-decoded into a wrong path. Encoding every non-unreserved byte per
///   segment (and re-joining with `/`) makes the parse round-trip back to the
///   exact full key. `NON_ALPHANUMERIC` over-encodes harmlessly (`=`, `-`, `.`
///   round-trip the same); the only structural byte we keep is the `/`
///   separator, preserved by the per-segment split.
pub(super) fn object_store_url_for_key(prefix: &[String], key: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let encode = |segment: &str| utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string();
    let encoded = prefix
        .iter()
        .map(|p| encode(p))
        .chain(key.split('/').map(encode))
        .collect::<Vec<_>>()
        .join("/");
    format!("{STORE_URL}/{encoded}")
}

/// Build the `DataFusion` table URLs for an **audit** scan (the drift query's
/// `ListingTable` over the audit stream) from a resolved [`AuditFiles`],
/// branching the same way as the bulk log scan (RFC 0019 §3.3):
///
/// - **Local** ([`AuditFiles::Local`]): the paths are already the
///   canonicalizing `std::fs` walk's output — absolute, canonical, deduped, and
///   tenant-isolation-checked (the symlink-escape / tenant-root backstops live
///   in [`audit_scan`]). Address each by its absolute local path.
/// - **S3** ([`AuditFiles::Remote`]): register the store on `ctx` and address
///   each key by its percent-encoded `ourios://store/<key>` object-store URL;
///   tenant isolation is the segment-wise prefix scope (RFC0019.5).
pub(crate) fn audit_table_urls(
    ctx: &SessionContext,
    backend: StoreRef<'_>,
    files: &audit_scan::AuditFiles,
) -> Result<Vec<ListingTableUrl>, QueryError> {
    match files {
        // The walk already produced absolute canonical paths, so address them
        // directly — no `root.join`, no CWD-relative path. The local branch
        // needs no `Store`.
        audit_scan::AuditFiles::Local(paths) => paths
            .iter()
            .map(|path| ListingTableUrl::parse(path.display().to_string()).map_err(storage_err))
            .collect(),
        // Remote keys imply the S3 backend, so `backend` is `Remote` here (it is
        // what produced these keys); a `Local` is an internal invariant
        // violation, surfaced rather than unwrapped (no panics, `CLAUDE.md` §6).
        audit_scan::AuditFiles::Remote(keys) => {
            let StoreRef::Remote(store) = backend else {
                return Err(QueryError::Storage {
                    detail: "internal: S3 audit URLs reached with a local backend".to_string(),
                });
            };
            object_store_urls(ctx, store, keys)
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::super::*;

    /// The S3 object-store URL for a key prepends the store prefix and
    /// percent-encodes every segment, so `ListingTableUrl::parse`'s URL-decode
    /// round-trips back to the **full** key the raw backend expects
    /// (`OURIOS_S3_PREFIX` + the store-relative key). The partition dir carries
    /// a literal `%` (`tenant_id=tenant%20ABC`) that must survive the parse.
    #[test]
    fn object_store_url_prepends_prefix_and_round_trips() {
        let prefix = vec!["ourios".to_string()];
        let key = "data/tenant_id=tenant%20ABC/year=2026/h.parquet";
        let url = object_store_url_for_key(&prefix, key);
        // The parsed URL's object-store path must decode back to prefix + key,
        // not double-decode the literal `%20` into a space.
        let parsed = ListingTableUrl::parse(&url).expect("parse url");
        let decoded = percent_encoding::percent_decode_str(parsed.as_ref())
            .decode_utf8()
            .expect("utf8");
        assert!(
            decoded.ends_with("ourios/data/tenant_id=tenant%20ABC/year=2026/h.parquet"),
            "decoded URL must carry the full prefixed key verbatim: {decoded}",
        );
    }

    /// With no store prefix (the local default), the URL is just the key —
    /// the prefix prepend is a no-op.
    #[test]
    fn object_store_url_with_no_prefix_is_just_the_key() {
        let url = object_store_url_for_key(&[], "data/tenant_id=t/h.parquet");
        let parsed = ListingTableUrl::parse(&url).expect("parse url");
        let decoded = percent_encoding::percent_decode_str(parsed.as_ref())
            .decode_utf8()
            .expect("utf8");
        assert!(
            decoded.ends_with("data/tenant_id=t/h.parquet"),
            "no-prefix URL is the bare key: {decoded}",
        );
    }

    use super::{Tally, live_keys_from_listing};
    use crate::test_support::{Call, CountingStore};

    const TENANT_PREFIX: &str = "data/tenant_id=a";

    /// A half-open `[start, end)` query window in UTC nanoseconds.
    type Window = (u64, u64);

    /// UTC nanoseconds of a `YYYY-MM-DDTHH:MM` instant.
    fn ns(instant: &str) -> u64 {
        let nanos = chrono::NaiveDateTime::parse_from_str(instant, "%Y-%m-%dT%H:%M")
            .expect("valid instant")
            .and_utc()
            .timestamp_nanos_opt()
            .expect("in nanosecond range");
        u64::try_from(nanos).expect("post-epoch")
    }

    fn hour_prefix(year: i32, month: u32, day: u32, hour: u32) -> String {
        format!("{TENANT_PREFIX}/year={year:04}/month={month:02}/day={day:02}/hour={hour:02}")
    }

    /// A local store under `root`, and the same store seen through a
    /// request-counting wrapper.
    fn counted(root: &std::path::Path) -> (Store, Store, CountingStore) {
        let plain = Store::local(root).expect("local store");
        let mut counter = None;
        let wrapped = plain.clone().wrap_backend(|inner| {
            let counting = CountingStore::new(inner);
            counter = Some(counting.clone());
            Arc::new(counting)
        });
        (
            plain,
            wrapped,
            counter.expect("wrap_backend calls the wrapper"),
        )
    }

    fn put(store: &Store, key: &str) {
        store.put_blocking(key, b"x".to_vec()).expect("put");
    }

    /// A manifest in `dir` that consumes the flush files `retired`, so every
    /// other flush file there stays live (RFC 0009 §3.4).
    fn put_manifest(store: &Store, dir: &str, retired: &[&str]) {
        let manifest = ourios_parquet::Manifest::new(
            1,
            Vec::new(),
            retired.iter().map(ToString::to_string).collect(),
        );
        store
            .put_blocking(
                &format!("{dir}/{}", ourios_parquet::MANIFEST_FILENAME),
                manifest.to_json().expect("manifest json"),
            )
            .expect("put manifest");
    }

    /// Four partitions a day on days 1–3 of `year`-`month`.
    fn seed_month(store: &Store, year: i32, month: u32) {
        for day in 1..=3 {
            for hour in [0, 6, 12, 18] {
                put(
                    store,
                    &format!("{}/f.parquet", hour_prefix(year, month, day, hour)),
                );
            }
        }
    }

    /// Three years of four-partitions-a-day history for tenant `a`, a sibling
    /// tenant `ab` inside the window, and the query hours of interest.
    fn seed_history(store: &Store) {
        for year in 2024..=2026 {
            for month in 1..=4 {
                seed_month(store, year, month);
            }
        }
        for hour in [10, 11] {
            put(
                store,
                &format!("{}/a.parquet", hour_prefix(2026, 4, 2, hour)),
            );
        }
        put(store, &format!("{}/b.parquet", hour_prefix(2026, 4, 2, 10)));
        put(
            store,
            "data/tenant_id=ab/year=2026/month=04/day=02/hour=10/other.parquet",
        );
    }

    /// Partitions the Hive walk cannot parse or prune, plus the manifest and
    /// uncommitted-file cases, around the query hours.
    fn seed_irregular(store: &Store) {
        put(store, &format!("{TENANT_PREFIX}/stray.parquet"));
        put(
            store,
            &format!("{TENANT_PREFIX}/year=abc/month=04/day=02/hour=10/u.parquet"),
        );
        put(
            store,
            &format!("{TENANT_PREFIX}/year=1969/month=12/day=31/hour=23/u.parquet"),
        );
        put(
            store,
            &format!("{TENANT_PREFIX}/year=2026/month=4/day=2/hour=10/u.parquet"),
        );
        put(
            store,
            &format!("{TENANT_PREFIX}/year=2026/month=4/day=2/hour=3/u.parquet"),
        );
        put(
            store,
            &format!("{TENANT_PREFIX}/foo/year=2026/month=04/day=02/hour=10/u.parquet"),
        );
        put(
            store,
            &format!("{TENANT_PREFIX}/foo/year=2024/month=04/day=02/hour=10/u.parquet"),
        );
        put(
            store,
            &format!("{}/c.parquet.tmp", hour_prefix(2026, 4, 2, 10)),
        );
        put_manifest(store, &hour_prefix(2026, 4, 2, 11), &["orphan.parquet"]);
        put(
            store,
            &format!("{}/orphan.parquet", hour_prefix(2026, 4, 2, 11)),
        );
        put(
            store,
            &format!("{}/f.parquet", hour_prefix(2025, 12, 31, 23)),
        );
        put(store, &format!("{}/f.parquet", hour_prefix(2026, 1, 1, 0)));
        put_manifest(store, &hour_prefix(2025, 12, 31, 23), &[]);
    }

    /// Stray keys below parsed Hive ancestors (`year=2026`, then the
    /// 2026-04-02 hour 10 partition's): the full listing keeps them under every
    /// window, the walk only when the window reaches those ancestors.
    fn junk_below_parsed_ancestors() -> Vec<String> {
        vec![
            format!("{TENANT_PREFIX}/year=2026/month=13/day=01/hour=00/u.parquet"),
            format!("{TENANT_PREFIX}/year=2026/month=04/stray.parquet"),
            format!("{TENANT_PREFIX}/year=2026/month=04/day=02/hour=xx/u.parquet"),
            format!("{}/sub/u.parquet", hour_prefix(2026, 4, 2, 10)),
        ]
    }

    /// The pre-#853 resolution: list every key under the tenant, then prune.
    fn full_listing_oracle(store: &Store, window: Option<Window>) -> Vec<String> {
        let keys = store
            .list_with_sizes_blocking(Some(TENANT_PREFIX))
            .expect("list");
        live_keys_from_listing(
            store,
            &keys,
            window,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve")
    }

    /// #853: a ten-minute window lists only the one hour it can reach, and
    /// the delimited levels on the way down to it — never the tenant's whole
    /// history.
    #[test]
    fn windowed_resolution_lists_only_in_window_prefixes() {
        // Arrange
        let tmp = tempfile::tempdir().expect("temp");
        let (plain, wrapped, counter) = counted(tmp.path());
        seed_history(&plain);
        let window = (ns("2026-04-02T10:20"), ns("2026-04-02T10:30"));
        let in_window_hour = hour_prefix(2026, 4, 2, 10);

        // Act
        let live = resolve_live_keys(
            &wrapped,
            TENANT_PREFIX,
            Some(window),
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve");

        // Assert
        assert_eq!(
            live,
            vec![
                format!("{in_window_hour}/a.parquet"),
                format!("{in_window_hour}/b.parquet"),
            ],
        );
        let calls = counter.calls();
        let ancestors = [
            TENANT_PREFIX.to_string(),
            format!("{TENANT_PREFIX}/year=2026"),
            format!("{TENANT_PREFIX}/year=2026/month=04"),
            format!("{TENANT_PREFIX}/year=2026/month=04/day=02"),
        ];
        for call in &calls {
            match call {
                Call::List(prefix) => assert_eq!(prefix, &in_window_hour, "{calls:?}"),
                Call::ListDelimited(prefix) => {
                    assert!(ancestors.contains(prefix), "{prefix} in {calls:?}");
                }
                Call::Get(_) | Call::GetRanges(_) => {}
            }
        }
        assert!(
            calls.len() <= 5,
            "one delimited listing per Hive level plus the hour: {calls:?}",
        );
        let listed = counter.listed_keys();
        assert_eq!(
            listed.len(),
            2,
            "only the in-window hour's keys: {listed:?}"
        );
    }

    /// A window spanning whole months lists each fully covered subtree in one
    /// recursive call and still returns no key outside the window: the listed
    /// key count follows the window, not the tenant's three years of history.
    #[test]
    fn wide_window_lists_covered_subtrees_whole_and_nothing_outside() {
        // Arrange
        let tmp = tempfile::tempdir().expect("temp");
        let (plain, wrapped, counter) = counted(tmp.path());
        seed_history(&plain);
        let window = (ns("2025-01-01T00:00"), ns("2025-03-02T06:00"));

        // Act
        let live = resolve_live_keys(
            &wrapped,
            TENANT_PREFIX,
            Some(window),
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve");

        // Assert
        assert_eq!(live, full_listing_oracle(&plain, Some(window)));
        let listed = counter.listed_keys();
        assert_eq!(
            listed.len(),
            live.len(),
            "every listed key is in the window: {listed:?}",
        );
        for key in &listed {
            assert!(
                key.starts_with(&format!("{TENANT_PREFIX}/year=2025/")),
                "{key}"
            );
        }
        let recursive: Vec<Call> = counter
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::List(_)))
            .collect();
        assert!(
            recursive.contains(&Call::List(format!("{TENANT_PREFIX}/year=2025/month=01"))),
            "a fully covered month is one recursive listing: {recursive:?}",
        );
    }

    /// The window walk returns exactly what listing the whole tenant and
    /// pruning afterwards returned, for in-window, boundary-hour,
    /// year-crossing, wide, and unbounded windows, including unparseable and
    /// non-canonical partitions, manifests, and uncommitted files. The one
    /// intended difference: a stray key below a parsed `year=`/`month=`/
    /// `day=`/`hour=` ancestor whose whole span misses the window is not
    /// listed, where the full listing kept it because its own directory did
    /// not parse.
    #[test]
    fn windowed_resolution_matches_the_full_listing() {
        let tmp = tempfile::tempdir().expect("temp");
        let store = Store::local(tmp.path()).expect("local store");
        seed_history(&store);
        seed_irregular(&store);
        let junk = junk_below_parsed_ancestors();
        for key in &junk {
            put(&store, key);
        }
        let below_april = &junk[1..];
        let below_hour_10 = &junk[3..];
        // (name, window, the junk keys the walk does not list)
        let cases: [(&str, Option<Window>, &[String]); 9] = [
            (
                "in-window",
                Some((ns("2026-04-02T10:20"), ns("2026-04-02T10:30"))),
                &[],
            ),
            (
                "exact hour",
                Some((ns("2026-04-02T10:00"), ns("2026-04-02T11:00"))),
                &[],
            ),
            (
                "hour boundary",
                Some((ns("2026-04-02T10:59"), ns("2026-04-02T11:01"))),
                &[],
            ),
            (
                "next hour",
                Some((ns("2026-04-02T11:00"), ns("2026-04-02T11:30"))),
                below_hour_10,
            ),
            (
                "year boundary",
                Some((ns("2025-12-31T23:30"), ns("2026-01-01T00:30"))),
                below_april,
            ),
            (
                "wide",
                Some((ns("2024-02-15T07:00"), ns("2026-03-02T05:00"))),
                below_april,
            ),
            ("everything", Some((0, u64::MAX)), &[]),
            (
                "no data",
                Some((ns("2030-01-01T00:00"), ns("2030-01-02T00:00"))),
                &junk,
            ),
            ("no window", None, &[]),
        ];
        for (name, window, skipped) in cases {
            let walked = resolve_live_keys(
                &store,
                TENANT_PREFIX,
                window,
                &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
            )
            .expect("resolve");
            let mut expected = full_listing_oracle(&store, window);
            for key in skipped {
                assert!(
                    expected.contains(key),
                    "case {name}: the full listing keeps {key}"
                );
            }
            expected.retain(|k| !skipped.contains(k));
            assert_eq!(walked, expected, "case {name}");
        }
    }

    /// RFC 0019 backend parity: over the same tree, the local walk and the S3
    /// listing resolve the same live set for every window, including the stray
    /// keys below an out-of-window Hive ancestor that neither lists.
    #[test]
    fn local_and_s3_resolution_agree_for_every_window() {
        let tmp = tempfile::tempdir().expect("temp");
        let store = Store::local(tmp.path()).expect("local store");
        seed_history(&store);
        seed_irregular(&store);
        for key in junk_below_parsed_ancestors() {
            put(&store, &key);
        }
        let tenant_dir = tmp.path().join(TENANT_PREFIX);
        let windows = [
            Some((ns("2026-04-02T10:20"), ns("2026-04-02T10:30"))),
            Some((ns("2026-04-02T11:00"), ns("2026-04-02T11:30"))),
            Some((ns("2025-12-31T23:30"), ns("2026-01-01T00:30"))),
            Some((ns("2024-02-15T07:00"), ns("2026-03-02T05:00"))),
            Some((ns("2030-01-01T00:00"), ns("2030-01-02T00:00"))),
            Some((0, u64::MAX)),
            None,
        ];
        for window in windows {
            let mut local: Vec<String> = resolve_live_files(
                &tenant_dir,
                window,
                &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
            )
            .expect("local resolve")
            .into_iter()
            .map(|path| {
                let relative = path.strip_prefix(tmp.path()).expect("under the root");
                relative.to_str().expect("utf-8 key").to_owned()
            })
            .collect();
            local.sort_unstable();
            let mut remote = resolve_live_keys(
                &store,
                TENANT_PREFIX,
                window,
                &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
            )
            .expect("remote resolve");
            remote.sort_unstable();
            assert_eq!(local, remote, "window {window:?}");
        }
    }

    /// A tenant that never wrote anything resolves to nothing under any window.
    #[test]
    fn windowed_resolution_of_an_absent_tenant_is_empty() {
        let tmp = tempfile::tempdir().expect("temp");
        let store = Store::local(tmp.path()).expect("local store");
        put(
            &store,
            "data/tenant_id=ab/year=2026/month=04/day=02/hour=10/x.parquet",
        );

        for window in [
            None,
            Some((0, u64::MAX)),
            Some((ns("2026-04-02T10:00"), ns("2026-04-02T11:00"))),
        ] {
            let live = resolve_live_keys(
                &store,
                TENANT_PREFIX,
                window,
                &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
            )
            .expect("resolve");
            assert!(live.is_empty(), "window {window:?}: {live:?}");
        }
    }

    /// Create `<root>/data/tenant_id=a/year=2026/.../hour=10` and
    /// return `(tenant_dir, partition_dir)`.
    fn tenant_and_partition(root: &std::path::Path) -> (PathBuf, PathBuf) {
        let tenant = root.join("data/tenant_id=a");
        let partition = tenant.join("year=2026/month=04/day=02/hour=10");
        std::fs::create_dir_all(&partition).expect("mkdir partition");
        (tenant, partition)
    }

    #[test]
    fn resolve_missing_tenant_dir_is_empty() {
        // Arrange — a tenant directory that was never written.
        let tmp = tempfile::tempdir().expect("temp");
        let ghost = tmp.path().join("data/tenant_id=ghost");

        // Act
        let files = resolve_live_files(
            &ghost,
            None,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve");

        // Assert
        assert!(files.is_empty());
    }

    #[test]
    fn resolve_tmp_only_partition_is_empty() {
        // Arrange — a partition holding only an uncommitted `.tmp`.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("x.parquet.tmp"), b"partial").expect("write tmp");

        // Act
        let files = resolve_live_files(
            &tenant,
            None,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve");

        // Assert
        assert!(files.is_empty(), "uncommitted .tmp files are not live");
    }

    #[test]
    fn resolve_globs_committed_parquet_without_a_manifest() {
        // Arrange — two committed files, no manifest.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(partition.join("b.parquet"), b"b").expect("write b");

        // Act
        let files = resolve_live_files(
            &tenant,
            None,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve");

        // Assert
        assert_eq!(
            files.len(),
            2,
            "both committed files are live without a manifest"
        );
    }

    #[test]
    fn resolve_manifest_is_authoritative() {
        // Arrange — two files on disk, a manifest that consumed one.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(partition.join("b.parquet"), b"b").expect("write b");
        let manifest = ourios_parquet::Manifest::new(1, Vec::new(), vec!["b.parquet".to_string()]);
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            manifest.to_json().unwrap(),
        )
        .expect("write manifest");

        // Act
        let files = resolve_live_files(
            &tenant,
            None,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        )
        .expect("resolve");

        // Assert
        assert_eq!(files.len(), 1, "only the manifest's file is live");
        assert!(files[0].ends_with("a.parquet"));
    }

    /// The compacted output a manifest names.
    const NAMED_OUTPUT: &str = "00000000-0000-7000-8000-000000000002.g2.parquet";

    /// A compacted partition that took a late write (RFC0009.13): the
    /// manifest names one output and consumed one input; beside them lie an
    /// unconsumed late flush file and an unnamed output of an earlier
    /// generation. Returns the tenant directory.
    fn compacted_with_late_write(root: &std::path::Path) -> PathBuf {
        let (tenant, partition) = tenant_and_partition(root);
        for name in [
            "input.parquet",
            "late.parquet",
            NAMED_OUTPUT,
            "00000000-0000-7000-8000-000000000001.g1.parquet",
        ] {
            std::fs::write(partition.join(name), b"x").expect("write file");
        }
        let manifest = ourios_parquet::Manifest::new(
            2,
            vec![NAMED_OUTPUT.to_string()],
            vec!["input.parquet".to_string()],
        );
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            manifest.to_json().expect("manifest json"),
        )
        .expect("write manifest");
        tenant
    }

    /// Sorted file names of `paths`.
    fn sorted_names<P: AsRef<std::path::Path>>(paths: &[P]) -> Vec<String> {
        let mut names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.as_ref().file_name()?.to_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    /// RFC0009.13 — both resolvers read the named output and the late flush
    /// file, and leave out the consumed input and the unnamed output.
    #[test]
    fn rfc0009_13_querier_reads_a_late_file_beside_a_compacted_one() {
        let tmp = tempfile::tempdir().expect("temp");
        let tenant = compacted_with_late_write(tmp.path());
        let fetched = crate::fetch_count::FetchCounter::default();

        let local = resolve_live_files(&tenant, None, &mut Tally::new(&fetched)).expect("local");
        let store = Store::local(tmp.path()).expect("local store");
        let keyed = resolve_live_keys(&store, TENANT_PREFIX, None, &mut Tally::new(&fetched))
            .expect("store");

        let expected = [NAMED_OUTPUT, "late.parquet"];
        assert_eq!(sorted_names(&local), expected, "local resolver");
        assert_eq!(sorted_names(&keyed), expected, "store resolver");
    }

    /// A store of `partitions` hour partitions on 2026-04-02, each with a
    /// manifest that consumed `b.parquet`, so only `a.parquet` is live,
    /// seen through a [`SlowStore`] whose GETs answer later partitions first.
    fn manifested_partitions(
        root: &std::path::Path,
        partitions: u32,
        fail: Option<String>,
    ) -> (Store, crate::test_support::SlowStore) {
        fn later_answers_first(key: &str) -> std::time::Duration {
            let hour = key
                .split("hour=")
                .nth(1)
                .and_then(|rest| rest.get(..2))
                .and_then(|hh| hh.parse::<u64>().ok())
                .unwrap_or(0);
            std::time::Duration::from_millis(24_u64.saturating_sub(hour) * 3)
        }
        let plain = Store::local(root).expect("local store");
        for hour in 0..partitions {
            let dir = hour_prefix(2026, 4, 2, hour);
            put(&plain, &format!("{dir}/a.parquet"));
            put(&plain, &format!("{dir}/b.parquet"));
            put_manifest(&plain, &dir, &["b.parquet"]);
        }
        let mut slow = None;
        let wrapped = plain.wrap_backend(|inner| {
            let delayed = crate::test_support::SlowStore::new(inner, later_answers_first, fail);
            slow = Some(delayed.clone());
            Arc::new(delayed)
        });
        (wrapped, slow.expect("wrap_backend calls the wrapper"))
    }

    /// The partition manifests are read with several GETs in flight, yet
    /// the live set is exactly the manifests' files in partition order,
    /// with every manifest byte counted.
    #[test]
    fn manifests_are_read_concurrently_in_partition_order() {
        // Arrange
        let tmp = tempfile::tempdir().expect("temp");
        let (store, slow) = manifested_partitions(tmp.path(), 12, None);
        let fetched = crate::fetch_count::FetchCounter::default();

        // Act
        let live = resolve_live_keys(&store, TENANT_PREFIX, None, &mut Tally::new(&fetched))
            .expect("resolve");

        // Assert
        let expected: Vec<_> = (0..12)
            .map(|hour| format!("{}/a.parquet", hour_prefix(2026, 4, 2, hour)))
            .collect();
        assert_eq!(
            live, expected,
            "only the manifests' files, in partition order"
        );
        assert!(
            slow.max_in_flight() > 1,
            "manifest GETs overlap, got {} in flight at most",
            slow.max_in_flight(),
        );
        assert!(fetched.get() > 0, "manifest bytes are counted");
    }

    /// A listed manifest that cannot be read fails the resolution, naming
    /// the manifest, rather than falling back to the partition's glob and
    /// reviving the files it retired.
    #[test]
    fn an_unreadable_listed_manifest_is_a_storage_error() {
        // Arrange
        let tmp = tempfile::tempdir().expect("temp");
        let broken = format!(
            "{}/{}",
            hour_prefix(2026, 4, 2, 5),
            ourios_parquet::MANIFEST_FILENAME
        );
        let (store, _) = manifested_partitions(tmp.path(), 8, Some(broken.clone()));

        // Act
        let result = resolve_live_keys(
            &store,
            TENANT_PREFIX,
            None,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        );

        // Assert
        match result {
            Err(QueryError::Storage { detail }) => {
                assert!(detail.contains(&broken), "names the manifest: {detail}");
            }
            other => panic!("expected a storage error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_malformed_manifest_is_a_storage_error() {
        // Arrange — a manifest that isn't valid JSON.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            b"not json",
        )
        .expect("write manifest");

        // Act
        let result = resolve_live_files(
            &tenant,
            None,
            &mut Tally::new(&crate::fetch_count::FetchCounter::default()),
        );

        // Assert
        assert!(matches!(result, Err(QueryError::Storage { .. })));
    }

    /// `store`'s resolution under `window`, with the counts it tallied.
    fn counted_resolution(
        store: &Store,
        window: Option<Window>,
    ) -> (Result<Vec<String>, QueryError>, super::ResolutionCounts) {
        let fetched = crate::fetch_count::FetchCounter::default();
        let mut tally = Tally::new(&fetched);
        let live = resolve_live_keys(store, TENANT_PREFIX, window, &mut tally);
        (live, tally.counts)
    }

    /// RFC 0038 §3.7.2 on S3: one listing request per `list_*` call on the
    /// store and one manifest read per manifest GET, for every window shape.
    #[test]
    fn resolution_counts_are_the_store_calls() {
        let tmp = tempfile::tempdir().expect("temp");
        let (plain, wrapped, counter) = counted(tmp.path());
        seed_history(&plain);
        seed_irregular(&plain);
        for window in [
            None,
            Some((ns("2026-04-02T10:20"), ns("2026-04-02T10:30"))),
            Some((ns("2025-12-31T23:30"), ns("2026-01-01T00:30"))),
            Some((ns("2024-02-15T07:00"), ns("2026-03-02T05:00"))),
        ] {
            counter.reset();
            let (live, counts) = counted_resolution(&wrapped, window);
            live.expect("resolve");
            let calls = counter.calls();
            let count = |pred: fn(&Call) -> bool| calls.iter().filter(|c| pred(c)).count();
            assert_eq!(
                counts.list_requests,
                count(|c| matches!(c, Call::List(_) | Call::ListDelimited(_))),
                "window {window:?}: {calls:?}",
            );
            assert_eq!(
                counts.manifest_reads,
                count(|c| matches!(c, Call::Get(_))),
                "window {window:?}: {calls:?}",
            );
        }
    }

    /// Unwindowed, the one recursive listing returns every key under the
    /// tenant, each counted before any manifest filtering.
    #[test]
    fn listed_objects_count_every_key_before_filtering() {
        let tmp = tempfile::tempdir().expect("temp");
        let (_, wrapped, counter) = counted(tmp.path());
        manifested_partitions(tmp.path(), 3, None);

        let (live, counts) = counted_resolution(&wrapped, None);

        assert_eq!(live.expect("resolve").len(), 3, "one live file a partition");
        assert_eq!(
            counts,
            super::ResolutionCounts {
                list_requests: 1,
                listed_objects: 9,
                manifest_reads: 3,
            },
        );
        assert_eq!(counter.listed_keys().len(), 9);
    }

    /// A resolution that fails on a manifest keeps the work it did: the
    /// listing and every manifest GET issued up to the failing one.
    #[test]
    fn a_failed_resolution_keeps_its_counts() {
        let tmp = tempfile::tempdir().expect("temp");
        let broken = format!(
            "{}/{}",
            hour_prefix(2026, 4, 2, 5),
            ourios_parquet::MANIFEST_FILENAME
        );
        let (store, _) = manifested_partitions(tmp.path(), 8, Some(broken));

        let (live, counts) = counted_resolution(&store, None);

        assert!(live.is_err());
        assert_eq!(counts.list_requests, 1);
        assert_eq!(
            counts.listed_objects, 24,
            "three keys in each of 8 partitions"
        );
        assert!(
            (6..=8).contains(&counts.manifest_reads),
            "the GETs up to the failing sixth: {counts:?}",
        );
    }
}
