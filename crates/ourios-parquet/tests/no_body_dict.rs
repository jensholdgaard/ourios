//! Scenario RFC0005.8 — `body` column carries no dictionary encoding.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Writes 100+ unique high-entropy body strings through the
//! writer, opens the resulting Parquet file's footer, and asserts:
//!
//! - The `body` column chunk's `compression` is `ZSTD`.
//! - The `body` column chunk's `encodings` does NOT include
//!   `PLAIN_DICTIONARY` or `RLE_DICTIONARY`.
//! - The `body` column chunk's `dictionary_page_offset` is unset.
//!
//! The three clauses are RFC0005.8's normative "Then" trio,
//! anchoring CLAUDE.md §3.2 ("Drain assumes parameters are short,
//! variable bits. Reality: ... Unbounded values destroy Parquet's
//! dictionary encoding and bloat files.") to a Parquet-metadata
//! check the implementer can run by hand against `parquet-tools meta`.

use std::fs::File;

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use parquet::basic::{Compression, Encoding};
use parquet::file::reader::{FileReader, SerializedFileReader};
use tempfile::TempDir;

fn high_entropy_body(seed: usize) -> String {
    // 256 bytes of pseudo-random ASCII per row — no two rows share
    // the same body, so a dictionary encoder would store every
    // row as its own dictionary entry (the cardinality blow-up
    // RFC 0005 §3.2's invariant prohibits).
    let mut s = String::with_capacity(256);
    let mut x: u64 = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for _ in 0..256 {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let c = ((x >> 56) as u8 & 0x3F) + b' '; // printable
        s.push(c as char);
    }
    s
}

fn body_record(seed: usize) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("tenant-x"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        time_unix_nano: 1_775_127_480_000_000_000,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new(), String::new()],
        body: Some(high_entropy_body(seed)),
        confidence: 0.5,
        lossy_flag: true, // body retained per §6.3 lossy-zone
    }
}

/// Scenario RFC0005.8 — body column metadata reflects the
/// §3.6 no-dictionary rule.
#[test]
fn rfc0005_8_body_column_has_no_dictionary_encoding() {
    let bucket = TempDir::new().unwrap();

    // Generate 200 unique-body records — well past §5 / RFC0005.8's
    // 100-record floor.
    let records: Vec<MinedRecord> = (0..200).map(body_record).collect();

    let partition = PartitionKey::derive(&records[0]).expect("derive partition");
    let mut writer = Writer::open(bucket.path(), partition).expect("open writer");
    writer.append_records(&records).expect("append");
    let written = writer.close().expect("close writer");
    assert_eq!(written.num_rows, 200);

    // Read the footer.
    let file = File::open(&written.path).expect("open parquet file");
    let reader = SerializedFileReader::new(file).expect("open parquet reader");
    let metadata = reader.metadata();
    assert!(metadata.num_row_groups() >= 1);

    // Locate the `body` column across every row group; assert
    // the three RFC0005.8 clauses for each.
    let body_col_name = "body";
    let mut body_checked = 0;
    for rg_idx in 0..metadata.num_row_groups() {
        let rg = metadata.row_group(rg_idx);
        for col_idx in 0..rg.num_columns() {
            let col = rg.column(col_idx);
            if col.column_path().string() != body_col_name {
                continue;
            }
            body_checked += 1;

            // Clause 1: compression == ZSTD.
            assert!(
                matches!(col.compression(), Compression::ZSTD(_)),
                "row group {rg_idx}: body compression must be ZSTD, got {:?}",
                col.compression(),
            );

            // Clause 2: encodings does not include any dictionary
            // variant.
            let encs = col.encodings();
            assert!(
                !encs.contains(&Encoding::PLAIN_DICTIONARY),
                "row group {rg_idx}: body encodings must not include PLAIN_DICTIONARY, got {encs:?}",
            );
            assert!(
                !encs.contains(&Encoding::RLE_DICTIONARY),
                "row group {rg_idx}: body encodings must not include RLE_DICTIONARY, got {encs:?}",
            );

            // Clause 3: no dictionary page on disk.
            assert!(
                col.dictionary_page_offset().is_none(),
                "row group {rg_idx}: body must have no dictionary_page_offset, got {:?}",
                col.dictionary_page_offset(),
            );
        }
    }
    assert!(
        body_checked >= 1,
        "expected at least one `body` column chunk in the file",
    );
}

/// RFC 0005 §3.6 sub-rule — the `params` list-value leaf
/// (`params.list.element.value`) also carries `Dictionary: no`
/// per "Per-row entropy too high". Verify by writing records
/// with non-empty `params` and asserting the leaf column's
/// encodings + `dictionary_page_offset` reflect the override.
#[test]
fn rfc0005_6_params_value_leaf_has_no_dictionary_encoding() {
    let bucket = TempDir::new().unwrap();

    // 50 records, each with one param. Per-row entropy is the
    // whole point — distinct values so dict, if enabled, would
    // grow the dictionary linearly.
    let records: Vec<MinedRecord> = (0..50)
        .map(|i| MinedRecord {
            tenant_id: TenantId::new("tenant-x"),
            template_id: 1,
            template_version: 1,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            time_unix_nano: 1_775_127_480_000_000_000,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: format!("unique-param-value-{i}-padded-with-noise-{i:08}"),
            }],
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        })
        .collect();

    let partition = PartitionKey::derive(&records[0]).expect("derive partition");
    let mut writer = Writer::open(bucket.path(), partition).expect("open writer");
    writer.append_records(&records).expect("append");
    let written = writer.close().expect("close writer");
    assert_eq!(written.num_rows, 50);

    // Walk the file's column chunks and find the params value
    // leaf by its dotted column path.
    let file = File::open(&written.path).expect("open parquet file");
    let reader = SerializedFileReader::new(file).expect("open parquet reader");
    let metadata = reader.metadata();
    assert!(metadata.num_row_groups() >= 1);

    let mut params_leaves_checked = 0;
    for rg_idx in 0..metadata.num_row_groups() {
        let rg = metadata.row_group(rg_idx);
        for col_idx in 0..rg.num_columns() {
            let col = rg.column(col_idx);
            // Both params LIST<STRUCT<...>> leaves get the §3.6
            // "(list values)" Dictionary = no treatment.
            let path = col.column_path().string();
            if path != "params.list.element.value" && path != "params.list.element.type_tag" {
                continue;
            }
            params_leaves_checked += 1;

            assert!(
                matches!(col.compression(), Compression::ZSTD(_)),
                "{path}: compression must be ZSTD, got {:?}",
                col.compression(),
            );
            let encs = col.encodings();
            assert!(
                !encs.contains(&Encoding::PLAIN_DICTIONARY),
                "{path}: encodings must not include PLAIN_DICTIONARY, got {encs:?}",
            );
            assert!(
                !encs.contains(&Encoding::RLE_DICTIONARY),
                "{path}: encodings must not include RLE_DICTIONARY, got {encs:?}",
            );
            assert!(
                col.dictionary_page_offset().is_none(),
                "{path}: must have no dictionary_page_offset, got {:?}",
                col.dictionary_page_offset(),
            );
        }
    }
    // Expect at least one of each leaf — if zero, the dotted
    // column-path naming convention has changed and the
    // writer's ColumnPath overrides are silently no-op.
    assert!(
        params_leaves_checked >= 2,
        "expected both `params.list.element.value` and \
         `params.list.element.type_tag` column chunks in the file, \
         found {params_leaves_checked}",
    );
}
