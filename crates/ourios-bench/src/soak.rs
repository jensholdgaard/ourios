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

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_ingester::compactor::run_sweep;
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::receiver::pipeline::{Journal, ReceiveError};
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, TenantRule};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{
    CompactionPolicy, PartitionKey, Store, percent_encode_tenant, plan_candidates,
};
use ourios_wal::{Wal, WalConfig, WalOffset};
use serde::Serialize;

/// §D1 throughput bar: sustained acked lines per second per core.
pub const D1_LINES_PER_SEC_PER_CORE: u64 = 100_000;
/// §D1 latency bar: p99 ingest-ack latency in milliseconds.
pub const D1_P99_ACK_MS: u64 = 200;

/// The soak tenant (derived from `service.name` by the production
/// [`TenantRule`], so the batches exercise the real fan-out). With
/// `--tenants N > 1` the load fans out over `"{TENANT}-{i}"` for
/// `i in 0..N` (see [`tenant_name`]); N == 1 keeps the bare `TENANT` so
/// the single-tenant §9.19/§9.20 numbers stay byte-for-byte comparable.
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
    /// Distinct tenants fed through the one shared WAL / commit stream /
    /// store, round-robin per batch. `1` (the default) is the
    /// single-tenant baseline, byte-for-byte the pre-#567 behaviour;
    /// `N > 1` measures honest node capacity — N template trees under one
    /// commit stream — instead of the multi-process approximation
    /// (`docs/benchmarks.md` §9.20).
    pub tenants: usize,
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
            tenants: 1,
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
    /// The backlog drained after its maximum: either the sample series
    /// itself hit zero at least once after the max, **or** the post-load
    /// drain sweep brought the final backlog to zero — the drain sweep
    /// reaching zero is itself the return-to-zero evidence, observed at
    /// the end of the run rather than mid-series. Trivially true when
    /// the backlog never rose above zero.
    pub returned_to_zero_after_max: bool,
}

/// The soak run's full result — serialized as the `--out` JSON.
#[derive(Debug, Clone, Serialize)]
pub struct SoakReport {
    pub config: SoakConfig,
    /// Wall seconds from first batch to last ack (includes the drain of
    /// in-flight batches) — the rate denominator for the §D1 numbers.
    pub load_wall_secs: f64,
    /// Wall seconds for the whole run, including the sampler join and
    /// the final drain sweep; every sample's `wall_secs` is ≤ this.
    pub total_wall_secs: f64,
    pub lines_acked: u64,
    pub batches_acked: u64,
    pub batches_failed: u64,
    pub achieved_lines_per_sec: f64,
    /// Distinct tenants driven through the one shared commit stream this
    /// run (mirrors [`SoakConfig::tenants`]; surfaced at the top level so
    /// the results file names the node-capacity dimension directly).
    pub tenants: usize,
    /// The whole-node achieved rate — identical to
    /// `achieved_lines_per_sec`, named explicitly because it is the #567
    /// headline: honest single-node capacity across all tenants sharing
    /// one WAL / commit stream / store.
    pub aggregate_lines_per_sec: f64,
    /// Mean achieved rate per tenant (`aggregate_lines_per_sec /
    /// tenants`). Round-robin fan-out makes the tenants symmetric, so the
    /// mean is each tenant's share; the aggregate is their sum.
    pub per_tenant_lines_per_sec: f64,
    /// `achieved_lines_per_sec / worker_threads` — the §D1 per-core rate.
    pub per_core_lines_per_sec: f64,
    pub latency: LatencySummary,
    /// Ack observations offered to the recorder (one per acked batch).
    pub latency_samples_total: u64,
    /// Observations retained as the percentile basis after the bounded
    /// recorder's decimation (a systematic 1-in-2^k sample once the
    /// stored cap is hit, so percentiles stay valid at bounded memory).
    pub latency_samples_stored: usize,
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
    let dir =
        tempfile::tempdir().map_err(|e| SoakError::Setup(format!("create soak tempdir: {e}")))?;
    // `dir` (the WAL + store fixture) is dropped only after `block_on`
    // returns, so the fixture outlives the whole run.
    runtime.block_on(soak(config, dir.path()))
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
    if config.tenants == 0 {
        return Err(SoakError::Config("tenants must be > 0"));
    }
    Ok(())
}

async fn soak(config: &SoakConfig, root: &Path) -> Result<SoakReport, SoakError> {
    let Some(pace) = batch_interval(config.target_lines_per_sec, config.batch_size) else {
        return Err(SoakError::Config("target rate and batch size must be > 0"));
    };
    let wal_root = root.join("wal");
    let store_root = root.join("store");
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
    // The production ingest shape (RFC 0035 §3.1): id assignment under
    // the gate, the sink emit on the concurrent encode pool — so the
    // harness measures what the server role runs.
    let pipeline = Arc::new(
        IngestPipeline::new(coordinator, miner, TenantRule::service_name())
            .with_encode_pool(EncodePool::new(&sink, config.worker_threads)),
    );

    let clock = SyntheticClock {
        started: Instant::now(),
        compression: config.time_compression,
    };
    let policy = CompactionPolicy::default();
    // The active tenant set — one shared store, but the backlog/drain
    // measurement enumerates each tenant's partition tree (§9.20's honest
    // node-capacity view). `Arc<[String]>` so the sampler and each
    // spawn_blocking closure clone the handle, not the strings.
    let tenants: Arc<[String]> = Arc::from(tenant_set(config.tenants));
    let stats = Arc::new(LoadStats::default());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sampler_task = tokio::spawn(run_sampler(SamplerCtx {
        store: store.clone(),
        sink: sink.clone(),
        wal: Arc::clone(&wal),
        clock: clock.clone(),
        policy,
        every: Duration::from_secs(config.sample_every_secs),
        stop: stop_rx,
        tenants: Arc::clone(&tenants),
    }));

    run_load(&pipeline, &clock, config, &stats, pace).await;
    // The D1 rate denominator stops here — the sampler join and the
    // drain sweep below are measurement overhead, not load.
    let load_wall_secs = as_f64(saturating_u64(clock.started.elapsed().as_millis())) / 1_000.0;

    // RFC 0035 §3.1: drain the encode pool before the final sample +
    // drain sweep, so every acked record has reached the sink (and the
    // D2 backlog counts it) before flush_all runs. The bounded queue
    // keeps this residue to a few batches per core.
    let quiesce_pipeline = Arc::clone(&pipeline);
    if tokio::task::spawn_blocking(move || quiesce_pipeline.quiesce_encodes())
        .await
        .is_err()
    {
        return Err(SoakError::Setup("encode-pool quiesce panicked".into()));
    }

    // A send error just means the sampler already exited.
    let _ = stop_tx.send(true);
    // A sampler panic loses the timeseries but not the run; report with
    // no samples (d2 then fails loudly on the missing final backlog).
    let mut samples: Vec<BacklogSample> = sampler_task.await.unwrap_or_default();

    let (drain_sample, final_backlog) =
        final_drain(&store, &sink, &wal, &tenants, &clock, &policy).await;
    samples.push(drain_sample);
    let total_wall_secs = as_f64(saturating_u64(clock.started.elapsed().as_millis())) / 1_000.0;

    Ok(build_report(
        config,
        load_wall_secs,
        total_wall_secs,
        &stats,
        samples,
        final_backlog,
    ))
}

/// Final drain: advance the synthetic clock past the last partition's
/// hour end + grace so *every* partition seals, then take one last
/// sample + sweep and total the still-pending candidates across all
/// tenants. This verifies compaction can fully clear the backlog the run
/// produced; the mid-run samples carry the steady-state D2 evidence.
///
/// An error listing any tenant means the final backlog is unknown — the
/// per-tenant sum saturates to `usize::MAX` so the D2 verdict fails
/// loudly rather than claim a drained run.
async fn final_drain(
    store: &Store,
    sink: &SharedParquetSink,
    wal: &Arc<Mutex<Wal>>,
    tenants: &Arc<[String]>,
    clock: &SyntheticClock,
    policy: &CompactionPolicy,
) -> (BacklogSample, usize) {
    let drain_now = clock
        .now_unix_nanos()
        .saturating_add(HOUR_NANOS)
        .saturating_add(policy.grace_nanos)
        .saturating_add(1);
    let drain_wall = as_f64(saturating_u64(clock.started.elapsed().as_millis())) / 1_000.0;
    let store = store.clone();
    let sink = sink.clone();
    let wal = Arc::clone(wal);
    let tenants = Arc::clone(tenants);
    let policy = *policy;
    match tokio::task::spawn_blocking(move || {
        let sample = blocking_sample(
            &store, &sink, &wal, &policy, &tenants, drain_wall, drain_now,
        );
        let final_backlog = tenants
            .iter()
            .map(|tenant| {
                plan_candidates(&store, tenant, drain_now, &policy).map_or(usize::MAX, |c| c.len())
            })
            .fold(0usize, usize::saturating_add);
        (sample, final_backlog)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => (error_sample(drain_wall, drain_now), usize::MAX),
    }
}

fn build_report(
    config: &SoakConfig,
    load_wall_secs: f64,
    total_wall_secs: f64,
    stats: &LoadStats,
    samples: Vec<BacklogSample>,
    final_backlog: usize,
) -> SoakReport {
    let (mut latencies, latency_samples_total) = {
        let mut recorder = stats
            .latencies_us
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (std::mem::take(&mut recorder.samples), recorder.total)
    };
    latencies.sort_unstable();
    let latency_samples_stored = latencies.len();
    let latency = LatencySummary {
        p50_ms: us_to_ms(percentile_us(&latencies, 50, 100)),
        p95_ms: us_to_ms(percentile_us(&latencies, 95, 100)),
        p99_ms: us_to_ms(percentile_us(&latencies, 99, 100)),
        max_ms: us_to_ms(latencies.last().copied().unwrap_or(0)),
    };

    let lines_acked = stats.lines_acked.load(Ordering::Relaxed);
    let achieved = if load_wall_secs > 0.0 {
        as_f64(lines_acked) / load_wall_secs
    } else {
        0.0
    };
    let per_core = achieved / as_f64(to_u64(config.worker_threads));
    // `tenants` is validated > 0, so this divisor is never zero.
    let per_tenant = achieved / as_f64(to_u64(config.tenants));
    let backlog_series: Vec<usize> = samples.iter().map(|s| s.backlog_partitions).collect();
    let total_compacted = samples.iter().map(|s| s.partitions_compacted).sum();

    let p99_ms = latency.p99_ms;
    SoakReport {
        config: config.clone(),
        load_wall_secs,
        total_wall_secs,
        lines_acked,
        batches_acked: stats.batches_acked.load(Ordering::Relaxed),
        batches_failed: stats.batches_failed.load(Ordering::Relaxed),
        achieved_lines_per_sec: achieved,
        tenants: config.tenants,
        aggregate_lines_per_sec: achieved,
        per_tenant_lines_per_sec: per_tenant,
        per_core_lines_per_sec: per_core,
        latency,
        latency_samples_total,
        latency_samples_stored,
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
/// post-drain-sweep final backlog. A `final_backlog` of zero counts as
/// the return-to-zero evidence even when no mid-series sample caught
/// the backlog at zero (see [`D2Verdict::returned_to_zero_after_max`]).
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

/// Cap on stored ack-latency samples (2^22 × 8 B = 32 MiB). Unbounded
/// storage would reach hundreds of millions of entries over an hour at
/// 100k lines/s with a small batch size.
const LATENCY_SAMPLE_CAP: usize = 1 << 22;

/// Bounded ack-latency recorder: stores every observation until the cap,
/// then decimates by two. The retained set is always the *systematic*
/// 1-in-2^`shift` sample of the observation stream (an observation is
/// kept iff its index ≡ 0 mod 2^`shift`), a property halving preserves —
/// so percentiles over the retained set stay valid estimates of the full
/// stream at bounded memory.
struct LatencyRecorder {
    samples: Vec<u64>,
    /// log2 of the decimation stride: observation `i` is retained iff
    /// `i % (1 << shift) == 0`.
    shift: u32,
    /// Observations offered, retained or not.
    total: u64,
    cap: usize,
}

impl Default for LatencyRecorder {
    fn default() -> Self {
        Self::with_cap(LATENCY_SAMPLE_CAP)
    }
}

impl LatencyRecorder {
    /// A cap below 2 could never halve; clamp so `record` always makes
    /// progress.
    fn with_cap(cap: usize) -> Self {
        Self {
            samples: Vec::new(),
            shift: 0,
            total: 0,
            cap: cap.max(2),
        }
    }

    fn record(&mut self, value_us: u64) {
        let index = self.total;
        self.total += 1;
        let stride = 1u64.checked_shl(self.shift).unwrap_or(u64::MAX);
        if !index.is_multiple_of(stride) {
            return;
        }
        self.samples.push(value_us);
        if self.samples.len() >= self.cap {
            // Keep every other retained sample: retained observation
            // indices go from multiples of 2^shift to multiples of
            // 2^(shift+1), matching the go-forward stride.
            let halved: Vec<u64> = self.samples.iter().copied().step_by(2).collect();
            self.samples = halved;
            self.shift += 1;
        }
    }
}

#[derive(Default)]
struct LoadStats {
    latencies_us: Mutex<LatencyRecorder>,
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
    // A stall must not burst back-to-back catch-up batches (default
    // `Burst`) — that breaks the paced-load assumption and inflates
    // in-flight depth; `Delay` keeps a full pace gap, as the sampler does.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = clock.started + Duration::from_secs(config.duration_secs);
    let mut batch_idx = 0u64;
    loop {
        ticker.tick().await;
        // Deadline check *after* the tick: a tick landing past the
        // deadline must not schedule one extra batch, or the last batch
        // would fall outside the wall/rate accounting window.
        if Instant::now() >= deadline {
            break;
        }
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            break; // Closed semaphore: cannot happen while we hold it.
        };
        let pipeline = Arc::clone(pipeline);
        let stats = Arc::clone(stats);
        let batch = build_batch_for_tenants(
            config.seed,
            batch_idx,
            config.batch_size,
            clock.now_unix_nanos(),
            config.tenants,
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
                        .record(elapsed_us);
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

/// The `service.name` (⇒ tenant, via [`TenantRule::service_name`]) for
/// batch `batch_idx` under round-robin fan-out over `tenants` tenants.
///
/// `tenants <= 1` returns the bare [`TENANT`] borrowed — the
/// single-tenant fast path, so the baseline batch is byte-for-byte
/// unchanged. `tenants > 1` cycles `"{TENANT}-{batch_idx % tenants}"`, so
/// each tenant receives every `tenants`-th batch: N distinct template
/// trees, one shared commit stream.
#[must_use]
fn tenant_name(batch_idx: u64, tenants: usize) -> Cow<'static, str> {
    if tenants <= 1 {
        Cow::Borrowed(TENANT)
    } else {
        Cow::Owned(format!("{TENANT}-{}", batch_idx % to_u64(tenants)))
    }
}

/// The active tenant ids for a run, matching [`tenant_name`]'s outputs:
/// `[TENANT]` for the single-tenant baseline, else `"{TENANT}-{i}"` for
/// `i in 0..tenants`. The measurement side (backlog / drain) enumerates
/// this set; the load side derives the same ids per batch.
#[must_use]
fn tenant_set(tenants: usize) -> Vec<String> {
    if tenants <= 1 {
        vec![TENANT.to_string()]
    } else {
        (0..tenants).map(|i| format!("{TENANT}-{i}")).collect()
    }
}

/// Build one deterministic OTLP export for the bare single [`TENANT`]:
/// `batch_size` records over the fixed template mix, params drawn from a
/// per-batch seed, timestamps on the synthetic timeline (1 µs apart
/// within the batch).
#[must_use]
pub fn build_batch(
    seed: u64,
    batch_idx: u64,
    batch_size: usize,
    ts_unix_nanos: u64,
) -> ExportLogsServiceRequest {
    build_batch_for(
        seed,
        batch_idx,
        batch_size,
        ts_unix_nanos,
        TENANT.to_string(),
    )
}

/// Build the batch for `batch_idx` under round-robin fan-out over
/// `tenants` tenants (see [`tenant_name`]): identical record payload to
/// [`build_batch`], only the `service.name` differs — so N tenants share
/// one deterministic template mix but land in N distinct per-tenant
/// trees. `tenants == 1` is byte-for-byte [`build_batch`].
#[must_use]
pub fn build_batch_for_tenants(
    seed: u64,
    batch_idx: u64,
    batch_size: usize,
    ts_unix_nanos: u64,
    tenants: usize,
) -> ExportLogsServiceRequest {
    match tenant_name(batch_idx, tenants) {
        // The single-tenant fast path *is* `build_batch`, so the baseline
        // batch stays byte-for-byte identical.
        Cow::Borrowed(_) => build_batch(seed, batch_idx, batch_size, ts_unix_nanos),
        // The name is already owned (a per-batch `format!`); move it in so
        // the hot loop allocates the tenant string exactly once.
        Cow::Owned(name) => build_batch_for(seed, batch_idx, batch_size, ts_unix_nanos, name),
    }
}

/// Shared body of the batch builders: the record payload is a pure
/// function of `(seed, batch_idx, batch_size, ts)`; `service_name` only
/// stamps the tenant. Takes the name **by value** and moves it into the
/// `ResourceLogs`, so a caller that already owns it (the multi-tenant hot
/// loop's per-batch `format!`) does not re-allocate it.
#[must_use]
fn build_batch_for(
    seed: u64,
    batch_idx: u64,
    batch_size: usize,
    ts_unix_nanos: u64,
    service_name: String,
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
                    // Infallible for String, and no per-param allocation
                    // in this hot loop (one write per record slot).
                    let _ = write!(body, "{param}");
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
                        value: Some(any_value::Value::StringValue(service_name)),
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
    stop: tokio::sync::watch::Receiver<bool>,
    tenants: Arc<[String]>,
}

/// Sample + sweep on an aligned wall cadence until stopped: ticks are
/// scheduled every `every` from the sampler's start (first at
/// `start + every`), so the period does not stretch by each sample's
/// blocking work. A sample that overruns its period delays the next
/// tick by a full `every` (`MissedTickBehavior::Delay`) rather than
/// bursting — and each sweep completes before the next tick is awaited,
/// so sweeps never overlap. Shutdown is prompt: the stop signal races
/// the tick, so the sampler never idles out a tick period (up to a full
/// `every`) just to notice it should exit.
async fn run_sampler(mut ctx: SamplerCtx) -> Vec<BacklogSample> {
    let mut samples = Vec::new();
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + ctx.every, ctx.every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            // The only value ever sent is `true`, and a dropped sender
            // equally means the run is over — exit on either.
            _ = ctx.stop.changed() => break,
        }
        if *ctx.stop.borrow() {
            break;
        }
        let store = ctx.store.clone();
        let sink = ctx.sink.clone();
        let wal = Arc::clone(&ctx.wal);
        let policy = ctx.policy;
        let tenants = Arc::clone(&ctx.tenants);
        let wall_secs = as_f64(saturating_u64(ctx.clock.started.elapsed().as_millis())) / 1_000.0;
        let synthetic_now = ctx.clock.now_unix_nanos();
        let sample = match tokio::task::spawn_blocking(move || {
            blocking_sample(
                &store,
                &sink,
                &wal,
                &policy,
                &tenants,
                wall_secs,
                synthetic_now,
            )
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
    tenants: &[String],
    wall_secs: f64,
    synthetic_now: u64,
) -> BacklogSample {
    sink.flush_all();
    let (backlog_partitions, backlog_bytes, mut errors) =
        backlog(store, tenants, synthetic_now, policy);
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

/// The candidate backlog as of `now`, aggregated across every active
/// tenant: total sealed candidate partitions, the summed size of their
/// Parquet files, and the error count. One tenant-wide listing per tenant
/// (see [`backlog_for_tenant`]) — N tenants ⇒ N listings, not N per
/// partition — so the per-sample list cost stays bounded at one call per
/// tenant regardless of backlog depth.
fn backlog(
    store: &Store,
    tenants: &[String],
    now: u64,
    policy: &CompactionPolicy,
) -> (usize, u64, usize) {
    tenants
        .iter()
        .fold((0, 0, 0), |(parts, bytes, errs), tenant| {
            let (p, b, e) = backlog_for_tenant(store, tenant, now, policy);
            (
                parts.saturating_add(p),
                bytes.saturating_add(b),
                errs.saturating_add(e),
            )
        })
}

/// One tenant's candidate backlog as of `now`: sealed candidate
/// partitions and the summed size of their Parquet files (derived from
/// the store listing — the backlog has no stored byte counter).
///
/// One tenant-wide listing per call, matched against the candidates'
/// partition prefixes — not one listing per candidate. On a non-local
/// store every listing is a remote call, so the per-sample list cost
/// must stay bounded regardless of backlog depth.
fn backlog_for_tenant(
    store: &Store,
    tenant: &str,
    now: u64,
    policy: &CompactionPolicy,
) -> (usize, u64, usize) {
    let Ok(candidates) = plan_candidates(store, tenant, now, policy) else {
        return (0, 0, 1);
    };
    if candidates.is_empty() {
        return (0, 0, 0);
    }
    let prefixes: Vec<String> = candidates
        .iter()
        .map(|key| format!("{}/", partition_prefix(key)))
        .collect();
    let tenant_prefix = format!("data/tenant_id={}", percent_encode_tenant(tenant));
    match store.list_with_sizes_blocking(Some(&tenant_prefix)) {
        Ok(entries) => {
            let bytes = entries
                .iter()
                .filter(|(key, _)| {
                    key.ends_with(".parquet")
                        && prefixes.iter().any(|p| key.starts_with(p.as_str()))
                })
                .map(|(_, size)| *size)
                .fold(0u64, u64::saturating_add);
            (candidates.len(), bytes, 0)
        }
        Err(_) => (candidates.len(), 0, 1),
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
        // The drain sweep reaching zero is itself the return-to-zero
        // evidence: the mid-series never hits zero after its max here.
        let drained_only = d2_verdict(&[1, 2, 2], 0);
        assert!(drained_only.returned_to_zero_after_max);
        assert!(
            !d2_verdict(&[1, 2, 2], 1).returned_to_zero_after_max,
            "without the drain reaching zero, this series never returned",
        );
    }

    /// The bounded recorder keeps a systematic 1-in-2^k sample: with
    /// cap 8 over observations 0..32, three halvings leave exactly the
    /// multiples of 8, and `total` still counts every observation.
    #[test]
    fn latency_recorder_decimates_to_a_systematic_sample() {
        let mut recorder = LatencyRecorder::with_cap(8);
        for i in 0..32u64 {
            recorder.record(i);
        }
        assert_eq!(recorder.total, 32);
        assert_eq!(recorder.samples, vec![0, 8, 16, 24]);

        // Below the cap nothing is dropped.
        let mut small = LatencyRecorder::with_cap(8);
        for i in 0..5u64 {
            small.record(i);
        }
        assert_eq!(small.total, 5);
        assert_eq!(small.samples, vec![0, 1, 2, 3, 4]);
        assert_eq!(small.shift, 0);
    }

    /// A tick landing past the deadline must not schedule one extra
    /// batch: with a 400 ms pace and a 1 s deadline only the ticks at
    /// ~0/400/800 ms may send (3 batches); the tick at ~1200 ms breaks
    /// first. Delayed ticks under load can only reduce the count.
    #[test]
    fn load_loop_schedules_no_batch_past_the_deadline() {
        let config = SoakConfig {
            duration_secs: 1,
            target_lines_per_sec: 250,
            batch_size: 100, // pace = 100 / 250 = 400 ms
            time_compression: 1,
            sample_every_secs: 1,
            sink_target_bytes: 4 * 1024 * 1024,
            seed: 7,
            worker_threads: 2,
            tenants: 1,
        };
        let report = run_soak(&config).expect("deadline soak runs");
        let batches = report.batches_acked + report.batches_failed;
        assert!(
            (1..=3).contains(&batches),
            "3 ticks fit before the 1 s deadline, got {batches}",
        );
        assert!(report.lines_acked <= 300);
    }

    fn service_name_of(req: &ExportLogsServiceRequest) -> String {
        req.resource_logs[0]
            .resource
            .as_ref()
            .expect("resource")
            .attributes
            .iter()
            .find(|kv| kv.key == "service.name")
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| match &v.value {
                Some(any_value::Value::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .expect("service.name string")
    }

    /// `--tenants N > 1` round-robins `service.name` across batches while
    /// leaving the record payload a pure function of `(seed, batch_idx)`;
    /// `N == 1` stays byte-for-byte the single-tenant baseline.
    #[test]
    fn build_batch_cycles_tenants_and_keeps_the_single_tenant_baseline() {
        // N == 1: bare TENANT, and byte-identical to `build_batch`.
        assert_eq!(tenant_name(3, 1), Cow::Borrowed(TENANT));
        let single = build_batch_for_tenants(1, 3, 10, BASE_UNIX_NANOS, 1);
        assert_eq!(service_name_of(&single), TENANT);
        assert_eq!(
            single,
            build_batch(1, 3, 10, BASE_UNIX_NANOS),
            "the single-tenant path is byte-for-byte build_batch",
        );

        // N > 1: service.name cycles soak-0..soak-3 by batch index.
        let names: Vec<String> = (0..9)
            .map(|i| service_name_of(&build_batch_for_tenants(1, i, 10, BASE_UNIX_NANOS, 4)))
            .collect();
        assert_eq!(
            names,
            vec![
                "soak-0", "soak-1", "soak-2", "soak-3", "soak-0", "soak-1", "soak-2", "soak-3",
                "soak-0",
            ],
        );

        // The active tenant set the measurement enumerates matches the
        // ids the load side stamps.
        assert_eq!(tenant_set(1), vec![TENANT.to_string()]);
        assert_eq!(tenant_set(4), vec!["soak-0", "soak-1", "soak-2", "soak-3"],);

        // Only service.name differs between the baseline and the tagged
        // batch for the same index — the template trees are the same mix,
        // just scoped per tenant.
        let bare = build_batch(1, 1, 10, BASE_UNIX_NANOS);
        let tagged = build_batch_for_tenants(1, 1, 10, BASE_UNIX_NANOS, 4);
        assert_eq!(service_name_of(&tagged), "soak-1");
        assert_eq!(
            bare.resource_logs[0].scope_logs, tagged.resource_logs[0].scope_logs,
            "record payload is tenant-independent",
        );
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
            SoakConfig {
                tenants: 0,
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
            tenants: 1,
        };
        let report = run_soak(&config).expect("smoke soak runs");

        assert!(report.lines_acked > 0, "load was ingested and acked");
        assert_eq!(report.batches_failed, 0, "no batch failed to commit");
        assert!(report.latency.p99_ms > 0.0, "latencies were recorded");
        assert!(report.achieved_lines_per_sec > 0.0);
        assert_eq!(
            report.latency_samples_total, report.batches_acked,
            "one latency observation per acked batch",
        );
        assert!(report.latency_samples_stored <= saturating_usize(report.latency_samples_total));
        assert!(
            report.total_wall_secs >= report.load_wall_secs,
            "total wall covers the sampler join + drain sweep",
        );
        for sample in &report.samples {
            assert!(
                sample.wall_secs <= report.total_wall_secs,
                "no sample timestamp past the run's total wall: {sample:?}",
            );
        }
        assert!(
            report.samples.iter().any(|s| s.backlog_partitions > 0),
            "some sample observed a sealed candidate partition: {:?}",
            report.samples,
        );
        assert!(
            report
                .samples
                .iter()
                .filter(|s| s.backlog_partitions > 0)
                .all(|s| s.backlog_bytes > 0),
            "candidate partitions carry bytes (the tenant-wide listing \
             matched their prefixes): {:?}",
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

    /// Multi-tenant smoke soak (#567): four tenants fan out through the
    /// one shared WAL / commit stream / store. Asserts (a) all four land
    /// distinct partition prefixes, (b) the backlog aggregates across
    /// tenants and the final drain clears it, (c) the aggregate rate is
    /// the sum of the per-tenant means, and (d) the report names four
    /// tenants. Calls [`soak`] on an inspectable dir (rather than
    /// [`run_soak`]'s internal tempdir) so the per-tenant partition trees
    /// can be observed directly.
    #[test]
    fn multi_tenant_smoke_soak_fans_out_over_one_commit_stream() {
        let config = SoakConfig {
            duration_secs: 4,
            target_lines_per_sec: 8_000,
            batch_size: 100,
            // As in the single-tenant smoke: 1 wall second = 2000
            // synthetic seconds, so hour partitions seal every ~2.25 s.
            time_compression: 2_000,
            sample_every_secs: 1,
            sink_target_bytes: 64 * 1024,
            seed: 42,
            worker_threads: 4,
            tenants: 4,
        };
        validate(&config).expect("config is valid");
        let dir = tempfile::tempdir().expect("soak tempdir");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.worker_threads)
            .enable_time()
            .build()
            .expect("soak runtime");
        let report = runtime
            .block_on(soak(&config, dir.path()))
            .expect("multi-tenant smoke soak runs");

        // (d) the report names four tenants.
        assert_eq!(report.tenants, 4);
        assert!(report.lines_acked > 0, "load was ingested and acked");
        assert_eq!(report.batches_failed, 0, "no batch failed to commit");

        // (c) the aggregate node rate is the sum of the per-tenant means.
        assert!(report.per_tenant_lines_per_sec > 0.0);
        assert!(
            (report.aggregate_lines_per_sec - report.per_tenant_lines_per_sec * 4.0).abs() < 1.0,
            "aggregate {} ≈ 4 × per-tenant mean {}",
            report.aggregate_lines_per_sec,
            report.per_tenant_lines_per_sec,
        );
        assert_eq!(
            report.aggregate_lines_per_sec.to_bits(),
            report.achieved_lines_per_sec.to_bits(),
            "the aggregate is the whole-node achieved rate",
        );

        // (a) all four tenants produced a distinct partition prefix in
        // the one shared store.
        let mut tenant_dirs: Vec<String> = std::fs::read_dir(dir.path().join("store").join("data"))
            .expect("store data dir exists")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("tenant_id="))
            .collect();
        tenant_dirs.sort();
        assert_eq!(
            tenant_dirs,
            vec![
                "tenant_id=soak-0",
                "tenant_id=soak-1",
                "tenant_id=soak-2",
                "tenant_id=soak-3",
            ],
            "each of the four tenants landed a distinct partition prefix",
        );

        // (b) the backlog aggregates across tenants and drains to zero.
        assert!(
            report.samples.iter().any(|s| s.backlog_partitions > 0),
            "some sample observed sealed candidates across tenants: {:?}",
            report.samples,
        );
        assert_eq!(
            report.samples.iter().map(|s| s.errors).sum::<usize>(),
            0,
            "no sweep/listing errors across the tenant enumeration: {:?}",
            report.samples,
        );
        assert_eq!(
            report.d2.final_backlog_partitions, 0,
            "the final drain cleared every tenant's backlog",
        );
        assert!(report.d2.pass, "the multi-tenant backlog is bounded");
    }
}
