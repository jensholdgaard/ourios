//! Explicit bucket boundaries shared by Ourios's histograms.
//!
//! A histogram built without `with_boundaries` gets the SDK default
//! `[0, 5, 10, …, 10000]`, which is shaped for milliseconds: every
//! seconds-scale duration, 0–1 ratio or byte size lands in one bucket and
//! its percentiles are meaningless (#949). Every histogram declares its
//! boundaries; the ones more than one crate needs live here.

/// The semantic conventions' advised boundaries for a `*.duration` in
/// seconds (`http.client.request.duration`, `rpc.server.call.duration`):
/// operations that take milliseconds to seconds.
pub const DURATION_S: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// [`DURATION_S`] extended down to 100 µs, for operations that are usually
/// sub-millisecond but can stall on an fsync (a WAL append).
pub const FAST_DURATION_S: [f64; 19] = [
    0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5,
    0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// [`DURATION_S`] extended up to an hour, for operations that can run for
/// minutes (a compaction sweep, a wide query).
pub const LONG_DURATION_S: [f64; 21] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0, 30.0, 60.0,
    120.0, 300.0, 600.0, 1_800.0, 3_600.0,
];

/// Powers of four from 64 KiB to 4 GiB, for byte sizes of Parquet files and
/// of the reads over them.
pub const SIZE_BYTES: [f64; 9] = [
    65_536.0,        // 64 KiB
    262_144.0,       // 256 KiB
    1_048_576.0,     // 1 MiB
    4_194_304.0,     // 4 MiB
    16_777_216.0,    // 16 MiB
    67_108_864.0,    // 64 MiB
    268_435_456.0,   // 256 MiB
    1_073_741_824.0, // 1 GiB
    4_294_967_296.0, // 4 GiB
];

/// File counts from none to thousands, dense at the low end where a
/// well-pruned query or a small compaction partition sits.
pub const FILE_COUNT: [f64; 11] = [
    0.0, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 1_000.0, 5_000.0,
];

/// Decades up to a million, for counts whose spread is orders of magnitude
/// (a whole-prefix object listing, an audit set read in full).
pub const COUNT_DECADES: [f64; 8] = [
    0.0,
    1.0,
    10.0,
    100.0,
    1_000.0,
    10_000.0,
    100_000.0,
    1_000_000.0,
];

#[cfg(test)]
mod tests {
    use super::*;

    const SDK_DEFAULT: [f64; 15] = [
        0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1_000.0, 2_500.0, 5_000.0,
        7_500.0, 10_000.0,
    ];

    #[test]
    fn every_bucket_set_is_strictly_increasing_and_not_the_sdk_default() {
        for (name, bounds) in [
            ("DURATION_S", &DURATION_S[..]),
            ("FAST_DURATION_S", &FAST_DURATION_S[..]),
            ("LONG_DURATION_S", &LONG_DURATION_S[..]),
            ("SIZE_BYTES", &SIZE_BYTES[..]),
            ("FILE_COUNT", &FILE_COUNT[..]),
            ("COUNT_DECADES", &COUNT_DECADES[..]),
        ] {
            assert!(
                bounds.windows(2).all(|w| w[0] < w[1]),
                "{name} must be strictly increasing",
            );
            assert_ne!(bounds, &SDK_DEFAULT[..], "{name} is the SDK default");
        }
    }

    #[test]
    fn the_extended_duration_sets_contain_the_advised_one() {
        for extended in [&FAST_DURATION_S[..], &LONG_DURATION_S[..]] {
            assert!(
                extended
                    .windows(DURATION_S.len())
                    .any(|w| w == &DURATION_S[..])
            );
        }
    }
}
