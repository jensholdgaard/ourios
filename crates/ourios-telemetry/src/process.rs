//! The upstream `OTel` process metrics (semantic conventions
//! `process.*`), observed when the reader collects rather than sampled on a
//! timer of our own, and the `process` entity's identifying attributes.
//!
//! Linux, the production target, reads `/proc/self` and emits all five:
//! `process.memory.usage` (resident set size), `process.cpu.time` by
//! `cpu.mode`, `process.thread.count`, `process.unix.file_descriptor.count`
//! and `process.uptime`. Other Unix systems emit only the descriptor count
//! and the uptime: on macOS the other three need `task_info`/`getrusage`
//! FFI, which is `unsafe` and denied workspace-wide.
//!
//! A source that cannot be read at collection time observes nothing for
//! that interval; a missing data point is the honest report, a zero is not.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

/// The instrumentation scope the process metrics are recorded under.
pub(crate) const SCOPE: &str = "ourios.process";

/// When this process started. The monotonic clock carries the precision;
/// the kernel's record of the start (Linux) adds the time the process ran
/// before telemetry first asked.
struct Start {
    observed: Instant,
    observed_wall: SystemTime,
    ran_before: Duration,
}

impl Start {
    fn get() -> &'static Self {
        static START: OnceLock<Start> = OnceLock::new();
        START.get_or_init(|| Self {
            observed: Instant::now(),
            observed_wall: SystemTime::now(),
            ran_before: ran_before_now().unwrap_or_default(),
        })
    }

    fn uptime(&self) -> Duration {
        self.ran_before + self.observed.elapsed()
    }

    fn created(&self) -> SystemTime {
        self.observed_wall
            .checked_sub(self.ran_before)
            .unwrap_or(self.observed_wall)
    }
}

/// The `process` entity's identifying attributes, `process.pid` and
/// `process.creation.time` (ISO 8601, UTC), for the telemetry resource:
/// together they tell replicas, and restarts of one replica, apart.
pub(crate) fn identity() -> [KeyValue; 2] {
    let created =
        DateTime::<Utc>::from(Start::get().created()).to_rfc3339_opts(SecondsFormat::Millis, true);
    [
        KeyValue::new("process.pid", i64::from(std::process::id())),
        KeyValue::new("process.creation.time", created),
    ]
}

/// Register the process metrics on `meter`. The callbacks live as long as
/// the meter provider does, so the returned instruments need not be kept.
pub(crate) fn register(meter: &Meter) {
    let start = Start::get();
    meter
        .f64_observable_gauge("process.uptime")
        .with_unit("s")
        .with_description("The time the process has been running.")
        .with_callback(move |observer| observer.observe(start.uptime().as_secs_f64(), &[]))
        .build();

    #[cfg(unix)]
    meter
        .i64_observable_up_down_counter("process.unix.file_descriptor.count")
        .with_unit("{file_descriptor}")
        .with_description("Number of unix file descriptors in use by the process.")
        .with_callback(|observer| {
            if let Some(count) = open_descriptors() {
                observer.observe(count, &[]);
            }
        })
        .build();

    #[cfg(target_os = "linux")]
    linux::register(meter);
}

/// How long the process ran before now, from `/proc`.
#[cfg(target_os = "linux")]
fn ran_before_now() -> Option<Duration> {
    let since_boot = std::fs::read_to_string("/proc/uptime").ok()?;
    let ran = linux::stat()?.ran_for(
        procfs::uptime(&since_boot)?,
        rustix::param::clock_ticks_per_second(),
    );
    Duration::try_from_secs_f64(ran).ok()
}

#[cfg(not(target_os = "linux"))]
fn ran_before_now() -> Option<Duration> {
    None
}

/// The descriptors open in this process, not counting the one the listing
/// itself holds open while it runs.
#[cfg(unix)]
fn open_descriptors() -> Option<i64> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let listed = std::fs::read_dir(dir).ok()?.count();
    i64::try_from(listed.saturating_sub(1)).ok()
}

#[cfg(target_os = "linux")]
mod linux {
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::Meter;

    use super::procfs::{Stat, Status, ticks_to_seconds};

    pub(super) fn register(meter: &Meter) {
        meter
            .i64_observable_up_down_counter("process.memory.usage")
            .with_unit("By")
            .with_description("The amount of physical memory in use.")
            .with_callback(|observer| {
                if let Some(bytes) = status().and_then(|status| status.rss_bytes) {
                    observer.observe(bytes, &[]);
                }
            })
            .build();

        meter
            .i64_observable_up_down_counter("process.thread.count")
            .with_unit("{thread}")
            .with_description("Process threads count.")
            .with_callback(|observer| {
                if let Some(threads) = status().and_then(|status| status.threads) {
                    observer.observe(threads, &[]);
                }
            })
            .build();

        let ticks = rustix::param::clock_ticks_per_second();
        let user = [KeyValue::new("cpu.mode", "user")];
        let system = [KeyValue::new("cpu.mode", "system")];
        meter
            .f64_observable_counter("process.cpu.time")
            .with_unit("s")
            .with_description("Total CPU seconds broken down by different CPU modes.")
            .with_callback(move |observer| {
                if let Some(stat) = stat() {
                    observer.observe(ticks_to_seconds(stat.user, ticks), &user);
                    observer.observe(ticks_to_seconds(stat.system, ticks), &system);
                }
            })
            .build();
    }

    fn status() -> Option<Status> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        Some(Status::parse(&text))
    }

    pub(super) fn stat() -> Option<Stat> {
        let text = std::fs::read_to_string("/proc/self/stat").ok()?;
        Stat::parse(&text)
    }
}

/// The `/proc` text parsers. Only Linux reads `/proc`, but the parsers are
/// plain functions over text, so their tests run on every host.
#[cfg(any(target_os = "linux", test))]
mod procfs {
    /// The fields Ourios reads from `/proc/<pid>/status`.
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct Status {
        /// `VmRSS`, in bytes.
        pub(super) rss_bytes: Option<i64>,
        /// `Threads`.
        pub(super) threads: Option<i64>,
    }

    impl Status {
        pub(super) fn parse(text: &str) -> Self {
            let mut status = Self::default();
            for (key, value) in text.lines().filter_map(|line| line.split_once(':')) {
                let value = value.trim();
                match key {
                    "VmRSS" => {
                        status.rss_bytes = value
                            .strip_suffix("kB")
                            .and_then(|kib| kib.trim().parse::<i64>().ok())
                            .and_then(|kib| kib.checked_mul(1024));
                    }
                    "Threads" => status.threads = value.parse().ok(),
                    _ => {}
                }
            }
            status
        }
    }

    /// The fields Ourios reads from `/proc/<pid>/stat`, all in clock ticks.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Stat {
        /// `utime`, field 14.
        pub(super) user: u64,
        /// `stime`, field 15.
        pub(super) system: u64,
        /// `starttime`, field 22: ticks from boot to the process starting.
        pub(super) start: u64,
    }

    impl Stat {
        /// The command name in field 2 is parenthesised and may itself
        /// contain spaces and `)`, so the fields are counted from the last
        /// `)`: what follows it starts at field 3.
        pub(super) fn parse(text: &str) -> Option<Self> {
            let (_, rest) = text.rsplit_once(')')?;
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let ticks = |field: usize| fields.get(field - 3)?.parse().ok();
            Some(Self {
                user: ticks(14)?,
                system: ticks(15)?,
                start: ticks(22)?,
            })
        }

        /// Seconds the process has run, given the system uptime.
        pub(super) fn ran_for(&self, since_boot: f64, per_second: u64) -> f64 {
            (since_boot - ticks_to_seconds(self.start, per_second)).max(0.0)
        }
    }

    /// The system uptime in seconds, the first field of `/proc/uptime`.
    pub(super) fn uptime(text: &str) -> Option<f64> {
        text.split_whitespace().next()?.parse().ok()
    }

    // Tick counts stay far below 2^52, where an f64 would start to round.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn ticks_to_seconds(ticks: u64, per_second: u64) -> f64 {
        ticks as f64 / per_second.max(1) as f64
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const STAT: &str = "4242 (ourios (x) y) S 1 4242 4242 0 -1 4194560 1043 0 0 0 \
                            731 214 0 0 20 0 9 0 12345 1000 200 18446744073709551615";

        #[test]
        fn stat_counts_fields_from_the_last_parenthesis() {
            assert_eq!(
                Stat::parse(STAT),
                Some(Stat {
                    user: 731,
                    system: 214,
                    start: 12345,
                })
            );
            assert_eq!(Stat::parse("4242 (truncated) S 1"), None);
        }

        #[test]
        fn status_parses_rss_and_threads() {
            let status = Status::parse(
                "Name:\tourios-server\nVmHWM:\t  9000 kB\nVmRSS:\t   4096 kB\nThreads:\t17\n",
            );
            assert_eq!(
                status,
                Status {
                    rss_bytes: Some(4096 * 1024),
                    threads: Some(17),
                }
            );
            assert_eq!(Status::parse("VmSwap:\t0 kB\n"), Status::default());
            assert_eq!(Status::parse("VmRSS:\t12 MB\n").rss_bytes, None);
        }

        #[test]
        fn ticks_convert_at_the_kernel_rate() {
            assert!((ticks_to_seconds(250, 100) - 2.5).abs() < f64::EPSILON);
        }

        #[test]
        fn uptime_is_system_uptime_less_the_start_tick() {
            let stat = Stat::parse(STAT).expect("parses");
            let since_boot = uptime("223.45 880.10\n").expect("parses");
            let ran = stat.ran_for(since_boot, 100);
            assert!((ran - 100.0).abs() < 1e-9, "got {ran}");
            assert_eq!(uptime(""), None);
            assert_eq!(Stat::parse("1 (x) S 1"), None);
        }
    }
}

#[cfg(test)]
mod tests {

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, GaugeDataPoint, Metric, MetricData, ResourceMetrics, ScopeMetrics,
    };
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

    /// A provider carrying only the process metrics, exporting in memory.
    struct Harness {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    impl Harness {
        fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_periodic_exporter(exporter.clone())
                .build();
            super::register(&provider.meter(super::SCOPE));
            Self { provider, exporter }
        }

        /// One collection of every process metric.
        fn collect(&self) -> Collected {
            self.exporter.reset();
            self.provider.force_flush().expect("flush");
            Collected(
                self.exporter
                    .get_finished_metrics()
                    .expect("metrics exported"),
            )
        }
    }

    struct Collected(Vec<ResourceMetrics>);

    impl Collected {
        fn metrics(&self) -> impl Iterator<Item = &Metric> {
            self.0
                .iter()
                .flat_map(ResourceMetrics::scope_metrics)
                .flat_map(ScopeMetrics::metrics)
        }

        fn get(&self, name: &str) -> Option<&Metric> {
            self.metrics().find(|metric| metric.name() == name)
        }
    }

    /// The single attribute-free value of an i64 `UpDownCounter`.
    fn up_down(metrics: &Collected, name: &str) -> i64 {
        match metrics.get(name).map(Metric::data) {
            Some(AggregatedMetrics::I64(MetricData::Sum(sum))) if !sum.is_monotonic() => {
                match sum.data_points().collect::<Vec<_>>().as_slice() {
                    [point] if point.attributes().next().is_none() => point.value(),
                    other => panic!("{name}: expected one attribute-free point, got {other:?}"),
                }
            }
            other => panic!("{name}: expected an i64 UpDownCounter, got {other:?}"),
        }
    }

    /// `process.cpu.time`'s points, by `cpu.mode`.
    #[cfg(target_os = "linux")]
    fn cpu_time(metrics: &Collected) -> std::collections::BTreeMap<String, f64> {
        match metrics.get("process.cpu.time").map(Metric::data) {
            Some(AggregatedMetrics::F64(MetricData::Sum(sum))) if sum.is_monotonic() => sum
                .data_points()
                .map(
                    |point| match point.attributes().collect::<Vec<_>>().as_slice() {
                        [mode] if mode.key.as_str() == "cpu.mode" => {
                            (mode.value.as_str().into_owned(), point.value())
                        }
                        other => panic!("process.cpu.time: expected only cpu.mode, got {other:?}"),
                    },
                )
                .collect(),
            other => panic!("process.cpu.time: expected an f64 Counter, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn every_metric_carries_its_semconv_unit_and_instrument() {
        let metrics = Harness::new().collect();
        let expected: &[(&str, &str)] = if cfg!(target_os = "linux") {
            &[
                ("process.cpu.time", "s"),
                ("process.memory.usage", "By"),
                ("process.thread.count", "{thread}"),
                ("process.unix.file_descriptor.count", "{file_descriptor}"),
                ("process.uptime", "s"),
            ]
        } else {
            &[
                ("process.unix.file_descriptor.count", "{file_descriptor}"),
                ("process.uptime", "s"),
            ]
        };
        let mut emitted: Vec<(&str, &str)> =
            metrics.metrics().map(|m| (m.name(), m.unit())).collect();
        emitted.sort_unstable();
        assert_eq!(emitted, expected);

        assert!(up_down(&metrics, "process.unix.file_descriptor.count") > 0);
        match metrics.get("process.uptime").map(Metric::data) {
            Some(AggregatedMetrics::F64(MetricData::Gauge(gauge))) => {
                let points: Vec<f64> = gauge.data_points().map(GaugeDataPoint::value).collect();
                assert!(
                    matches!(points.as_slice(), [uptime] if *uptime > 0.0),
                    "process.uptime: expected one positive point, got {points:?}"
                );
            }
            other => panic!("process.uptime: expected an f64 gauge, got {other:?}"),
        }
        if cfg!(target_os = "linux") {
            assert!(up_down(&metrics, "process.thread.count") >= 1);
        }
    }

    /// The OOM signal must move with the heap: touch 64 MiB and the resident
    /// set grows by most of it. Compared with the process's own earlier
    /// reading, so the test runner's baseline does not matter.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn memory_usage_rises_when_64_mib_is_touched() {
        const BALLAST: usize = 64 << 20;
        let harness = Harness::new();
        let before = up_down(&harness.collect(), "process.memory.usage");

        // A non-zero fill writes every page, so none stays unbacked.
        let ballast = std::hint::black_box(vec![0xA5_u8; BALLAST]);
        let after = up_down(&harness.collect(), "process.memory.usage");
        drop(ballast);

        let rise = after - before;
        assert!(
            rise >= i64::try_from(BALLAST / 2).expect("fits"),
            "touching {BALLAST} bytes raised process.memory.usage by only {rise} \
             ({before} -> {after})"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cpu_time_splits_user_and_system_and_never_decreases() {
        let harness = Harness::new();
        let first = cpu_time(&harness.collect());

        let mut acc = 0_u64;
        for i in 0..50_000_000_u64 {
            acc = std::hint::black_box(acc.wrapping_mul(31).wrapping_add(i));
        }
        let second = cpu_time(&harness.collect());

        assert_eq!(first.keys().collect::<Vec<_>>(), ["system", "user"]);
        assert_eq!(second.keys().collect::<Vec<_>>(), ["system", "user"]);
        for (mode, earlier) in &first {
            let later = second[mode];
            assert!(
                later >= *earlier,
                "cpu.mode={mode} went backwards: {earlier} -> {later}"
            );
        }
        assert!(
            second["user"] > first["user"],
            "busy-looping should advance user time ({} -> {})",
            first["user"],
            second["user"]
        );
    }
}
