//! Ingest write-path wall-clock benches (`CLAUDE.md` §6.2 hot paths:
//! OTLP → WAL, WAL → Parquet). Supportive, **indicative** evidence — run on
//! the CI runner first (see the `bench-on-ci-runner-first` discipline); the
//! authoritative baseline-hardware numbers are a separate, opt-in run.
//!
//! - `wal_append` — the OTLP → WAL half: append one batch frame and fsync it
//!   (the WAL-before-ack unit; one batch → one frame → one sync). Mirrors the
//!   `ourios.wal.append.duration` metric. Throughput is reported in bytes.
//! - `sink_write/{N}` — the WAL → Parquet half: feed `N` mined records to a
//!   `ParquetRecordSink` and flush them to one Parquet object on a
//!   `LocalFileSystem` store (encode + put). Mirrors `ourios.sink.flush.*`.
//!   Throughput is reported in records.
//!
//! Both rebuild their fixture in the (untimed) `iter_batched` setup so only
//! the durable append / the emit + flush is measured.

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink};
use ourios_parquet::Store;
use ourios_wal::{FrameKind, Wal, WalConfig};

/// A representative ack unit: a single batch frame (~one small OTLP export).
const FRAME_LEN: usize = 4 * 1024;
/// Record counts for the sink-flush throughput sweep.
const SINK_RECORDS: [usize; 2] = [1_000, 10_000];

fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// A clean mined record whose varying param keeps the column non-degenerate
/// (so encode/compression is representative, not a trivial all-equal run).
fn rec(i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("bench"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.bench".to_string()),
        scope_version: Some("1.0.0".to_string()),
        time_unix_nano: 1_775_127_480_000_000_000 + i * 1_000,
        observed_time_unix_nano: Some(1_775_127_480_000_000_000 + i * 1_000 + 1),
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
            value: format!("{i}"),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// OTLP → WAL: append one frame + fsync (the durable-ack unit).
fn wal_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_append");
    group.throughput(Throughput::Bytes(FRAME_LEN as u64));
    let payload = vec![0xA5u8; FRAME_LEN];
    group.bench_function("batch", |b| {
        b.iter_batched(
            || {
                // Keep the `TempDir` alive alongside the `Wal` — dropping it
                // would delete the segment directory mid-measurement. Warm up
                // with one append + sync: the *first* sync on a fresh WAL also
                // fsyncs the segment directory (entry durability), which
                // steady-state syncs don't, so doing it here (untimed) isolates
                // the steady-state per-batch append + fsync cost.
                let dir = tempfile::TempDir::new().expect("temp");
                let mut wal = Wal::open(wal_config(dir.path())).expect("open");
                wal.append(FrameKind::OtlpBatch, &payload)
                    .expect("warm append");
                wal.sync().expect("warm sync");
                (dir, wal)
            },
            |(_dir, mut wal)| {
                wal.append(FrameKind::OtlpBatch, &payload).expect("append");
                black_box(wal.sync().expect("sync"));
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

/// WAL → Parquet: feed `N` records to the sink and flush to one object.
fn sink_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("sink_write");
    for n in SINK_RECORDS {
        let records: Vec<MinedRecord> = (0..n as u64).map(rec).collect();
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &records, |b, records| {
            b.iter_batched(
                || {
                    let dir = tempfile::TempDir::new().expect("temp");
                    // Size never triggers mid-stream: emit the whole batch,
                    // then time one explicit flush of the full partition.
                    let sink = ParquetRecordSink::new(
                        Store::local(dir.path()).expect("store"),
                        FlushConfig {
                            target_bytes: usize::MAX,
                            max_buffer_age: Duration::from_secs(86_400),
                            ceiling_bytes: usize::MAX,
                        },
                    );
                    // Clone the batch in the untimed setup — production `emit`
                    // takes owned records, so the timed routine emits by value
                    // (no clone cost polluting the WAL→Parquet signal).
                    (dir, sink, records.clone())
                },
                |(_dir, mut sink, batch)| {
                    for r in batch {
                        sink.emit(r);
                    }
                    sink.flush_all();
                    black_box(sink.flushes());
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, wal_append, sink_write);
criterion_main!(benches);
