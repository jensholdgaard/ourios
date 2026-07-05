//! RFC 0024 §5 — the storage-owned pipeline property: P1 (round-trip
//! fidelity, `.3`) over generated OTLP batches from `ourios-testgen`.
//! See `crates/ourios-bench/tests/rfc0024_calibration.rs` for the
//! scenario placement map.
//!
//! Batches enter through the real miner (`MinerCluster` with a
//! `SharedRecordSink`) so the writer sees production-shaped rows,
//! then write → read through `Writer` / `Reader` per partition and
//! assert the RFC 0017/0018 fidelity contract on what comes back:
//! full `MinedRecord` struct equality (every §3.2 column), plus
//! body fidelity at the OTLP level — string bodies reconstruct
//! bit-identically, structured bodies decode canonical-JSON equal.

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord, canonical};
use ourios_core::record::{MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::reconstruct::reconstruct;
use ourios_miner::tree::OwnedToken;
use ourios_parquet::{BatchError, PartitionKey, Reader, Writer, mined_records_to_batch};
use ourios_testgen::manifest::{AnyValueShapes, BodyKindMix, CalibrationManifest, SeverityBucket};
use ourios_testgen::strategies;
use proptest::prelude::*;
use tempfile::TempDir;

/// Same hand-shaped manifest as the miner-side suite, so the
/// calibrated arm draws a realistic severity / body / attribute mix.
fn synthetic_manifest() -> CalibrationManifest {
    CalibrationManifest {
        corpus_tag: "rfc0024-synthetic".to_string(),
        records: 100,
        log_attribute_count: [(0, 30), (1, 40), (3, 30)].into_iter().collect(),
        resource_attribute_count: [(2, 100)].into_iter().collect(),
        body_kind: BodyKindMix {
            string: 85,
            structured: 10,
            absent: 5,
        },
        string_body_len: [(4, 20), (5, 50), (6, 30)].into_iter().collect(),
        severity: vec![
            SeverityBucket {
                number: 9,
                text: Some("INFO".to_string()),
                count: 70,
            },
            SeverityBucket {
                number: 13,
                text: Some("WARN".to_string()),
                count: 20,
            },
            SeverityBucket {
                number: 17,
                text: Some("ERROR".to_string()),
                count: 10,
            },
        ],
        any_value_shapes: AnyValueShapes {
            string: 60,
            int: 20,
            double: 5,
            boolean: 5,
            array: 5,
            kvlist: 5,
            ..Default::default()
        },
        any_value_max_depth: 3,
        distinct_attribute_keys: 12,
    }
}

fn fail(what: &str, e: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(format!("{what}: {e}"))
}

/// Mine `batch`, store every writable row, read it back, and assert
/// fidelity. Adversarial timestamps can exceed `i64::MAX`, which the
/// writer *rejects by contract* (RFC 0005 §3.2 timestamp overflow) —
/// those rows are asserted rejected, everything else must round-trip.
fn assert_round_trip(batch: &[OtlpLogRecord]) -> Result<(), TestCaseError> {
    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    for record in batch {
        cluster.ingest(record);
    }
    let emitted = sink.drain();
    prop_assert_eq!(emitted.len(), batch.len(), "one emitted row per record");

    // Group writable rows by partition, remembering each row's batch
    // index so read-back rows pair with their originals.
    // The writer's two documented loud rejections. Everything else
    // must round-trip.
    let fits_i64 = |t: u64| i64::try_from(t).is_ok();
    let mut groups: Vec<(PartitionKey, Vec<(usize, MinedRecord)>)> = Vec::new();
    let mut writable = vec![false; batch.len()];
    for (i, mined) in emitted.into_iter().enumerate() {
        let absent_body = mined.body_kind == ourios_core::record::BodyKind::Absent;
        let ts_overflow = !fits_i64(mined.time_unix_nano)
            || mined.observed_time_unix_nano.is_some_and(|t| !fits_i64(t));
        if absent_body || ts_overflow {
            // - Absent body: KNOWN GAP (#362, found by this suite) —
            //   no §3.2 on-disk representation until the RFC 0005
            //   amendment lands; this arm then turns into a
            //   round-trip.
            // - Timestamp overflow: the §3.2 u64→i64 contract, on
            //   *either* timestamp column.
            // Both must be loud rejections, never silent drops.
            let err = mined_records_to_batch(std::slice::from_ref(&mined))
                .expect_err("the writer must reject this record, not silently map it");
            prop_assert!(
                matches!(
                    err,
                    BatchError::UnsupportedAbsentBody | BatchError::TimestampOverflow { .. }
                ),
                "record {}: rejected for an undocumented reason: {}",
                i,
                err
            );
            continue;
        }
        let key =
            PartitionKey::derive(&mined).map_err(|e| fail(&format!("record {i}: derive"), e))?;
        writable[i] = true;
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, rows)) => rows.push((i, mined)),
            None => groups.push((key, vec![(i, mined)])),
        }
    }

    let bucket = TempDir::new().map_err(|e| fail("temp dir", e))?;
    let mut read_back: Vec<Option<MinedRecord>> = vec![None; batch.len()];
    for (key, rows) in groups {
        let originals: Vec<MinedRecord> = rows.iter().map(|(_, r)| r.clone()).collect();
        let mut writer =
            Writer::open(bucket.path(), key.clone()).map_err(|e| fail("open writer", e))?;
        writer
            .append_records(&originals)
            .map_err(|e| fail("append", e))?;
        let written = writer.close().map_err(|e| fail("close", e))?;
        let reader =
            Reader::open_partition(&written.path, key).map_err(|e| fail("open_partition", e))?;
        let round_tripped = reader.read_all().map_err(|e| fail("read_all", e))?;

        // Storage-layer fidelity: every §3.2 column, struct equality.
        prop_assert_eq!(
            &round_tripped,
            &originals,
            "partition rows must round-trip exactly"
        );
        for ((i, _), row) in rows.into_iter().zip(round_tripped) {
            read_back[i] = Some(row);
        }
    }

    // OTLP-level fidelity on the rows that came back from disk.
    let tenant = TenantId::new(strategies::TESTGEN_TENANT);
    let templates: HashMap<u64, Vec<OwnedToken>> = cluster
        .templates_for(&tenant)
        .into_iter()
        .map(|leaf| (leaf.template_id, leaf.template))
        .collect();
    let no_template: Vec<OwnedToken> = Vec::new();

    for (i, original) in batch.iter().enumerate() {
        if !writable[i] {
            continue;
        }
        let mined = read_back[i]
            .as_ref()
            .unwrap_or_else(|| panic!("record {i} was written but never read back"));
        let template = templates.get(&mined.template_id).unwrap_or(&no_template);
        let rebuilt = reconstruct(mined, template);
        match &original.body {
            Some(Body::String(s)) => prop_assert_eq!(
                rebuilt.as_slice(),
                s.as_bytes(),
                "record {}: string body must survive storage bit-identically",
                i
            ),
            Some(Body::Structured(av)) => {
                let back = canonical::decode_any_value(&rebuilt)
                    .map_err(|e| fail(&format!("record {i}: structured body decode"), e))?;
                prop_assert_eq!(
                    &back,
                    av,
                    "record {}: structured body must survive storage canonical-JSON equal",
                    i
                );
            }
            None => prop_assert!(rebuilt.is_empty(), "record {}: absent body stays absent", i),
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// Scenario RFC0024.3 — P1: round-trip fidelity over generated
    /// batches, both modes.
    /// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
    #[test]
    fn rfc0024_3_round_trip_fidelity_over_generated_batches(
        adversarial in proptest::collection::vec(strategies::adversarial(), 1..10),
        calibrated in proptest::collection::vec(
            strategies::calibrated(&synthetic_manifest()), 1..10),
    ) {
        assert_round_trip(&adversarial)?;
        assert_round_trip(&calibrated)?;
    }
}
