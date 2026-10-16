//! RFC 0009 §3.3 (2026-10-16 amendment): candidate planning reads a tenant
//! from one listing, and a partition's manifest only when two or more
//! committed files are listed there (RFC0009.8–.11).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::stream::BoxStream;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, Result as OsResult,
};
use proptest::prelude::*;

use super::*;

/// Requests a planning pass sent to the backend, by kind.
#[derive(Debug, Default)]
struct Counts {
    gets: AtomicU64,
    lists: AtomicU64,
    writes: AtomicU64,
}

impl Counts {
    fn gets(&self) -> u64 {
        self.gets.load(Ordering::SeqCst)
    }

    fn lists(&self) -> u64 {
        self.lists.load(Ordering::SeqCst)
    }

    fn writes(&self) -> u64 {
        self.writes.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        for counter in [&self.gets, &self.lists, &self.writes] {
            counter.store(0, Ordering::SeqCst);
        }
    }
}

/// Forwards every call to `inner`, counting it into `counts`.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn ObjectStore>,
    counts: Arc<Counts>,
}

impl fmt::Display for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting({})", self.inner)
    }
}

impl Counting {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ObjectStore for Counting {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> OsResult<PutResult> {
        Self::bump(&self.counts.writes);
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> OsResult<Box<dyn MultipartUpload>> {
        Self::bump(&self.counts.writes);
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &ObjectPath, options: GetOptions) -> OsResult<GetResult> {
        Self::bump(&self.counts.gets);
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, OsResult<ObjectPath>>,
    ) -> BoxStream<'static, OsResult<ObjectPath>> {
        Self::bump(&self.counts.writes);
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&ObjectPath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
        Self::bump(&self.counts.lists);
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&ObjectPath>,
        offset: &ObjectPath,
    ) -> BoxStream<'static, OsResult<ObjectMeta>> {
        Self::bump(&self.counts.lists);
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&ObjectPath>) -> OsResult<ListResult> {
        Self::bump(&self.counts.lists);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> OsResult<()> {
        Self::bump(&self.counts.writes);
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: RenameOptions,
    ) -> OsResult<()> {
        Self::bump(&self.counts.writes);
        self.inner.rename_opts(from, to, options).await
    }
}

/// A local store under `bucket`, seen through a request-counting backend.
fn counted(bucket: &Path) -> (Store, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    let seen = Arc::clone(&counts);
    let store = store_at(bucket).wrap_backend(move |inner| {
        Arc::new(Counting {
            inner,
            counts: seen,
        })
    });
    (store, counts)
}

/// Tenant `a`'s partition for hour 10 + `offset` of 2026-04-02.
fn hour(offset: u32) -> PartitionKey {
    PartitionKey {
        hour: 10 + offset,
        ..partition()
    }
}

/// Put a raw object of `len` bytes named `name` directly in `part`. The
/// planner reads only names and listed sizes, never the bytes.
fn put_object(store: &Store, part: &PartitionKey, name: &str, len: usize) {
    store
        .put_blocking(
            &format!("{}/{name}", partition_data_prefix(part)),
            vec![0u8; len],
        )
        .expect("put object");
}

/// A generation-1 manifest naming the compacted `files` and consuming the
/// flush files `consumed`.
fn manifest_of(files: &[&str], consumed: &[&str]) -> Manifest {
    let owned = |names: &[&str]| names.iter().map(ToString::to_string).collect();
    Manifest::new(1, owned(files), owned(consumed))
}

/// Compacted-file names for generation 1 (`<uuid>.g1.parquet`).
const COMPACTED: [&str; 3] = [
    "00000000-0000-7000-8000-000000000000.g1.parquet",
    "00000000-0000-7000-8000-000000000001.g1.parquet",
    "00000000-0000-7000-8000-000000000002.g1.parquet",
];

/// No file counts as small: a 1-byte threshold.
fn large_files_policy() -> CompactionPolicy {
    CompactionPolicy {
        small_file_bytes: 1,
        ..CompactionPolicy::default()
    }
}

/// The pre-amendment planner, kept as the RFC0009.9 oracle: enumerate the
/// partitions by delimiter walk, then read each sealed one's manifest and list
/// it. Its live set follows the format-2 rule (RFC 0009 §3.4 late-write
/// note), written out here rather than shared with the production code.
fn per_partition_plan(store: &Store, now: u64, policy: &CompactionPolicy) -> Vec<PartitionKey> {
    hour_partitions(store, "a")
        .expect("partitions")
        .into_iter()
        .filter(|p| is_sealed(p, now, policy) && per_partition_is_candidate(store, p, policy))
        .collect()
}

fn per_partition_is_candidate(
    store: &Store,
    partition: &PartitionKey,
    policy: &CompactionPolicy,
) -> bool {
    let prefix = partition_data_prefix(partition);
    let manifest = read_manifest(store, partition).expect("manifest");
    let listed_in = |list: fn(&Manifest) -> &Vec<String>, name: &str| {
        manifest
            .as_ref()
            .is_some_and(|m| list(m).iter().any(|n| n == name))
    };
    let is_live = |name: &str| {
        if compacted_generation(name).is_some() {
            listed_in(|m| &m.files, name)
        } else {
            !listed_in(|m| &m.consumed, name)
        }
    };
    let sizes: Vec<u64> = store
        .list_with_sizes_blocking(Some(&prefix))
        .expect("list")
        .iter()
        .filter(|(key, _)| is_committed_parquet(key) && is_immediate_child(key, &prefix))
        .filter(|(key, _)| is_live(basename(key)))
        .map(|(_, size)| *size)
        .collect();
    // The pre-amendment predicate, frozen here rather than shared with the
    // planner, so a change to the production predicate fails the property.
    if sizes.len() < 2 {
        return false;
    }
    if sizes.len() > policy.min_files {
        return true;
    }
    sizes.iter().any(|&len| len < policy.small_file_bytes)
}

#[test]
fn rfc0009_8_settled_partitions_cost_no_request_of_their_own() {
    // Arrange — 48 sealed hours of one file each; every other one is the
    // shape a compaction leaves: one compacted file its manifest names.
    let bucket = tempfile::tempdir().expect("temp");
    let (store, counts) = counted(bucket.path());
    let start = partition();
    for offset in 0..48_u32 {
        let part = PartitionKey {
            day: start.day + offset / 24,
            hour: offset % 24,
            ..start.clone()
        };
        if offset % 2 == 0 {
            put_object(&store, &part, COMPACTED[0], 64);
            seed_manifest(&store, &part, &manifest_of(&[COMPACTED[0]], &[]));
        } else {
            put_object(&store, &part, "only.parquet", 64);
        }
    }
    let one = tempfile::tempdir().expect("temp");
    let (single, single_counts) = counted(one.path());
    put_object(&single, &partition(), "only.parquet", 64);
    counts.reset();
    single_counts.reset();
    let now = NOW_SEALED + 3 * 24 * HOUR_NANOS;

    // Act
    let selected = plan_candidates(&store, "a", now, &large_files_policy()).expect("plan");
    plan_candidates(&single, "a", now, &large_files_policy()).expect("plan one");

    // Assert
    assert!(
        selected.is_empty(),
        "a one-file partition is never a candidate"
    );
    assert_eq!(
        counts.gets(),
        0,
        "no manifest is read for a settled partition"
    );
    assert_eq!(
        counts.lists(),
        single_counts.lists(),
        "the listing cost does not grow with the number of partitions",
    );
    assert_eq!(counts.lists(), 1, "one listing per tenant");
}

#[test]
fn rfc0009_9_a_manifest_decides_when_two_files_are_listed() {
    // Arrange — two small compacted files listed, but the manifest names
    // only one (the other is an orphan from its own generation): one live
    // file, not a candidate.
    let bucket = tempfile::tempdir().expect("temp");
    let (store, counts) = counted(bucket.path());
    put_object(&store, &partition(), COMPACTED[0], 8);
    put_object(&store, &partition(), COMPACTED[1], 8);
    seed_manifest(&store, &partition(), &manifest_of(&[COMPACTED[0]], &[]));
    counts.reset();

    // Act
    let selected =
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert!(selected.is_empty(), "the manifest's live set decides");
    assert_eq!(counts.gets(), 1, "the manifest is read exactly once");
}

#[test]
fn rfc0009_10_a_late_write_into_a_settled_partition_is_replanned() {
    // Arrange — one 100-byte file against a 50-byte small-file threshold:
    // sealed, one live file at or above the threshold, so settled.
    let bucket = tempfile::tempdir().expect("temp");
    let (store, counts) = counted(bucket.path());
    let policy = CompactionPolicy {
        small_file_bytes: 50,
        ..CompactionPolicy::default()
    };
    put_object(&store, &partition(), "settled.parquet", 100);
    counts.reset();
    assert!(
        plan_candidates(&store, "a", NOW_SEALED, &policy)
            .expect("plan")
            .is_empty(),
        "a settled partition is not selected",
    );
    assert_eq!(counts.gets(), 0, "and costs no request of its own");

    // Act — a late, small write lands in the same long-sealed hour.
    put_object(&store, &partition(), "late.parquet", 10);
    let selected = plan_candidates(&store, "a", NOW_SEALED, &policy).expect("replan");

    // Assert
    assert_eq!(selected, vec![partition()], "the late write re-flags it");
}

#[test]
fn rfc0009_10_a_late_write_into_a_compacted_partition_is_read() {
    // Arrange — a compacted partition: a manifest naming its one file.
    let bucket = tempfile::tempdir().expect("temp");
    let (store, counts) = counted(bucket.path());
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    compact_partition(&store, &partition()).expect("compact");
    counts.reset();
    assert!(
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default())
            .expect("plan")
            .is_empty()
    );
    assert_eq!(counts.gets(), 0, "settled: no manifest read");

    // Act — a late write lands next to the compacted file.
    write_file(&store, &[rec(3, TS0 + 2_000_000)]);
    counts.reset();
    let selected =
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("replan");

    // Assert — the partition is no longer skipped unread, and the late
    // file is live beside the compacted one, so the partition is selected
    // (RFC0009.13, #1026).
    assert_eq!(
        counts.gets(),
        1,
        "the late file makes the planner read the manifest"
    );
    assert_eq!(
        selected,
        vec![partition()],
        "the late file is live and re-flags the partition (#1026)"
    );
}

#[test]
fn rfc0009_11_planning_writes_nothing_and_two_planners_agree() {
    // Arrange — candidates in two hours.
    let bucket = tempfile::tempdir().expect("temp");
    let (store, counts) = counted(bucket.path());
    for offset in [0, 1] {
        put_object(&store, &hour(offset), "a.parquet", 8);
        put_object(&store, &hour(offset), "b.parquet", 8);
    }
    let before = store.list_blocking(None).expect("list before");
    counts.reset();
    let now = NOW_SEALED + HOUR_NANOS;

    // Act
    let first = plan_candidates(&store, "a", now, &CompactionPolicy::default()).expect("plan");
    let other = store_at(bucket.path());
    let second = plan_candidates(&other, "a", now, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert_eq!(counts.writes(), 0, "planning writes nothing");
    assert_eq!(store.list_blocking(None).expect("list after"), before);
    assert_eq!(first, vec![hour(0), hour(1)]);
    assert_eq!(first, second, "an independent planner selects the same");
}

#[test]
fn listed_partitions_skip_nested_and_non_canonical_objects() {
    // Arrange — two files nested one level too deep, and two under a
    // non-zero-padded hour: neither is a partition input.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let prefix = partition_data_prefix(&partition());
    for name in ["a.parquet", "b.parquet"] {
        store
            .put_blocking(&format!("{prefix}/nested/{name}"), b"x".to_vec())
            .expect("put nested");
        store
            .put_blocking(
                &format!("data/tenant_id=a/year=2026/month=04/day=02/hour=9/{name}"),
                b"x".to_vec(),
            )
            .expect("put non-canonical");
    }

    // Act
    let selected =
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert!(selected.is_empty());
}

/// One partition's generated contents: committed file sizes, which of a
/// pool of names a manifest lists (if any) — a compacted name in `files`, a
/// flush name in `consumed` — and whether a `*.parquet.tmp` is present.
#[derive(Debug, Clone)]
struct GenPartition {
    sizes: Vec<usize>,
    manifest: Option<Vec<bool>>,
    tmp: bool,
}

/// File-name pool, flush and compacted names interleaved: the first
/// `sizes.len()` are written, the rest only named.
const NAMES: [&str; 6] = [
    "f0.parquet",
    COMPACTED[0],
    "f1.parquet",
    COMPACTED[1],
    "f2.parquet",
    COMPACTED[2],
];

fn gen_partition() -> impl Strategy<Value = GenPartition> {
    (
        prop::collection::vec(1usize..20, 0..=5),
        prop::option::of(prop::collection::vec(any::<bool>(), NAMES.len())),
        any::<bool>(),
    )
        .prop_map(|(sizes, manifest, tmp)| GenPartition {
            sizes,
            manifest,
            tmp,
        })
}

fn seed(store: &Store, part: &PartitionKey, contents: &GenPartition) {
    for (name, len) in NAMES.iter().zip(&contents.sizes) {
        put_object(store, part, name, *len);
    }
    if contents.tmp {
        put_object(store, part, "t.parquet.tmp", 1);
    }
    if let Some(mask) = &contents.manifest {
        let (files, consumed): (Vec<&str>, Vec<&str>) = NAMES
            .iter()
            .zip(mask)
            .filter_map(|(name, keep)| keep.then_some(*name))
            .partition(|name| compacted_generation(name).is_some());
        seed_manifest(store, part, &manifest_of(&files, &consumed));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rfc0009_9_listing_plan_selects_what_per_partition_reads_select(
        partitions in prop::collection::vec(gen_partition(), 1..=4),
        min_files in 1usize..5,
        small in 1u64..20,
        sealed_hours in 0u64..5,
    ) {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        for (offset, contents) in (0u32..).zip(&partitions) {
            seed(&store, &hour(offset), contents);
        }
        let policy = CompactionPolicy {
            min_files,
            small_file_bytes: small,
            ..CompactionPolicy::default()
        };
        // Seals hours 10 .. 10 + sealed_hours (exclusive) and no later.
        let now = TS0 - 58 * 60 * 1_000_000_000 + sealed_hours * HOUR_NANOS + policy.grace_nanos;

        let planned = plan_candidates(&store, "a", now, &policy).expect("plan");

        prop_assert_eq!(planned, per_partition_plan(&store, now, &policy));
    }
}
