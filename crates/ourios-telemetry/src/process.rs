//! The upstream `OTel` process metrics (semantic conventions
//! `process.*`), observed when the reader collects rather than sampled on a
//! timer of our own.
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

use std::time::Instant;

use opentelemetry::metrics::Meter;

/// The instrumentation scope the process metrics are recorded under.
pub(crate) const SCOPE: &str = "ourios.process";

/// Register the process metrics on `meter`. The callbacks live as long as
/// the meter provider does, so the returned instruments need not be kept.
pub(crate) fn register(meter: &Meter) {
    // The monotonic clock carries the precision; the kernel's record of when
    // the process started (Linux) adds the time spent before this call.
    let registered = Instant::now();
    let ran_before = ran_before_registration().unwrap_or(0.0);
    meter
        .f64_observable_gauge("process.uptime")
        .with_unit("s")
        .with_description("The time the process has been running.")
        .with_callback(move |observer| {
            observer.observe(ran_before + registered.elapsed().as_secs_f64(), &[]);
        })
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

/// Seconds between the process starting and now, from `/proc`.
#[cfg(target_os = "linux")]
fn ran_before_registration() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let since_boot = std::fs::read_to_string("/proc/uptime").ok()?;
    procfs::seconds_since_start(&stat, &since_boot, rustix::param::clock_ticks_per_second())
}

#[cfg(not(target_os = "linux"))]
fn ran_before_registration() -> Option<f64> {
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

    pub(super) fn register(meter: &Meter) {
        meter
            .i64_observable_up_down_counter("process.memory.usage")
            .with_unit("By")
            .with_description("The amount of physical memory in use.")
            .with_callback(|observer| {
                if let Some(bytes) = status_field("VmRSS:")
                    .as_deref()
                    .and_then(super::procfs::kib_to_bytes)
                {
                    observer.observe(bytes, &[]);
                }
            })
            .build();

        meter
            .i64_observable_up_down_counter("process.thread.count")
            .with_unit("{thread}")
            .with_description("Process threads count.")
            .with_callback(|observer| {
                if let Some(threads) = status_field("Threads:").and_then(|v| v.parse().ok()) {
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
                let Some(cpu) = std::fs::read_to_string("/proc/self/stat")
                    .ok()
                    .and_then(|stat| super::procfs::cpu_ticks(&stat))
                else {
                    return;
                };
                observer.observe(super::procfs::ticks_to_seconds(cpu.user, ticks), &user);
                observer.observe(super::procfs::ticks_to_seconds(cpu.system, ticks), &system);
            })
            .build();
    }

    /// The value of one `/proc/self/status` line, e.g. `VmRSS:` → `"1234 kB"`.
    fn status_field(key: &str) -> Option<String> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        super::procfs::field(&status, key).map(str::to_owned)
    }
}

/// The `/proc` text parsers. Only Linux reads `/proc`, but the parsers are
/// plain functions over text, so their tests run on every host.
#[cfg(any(target_os = "linux", test))]
mod procfs {
    /// The value after `key` on the first line that starts with it.
    pub(super) fn field<'a>(status: &'a str, key: &str) -> Option<&'a str> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
    }

    /// `"1234 kB"`, as `/proc` reports sizes, in bytes.
    pub(super) fn kib_to_bytes(value: &str) -> Option<i64> {
        let kib: i64 = value.strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct CpuTicks {
        pub(super) user: u64,
        pub(super) system: u64,
    }

    /// `utime` and `stime` from a `/proc/<pid>/stat` line. The command name
    /// in field 2 is parenthesised and may itself contain spaces and `)`, so
    /// the fields are counted from the last `)`.
    pub(super) fn cpu_ticks(stat: &str) -> Option<CpuTicks> {
        let (_, rest) = stat.rsplit_once(')')?;
        // `rest` starts at field 3 (`state`); `utime` and `stime` are 14 and 15.
        let mut fields = rest.split_whitespace().skip(11);
        let user = fields.next()?.parse().ok()?;
        let system = fields.next()?.parse().ok()?;
        Some(CpuTicks { user, system })
    }

    // Tick counts stay far below 2^52, where an f64 would start to round.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn ticks_to_seconds(ticks: u64, per_second: u64) -> f64 {
        ticks as f64 / per_second.max(1) as f64
    }

    /// How long the process has run: the system uptime (`/proc/uptime`)
    /// less the process's `starttime` (field 22 of its `stat`, in ticks
    /// since boot).
    pub(super) fn seconds_since_start(stat: &str, uptime: &str, per_second: u64) -> Option<f64> {
        let (_, rest) = stat.rsplit_once(')')?;
        let start_ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
        let since_boot: f64 = uptime.split_whitespace().next()?.parse().ok()?;
        Some((since_boot - ticks_to_seconds(start_ticks, per_second)).max(0.0))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cpu_ticks_count_fields_from_the_last_parenthesis() {
            let stat = "4242 (ourios (x) y) S 1 4242 4242 0 -1 4194560 1043 0 0 0 \
                        731 214 0 0 20 0 9 0 12345 1000 200 18446744073709551615";
            assert_eq!(
                cpu_ticks(stat),
                Some(CpuTicks {
                    user: 731,
                    system: 214
                })
            );
            assert_eq!(cpu_ticks("4242 (truncated) S 1"), None);
        }

        #[test]
        fn status_fields_parse_rss_and_threads() {
            let status =
                "Name:\tourios-server\nVmHWM:\t  9000 kB\nVmRSS:\t   4096 kB\nThreads:\t17\n";
            assert_eq!(
                field(status, "VmRSS:").and_then(kib_to_bytes),
                Some(4096 * 1024)
            );
            assert_eq!(field(status, "Threads:"), Some("17"));
            assert_eq!(field(status, "VmSwap:"), None);
            assert_eq!(kib_to_bytes("12 MB"), None);
        }

        #[test]
        fn ticks_convert_at_the_kernel_rate() {
            assert!((ticks_to_seconds(250, 100) - 2.5).abs() < f64::EPSILON);
        }

        #[test]
        fn uptime_is_system_uptime_less_the_start_tick() {
            let stat = "4242 (ourios (x) y) S 1 4242 4242 0 -1 4194560 1043 0 0 0 \
                        731 214 0 0 20 0 9 0 12345 1000 200 18446744073709551615";
            let ran = seconds_since_start(stat, "223.45 880.10\n", 100).expect("parses");
            assert!((ran - 100.0).abs() < 1e-9, "got {ran}");
            assert_eq!(seconds_since_start(stat, "", 100), None);
            assert_eq!(seconds_since_start("1 (x) S 1", "5.0 1.0", 100), None);
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
