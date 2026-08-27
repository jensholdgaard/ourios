//! The compaction suite, split alongside the module directory
//! (epic #745 wave 1). `use super::*` keeps every original `super::X`
//! path resolving through the parent scope.

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;

use super::*;

/// 2026-04-02T10:58:00 UTC — offsets stay within hour 10, so
/// every record shares one partition.
const TS0: u64 = 1_775_127_480_000_000_000;

fn rec(template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn partition() -> PartitionKey {
    PartitionKey::derive(&rec(1, TS0)).expect("derive partition")
}

/// A local [`Store`] rooted at `bucket` — the test seam every migrated
/// compaction call goes through (RFC 0019 §3.3).
fn store_at(bucket: &Path) -> Store {
    Store::local(bucket).expect("local store")
}

/// Write `recs` (sharing one partition) as one committed file through the
/// store-backed [`Writer`].
fn write_file(store: &Store, recs: &[MinedRecord]) {
    let mut w = Writer::open_in(store, partition()).expect("open writer");
    w.append_records(recs).expect("append");
    w.close().expect("close");
}

/// RFC 0048 §3.4 — `visit_partition_rows` reads a partition's live
/// rows without rewriting: every row of every input file is delivered
/// in batches (glob fallback before any manifest exists), the
/// manifest is honoured once one does, and a missing partition is an
/// empty visit — never an error.
#[test]
fn visit_partition_rows_delivers_live_rows_only() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = store_at(bucket.path());
    // Empty partition: no manifest, no files.
    let mut seen: Vec<u64> = Vec::new();
    visit_partition_rows(&store, &partition(), |batch| {
        seen.extend(batch.iter().map(|r| r.template_id));
    })
    .expect("empty visit");
    assert!(seen.is_empty());

    // Two committed files, no manifest yet — the glob fallback path.
    write_file(&store, &[rec(1, TS0), rec(2, TS0 + 1)]);
    write_file(&store, &[rec(3, TS0 + 2)]);
    let mut seen: Vec<u64> = Vec::new();
    let mut batches = 0usize;
    visit_partition_rows(&store, &partition(), |batch| {
        batches += 1;
        seen.extend(batch.iter().map(|r| r.template_id));
    })
    .expect("visit");
    seen.sort_unstable();
    assert_eq!(seen, [1, 2, 3], "every live row, no rewrite");
    assert!(batches >= 2, "delivered per file/batch, got {batches}");
    let before = on_disk_parquet_count(&store, &partition());

    // After compaction the manifest is authoritative: the same rows
    // arrive once, from the consolidated file, and the superseded
    // inputs (if any linger as orphans) are not re-read.
    compact_partition(&store, &partition()).expect("compact");
    let mut seen: Vec<u64> = Vec::new();
    visit_partition_rows(&store, &partition(), |batch| {
        seen.extend(batch.iter().map(|r| r.template_id));
    })
    .expect("visit");
    seen.sort_unstable();
    assert_eq!(seen, [1, 2, 3], "manifest-listed rows, each exactly once");
    assert!(before >= 2, "the pre-compaction listing saw both inputs");
}

/// RFC 0022 §3.4 — compaction re-projects the rows it rewrites under the
/// promoted set it is *given*: inputs written under the default
/// (`service.name`-only) set consolidate into a file that carries the
/// configured key's column (history converges toward pruneability as a
/// side effect of ordinary compaction). The bare [`compact_partition`]
/// stays on the default set.
#[test]
fn compaction_reprojects_under_the_given_promoted_set() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = store_at(bucket.path());
    let with_ns = |template_id: u64, ts_ns: u64| {
        let kv = |key: &str, value: &str| ourios_core::otlp::KeyValue {
            key: key.to_string(),
            value: Some(ourios_core::otlp::AnyValue {
                value: Some(ourios_core::otlp::any_value::Value::StringValue(
                    value.to_string(),
                )),
            }),
            ..Default::default()
        };
        MinedRecord {
            resource_attributes: vec![kv("service.name", "api"), kv("k8s.namespace.name", "prod")],
            ..rec(template_id, ts_ns)
        }
    };
    write_file(&store, &[with_ns(1, TS0)]);
    write_file(&store, &[with_ns(2, TS0 + 1_000)]);

    let promoted = PromotedAttributes::new(["k8s.namespace.name".to_string()], []);
    let outcome =
        compact_partition_with_promoted(&store, &partition(), &promoted).expect("compact");
    let committed = outcome.committed.expect("committed");

    let key = format!("{}/{}", partition_data_prefix(&partition()), committed.file);
    let bytes = store.get_blocking(&key).expect("get consolidated file");
    let reader =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
            .expect("open consolidated file");
    let names: Vec<&str> = reader
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect();
    assert!(
        names.contains(&"resource.k8s.namespace.name"),
        "the consolidated file re-projects the configured key: {names:?}"
    );
    assert!(
        names.contains(&"resource.service.name"),
        "the implicit promotion rides along: {names:?}"
    );
}

/// RFC0042.8 — compaction re-projects under the *current* typed
/// declaration, across a re-typing: inputs whose files promoted the
/// key as `string` (or not at all) consolidate into an `Int64`
/// column projected from JSON truth, and the rewrite is
/// deterministic — byte-identical across two identical runs, the
/// RFC0036.4 property under a fixed config.
/// See `docs/rfcs/0042-typed-numeric-promotion.md` §5.
#[test]
fn rfc0042_8_compaction_reprojects_across_a_retyping() {
    use arrow_array::Array as _;
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int64Type;

    let kv_int = |key: &str, value: i64| ourios_core::otlp::KeyValue {
        key: key.to_string(),
        value: Some(ourios_core::otlp::AnyValue {
            value: Some(ourios_core::otlp::any_value::Value::IntValue(value)),
        }),
        ..Default::default()
    };
    let with_tokens = |template_id: u64, ts_ns: u64, tokens: i64| MinedRecord {
        attributes: vec![kv_int("input_tokens", tokens)],
        ..rec(template_id, ts_ns)
    };
    let string_class =
        PromotedAttributes::new(Vec::<String>::new(), vec!["input_tokens".to_string()]);
    let typed = PromotedAttributes::new_typed(
        [],
        [crate::promoted::PromotedKey {
            key: "input_tokens".into(),
            class: crate::promoted::PromotedClass::I64,
        }],
    );

    let compact_once = || {
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = store_at(bucket.path());
        // File 1: the key promoted under the STRING class (its Utf8
        // cells are NULL — the values are ints — but the epoch's
        // *type* is the point).
        let mut w = Writer::open_in_with_promoted(
            &store,
            partition(),
            DEFAULT_ZSTD_LEVEL,
            string_class.clone(),
        )
        .expect("open writer");
        w.append_records(&[with_tokens(1, TS0, 7)]).expect("append");
        w.close().expect("close");
        // File 2: not promoted at all.
        write_file(&store, &[with_tokens(2, TS0 + 1_000, 40)]);

        let outcome =
            compact_partition_with_promoted(&store, &partition(), &typed).expect("compact");
        let committed = outcome.committed.expect("committed");
        let key = format!("{}/{}", partition_data_prefix(&partition()), committed.file);
        store.get_blocking(&key).expect("get consolidated file")
    };

    let bytes = compact_once();
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        Bytes::from(bytes.clone()),
    )
    .expect("open consolidated file");
    let schema = reader.schema().clone();
    let field = schema
        .fields()
        .iter()
        .find(|f| f.name() == "attr.input_tokens")
        .expect("re-typed column present");
    assert_eq!(
        *field.data_type(),
        arrow_schema::DataType::Int64,
        "the current declaration's class wins on rewrite"
    );
    let batches: Vec<_> = reader
        .build()
        .expect("reader")
        .collect::<Result<Vec<_>, _>>()
        .expect("batches");
    let mut values: Vec<Option<i64>> = Vec::new();
    for batch in &batches {
        let idx = batch.schema().index_of("attr.input_tokens").expect("col");
        let arr = batch.column(idx).as_primitive::<Int64Type>();
        values.extend((0..arr.len()).map(|i| (!arr.is_null(i)).then(|| arr.value(i))));
    }
    values.sort_unstable();
    assert_eq!(
        values,
        [Some(7), Some(40)],
        "cells are projected from JSON truth, both epochs included"
    );

    // Determinism (the RFC0036.4 property under a fixed config): a
    // second identical run produces byte-identical output.
    assert_eq!(bytes, compact_once(), "rewrite is byte-identical");
}

/// Seed a manifest at the partition's manifest key (the test equivalent of
/// the pre-RFC-0019 `write_atomic`, but through the store seam).
fn seed_manifest(store: &Store, part: &PartitionKey, manifest: &Manifest) {
    store
        .put_blocking(&manifest_key(part), manifest.to_json().expect("json"))
        .expect("seed manifest");
}

/// Resolve [`partition`]'s live file keys the way a reader does
/// (manifest-authoritative, glob fallback).
fn live_keys(store: &Store, part: &PartitionKey) -> Vec<String> {
    let manifest = read_manifest(store, part).expect("manifest");
    live_file_keys(store, part, manifest.as_ref()).expect("live")
}

/// Count committed `*.parquet` objects physically present under the
/// partition prefix (what the H4 small-file detector counts).
fn on_disk_parquet_count(store: &Store, part: &PartitionKey) -> usize {
    store
        .list_blocking(Some(&partition_data_prefix(part)))
        .expect("list")
        .into_iter()
        .filter(|k| is_committed_parquet(k))
        .count()
}

/// Read every row in one live file key, through the store seam.
fn read_key(store: &Store, part: &PartitionKey, key: &str) -> Vec<MinedRecord> {
    let bytes = store.get_blocking(key).expect("get");
    Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), key)
        .expect("open")
        .read_all()
        .expect("read")
}

/// Hour-10 start (2026-04-02T10:00:00Z): a record at `+off` for any
/// `off` in `[0, HOUR_NANOS)` lands in the same partition as
/// [`partition`].
const HOUR10_START: u64 = 1_775_124_000_000_000_000;

/// A record varying only the fields the row-conservation property
/// exercises (template, in-hour timestamp, severity, one param's
/// value); everything else is held to the clean-round-trip shape so
/// equality reflects compaction, not codec edge cases.
fn prop_rec(template_id: u64, ts_ns: u64, severity_number: u8, param_value: &str) -> MinedRecord {
    MinedRecord {
        template_id,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        severity_number,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: param_value.to_string(),
        }],
        ..rec(template_id, ts_ns)
    }
}

/// Total order over the fields `prop_rec` varies — borrows the param
/// value (a free fn so the borrow's lifetime ties to the record, which
/// a closure can't express here).
fn row_key(r: &MinedRecord) -> (u64, u64, u8, &str) {
    (
        r.template_id,
        r.time_unix_nano,
        r.severity_number,
        r.params[0].value.as_str(),
    )
}

/// Resolve [`partition`]'s live files under `store` and read every row, the
/// way a reader does (manifest-authoritative, glob fallback). Both the file
/// set and the row-vs-path validation derive from the same [`partition`], so
/// they can't disagree.
fn read_partition_rows(store: &Store) -> Vec<MinedRecord> {
    let part = partition();
    let mut rows = Vec::new();
    for key in live_keys(store, &part) {
        rows.extend(read_key(store, &part, &key));
    }
    rows.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
    rows
}

proptest::proptest! {
    // Each case builds + compacts + re-reads a multi-file store, so
    // cap the case count to keep the suite fast while still covering
    // a broad spread of splits/contents.
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    /// RFC0009.2 — compaction preserves **every stored row**. For any
    /// split of records across ≥2 files in one partition, the
    /// consolidated file holds exactly the same multiset of rows
    /// (count + content), reordering aside.
    #[test]
    fn compaction_conserves_every_row(
        files in proptest::collection::vec(
            proptest::collection::vec(
                (
                    proptest::prelude::any::<u64>(),
                    0u64..HOUR_NANOS,
                    proptest::prelude::any::<u8>(),
                    // Numeric, to match the `ParamType::Num` tag
                    // `prop_rec` sets (a clean-round-trip fixture).
                    "[0-9]{1,12}",
                ),
                // 1..=15 records, 2..=5 files — 5 files also exceeds
                // the default `min_files` (4), exercising the count arm.
                1..=15usize,
            ),
            2..=5usize,
        )
    ) {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        let mut expected: Vec<MinedRecord> = Vec::new();
        for file in &files {
            let recs: Vec<MinedRecord> = file
                .iter()
                .map(|(tid, off, sev, val)| prop_rec(*tid, HOUR10_START + off, *sev, val))
                .collect();
            expected.extend(recs.iter().cloned());
            let mut w = Writer::open_in(&store, part.clone()).expect("open writer");
            w.append_records(&recs).expect("append");
            w.close().expect("close");
        }

        let outcome = compact_partition(&store, &part).expect("compact");
        proptest::prop_assert!(outcome.committed.is_some(), "≥2 files ⇒ a commit");
        proptest::prop_assert_eq!(outcome.rows, expected.len() as u64, "row count conserved");

        let live = live_keys(&store, &part);
        proptest::prop_assert_eq!(live.len(), 1, "one consolidated file");
        let mut got = read_key(&store, &part, &live[0]);

        // Multiset equality: only `(template, ts, severity, param)`
        // vary, so that tuple is a total key over distinguishable
        // rows; sorting both by it lets the element-wise `==` (full
        // record) confirm content is preserved, not just the count.
        got.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
        expected.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
        proptest::prop_assert_eq!(got, expected, "every row preserved (value-equal)");
    }
}

/// RFC0009.3 — atomic publish / no torn read. A compaction first
/// bootstraps a manifest naming the *inputs*, then writes the
/// consolidated file, then atomically swaps the manifest to name only
/// that file. This models the two states a crash could freeze and
/// asserts a reader is never torn: pre-commit it sees exactly the
/// inputs (the stored consolidated file is invisible — no double
/// count), post-commit exactly the consolidated rows (no loss).
#[test]
fn atomic_publish_is_never_torn_across_the_swap() {
    // Arrange — two committed input files (3 rows) in one partition.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
    write_file(&store, &[rec(2, TS0 + 2_000_000)]);
    let inputs = live_keys(&store, &part);
    let input_names = basenames(&inputs);
    let originals = read_partition_rows(&store);
    assert_eq!(originals.len(), 3, "three input rows");

    // Mid-compaction, in compact_partition's order: bootstrap the
    // manifest naming the inputs *first* (so the reader is
    // manifest-authoritative before any new file appears)...
    seed_manifest(
        &store,
        &part,
        &Manifest {
            generation: 1,
            files: input_names,
        },
    );
    // ...then write the consolidated file. It now exists in the store but
    // the manifest still names only the inputs.
    let mut w = Writer::open_in(&store, part.clone()).expect("writer");
    w.append_records(&originals).expect("append");
    let consolidated = w.close().expect("close");
    let consolidated_name = basename(&consolidated.key).to_owned();

    // All three files are physically present...
    assert_eq!(
        on_disk_parquet_count(&store, &part),
        3,
        "inputs + consolidated all present pre-commit"
    );
    // ...but the manifest hides the consolidated file: a reader sees
    // exactly the 3 input rows, never 6 (no torn read / double count).
    let pre = read_partition_rows(&store);
    assert_eq!(pre, originals, "pre-commit reader sees only the inputs");

    // Commit: atomic swap to name only the consolidated file.
    seed_manifest(
        &store,
        &part,
        &Manifest {
            generation: 2,
            files: vec![consolidated_name],
        },
    );

    // Post-commit: exactly the consolidated rows — no loss, no dup.
    let post = read_partition_rows(&store);
    assert_eq!(
        post, originals,
        "post-commit reader sees the consolidated rows"
    );
}

/// RFC0009.4 — crash safety (shared note). The only commit point is
/// the atomic manifest swap, so a crash always freezes the partition
/// at a clean generation (the no-torn-read half is `atomic_publish_…`
/// above). These three tests assert the other half: the dead files a
/// crash leaves are *reclaimable* by `gc_orphans`, which never removes
/// a live file. Each builds the exact on-disk state a `SIGKILL` at
/// that point would leave — faithful because the commit is a single
/// atomic swap.
///
/// Crash AFTER the commit swap, before input GC: the manifest names
/// the consolidated file; the superseded inputs are still present (the
/// post-commit generation with orphans).
#[test]
fn rfc0009_4_post_commit_orphan_inputs_are_reclaimable() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
    write_file(&store, &[rec(2, TS0 + 2_000_000)]);
    let originals = read_partition_rows(&store);
    let mut w = Writer::open_in(&store, part.clone()).expect("writer");
    w.append_records(&originals).expect("append");
    let consolidated = w.close().expect("close");
    let consolidated_name = basename(&consolidated.key).to_owned();
    seed_manifest(
        &store,
        &part,
        &Manifest {
            generation: 2,
            files: vec![consolidated_name],
        },
    );
    // Reader is already at the clean post generation despite orphans.
    assert_eq!(
        read_partition_rows(&store),
        originals,
        "post-commit reader sees the consolidated rows, ignoring orphans",
    );
    let gc = gc_orphans(&store, &part).expect("gc");
    assert_eq!(
        gc,
        OrphanGc {
            reclaimed: 2,
            failures: 0
        },
        "two orphan inputs reclaimed"
    );
    assert_eq!(live_keys(&store, &part).len(), 1, "consolidated stays live");
    assert_eq!(
        read_partition_rows(&store),
        originals,
        "GC left the live data exactly intact",
    );
    assert_eq!(
        gc_orphans(&store, &part).expect("gc again"),
        OrphanGc::default(),
        "idempotent"
    );
}

/// RFC0009.4 — crash BEFORE the commit swap: the manifest still names
/// the inputs; the freshly written consolidated file is a dead orphan
/// (the pre-commit generation). See the post-commit test for the
/// shared crash-safety note.
#[test]
fn rfc0009_4_pre_commit_orphan_consolidated_is_reclaimable() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(7, TS0), rec(7, TS0 + 1_000_000)]);
    write_file(&store, &[rec(8, TS0 + 2_000_000)]);
    let inputs = live_keys(&store, &part);
    let originals = read_partition_rows(&store);
    seed_manifest(
        &store,
        &part,
        &Manifest {
            generation: 1,
            files: basenames(&inputs),
        },
    );
    let mut w = Writer::open_in(&store, part.clone()).expect("writer");
    w.append_records(&originals).expect("append");
    w.close().expect("close"); // consolidated present, NOT in manifest
    assert_eq!(
        read_partition_rows(&store),
        originals,
        "pre-commit reader sees only the inputs (consolidated invisible)",
    );
    let gc = gc_orphans(&store, &part).expect("gc");
    assert_eq!(
        gc,
        OrphanGc {
            reclaimed: 1,
            failures: 0
        },
        "orphan consolidated reclaimed"
    );
    assert_eq!(
        live_keys(&store, &part).len(),
        inputs.len(),
        "inputs stay live"
    );
    assert_eq!(read_partition_rows(&store), originals, "inputs intact");
}

/// RFC0009.4 — a stray `*.parquet.tmp` with NO manifest (glob live
/// set): every `.parquet` is live, so only the interrupted `.tmp`
/// publish is reclaimed. See the post-commit test for the shared note.
#[test]
fn rfc0009_4_stray_tmp_reclaimed_under_glob_fallback() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(9, TS0)]);
    let tmp_key = format!(
        "{}/0190abcd-dead-7eef-8aaa-000000000000.parquet.tmp",
        partition_data_prefix(&part),
    );
    store
        .put_blocking(&tmp_key, b"torn".to_vec())
        .expect("stray tmp");
    let before = read_partition_rows(&store);
    let gc = gc_orphans(&store, &part).expect("gc");
    assert_eq!(
        gc,
        OrphanGc {
            reclaimed: 1,
            failures: 0
        },
        "only the .tmp reclaimed"
    );
    assert_eq!(
        live_keys(&store, &part).len(),
        1,
        "the live .parquet is untouched"
    );
    assert_eq!(read_partition_rows(&store), before, "glob data intact");
}

/// RFC0009.1 — compaction drives the H4 small-file **count** down. A
/// partition fragmented into more than `CompactionPolicy::min_files`
/// files (the over-fragmentation trigger) collapses to a single file,
/// dropping the per-tenant small-file count that H4's "fewer than 5 %
/// of files below 128 MiB" signal tracks. At unit scale the
/// consolidated file is itself small — the file-*size* distribution is
/// the §6 corpus test's job; this asserts the file-count lever and row
/// conservation across the collapse. The input count derives from the
/// policy so it can't drift out of sync with the default.
#[test]
fn rfc0009_1_many_small_files_collapse_to_one() {
    let policy = CompactionPolicy::default();
    // One past the over-fragmentation trigger. Every record uses the
    // same in-hour timestamp, so all inputs belong to one partition
    // regardless of how large `min_files` is — a per-record time step
    // could otherwise spill past the hour and trip the RFC0009.5
    // row-vs-path check for a reason unrelated to this test.
    let n = policy.min_files + 1;
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    for i in 0..n {
        let template_id = u64::try_from(i + 1).expect("small count");
        write_file(&store, &[rec(template_id, TS0)]);
    }
    let before = live_keys(&store, &part);
    assert_eq!(before.len(), n, "one small file per write");
    assert!(before.len() > policy.min_files, "starts over-fragmented");

    let outcome = compact_partition(&store, &part).expect("compact");
    assert_eq!(outcome.files_before, n);
    assert_eq!(
        outcome.rows,
        u64::try_from(n).expect("small count"),
        "all rows carried",
    );

    let after = live_keys(&store, &part);
    assert_eq!(after.len(), 1, "collapsed to a single live file");
    assert!(
        after.len() <= policy.min_files,
        "no longer over-fragmented (H4 small-file count down)",
    );
    // H4 counts *physical* files (footer reads), so the inputs must
    // actually be gone — not merely manifest-excluded orphans that
    // `live_keys` would hide. Assert both: the GC removed them and
    // exactly one `.parquet` remains present.
    assert_eq!(outcome.gc_failures, 0, "every superseded input removed");
    assert_eq!(
        on_disk_parquet_count(&store, &part),
        1,
        "exactly one physical .parquet file remains"
    );
    let rows = read_key(&store, &part, &after[0]);
    assert_eq!(rows.len(), n, "row conservation across the collapse");
}

/// RFC0009.6 — forward-compatible (union-schema) merge. Inputs that
/// span a schema amendment — one written with the current full schema,
/// one a pre-amendment file missing an OPTIONAL column — compact into a
/// single file carrying the union schema, read back without error
/// (RFC 0005 §3.9), with every row preserved. Compaction reads each
/// input through `Reader` (which fills a missing OPTIONAL as the §3.9
/// default) and rewrites via `Writer` (the full schema), so the output
/// is the superset.
#[test]
fn rfc0009_6_merges_inputs_spanning_a_schema_amendment() {
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    // File A — current full schema.
    write_file(&store, &[rec(1, TS0)]);
    let dir = part.data_path(bucket.path());

    // File B — a pre-amendment file missing the OPTIONAL
    // `effective_time_unix_nano` column (added 2026-06-11). Built by
    // projecting a full batch down by that one column, so no arrays
    // are hand-rolled. Same tenant + hour as A, so the row-vs-path
    // check (RFC0009.5) passes via the surviving `time_unix_nano`.
    // Written directly to the local store path (File A's write already
    // created the partition dir); compaction reads it back via the store.
    let full = crate::mined_records_to_batch(&[rec(2, TS0)]).expect("full batch");
    let drop = full
        .schema()
        .index_of(crate::columns::EFFECTIVE_TIME_UNIX_NANO)
        .expect("amended column present in the full schema");
    let keep: Vec<usize> = (0..full.num_columns()).filter(|&i| i != drop).collect();
    let reduced = full
        .project(&keep)
        .expect("project off the OPTIONAL column");
    assert!(
        reduced
            .schema()
            .index_of(crate::columns::EFFECTIVE_TIME_UNIX_NANO)
            .is_err(),
        "file B is missing the OPTIONAL column",
    );
    let path_b = dir.join("0190abcd-0000-7000-8000-000000000002.parquet");
    let file_b = std::fs::File::create(&path_b).expect("create B");
    let mut w = ArrowWriter::try_new(file_b, reduced.schema(), None).expect("arrow writer");
    w.write(&reduced).expect("write B");
    w.close().expect("close B");

    // Two inputs with differing schemas → union merge.
    let outcome = compact_partition(&store, &part).expect("union merge");
    assert_eq!(outcome.files_before, 2);
    assert_eq!(outcome.rows, 2, "both rows carried across the union merge");

    // Output carries the full (union) schema and reads without error.
    let live = live_keys(&store, &part);
    assert_eq!(live.len(), 1, "consolidated to one file");
    // Assert the union directly: the consolidated Parquet schema
    // carries the amended column file B lacked (not B's reduced one).
    let out_bytes = store.get_blocking(&live[0]).expect("get output");
    let out_schema = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(out_bytes))
        .expect("output reader builder")
        .schema()
        .clone();
    assert!(
        out_schema
            .index_of(crate::columns::EFFECTIVE_TIME_UNIX_NANO)
            .is_ok(),
        "consolidated output carries the union (amended) schema",
    );
    let rows = read_key(&store, &part, &live[0]);
    assert_eq!(rows.len(), 2, "every row preserved across the amendment");
}

/// RFC0009.5 — tenant + partition isolation. Compaction reads every
/// input through `Reader::open_partition_bytes`, which enforces the
/// RFC 0005 §3.9 row-vs-path contract, so an input file holding a row
/// that belongs to a *different* time bucket (or tenant) aborts the
/// compaction instead of being silently merged across the boundary.
#[test]
fn rfc0009_5_mis_partitioned_input_aborts_rather_than_merging() {
    use parquet::arrow::ArrowWriter;

    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    // A legitimate input for partition P.
    write_file(&store, &[rec(1, TS0)]);
    let dir = part.data_path(bucket.path());

    // A second file dropped into P's directory whose row belongs to a
    // *different* hour (TS0 + 2 h) — a mis-partitioned input.
    let foreign = rec(2, TS0 + 2 * HOUR_NANOS);
    assert_ne!(
        PartitionKey::derive(&foreign).expect("derive foreign"),
        part,
        "the foreign row really maps to another partition",
    );
    let batch = crate::mined_records_to_batch(&[foreign]).expect("batch");
    let path = dir.join("0190abcd-0000-7000-8000-0000000000f0.parquet");
    let file = std::fs::File::create(&path).expect("create foreign");
    let mut w = ArrowWriter::try_new(file, batch.schema(), None).expect("writer");
    w.write(&batch).expect("write foreign");
    w.close().expect("close foreign");

    // Two inputs, one mis-partitioned → compaction aborts on the
    // row-vs-path check; it never merges rows across partition keys.
    let err = compact_partition(&store, &part).expect_err("must reject");
    assert!(
        matches!(
            err,
            CompactionError::Read(ReaderError::PartitionMismatch { .. })
        ),
        "aborts specifically on the §3.9 row-vs-path check, not some other \
         read failure; got {err:?}",
    );
}

#[test]
fn compacts_two_files_into_one_preserving_rows() {
    // Arrange — two committed files in one partition (5 rows total).
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
    write_file(
        &store,
        &[
            rec(2, TS0 + 2_000_000),
            rec(2, TS0 + 3_000_000),
            rec(2, TS0 + 4_000_000),
        ],
    );

    // Act
    let outcome = compact_partition(&store, &part).expect("compact");

    // Assert — consolidated to one file with all 5 rows, manifest
    // names it, inputs GC'd, rows preserved.
    assert_eq!(outcome.files_before, 2);
    assert_eq!(outcome.rows, 5);
    assert_eq!(outcome.gc_failures, 0, "both inputs removed");
    let committed = outcome.committed.expect("committed");
    let live = live_keys(&store, &part);
    assert_eq!(live.len(), 1, "one file remains live");
    assert!(live[0].ends_with(&committed.file));
    let rows = read_key(&store, &part, &live[0]);
    assert_eq!(rows.len(), 5, "every row preserved");
}

#[test]
fn reports_byte_volumes_for_io_and_file_size_metrics() {
    // Arrange — two committed files in one partition.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
    write_file(&store, &[rec(2, TS0 + 2_000_000)]);

    // Act
    let outcome = compact_partition(&store, &part).expect("compact");

    // Assert — read volume covers both inputs, write volume is the
    // (sole, live) consolidated file's actual stored byte size.
    let committed = outcome.committed.expect("committed");
    let live = live_keys(&store, &part);
    assert_eq!(live.len(), 1, "one consolidated file remains live");
    let stored = store.get_blocking(&live[0]).expect("get").len() as u64;
    assert!(outcome.bytes_read > 0, "read volume is recorded");
    assert_eq!(
        outcome.bytes_written, stored,
        "write volume is the consolidated file's byte size"
    );
    assert!(live[0].ends_with(&committed.file));
}

#[test]
fn no_op_reports_zero_byte_volumes() {
    // Arrange — one file: a no-op, nothing read or written.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0)]);

    // Act
    let outcome = compact_partition(&store, &part).expect("compact");

    // Assert
    assert!(outcome.committed.is_none());
    assert_eq!(outcome.bytes_read, 0);
    assert_eq!(outcome.bytes_written, 0);
}

#[test]
fn single_file_partition_is_a_no_op() {
    // Arrange — one file, nothing to consolidate.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0)]);

    // Act
    let outcome = compact_partition(&store, &part).expect("compact");

    // Assert — no-op: no commit, no manifest written.
    assert_eq!(outcome.files_before, 1);
    assert!(outcome.committed.is_none());
    assert!(
        read_manifest(&store, &part).expect("manifest").is_none(),
        "a no-op writes no manifest",
    );
}

#[test]
fn bumps_generation_from_an_existing_manifest() {
    // Arrange — two files plus a manifest already at generation 5.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    let names = basenames(&live_keys(&store, &part));
    seed_manifest(
        &store,
        &part,
        &Manifest {
            generation: 5,
            files: names,
        },
    );

    // Act
    let outcome = compact_partition(&store, &part).expect("compact");

    // Assert — committed at generation 6.
    assert_eq!(outcome.committed.expect("committed").generation, 6);
}

// --- plan_candidates (RFC 0009 §3.3 sealed + candidate selection) ---

/// `now` inside the partition's hour → not sealed; well past the
/// hour-end + grace → sealed.
const NOW_UNSEALED: u64 = TS0;
const NOW_SEALED: u64 = TS0 + 2 * HOUR_NANOS;

#[test]
fn plan_skips_an_unsealed_partition() {
    // Arrange — two small files, but `now` is still inside the hour.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);

    // Act
    let selected =
        plan_candidates(&store, "a", NOW_UNSEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert!(
        selected.is_empty(),
        "an unsealed partition is never selected"
    );
}

#[test]
fn plan_selects_a_sealed_small_file_partition() {
    // Arrange — two committed files (each well under 128 MiB), and
    // `now` past the hour-end + grace.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);

    // Act
    let selected =
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert_eq!(
        selected,
        vec![partition()],
        "the sealed small-file partition is selected"
    );
}

#[test]
fn plan_returns_partitions_in_chronological_order() {
    // Arrange — two sealed small-file partitions, hour 10 and 11.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    for ts in [TS0, TS0 + HOUR_NANOS] {
        for template_id in [1_u64, 2] {
            let record = rec(template_id, ts);
            let mut w =
                Writer::open_in(&store, PartitionKey::derive(&record).unwrap()).expect("open");
            w.append_records(&[record]).expect("append");
            w.close().expect("close");
        }
    }
    let now = TS0 + 3 * HOUR_NANOS; // past hour 11's end + grace

    // Act
    let selected = plan_candidates(&store, "a", now, &CompactionPolicy::default()).expect("plan");

    // Assert — both selected, oldest first, regardless of listing order.
    let hours: Vec<u32> = selected.iter().map(|p| p.hour).collect();
    assert_eq!(hours, vec![10, 11], "deterministic, chronological");
}

#[test]
fn plan_skips_a_single_file_partition() {
    // Arrange — one file: sealed, but nothing to consolidate.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, &[rec(1, TS0)]);

    // Act
    let selected =
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert!(
        selected.is_empty(),
        "a one-file partition can't be consolidated"
    );
}

#[test]
fn plan_selects_a_sealed_many_file_partition_via_count() {
    // Arrange — five files (> default min_files of 4), sealed, with
    // the size arm disabled (1-byte threshold) so *only* the count
    // arm can select.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    for i in 0..5 {
        write_file(&store, &[rec(1, TS0 + i * 1_000)]);
    }
    let policy = CompactionPolicy {
        min_files: 4,
        small_file_bytes: 1,
        grace_nanos: CompactionPolicy::default().grace_nanos,
    };

    // Act
    let selected = plan_candidates(&store, "a", NOW_SEALED, &policy).expect("plan");

    // Assert
    assert_eq!(
        selected,
        vec![partition()],
        "the count arm selects a partition with more than min_files"
    );
}

#[test]
fn plan_skips_when_files_are_large_and_few() {
    // Arrange — two files, sealed, but a policy where neither the
    // count (2 ≤ min_files) nor the size (1-byte threshold) arm
    // fires.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    let policy = CompactionPolicy {
        min_files: 4,
        small_file_bytes: 1,
        grace_nanos: CompactionPolicy::default().grace_nanos,
    };

    // Act
    let selected = plan_candidates(&store, "a", NOW_SEALED, &policy).expect("plan");

    // Assert
    assert!(selected.is_empty(), "few large files are not a candidate");
}

#[test]
fn plan_skips_non_canonical_partition_dir_names() {
    // Arrange — a sealed partition whose `month` segment isn't
    // zero-padded (`month=4`, not `month=04`). A PartitionKey from
    // it would render `month=04` via data_path and miss this key,
    // so it must not be selected.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    for name in ["a.parquet", "b.parquet"] {
        store
            .put_blocking(
                &format!("data/tenant_id=a/year=2026/month=4/day=02/hour=10/{name}"),
                b"x".to_vec(),
            )
            .expect("put");
    }

    // Act
    let selected =
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert!(selected.is_empty(), "non-canonical dir names are skipped");
}

#[test]
fn plan_for_a_tenant_with_no_data_is_empty() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());

    // Act
    let selected =
        plan_candidates(&store, "ghost", NOW_SEALED, &CompactionPolicy::default()).expect("plan");

    // Assert
    assert!(selected.is_empty());
}

// --- RFC 0036 §3.2 sorted-compaction internals ---

/// A record for the sort tests: `service` becomes the promoted
/// `service.name` resource attribute (`None` = absent, the §3.1
/// nulls-first case) and `id` a unique param payload so equal-key
/// rows stay distinguishable for tie-break assertions.
fn sort_rec(service: Option<&str>, ts_ns: u64, id: u64) -> MinedRecord {
    let resource_attributes = match service {
        Some(name) => vec![ourios_core::otlp::KeyValue {
            key: SERVICE_NAME_KEY.to_string(),
            value: Some(ourios_core::otlp::AnyValue {
                value: Some(ourios_core::otlp::any_value::Value::StringValue(
                    name.to_string(),
                )),
            }),
            ..Default::default()
        }],
        None => Vec::new(),
    };
    MinedRecord {
        resource_attributes,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: id.to_string(),
        }],
        ..rec(id, ts_ns)
    }
}

/// Mirror partition `part`'s data files from `from` into `to`
/// byte-for-byte under the same names, so two stores hold the
/// RFC0036.4 "same bytes, same names" input set.
fn mirror_partition(from: &Store, to: &Store, part: &PartitionKey) {
    for key in from
        .list_blocking(Some(&partition_data_prefix(part)))
        .expect("list source")
    {
        let bytes = from.get_blocking(&key).expect("get source");
        to.put_blocking(&key, bytes).expect("put mirror");
    }
}

/// Read the consolidated file's raw bytes after a committed
/// compaction.
fn consolidated_bytes(store: &Store, part: &PartitionKey, committed: &Committed) -> Vec<u8> {
    let key = format!("{}/{}", partition_data_prefix(part), committed.file);
    store.get_blocking(&key).expect("get consolidated")
}

proptest::proptest! {
    // Each case compacts the same inputs through both §3.2 paths
    // (in-memory and forced spill + fan-in-2 hierarchical merge),
    // so keep the case count moderate.
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

    /// RFC0036.1 (§6 merge property, internal half) — for arbitrary
    /// service/time/duplicate-key mixes, both §3.2 paths produce the
    /// §3.1 total order: the multiset equals the inputs' union,
    /// rows are (service, time)-sorted with absent-service first,
    /// equal-key rows land in (sorted-basename input ordinal,
    /// pre-sort row ordinal) tie-break order — and the spill path's
    /// bytes are identical to the in-memory path's, which is the
    /// §3.5 determinism argument across the §7 skip-spill fork.
    /// See `docs/rfcs/0036-write-side-layout.md` §5 / §6.
    #[test]
    fn sorted_merge_realises_the_total_order_on_both_paths(
        files in proptest::collection::vec(
            proptest::collection::vec(
                // (service index; 0 = absent, times from a small
                // pool to force duplicate keys)
                (0usize..4, 0u64..6),
                1..=12usize,
            ),
            2..=5usize,
        )
    ) {
        let services = [None, Some("svc-a"), Some("svc-b"), Some("svc-c")];
        let bucket_a = tempfile::tempdir().expect("temp a");
        let bucket_b = tempfile::tempdir().expect("temp b");
        let store_a = store_at(bucket_a.path());
        let store_b = store_at(bucket_b.path());
        let part = partition();

        let mut id: u64 = 0;
        let mut inputs: Vec<(String, Vec<MinedRecord>)> = Vec::new();
        for file in &files {
            let recs: Vec<MinedRecord> = file
                .iter()
                .map(|(svc, toff)| {
                    id += 1;
                    sort_rec(services[*svc], HOUR10_START + toff * 1_000, id)
                })
                .collect();
            let mut w = Writer::open_in(&store_a, part.clone()).expect("open writer");
            w.append_records(&recs).expect("append");
            let written = w.close().expect("close");
            inputs.push((basename(&written.key).to_owned(), recs));
        }
        mirror_partition(&store_a, &store_b, &part);

        // The §3.1 model order: concatenate in sorted-basename input
        // order, then stable-sort by (service, time) — leaving
        // equal-key rows in (input ordinal, row ordinal) order.
        inputs.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut expected: Vec<MinedRecord> =
            inputs.into_iter().flat_map(|(_, recs)| recs).collect();
        sort_records(ClusterKeys::ServiceThenTime, &mut expected);

        let in_memory = compact_partition(&store_a, &part).expect("compact in-memory");
        let spilled = compact_sorted(
            &store_b,
            &part,
            &PromotedAttributes::default(),
            ClusterKeys::ServiceThenTime,
            SortTuning {
                in_memory_max_bytes: 0,
                fan_in: 2,
                ..SortTuning::default()
            },
        )
        .expect("compact spilled");
        let in_memory = in_memory.committed.expect("in-memory commit");
        let spilled = spilled.committed.expect("spill commit");

        let bytes_a = consolidated_bytes(&store_a, &part, &in_memory);
        let bytes_b = consolidated_bytes(&store_b, &part, &spilled);
        proptest::prop_assert!(
            bytes_a == bytes_b,
            "in-memory and spill paths must emit byte-identical output \
             ({} vs {} bytes)",
            bytes_a.len(),
            bytes_b.len(),
        );

        let got = Reader::open_partition_bytes(
            Bytes::from(bytes_a),
            part.clone(),
            &in_memory.file,
        )
        .expect("open consolidated")
        .read_all()
        .expect("read consolidated");
        proptest::prop_assert_eq!(got, expected, "§3.1 total order realised");
    }
}

/// RFC 0036 §3.1 / §7 — the time-only fallback. A promoted set
/// without `service.name` is unrepresentable today (RFC 0022 makes
/// the key implicit and non-removable), so the degradation is
/// driven through the internal seam: under `ClusterKeys::TimeOnly`
/// the consolidated rows sort by `time_unix_nano` alone (service
/// values deliberately anti-lexicographic to prove they are
/// ignored) and every row group declares the single time sorting
/// column.
#[test]
fn time_only_keys_sort_and_declare_time_alone() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[sort_rec(Some("zzz"), TS0, 1)]);
    write_file(&store, &[sort_rec(Some("aaa"), TS0 + 1_000, 2)]);
    write_file(&store, &[sort_rec(None, TS0 + 500, 3)]);

    let outcome = compact_sorted(
        &store,
        &part,
        &PromotedAttributes::default(),
        ClusterKeys::TimeOnly,
        SortTuning::default(),
    )
    .expect("compact");
    let committed = outcome.committed.expect("committed");
    let bytes = consolidated_bytes(&store, &part, &committed);

    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone()))
        .expect("open consolidated");
    let meta = builder.metadata();
    for rg in meta.row_groups() {
        let declared = rg.sorting_columns().expect("sorting_columns declared");
        assert_eq!(declared.len(), 1, "time-only declares a single key");
        let leaf = usize::try_from(declared[0].column_idx).expect("leaf index");
        assert_eq!(
            rg.column(leaf).column_path().string(),
            crate::columns::TIME_UNIX_NANO,
            "the single key is time_unix_nano",
        );
        assert!(!declared[0].descending, "ascending");
    }

    let rows = Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), &committed.file)
        .expect("open")
        .read_all()
        .expect("read");
    let times: Vec<u64> = rows.iter().map(|r| r.time_unix_nano).collect();
    assert_eq!(
        times,
        vec![TS0, TS0 + 500, TS0 + 1_000],
        "time-only order ignores service values",
    );
}

/// Build a `k`-file partition of `s` rows each, in [`partition`], with
/// rotating promoted `service.name` values and per-row-unique times so
/// the §3.1 sort has real work.
fn build_k_file_partition(store: &Store, k: u64, s: u64) {
    let part = partition();
    let mut id: u64 = 0;
    for _ in 0..k {
        let recs: Vec<MinedRecord> = (0..s)
            .map(|_| {
                id += 1;
                let svc = ["svc-a", "svc-b", "svc-c"][usize::try_from(id % 3).expect("mod 3")];
                sort_rec(Some(svc), HOUR10_START + id, id)
            })
            .collect();
        let mut w = Writer::open_in(store, part.clone()).expect("open writer");
        w.append_records(&recs).expect("append");
        w.close().expect("close");
    }
}

/// RFC0036.3 (memory bound) — the load-bearing §3.2 claim. On a
/// partition of `K` inputs of `S` rows each, the forced-spill sort's
/// peak decoded-row residency is bounded by one input (phase 1,
/// inputs decoded strictly one at a time) plus `F × batch` (phase 2,
/// one streamed batch per open run) — it must NOT regress to holding
/// the whole `K × S` partition decoded, which is the whole reason the
/// external merge sort exists. The in-memory (skip-spill) path, by
/// contrast, deliberately holds the whole partition (§7 tradeoff,
/// bounded by `in_memory_max_bytes`); measuring both peaks on the
/// *same* fixture pins both halves of §3.2's accurate bound. The gauge
/// is thread-local (a `compact_*` call runs entirely on this thread),
/// so the assertion is immune to `cargo test`'s in-process parallelism.
/// See `docs/rfcs/0036-write-side-layout.md` §5 / §6.
#[test]
fn rfc0036_3_forced_spill_peak_far_below_whole_partition() {
    // K inputs of S rows. The spill path's peak is dominated by
    // phase-1's one fully decoded input (S rows, decoded strictly one
    // at a time); phase-2 opens only K < F cursors — no hierarchical
    // pass — each holding one small reader batch, well under S. So the
    // peak sits at ~one input, an order of magnitude below the whole
    // partition (K × S), making a whole-partition regression
    // unambiguous. (The F × batch term in the RFC 0036 §3.2 bound is
    // the worst case for F saturated runs; it does not bite here.)
    const K: u64 = 6;
    const S: u64 = 12_000;
    let total = usize::try_from(K * S).expect("fits usize");
    let fan_in = SortTuning::default().fan_in;

    // --- Spill path: force it with in_memory_max_bytes = 0 so every
    // input spills one at a time. ---
    let bucket_spill = tempfile::tempdir().expect("temp spill");
    let store_spill = store_at(bucket_spill.path());
    build_k_file_partition(&store_spill, K, S);
    residency::reset();
    let spilled = compact_sorted(
        &store_spill,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: 0,
            fan_in,
            ..SortTuning::default()
        },
    )
    .expect("compact spilled");
    let spill_peak = residency::peak();
    assert!(spilled.committed.is_some(), "≥2 files ⇒ a commit");
    assert_eq!(spilled.rows, K * S, "every row carried");

    // RFC0036.3's property is an *upper* bound — "not whole-partition".
    // The teeth: peak must stay far below the whole partition; this fails
    // if the merge ever buffers everything decoded. We deliberately do
    // NOT assert a lower bound near one input: a future formation that
    // streams within an input could peak below S and still satisfy the
    // RFC. `> 0` is only a gauge-liveness sanity (spilling decodes rows).
    assert!(spill_peak > 0, "the residency gauge recorded nothing");
    assert!(
        spill_peak < total / 2,
        "forced-spill peak {spill_peak} regressed toward whole-partition \
         residency (total {total}) — the merge must not hold the partition decoded",
    );

    // --- In-memory path: same fixture, unbounded skip-spill window. ---
    let bucket_mem = tempfile::tempdir().expect("temp mem");
    let store_mem = store_at(bucket_mem.path());
    build_k_file_partition(&store_mem, K, S);
    residency::reset();
    let in_memory = compact_sorted(
        &store_mem,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: u64::MAX,
            fan_in,
            ..SortTuning::default()
        },
    )
    .expect("compact in-memory");
    let mem_peak = residency::peak();
    assert_eq!(in_memory.rows, K * S, "every row carried");
    assert_eq!(
        mem_peak, total,
        "the in-memory path holds the whole partition decoded (bounded by \
         in_memory_max_bytes — the §7 skip-spill tradeoff)",
    );

    // The contrast is the point: the spill path holds a fraction of what
    // the in-memory path holds on the identical partition.
    assert!(
        spill_peak * 4 < mem_peak,
        "the spill path ({spill_peak}) must hold far less than the \
         in-memory path ({mem_peak})",
    );
}
