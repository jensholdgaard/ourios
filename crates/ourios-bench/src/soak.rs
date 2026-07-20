//! RFC 0009 D1/D2 sustained-ingest soak harness (`docs/benchmarks.md`
//! §D1 / §D2).
//!
//! Drives the **real** ingest path in-process — OTLP export →
//! [`IngestPipeline`] → group-commit WAL fsync (100 ms window,
//! `CLAUDE.md` §3.4) → miner → Parquet record sink on a local [`Store`]
//! over a tempdir — at a paced target rate, while a sampler interleaves
//! manual compaction sweeps ([`run_sweep`]) and records the WAL +
//! compaction backlog over time.
//!
//! # Synthetic clock
//!
//! Compaction only acts on **sealed** partitions: an hour partition is
//! sealed once its hour has ended plus a grace period
//! ([`plan_candidates`]' seal rule) — *time-driven* logic, so a
//! wall-clock soak would wait real hours to see one
//! seal → sweep → compact cycle. The harness therefore runs a
//! **synthetic clock**: record timestamps advance on a compressed
//! timeline ([`SoakConfig::time_compression`] synthetic seconds per wall
//! second; the default 60 makes one wall-minute of load one synthetic
//! hour), and the *same* synthetic now feeds [`run_sweep`] /
//! [`plan_candidates`]. The sealing logic runs unmodified — only the
//! timestamps it compares are compressed — so the real
//! seal → sweep → compact path is exercised without waiting real hours.
//! The D1 ack measurement is unaffected: throughput and ack latency are
//! measured in real wall-clock time; the synthetic clock only stamps
//! record timestamps and drives the compaction cadence.
//!
//! Ack latency is measured at the [`IngestPipeline::ingest`] boundary —
//! the elapsed time until the caller could ack, i.e. the group-commit
//! wait plus the in-order miner hand-off. That is a conservative upper
//! bound on the §D1 WAL-commit latency, and exactly what an OTLP client
//! observes.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_ingester::compactor::run_sweep;
use ourios_ingester::receiver::pipeline::{Journal, ReceiveError};
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, TenantRule};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{CompactionPolicy, PartitionKey, Store, plan_candidates};
use ourios_wal::{Wal, WalConfig, WalOffset};
use serde::Serialize;

/// §D1 throughput bar: sustained acked lines per second per core.
pub const D1_LINES_PER_SEC_PER_CORE: u64 = 100_000;
/// §D1 latency bar: p99 ingest-ack latency in milliseconds.
pub const D1_P99_ACK_MS: u64 = 200;

/// The single soak tenant (derived from `service.name` by the production
/// [`TenantRule`], so the batches exercise the real fan-out).
const TENANT: &str = "soak";
/// 2026-01-01T00:00:00Z — the synthetic timeline's fixed origin, so
/// partition keys are deterministic across runs.
const BASE_UNIX_NANOS: u64 = 1_767_225_600 * NANOS_PER_SEC;
const NANOS_PER_SEC: u64 = 1_000_000_000;
const HOUR_NANOS: u64 = 3_600 * NANOS_PER_SEC;
/// §D1's WAL knob: fsync batched at 100 ms.
const WAL_BATCH_WINDOW_MS: u64 = 100;
const WAL_SEGMENT_BYTES: u64 = 128 * 1024 * 1024;
/// Bounds in-flight ingest tasks. Well above the steady-state depth
/// (~rate × ack latency); reaching it means ingest is falling behind,
/// and the pacing loop then degrades — which the achieved rate records.
const MAX_IN_FLIGHT: usize = 512;

/// Tunables for one soak run. Serialized into the report so a results
/// file is self-describing.
#[derive(Debug, Clone, Serialize)]
pub struct SoakConfig {
    /// Load duration in wall seconds.
    pub duration_secs: u64,
    /// Paced target ingest rate (lines per second, whole process).
    pub target_lines_per_sec: u64,
    /// Log records per OTLP export batch.
    pub batch_size: usize,
    /// Synthetic seconds per wall second (see the module doc). The
    /// default 60 makes one wall-minute of load one synthetic hour.
    pub time_compression: u64,
    /// Wall seconds between backlog samples (each sample also flushes
    /// the record sink and runs one compaction sweep).
    pub sample_every_secs: u64,
    /// Record-sink flush threshold. Deliberately small so sustained
    /// ingest produces the multi-file partitions compaction exists to
    /// consolidate (hazard #4).
    pub sink_target_bytes: usize,
    /// Seed for the deterministic batch generator.
    pub seed: u64,
    /// Tokio worker threads for the load runtime — also the divisor for
    /// the §D1 per-core rate.
    pub worker_threads: usize,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            duration_secs: 3_600,
            target_lines_per_sec: 100_000,
            batch_size: 1_000,
            time_compression: 60,
            sample_every_secs: 10,
            sink_target_bytes: 4 * 1024 * 1024,
            seed: 0xD1D2_50AC,
            worker_threads: default_worker_threads(),
        }
    }
}

/// Default [`SoakConfig::worker_threads`]: the host's parallelism.
#[must_use]
pub fn default_worker_threads() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
}

/// One backlog observation: candidate state *before* that sample's
/// sweep, what the sweep then compacted, and the WAL/sink gauges.
#[derive(Debug, Clone, Serialize)]
pub struct BacklogSample {
    /// Wall seconds since load start.
    pub wall_secs: f64,
    /// The synthetic now the sweep ran at.
    pub synthetic_unix_nanos: u64,
    /// Sealed candidate partitions pending before the sweep.
    pub backlog_partitions: usize,
    /// Total bytes of the candidate partitions' Parquet files.
    pub backlog_bytes: u64,
    /// Partitions the sweep consolidated.
    pub partitions_compacted: usize,
    /// Planning/sweep/listing errors at this sample (0 on a clean pass).
    pub errors: usize,
    /// WAL segment count at sample time.
    pub wal_segments: u32,
    /// WAL on-disk bytes at sample time.
    pub wal_disk_bytes: u64,
    /// Records still buffered in the sink after the sample's flush.
    pub sink_buffered_records: usize,
}

/// Ack-latency percentiles over the whole run, in milliseconds.
#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// §D1 verdict with its exact bars.
#[derive(Debug, Clone, Serialize)]
pub struct D1Verdict {
    /// `per_core_lines_per_sec >= bar_lines_per_sec_per_core &&
    /// p99_ack_ms <= bar_p99_ack_ms`.
    pub pass: bool,
    pub per_core_lines_per_sec: f64,
    pub bar_lines_per_sec_per_core: u64,
    pub p99_ack_ms: f64,
    pub bar_p99_ack_ms: u64,
}

/// §D2 verdict: the backlog stayed bounded and drained.
#[derive(Debug, Clone, Serialize)]
pub struct D2Verdict {
    /// `final_backlog_partitions == 0 && returned_to_zero_after_max`.
    pub pass: bool,
    pub max_backlog_partitions: usize,
    /// Candidates still pending after the final drain sweep.
    pub final_backlog_partitions: usize,
    /// The backlog series hit zero at least once after its maximum
    /// (trivially true when it never rose above zero).
    pub returned_to_zero_after_max: bool,
}

/// The soak run's full result — serialized as the `--out` JSON.
#[derive(Debug, Clone, Serialize)]
pub struct SoakReport {
    pub config: SoakConfig,
    /// Wall seconds from first batch to last ack (includes the drain of
    /// in-flight batches).
    pub wall_secs: f64,
    pub lines_acked: u64,
    pub batches_acked: u64,
    pub batches_failed: u64,
    pub achieved_lines_per_sec: f64,
    /// `achieved_lines_per_sec / worker_threads` — the §D1 per-core rate.
    pub per_core_lines_per_sec: f64,
    pub latency: LatencySummary,
    pub samples: Vec<BacklogSample>,
    pub total_partitions_compacted: usize,
    pub d1: D1Verdict,
    pub d2: D2Verdict,
}

/// Failure to set up or drive a soak run (load-side ingest errors are
/// *counted* in the report, not raised — a soak measures, it doesn't
/// abort on a failed batch).
#[derive(Debug)]
pub enum SoakError {
    /// A config field is out of range. Names the constraint.
    Config(&'static str),
    /// Building the tokio runtime failed.
    Runtime(std::io::Error),
    /// Opening the WAL / store fixture failed.
    Setup(String),
}

impl std::fmt::Display for SoakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(detail) => write!(f, "soak config: {detail}"),
            Self::Runtime(e) => write!(f, "soak runtime: {e}"),
            Self::Setup(detail) => write!(f, "soak setup: {detail}"),
        }
    }
}

impl std::error::Error for SoakError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(e) => Some(e),
            Self::Config(_) | Self::Setup(_) => None,
        }
    }
}

/// Run one soak to completion and return the report.
///
/// Builds a **multi-thread** tokio runtime: the group-commit
/// coordinator offloads its fsync via `spawn_blocking`, so a
/// current-thread runtime would serialize every fsync behind the load
/// loop and depress the D1 numbers.
///
/// # Errors
///
/// [`SoakError`] on an invalid config or a setup failure (runtime, WAL,
/// store). Ingest and sweep failures during the run are counted in the
/// report instead.
pub fn run_soak(config: &SoakConfig) -> Result<SoakReport, SoakError> {
    validate(config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_threads)
        .enable_time()
        .build()
        .map_err(SoakError::Runtime)?;
    runtime.block_on(soak(config))
}

fn validate(config: &SoakConfig) -> Result<(), SoakError> {
    if config.duration_secs == 0 {
        return Err(SoakError::Config("duration_secs must be > 0"));
    }
    if config.target_lines_per_sec == 0 {
        return Err(SoakError::Config("target_lines_per_sec must be > 0"));
    }
    if config.batch_size == 0 {
        return Err(SoakError::Config("batch_size must be > 0"));
    }
    if config.time_compression == 0 {
        return Err(SoakError::Config("time_compression must be > 0"));
    }
    if config.sample_every_secs == 0 {
        return Err(SoakError::Config("sample_every_secs must be > 0"));
    }
    if config.sink_target_bytes == 0 {
        return Err(SoakError::Config("sink_target_bytes must be > 0"));
    }
    if config.worker_threads == 0 {
        return Err(SoakError::Config("worker_threads must be > 0"));
    }
    Ok(())
}

async fn soak(config: &SoakConfig) -> Result<SoakReport, SoakError> {
    let Some(pace) = batch_interval(config.target_lines_per_sec, config.batch_size) else {
        return Err(SoakError::Config("target rate and batch size must be > 0"));
    };
    let dir =
        tempfile::tempdir().map_err(|e| SoakError::Setup(format!("create soak tempdir: {e}")))?;
    let wal_root = dir.path().join("wal");
    let store_root = dir.path().join("store");
    for path in [&wal_root, &store_root] {
        std::fs::create_dir_all(path)
            .map_err(|e| SoakError::Setup(format!("create {}: {e}", path.display())))?;
    }

    // Mirror the server's construction (`ourios-server` `serve`): WAL →
    // group-commit coordinator → miner (record sink attached) →
    // pipeline. `macos_full_fsync: false` matches the write-path bench.
    // The WAL is shared behind a mutex so the sampler can read
    // `Wal::metrics` while the coordinator owns the append/sync path.
    let wal = Wal::open(WalConfig {
        root: wal_root,
        batch_window_ms: WAL_BATCH_WINDOW_MS,
        segment_size_bytes: WAL_SEGMENT_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    })
    .map_err(|e| SoakError::Setup(format!("open WAL: {e:?}")))?;
    let wal = Arc::new(Mutex::new(wal));
    let coordinator = CommitCoordinator::new(
        Box::new(SharedWal(Arc::clone(&wal))),
        Duration::from_millis(WAL_BATCH_WINDOW_MS),
        WAL_SEGMENT_BYTES,
    );
    let store =
        Store::local(&store_root).map_err(|e| SoakError::Setup(format!("open store: {e}")))?;
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        store.clone(),
        FlushConfig {
            target_bytes: config.sink_target_bytes,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        },
    ));
    let miner = MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let pipeline = Arc::new(IngestPipeline::new(
        coordinator,
        miner,
        TenantRule::service_name(),
    ));

    let clock = SyntheticClock {
        started: Instant::now(),
        compression: config.time_compression,
    };
    let policy = CompactionPolicy::default();
    let stats = Arc::new(LoadStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let sampler_task = tokio::spawn(run_sampler(SamplerCtx {
        store: store.clone(),
        sink: sink.clone(),
        wal: Arc::clone(&wal),
        clock: clock.clone(),
        policy,
        every: Duration::from_secs(config.sample_every_secs),
        stop: Arc::clone(&stop),
    }));

    run_load(&pipeline, &clock, config, &stats, pace).await;
    let wall_secs = as_f64(saturating_u64(clock.started.elapsed().as_millis())) / 1_000.0;

    stop.store(true, Ordering::Relaxed);
    // A sampler panic loses the timeseries but not the run; report with
    // no samples (d2 then fails loudly on the missing final backlog).
    let mut samples: Vec<BacklogSample> = sampler_task.await.unwrap_or_default();

    // Final drain: advance the synthetic clock past the last partition's
    // hour end + grace so *every* partition seals, then sweep. This
    // verifies compaction can fully clear the backlog the run produced;
    // the mid-run samples carry the steady-state D2 evidence.
    let drain_now = clock
        .now_unix_nanos()
        .saturating_add(HOUR_NANOS)
        .saturating_add(policy.grace_nanos)
        .saturating_add(1);
    let drain_wall = as_f64(saturating_u64(clock.started.elapsed().as_millis())) / 1_000.0;
    let (drain_sample, final_backlog) = {
        let store = store.clone();
        let sink = sink.clone();
        let wal = Arc::clone(&wal);
        match tokio::task::spawn_blocking(move || {
            let sample = blocking_sample(&store, &sink, &wal, &policy, drain_wall, drain_now);
            // An error listing here means the final backlog is unknown —
            // fail the D2 verdict loudly rather than claim a drained run.
            let final_backlog =
                plan_candidates(&store, TENANT, drain_now, &policy).map_or(usize::MAX, |c| c.len());
            (sample, final_backlog)
        })
        .await
        {
            Ok(result) => result,
            Err(_) => (error_sample(drain_wall, drain_now), usize::MAX),
        }
    };
    samples.push(drain_sample);

    Ok(build_report(
        config,
        wall_secs,
        &stats,
        samples,
        final_backlog,
    ))
}

fn build_report(
    config: &SoakConfig,
    wall_secs: f64,
    stats: &LoadStats,
    samples: Vec<BacklogSample>,
    final_backlog: usize,
) -> SoakReport {
    let mut latencies = stats
        .latencies_us
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    latencies.sort_unstable();
    let latency = LatencySummary {
        p50_ms: us_to_ms(percentile_us(&latencies, 50, 100)),
        p95_ms: us_to_ms(percentile_us(&latencies, 95, 100)),
        p99_ms: us_to_ms(percentile_us(&latencies, 99, 100)),
        max_ms: us_to_ms(latencies.last().copied().unwrap_or(0)),
    };
    drop(latencies);

    let lines_acked = stats.lines_acked.load(Ordering::Relaxed);
    let achieved = if wall_secs > 0.0 {
        as_f64(lines_acked) / wall_secs
    } else {
        0.0
    };
    let per_core = achieved / as_f64(to_u64(config.worker_threads));
    let backlog_series: Vec<usize> = samples.iter().map(|s| s.backlog_partitions).collect();
    let total_compacted = samples.iter().map(|s| s.partitions_compacted).sum();

    let p99_ms = latency.p99_ms;
    SoakReport {
        config: config.clone(),
        wall_secs,
        lines_acked,
        batches_acked: stats.batches_acked.load(Ordering::Relaxed),
        batches_failed: stats.batches_failed.load(Ordering::Relaxed),
        achieved_lines_per_sec: achieved,
        per_core_lines_per_sec: per_core,
        latency,
        samples,
        total_partitions_compacted: total_compacted,
        d1: d1_verdict(per_core, p99_ms),
        d2: d2_verdict(&backlog_series, final_backlog),
    }
}

/// The §D1 verdict over the measured per-core rate and p99 ack latency.
#[must_use]
pub fn d1_verdict(per_core_lines_per_sec: f64, p99_ack_ms: f64) -> D1Verdict {
    D1Verdict {
        pass: per_core_lines_per_sec >= as_f64(D1_LINES_PER_SEC_PER_CORE)
            && p99_ack_ms <= as_f64(D1_P99_ACK_MS),
        per_core_lines_per_sec,
        bar_lines_per_sec_per_core: D1_LINES_PER_SEC_PER_CORE,
        p99_ack_ms,
        bar_p99_ack_ms: D1_P99_ACK_MS,
    }
}

/// The §D2 verdict over the per-sample backlog series and the
/// post-drain-sweep final backlog.
#[must_use]
pub fn d2_verdict(backlog_series: &[usize], final_backlog: usize) -> D2Verdict {
    let max = backlog_series.iter().copied().max().unwrap_or(0);
    let returned = if max == 0 {
        // The backlog never accumulated; nothing to return from.
        final_backlog == 0
    } else {
        let after_max = backlog_series
            .iter()
            .position(|&b| b == max)
            .map_or(&[][..], |i| &backlog_series[i + 1..]);
        after_max.contains(&0) || final_backlog == 0
    };
    D2Verdict {
        pass: final_backlog == 0 && returned,
        max_backlog_partitions: max,
        final_backlog_partitions: final_backlog,
        returned_to_zero_after_max: returned,
    }
}

/// The pacing interval: one `batch_size`-line batch per tick sustains
/// `target_lines_per_sec`. `None` when either input is zero. Floors at
/// 1 ns (tokio's interval rejects zero).
#[must_use]
pub fn batch_interval(target_lines_per_sec: u64, batch_size: usize) -> Option<Duration> {
    if target_lines_per_sec == 0 || batch_size == 0 {
        return None;
    }
    let nanos = u128::from(to_u64(batch_size)) * u128::from(NANOS_PER_SEC)
        / u128::from(target_lines_per_sec);
    Some(Duration::from_nanos(saturating_u64(nanos).max(1)))
}

/// Nearest-rank percentile (`num`/`den`, e.g. 99/100 for p99) over an
/// ascending-sorted slice. `0` for an empty slice.
#[must_use]
pub fn percentile_us(sorted: &[u64], num: usize, den: usize) -> u64 {
    if sorted.is_empty() || den == 0 {
        return 0;
    }
    let rank = (sorted.len() * num).div_ceil(den).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

// ----------------------------------------------------------------------
// Load generation
// ----------------------------------------------------------------------

#[derive(Default)]
struct LoadStats {
    latencies_us: Mutex<Vec<u64>>,
    lines_acked: AtomicU64,
    batches_acked: AtomicU64,
    batches_failed: AtomicU64,
}

async fn run_load(
    pipeline: &Arc<IngestPipeline>,
    clock: &SyntheticClock,
    config: &SoakConfig,
    stats: &Arc<LoadStats>,
    pace: Duration,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let mut ticker = tokio::time::interval(pace);
    let deadline = clock.started + Duration::from_secs(config.duration_secs);
    let mut batch_idx = 0u64;
    while Instant::now() < deadline {
        ticker.tick().await;
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            break; // Closed semaphore: cannot happen while we hold it.
        };
        let pipeline = Arc::clone(pipeline);
        let stats = Arc::clone(stats);
        let batch = build_batch(
            config.seed,
            batch_idx,
            config.batch_size,
            clock.now_unix_nanos(),
        );
        tokio::spawn(async move {
            let started = Instant::now();
            match pipeline.ingest(batch).await {
                Ok(lines) => {
                    let elapsed_us = saturating_u64(started.elapsed().as_micros());
                    stats
                        .lines_acked
                        .fetch_add(to_u64(lines), Ordering::Relaxed);
                    stats.batches_acked.fetch_add(1, Ordering::Relaxed);
                    stats
                        .latencies_us
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(elapsed_us);
                }
                Err(_) => {
                    stats.batches_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            drop(permit);
        });
        batch_idx += 1;
    }
    // Drain: once every permit is reacquirable, no batch is in flight.
    let permits = u32::try_from(MAX_IN_FLIGHT).unwrap_or(u32::MAX);
    drop(semaphore.acquire_many(permits).await);
}

/// A printf-style template: fixed text segments with one generated
/// numeric parameter between each adjacent pair.
struct TemplateSpec {
    severity_number: i32,
    severity_text: &'static str,
    segments: &'static [&'static str],
}

/// A small fixed template mix (bodies ~100–200 bytes) so the miner
/// converges to a stable template set while the params vary — the
/// workload shape the D-gates assume.
const TEMPLATES: &[TemplateSpec] = &[
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "GET /api/v1/orders/",
            " completed in ",
            " ms with status ",
            " for client session ",
            " carrying a paged response body",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "POST /api/v1/checkout accepted cart ",
            " with ",
            " items totalling ",
            " cents; reserved inventory across ",
            " warehouse shards",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "connection pool lease granted for backend ",
            " after ",
            " us of queueing (",
            " of ",
            " connections currently busy)",
        ],
    },
    TemplateSpec {
        severity_number: 5,
        severity_text: "DEBUG",
        segments: &[
            "cache lookup for key shard ",
            " resolved in ",
            " us: generation ",
            " hit ratio holding near expectations for the window",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "user ",
            " authenticated from device ",
            " in region ",
            "; session token minted with standard expiry policy applied",
        ],
    },
    TemplateSpec {
        severity_number: 13,
        severity_text: "WARN",
        segments: &[
            "retrying upstream call to inventory service, attempt ",
            " of ",
            " after ",
            " ms backoff; circuit breaker remains closed for now",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "scheduled reconciliation batch ",
            " processed ",
            " ledger entries in ",
            " ms with zero divergent balances detected",
        ],
    },
    TemplateSpec {
        severity_number: 17,
        severity_text: "ERROR",
        segments: &[
            "payment authorization ",
            " declined by processor with code ",
            " after ",
            " ms; customer notified through the standard fallback flow",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "grpc stream ",
            " closed cleanly after ",
            " messages and ",
            " bytes; peer acknowledged final frame without truncation",
        ],
    },
    TemplateSpec {
        severity_number: 13,
        severity_text: "WARN",
        segments: &[
            "queue depth for topic partition ",
            " reached ",
            " messages (threshold ",
            "); consumer group lag is being monitored closely",
        ],
    },
    TemplateSpec {
        severity_number: 5,
        severity_text: "DEBUG",
        segments: &[
            "compacted segment ",
            " merged ",
            " entries into ",
            " blocks; tombstone ratio stayed inside the configured budget",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "DELETE /api/v1/sessions/",
            " completed in ",
            " ms; ",
            " downstream cache invalidations fanned out successfully",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "search query shard ",
            " returned ",
            " hits in ",
            " ms with ranking pass applied under the default profile",
        ],
    },
    TemplateSpec {
        severity_number: 17,
        severity_text: "ERROR",
        segments: &[
            "dns resolution for replica ",
            " failed after ",
            " ms and ",
            " attempts; falling back to the cached endpoint set",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "tls handshake with peer ",
            " completed in ",
            " ms using the negotiated cipher suite for connection ",
            " on the shared listener",
        ],
    },
    TemplateSpec {
        severity_number: 13,
        severity_text: "WARN",
        segments: &[
            "slow query detected on statement ",
            ": ",
            " ms elapsed scanning ",
            " rows; plan cache entry flagged for review",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "background export job ",
            " uploaded ",
            " objects (",
            " bytes) to the archive bucket within the maintenance window",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "feature flag evaluation for cohort ",
            " served variant ",
            " in ",
            " us; assignment recorded for the experimentation pipeline",
        ],
    },
    TemplateSpec {
        severity_number: 5,
        severity_text: "DEBUG",
        segments: &[
            "heartbeat from worker ",
            " received after ",
            " ms; lease ",
            " renewed and membership view left unchanged this round",
        ],
    },
    TemplateSpec {
        severity_number: 9,
        severity_text: "INFO",
        segments: &[
            "PUT /api/v1/profiles/",
            " updated ",
            " fields in ",
            " ms; audit trail entry appended with the request identifier",
        ],
    },
];

/// Deterministic 64-bit generator (splitmix64) — no runtime `proptest`
/// and no external RNG dependency for the load loop.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Build one deterministic OTLP export: `batch_size` records over the
/// fixed template mix, params drawn from a per-batch seed, timestamps
/// on the synthetic timeline (1 µs apart within the batch).
#[must_use]
pub fn build_batch(
    seed: u64,
    batch_idx: u64,
    batch_size: usize,
    ts_unix_nanos: u64,
) -> ExportLogsServiceRequest {
    let mut rng_state = seed ^ batch_idx.wrapping_mul(0xA076_1D64_78BD_642F);
    let log_records = (0..batch_size)
        .map(|i| {
            let pick = splitmix64(&mut rng_state);
            let template = &TEMPLATES[saturating_usize(pick) % TEMPLATES.len()];
            let mut body = String::with_capacity(192);
            for (slot, segment) in template.segments.iter().enumerate() {
                if slot > 0 {
                    let param = splitmix64(&mut rng_state) % 1_000_000;
                    body.push_str(&param.to_string());
                }
                body.push_str(segment);
            }
            LogRecord {
                time_unix_nano: ts_unix_nanos.saturating_add(to_u64(i).saturating_mul(1_000)),
                severity_number: template.severity_number,
                severity_text: template.severity_text.to_string(),
                body: Some(AnyValue {
                    value: Some(any_value::Value::StringValue(body)),
                }),
                ..LogRecord::default()
            }
        })
        .collect();
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue(TENANT.to_string())),
                    }),
                    ..KeyValue::default()
                }],
                ..Resource::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records,
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
}

// ----------------------------------------------------------------------
// Synthetic clock + sampler
// ----------------------------------------------------------------------

#[derive(Clone)]
struct SyntheticClock {
    started: Instant,
    compression: u64,
}

impl SyntheticClock {
    fn now_unix_nanos(&self) -> u64 {
        let wall = self.started.elapsed().as_nanos();
        let advanced = wall.saturating_mul(u128::from(self.compression));
        BASE_UNIX_NANOS.saturating_add(saturating_u64(advanced))
    }
}

struct SamplerCtx {
    store: Store,
    sink: SharedParquetSink,
    wal: Arc<Mutex<Wal>>,
    clock: SyntheticClock,
    policy: CompactionPolicy,
    every: Duration,
    stop: Arc<AtomicBool>,
}

/// Sample + sweep on a fixed wall cadence until stopped. Sequential by
/// construction: each sweep completes before the next tick is awaited,
/// so sweeps never overlap.
async fn run_sampler(ctx: SamplerCtx) -> Vec<BacklogSample> {
    let mut samples = Vec::new();
    loop {
        tokio::time::sleep(ctx.every).await;
        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }
        let store = ctx.store.clone();
        let sink = ctx.sink.clone();
        let wal = Arc::clone(&ctx.wal);
        let policy = ctx.policy;
        let wall_secs = as_f64(saturating_u64(ctx.clock.started.elapsed().as_millis())) / 1_000.0;
        let synthetic_now = ctx.clock.now_unix_nanos();
        let sample = match tokio::task::spawn_blocking(move || {
            blocking_sample(&store, &sink, &wal, &policy, wall_secs, synthetic_now)
        })
        .await
        {
            Ok(sample) => sample,
            Err(_) => error_sample(wall_secs, synthetic_now),
        };
        samples.push(sample);
    }
    samples
}

/// One blocking observation: flush the sink so buffered rows are
/// visible, record the pre-sweep candidate backlog, run one sweep at the
/// synthetic now, and read the WAL gauges.
fn blocking_sample(
    store: &Store,
    sink: &SharedParquetSink,
    wal: &Arc<Mutex<Wal>>,
    policy: &CompactionPolicy,
    wall_secs: f64,
    synthetic_now: u64,
) -> BacklogSample {
    sink.flush_all();
    let (backlog_partitions, backlog_bytes, mut errors) = backlog(store, synthetic_now, policy);
    let partitions_compacted = if let Ok(report) = run_sweep(store, synthetic_now, policy) {
        errors += report.errors.len();
        report.partitions_compacted
    } else {
        errors += 1;
        0
    };
    let (wal_segments, wal_disk_bytes) = {
        let metrics = lock_wal(wal).metrics();
        (metrics.segment_count, metrics.disk_bytes)
    };
    BacklogSample {
        wall_secs,
        synthetic_unix_nanos: synthetic_now,
        backlog_partitions,
        backlog_bytes,
        partitions_compacted,
        errors,
        wal_segments,
        wal_disk_bytes,
        sink_buffered_records: sink.buffered_records(),
    }
}

fn error_sample(wall_secs: f64, synthetic_now: u64) -> BacklogSample {
    BacklogSample {
        wall_secs,
        synthetic_unix_nanos: synthetic_now,
        backlog_partitions: 0,
        backlog_bytes: 0,
        partitions_compacted: 0,
        errors: 1,
        wal_segments: 0,
        wal_disk_bytes: 0,
        sink_buffered_records: 0,
    }
}

/// The candidate backlog as of `now`: sealed candidate partitions and
/// the summed size of their Parquet files (derived from the store
/// listing — the backlog has no stored byte counter).
fn backlog(store: &Store, now: u64, policy: &CompactionPolicy) -> (usize, u64, usize) {
    match plan_candidates(store, TENANT, now, policy) {
        Ok(candidates) => {
            let mut bytes = 0u64;
            let mut errors = 0usize;
            for key in &candidates {
                match store.list_with_sizes_blocking(Some(&partition_prefix(key))) {
                    Ok(entries) => {
                        bytes = bytes.saturating_add(
                            entries
                                .iter()
                                .filter(|(key, _)| key.ends_with(".parquet"))
                                .map(|(_, size)| *size)
                                .sum(),
                        );
                    }
                    Err(_) => errors += 1,
                }
            }
            (candidates.len(), bytes, errors)
        }
        Err(_) => (0, 0, 1),
    }
}

/// The partition's object-key prefix relative to the store root
/// (`data/tenant_id=…/year=…/…/hour=…`), matching the writer's layout.
fn partition_prefix(key: &PartitionKey) -> String {
    key.data_path(Path::new(""))
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

// ----------------------------------------------------------------------
// WAL sharing
// ----------------------------------------------------------------------

/// [`Journal`] over a shared WAL handle, so the sampler can read
/// [`Wal::metrics`] while the group-commit coordinator drives
/// append/sync. Delegates to the production `impl Journal for Wal`; the
/// coordinator already serializes journal access, so this mutex is
/// uncontended except for the sampler's brief metrics reads.
struct SharedWal(Arc<Mutex<Wal>>);

fn lock_wal(wal: &Arc<Mutex<Wal>>) -> MutexGuard<'_, Wal> {
    wal.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Journal for SharedWal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
        Journal::append_batch(&mut *lock_wal(&self.0), payload)
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Journal::sync(&mut *lock_wal(&self.0))
    }

    fn unflushed_bytes(&self) -> u64 {
        Journal::unflushed_bytes(&*lock_wal(&self.0))
    }
}

// ----------------------------------------------------------------------
// Numeric helpers
// ----------------------------------------------------------------------

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn saturating_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

// Report-scale values (lines, µs) are far below 2^52, so the f64
// conversion is exact in practice; the report is ms/lines-per-sec scale.
#[allow(clippy::cast_precision_loss)]
fn as_f64(value: u64) -> f64 {
    value as f64
}

fn us_to_ms(us: u64) -> f64 {
    as_f64(us) / 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_interval_matches_target_rate() {
        assert_eq!(
            batch_interval(100_000, 1_000),
            Some(Duration::from_millis(10)),
            "100k lines/s in 1000-line batches = one batch per 10 ms",
        );
        assert_eq!(batch_interval(5_000, 100), Some(Duration::from_millis(20)));
        assert_eq!(batch_interval(0, 100), None);
        assert_eq!(batch_interval(100, 0), None);
        assert_eq!(
            batch_interval(u64::MAX, 1),
            Some(Duration::from_nanos(1)),
            "floors at 1 ns rather than a zero interval",
        );
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_us(&v, 50, 100), 50);
        assert_eq!(percentile_us(&v, 95, 100), 95);
        assert_eq!(percentile_us(&v, 99, 100), 99);
        assert_eq!(percentile_us(&[7], 99, 100), 7);
        assert_eq!(percentile_us(&[], 99, 100), 0);
        assert_eq!(percentile_us(&[1, 2, 3], 99, 0), 0);
    }

    #[test]
    fn d1_verdict_applies_both_bars() {
        assert!(d1_verdict(100_000.0, 200.0).pass, "exactly at both bars");
        assert!(!d1_verdict(99_999.0, 10.0).pass, "rate below the bar");
        assert!(!d1_verdict(500_000.0, 200.1).pass, "p99 above the bar");
        let v = d1_verdict(1.0, 1.0);
        assert_eq!(v.bar_lines_per_sec_per_core, 100_000);
        assert_eq!(v.bar_p99_ack_ms, 200);
    }

    #[test]
    fn d2_verdict_requires_drain_and_return_to_zero() {
        assert!(d2_verdict(&[0, 1, 2, 0, 1], 0).pass, "returned after max");
        assert!(
            d2_verdict(&[1, 2, 2], 0).pass,
            "the final drained backlog counts as the return to zero",
        );
        assert!(!d2_verdict(&[0, 2, 1], 1).pass, "final backlog nonzero");
        assert!(!d2_verdict(&[0, 3, 1], usize::MAX).pass, "unknown final");
        assert!(d2_verdict(&[], 0).pass, "no accumulation, drained");
        let v = d2_verdict(&[0, 3, 0, 1], 0);
        assert_eq!(v.max_backlog_partitions, 3);
        assert!(v.returned_to_zero_after_max);
    }

    #[test]
    fn batch_generation_is_deterministic_and_tenant_scoped() {
        let a = build_batch(42, 7, 50, BASE_UNIX_NANOS);
        let b = build_batch(42, 7, 50, BASE_UNIX_NANOS);
        assert_eq!(a, b, "same seed + index ⇒ identical batch");
        let c = build_batch(42, 8, 50, BASE_UNIX_NANOS);
        assert_ne!(a, c, "a different batch index draws different params");
        let records = &a.resource_logs[0].scope_logs[0].log_records;
        assert_eq!(records.len(), 50);
        let attrs = &a.resource_logs[0]
            .resource
            .as_ref()
            .expect("resource")
            .attributes;
        assert_eq!(attrs[0].key, "service.name");
    }

    #[test]
    fn config_validation_rejects_zeroes() {
        let ok = SoakConfig::default();
        assert!(validate(&ok).is_ok());
        for broken in [
            SoakConfig {
                duration_secs: 0,
                ..ok.clone()
            },
            SoakConfig {
                target_lines_per_sec: 0,
                ..ok.clone()
            },
            SoakConfig {
                batch_size: 0,
                ..ok.clone()
            },
            SoakConfig {
                time_compression: 0,
                ..ok.clone()
            },
            SoakConfig {
                sample_every_secs: 0,
                ..ok.clone()
            },
            SoakConfig {
                worker_threads: 0,
                ..ok.clone()
            },
        ] {
            assert!(matches!(validate(&broken), Err(SoakError::Config(_))));
        }
    }

    /// End-to-end smoke soak: a few seconds of paced load under an
    /// aggressive synthetic clock must drive the full
    /// seal → sweep → compact cycle — at least one sample observes a
    /// nonzero candidate backlog, at least one sweep compacts, and the
    /// final drain leaves zero backlog.
    #[test]
    fn smoke_soak_runs_the_seal_sweep_compact_cycle() {
        let config = SoakConfig {
            duration_secs: 4,
            target_lines_per_sec: 5_000,
            batch_size: 100,
            // 1 wall second = 2000 synthetic seconds: an hour partition
            // seals (hour + 15 min grace) every ~2.25 wall seconds.
            time_compression: 2_000,
            sample_every_secs: 1,
            // Small flush target so each synthetic hour lands as several
            // Parquet files (a compaction candidate needs ≥ 2).
            sink_target_bytes: 64 * 1024,
            seed: 42,
            worker_threads: 4,
        };
        let report = run_soak(&config).expect("smoke soak runs");

        assert!(report.lines_acked > 0, "load was ingested and acked");
        assert_eq!(report.batches_failed, 0, "no batch failed to commit");
        assert!(report.latency.p99_ms > 0.0, "latencies were recorded");
        assert!(report.achieved_lines_per_sec > 0.0);
        assert!(
            report.samples.iter().any(|s| s.backlog_partitions > 0),
            "some sample observed a sealed candidate partition: {:?}",
            report.samples,
        );
        assert!(
            report.total_partitions_compacted >= 1,
            "at least one sweep compacted a partition: {:?}",
            report.samples,
        );
        assert_eq!(
            report.samples.iter().map(|s| s.errors).sum::<usize>(),
            0,
            "no sweep/listing errors: {:?}",
            report.samples,
        );
        assert_eq!(report.d2.final_backlog_partitions, 0, "drained");
        assert!(report.d2.pass, "the smoke run's backlog is bounded");
        // The report is the workflow artifact — it must serialize.
        let json = serde_json::to_string(&report).expect("report serializes");
        assert!(json.contains("\"d1\""));
    }
}
