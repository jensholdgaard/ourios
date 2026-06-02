//! RFC 0009 §3.4 reader side (sequenced first per RFC0009 §7): the
//! querier resolves a partition's files through a `manifest.json`
//! when present, falling back to a `*.parquet` glob when absent. This
//! is the read-half of RFC0009.3 (a query reads one consistent
//! generation — never a compaction's superseded inputs). The
//! compactor that *writes* manifests is a later slice (epic #94); a
//! manifest is hand-written here to stand in for a committed
//! compaction.

use std::path::{Path, PathBuf};

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{MANIFEST_FILENAME, Manifest, PartitionKey, Writer};
use ourios_querier::{Querier, QueryRequest};

/// 2026-04-02T10:58:00 UTC — all offsets below stay within hour 10,
/// so every file lands in the same partition directory.
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

fn req(template_id: Option<u64>) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("a"),
        time_range: None,
        template_id,
    }
}

/// Write `recs` (which must share a partition) as one committed file;
/// return the partition directory and the committed file name.
fn write_file(bucket: &Path, recs: &[MinedRecord]) -> (PathBuf, String) {
    let part = PartitionKey::derive(&recs[0]).expect("derive partition");
    let mut w = Writer::open(bucket, part).expect("open writer");
    w.append_records(recs).expect("append");
    let written = w.close().expect("close");
    let name = written
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .expect("file name")
        .to_string();
    let dir = written.path.parent().expect("parent").to_path_buf();
    (dir, name)
}

/// A manifest is authoritative: with two files in one partition but a
/// manifest naming only one, the query sees only the named file's
/// rows — the unlisted file is ignored even though it is on disk.
#[tokio::test]
async fn rfc0009_3_manifest_restricts_to_named_files() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let (dir_a, file_a) = write_file(bucket.path(), &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
    let (dir_b, _file_b) = write_file(
        bucket.path(),
        &[rec(2, TS0 + 2_000_000), rec(2, TS0 + 3_000_000)],
    );
    assert_eq!(dir_a, dir_b, "both files share the hour-10 partition");

    let q = Querier::new(bucket.path());

    // No manifest yet → glob fallback sees both files.
    let all = q.run(req(None)).await.expect("glob-fallback query");
    assert_eq!(all.rows, 4, "without a manifest, both files are read");

    // Manifest naming only file A ⇒ file B is no longer live.
    let manifest = Manifest {
        generation: 1,
        files: vec![file_a],
    };
    std::fs::write(dir_a.join(MANIFEST_FILENAME), manifest.to_json().unwrap())
        .expect("write manifest");

    let scoped = q.run(req(None)).await.expect("manifest query");
    assert_eq!(
        scoped.rows, 2,
        "the manifest's single file is authoritative"
    );
}

/// Models a committed compaction (RFC0009.3): the partition holds the
/// two original inputs *and* a consolidated file with all their rows,
/// and the manifest names only the consolidated file. The query must
/// return each row exactly once — the superseded inputs are not
/// double-counted.
#[tokio::test]
async fn rfc0009_3_manifest_naming_compacted_file_avoids_double_count() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_file(bucket.path(), &[rec(1, TS0), rec(1, TS0 + 1_000_000)]);
    write_file(
        bucket.path(),
        &[rec(2, TS0 + 2_000_000), rec(2, TS0 + 3_000_000)],
    );
    // The "compacted" output: all four rows in one file.
    let (dir, compacted) = write_file(
        bucket.path(),
        &[
            rec(1, TS0),
            rec(1, TS0 + 1_000_000),
            rec(2, TS0 + 2_000_000),
            rec(2, TS0 + 3_000_000),
        ],
    );

    let q = Querier::new(bucket.path());

    // Pre-commit (no manifest): the glob sees inputs *and* the
    // compacted file — the double-count the manifest exists to prevent.
    let pre = q.run(req(None)).await.expect("pre-commit query");
    assert_eq!(pre.rows, 8, "glob without a manifest double-counts");

    // Commit: manifest names only the compacted file.
    let manifest = Manifest {
        generation: 2,
        files: vec![compacted],
    };
    std::fs::write(dir.join(MANIFEST_FILENAME), manifest.to_json().unwrap())
        .expect("write manifest");

    let post = q.run(req(None)).await.expect("post-commit query");
    assert_eq!(
        post.rows, 4,
        "after the manifest commit, each row counts once"
    );
    // And template-exact pushdown still works against the manifested set.
    let t1 = q.run(req(Some(1))).await.expect("template query");
    assert_eq!(t1.rows, 2);
}
