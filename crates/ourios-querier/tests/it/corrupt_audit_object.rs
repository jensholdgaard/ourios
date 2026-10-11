//! The audit scan over a tenant's audit set holding an object the upstream
//! Parquet decoders panic on fails the fold with a storage error naming
//! the object, as for any unreadable audit file, through both the local
//! and the store-backed branch. Before the decoder panic boundary it
//! aborted the querier.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, Store};
use ourios_querier::{QueryError, StoreRef, derive_template_registry};
use tempfile::TempDir;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corrupt-parquet")
        .join(name);
    std::fs::read(&path).expect("read the fixture")
}

/// One committed audit file for `tenant`, then overwritten with `corrupt`;
/// its key relative to `root`.
fn corrupt_audit_file(root: &Path, tenant: &str, corrupt: Vec<u8>) -> String {
    let mut sink = ParquetAuditSink::new(Store::local(root).expect("store"));
    sink.emit(AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: UNIX_EPOCH + Duration::from_secs(1_791_676_800),
        payload: AuditPayload::Template {
            template_id: 1,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: "user <*>".to_owned(),
            },
        },
    });
    assert_eq!(sink.write_failures(), 0, "the fixture event persists");
    let file = walk(&root.join("audit"))
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "parquet"))
        .expect("the written audit file");
    std::fs::write(&file, corrupt).expect("corrupt the file");
    file.strip_prefix(root)
        .expect("under the root")
        .to_str()
        .expect("utf-8 key")
        .to_owned()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn a_decoder_panic_in_the_audit_scan_is_a_storage_error_naming_the_object() {
    for name in [
        "audit-dict-decoder-unset.parquet",
        "audit-negative-column-range.parquet",
        "audit-page-header-unwrap.parquet",
    ] {
        // Arrange
        let bucket = TempDir::new().expect("temp dir");
        let key = corrupt_audit_file(bucket.path(), "acme", fixture(name));
        let store = Store::local(bucket.path()).expect("store");
        let tenant = TenantId::new("acme");

        for backend in [StoreRef::Local(bucket.path()), StoreRef::Remote(&store)] {
            // Act
            let derived = derive_template_registry(backend, &tenant);

            // Assert
            assert!(
                matches!(
                    &derived,
                    Err(QueryError::Storage { detail })
                        if detail.contains("panicked") && detail.contains(&key)
                ),
                "{name}: {derived:?}",
            );
        }
    }
}
