//! #1016: each committed partition's compaction metrics carry the
//! partition's tenant, so one tenant's compaction cost can be told from
//! another's.

use std::collections::BTreeMap;
use std::sync::Arc;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use ourios_core::audit::NoOpAuditSink;

use super::sweep_recorded;
use super::tests::{store_at, target, write_sealed_candidate, write_sealed_hours};
use crate::metrics::CompactionMetrics;

/// Per tenant, the summed value of a u64 counter's datapoints, or the
/// summed sample count of a u64 histogram's; datapoints with no tenant are
/// keyed by the empty string. Only datapoints matching `filter` count.
pub(super) fn by_tenant(
    rms: &[ResourceMetrics],
    name: &str,
    filter: Option<(&str, &str)>,
) -> BTreeMap<String, u64> {
    let mut totals = BTreeMap::new();
    let metrics = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == name);
    for metric in metrics {
        let points: Vec<(Vec<opentelemetry::KeyValue>, u64)> = match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|dp| (dp.attributes().cloned().collect(), dp.value()))
                .collect(),
            AggregatedMetrics::U64(MetricData::Histogram(hist)) => hist
                .data_points()
                .map(|dp| (dp.attributes().cloned().collect(), dp.count()))
                .collect(),
            other => panic!("{name} is not a u64 sum or histogram: {other:?}"),
        };
        for (attributes, value) in points {
            let has = |key: &str, value: &str| {
                attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
            };
            if filter.is_some_and(|(key, value)| !has(key, value)) {
                continue;
            }
            let tenant = attributes
                .iter()
                .find(|kv| kv.key.as_str() == ourios_semconv::OURIOS_TENANT)
                .map(|kv| kv.value.as_str().into_owned())
                .unwrap_or_default();
            *totals.entry(tenant).or_default() += value;
        }
    }
    totals
}

pub(super) fn tenants(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs.iter().map(|&(t, v)| (t.to_owned(), v)).collect()
}

/// A sweep over tenant `a` (three two-file partitions) and `b` (one)
/// attributes `partitions`, `files`, `rows`, `io` and both
/// `partition.input.*` histograms per tenant. The only untagged datapoints
/// are the zero observations no tenant owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_partitions_compaction_metrics_carry_its_tenant() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, 3);
    write_sealed_candidate(&store, "b");
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));

    // Act
    let (result, _, _) =
        sweep_recorded(target(&store), Box::new(NoOpAuditSink), Some(metrics)).await;
    provider.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");

    // Assert
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 4, "{report:?}");
    // `partitions`, `files` and `rows` keep their untagged zero seed, the
    // observation no single tenant owns.
    let name = |n| by_tenant(&rms, n, None);
    assert_eq!(
        name(ourios_semconv::OURIOS_COMPACTION_PARTITIONS),
        tenants(&[("", 0), ("a", 3), ("b", 1)])
    );
    assert_eq!(
        name(ourios_semconv::OURIOS_COMPACTION_FILES),
        tenants(&[("", 0), ("a", 6), ("b", 2)])
    );
    assert_eq!(
        name(ourios_semconv::OURIOS_COMPACTION_PARTITION_INPUT_FILES),
        tenants(&[("a", 3), ("b", 1)]),
        "one input-files sample per partition"
    );
    let rows = name(ourios_semconv::OURIOS_COMPACTION_ROWS);
    assert_eq!(rows.keys().collect::<Vec<_>>(), ["", "a", "b"]);
    assert!(rows["a"] > 0 && rows["b"] > 0, "{rows:?}");
    assert_eq!(rows.values().sum::<u64>(), report.rows_compacted);
    let kind = ourios_semconv::OURIOS_COMPACTION_SIZE_KIND;
    for size_kind in ["encoded", "decoded_estimate"] {
        assert_eq!(
            by_tenant(
                &rms,
                ourios_semconv::OURIOS_COMPACTION_PARTITION_INPUT_SIZE,
                Some((kind, size_kind)),
            ),
            tenants(&[("a", 3), ("b", 1)]),
            "{size_kind}: one sample per partition"
        );
    }
    let io = ourios_semconv::OURIOS_IO_DIRECTION;
    let read = by_tenant(
        &rms,
        ourios_semconv::OURIOS_COMPACTION_IO,
        Some((io, "read")),
    );
    assert_eq!(read.get("").copied().unwrap_or(0), 0, "no untagged bytes");
    assert!(read["a"] > 0 && read["b"] > 0, "{read:?}");
    assert_eq!(read.values().sum::<u64>(), report.bytes_read);
}

/// Write `records`, each with an ingest position, as one committed file.
fn write_positioned(store: &ourios_parquet::Store, records: &[ourios_core::record::MinedRecord]) {
    let partition = ourios_parquet::PartitionKey::derive(&records[0]).expect("derive");
    let mut w = ourios_parquet::Writer::open_in_with_ingest_positions(
        store,
        partition,
        ourios_parquet::DEFAULT_ZSTD_LEVEL,
        ourios_parquet::PromotedAttributes::default(),
    )
    .expect("open writer");
    w.append_records(records).expect("append");
    w.close().expect("close");
}

/// RFC 0065 §6.6: the rows a compaction drops for repeating an ingest
/// position are counted per tenant on `ourios.compaction.row.deduplicated`,
/// and a partition without positions counts none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_partitions_deduplicated_rows_carry_its_tenant() {
    use super::tests::{TS0, rec};
    use ourios_core::record::IngestPosition;

    // Arrange: tenant `a` holds two flushers' copies of two rows, tenant
    // `b` a local-backend candidate.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let flushed: Vec<_> = (0..2)
        .map(|index| ourios_core::record::MinedRecord {
            ingest_position: Some(IngestPosition {
                group: 3,
                seq: 40,
                index,
            }),
            ..rec("a", 1, TS0 + u64::from(index))
        })
        .collect();
    write_positioned(&store, &flushed);
    write_positioned(&store, &flushed);
    write_sealed_candidate(&store, "b");
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));

    // Act
    let (result, _, _) =
        sweep_recorded(target(&store), Box::new(NoOpAuditSink), Some(metrics)).await;
    provider.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");

    // Assert
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 2, "{report:?}");
    assert_eq!(
        by_tenant(
            &rms,
            ourios_semconv::OURIOS_COMPACTION_ROW_DEDUPLICATED,
            None
        ),
        tenants(&[("a", 2), ("b", 0)])
    );
    assert_eq!(
        by_tenant(&rms, ourios_semconv::OURIOS_COMPACTION_ROWS, None),
        tenants(&[("", 0), ("a", 2), ("b", 2)]),
        "the repeats are not counted as rows rewritten"
    );
}
