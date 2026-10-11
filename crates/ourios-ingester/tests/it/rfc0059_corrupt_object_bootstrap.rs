//! The template-id bootstrap scan (RFC 0059 §3.5) over a store holding an
//! object the upstream Parquet decoders panic on fails closed with a typed
//! scan error naming the object, as for any unreadable file, and writes no
//! high-water. Before the decoder panic boundary it aborted the receiver.

use std::path::PathBuf;

use ourios_ingester::template_ids::{HIGH_WATER_KEY, ScanOptions, TemplateIdsError, bootstrap};
use ourios_parquet::Store;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corrupt-parquet")
        .join(name);
    std::fs::read(&path).expect("read the fixture")
}

#[test]
fn a_decoder_panic_fails_the_scan_with_a_typed_error_and_nothing_is_written() {
    for name in [
        "audit-ids-dict-decoder-unset.parquet",
        "audit-negative-column-range.parquet",
    ] {
        // Arrange
        let bucket = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(bucket.path()).expect("local store");
        let key = "audit/tenant_id=acme/year=2026/month=10/day=11/corrupt.parquet";
        store.put_blocking(key, fixture(name)).expect("put");

        // Act
        let scanned = bootstrap(&store, 0, &ScanOptions::default());

        // Assert
        let err = scanned.expect_err("the scan fails closed");
        assert!(
            matches!(&err, TemplateIdsError::Scan(e) if e.is_decode_panicked()),
            "{name}: {err}",
        );
        assert_eq!(err.error_type(), "scan", "RFC 0059 §3.5: a failed scan");
        assert!(err.to_string().contains(key), "{name}: {err}");
        assert!(
            store
                .get_blocking(HIGH_WATER_KEY)
                .is_err_and(|e| e.is_not_found()),
            "{name}: no high-water is written",
        );
    }
}
