//! Partition path derivation per RFC 0005 §3.4.
//!
//! Two surfaces:
//!
//! - [`PartitionKey`] — `(tenant_id, year, month, day, hour)`
//!   derived from a [`MinedRecord`] via the §3.4 time-fallback
//!   algorithm (prefer `time_unix_nano`; if zero, fall back to
//!   `observed_time_unix_nano`; if also zero / absent, the
//!   1970-01-01T00 epoch). The same algorithm runs on the writer
//!   side (this module) and the reader side (§3.9 row-vs-path
//!   validation) so a record placed under one bucket validates
//!   under the same bucket.
//!
//! - [`percent_encode_tenant`] — RFC 3986 percent-encoding with
//!   the §3.4 overrides (UTF-8 bytes verbatim, every byte outside
//!   the unreserved set is escaped including `/`, `=`, `%`).

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Timelike, Utc};
use ourios_core::record::MinedRecord;

/// Partition key for the on-disk Hive-style layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionKey {
    /// Raw (un-encoded) tenant id, as carried on the row.
    pub tenant_id: String,
    /// UTC calendar year. Negative values are valid in
    /// `chrono`'s proleptic Gregorian calendar but never appear
    /// from a `u64` nanos input (the earliest representable date
    /// is the epoch). Kept as `i32` to match `chrono::Datelike`.
    pub year: i32,
    /// UTC month, 1..=12.
    pub month: u32,
    /// UTC day of month, 1..=31.
    pub day: u32,
    /// UTC hour, 0..=23.
    pub hour: u32,
}

/// Error returned when a record's nanosecond timestamp cannot
/// fit in the `i64` physical type Parquet's `TIMESTAMP(NANOS)`
/// uses (RFC 0005 §3.2's `u64`→`i64` overflow contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampOverflowError {
    /// Which field overflowed (`time_unix_nano` or
    /// `observed_time_unix_nano`).
    pub field: &'static str,
    /// The offending `u64` value.
    pub value: u64,
}

impl fmt::Display for TimestampOverflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} = {} exceeds i64::MAX (year 2262 boundary per RFC 0005 §3.2 overflow contract)",
            self.field, self.value,
        )
    }
}

impl std::error::Error for TimestampOverflowError {}

impl PartitionKey {
    /// Derive a partition key from a [`MinedRecord`] per
    /// RFC 0005 §3.4 (with the time-fallback rule).
    ///
    /// # Errors
    ///
    /// Returns [`TimestampOverflowError`] if the chosen nanosecond
    /// timestamp exceeds `i64::MAX` — the writer-rejects-overflow
    /// contract from RFC 0005 §3.2.
    pub fn derive(record: &MinedRecord) -> Result<Self, TimestampOverflowError> {
        let chosen = choose_partition_timestamp(record)?;
        // chosen is already i64-safe (checked above); `chrono::
        // DateTime::from_timestamp_nanos` accepts i64.
        let dt = DateTime::<Utc>::from_timestamp_nanos(chosen);
        Ok(Self {
            tenant_id: record.tenant_id.as_str().to_owned(),
            year: dt.year(),
            month: dt.month(),
            day: dt.day(),
            hour: dt.hour(),
        })
    }

    /// Absolute path to the data-file partition directory under
    /// `bucket_root` per §3.4:
    /// `<bucket_root>/data/tenant_id=<urlenc>/year=YYYY/month=MM/day=DD/hour=HH/`.
    ///
    /// The directory does not include the file name; the writer
    /// appends `<flush_uuid>.parquet` per §3.4.
    #[must_use]
    pub fn data_path(&self, bucket_root: &Path) -> PathBuf {
        let mut p = bucket_root.to_path_buf();
        p.push("data");
        p.push(format!(
            "tenant_id={}",
            percent_encode_tenant(&self.tenant_id)
        ));
        p.push(format!("year={:04}", self.year));
        p.push(format!("month={:02}", self.month));
        p.push(format!("day={:02}", self.day));
        p.push(format!("hour={:02}", self.hour));
        p
    }

    /// Absolute path to the audit-file partition directory under
    /// `bucket_root` per §3.4. Audit partitioning stops at `day`
    /// (no `hour` segment) per §3.4's "audit volume is far lower"
    /// rationale.
    #[must_use]
    pub fn audit_path(&self, bucket_root: &Path) -> PathBuf {
        let mut p = bucket_root.to_path_buf();
        p.push("audit");
        p.push(format!(
            "tenant_id={}",
            percent_encode_tenant(&self.tenant_id)
        ));
        p.push(format!("year={:04}", self.year));
        p.push(format!("month={:02}", self.month));
        p.push(format!("day={:02}", self.day));
        p
    }
}

/// Choose the nanosecond timestamp for partition derivation per
/// §3.4: prefer `time_unix_nano` if non-zero, else
/// `observed_time_unix_nano` if non-zero, else the 1970 epoch
/// (returned as `0_i64`). Each candidate is checked against the
/// `u64`→`i64` overflow contract before being adopted.
fn choose_partition_timestamp(record: &MinedRecord) -> Result<i64, TimestampOverflowError> {
    if record.time_unix_nano != 0 {
        return i64::try_from(record.time_unix_nano).map_err(|_| TimestampOverflowError {
            field: "time_unix_nano",
            value: record.time_unix_nano,
        });
    }
    if let Some(observed) = record.observed_time_unix_nano
        && observed != 0
    {
        return i64::try_from(observed).map_err(|_| TimestampOverflowError {
            field: "observed_time_unix_nano",
            value: observed,
        });
    }
    Ok(0)
}

/// Percent-encode a tenant id per RFC 0005 §3.4: input is the
/// UTF-8 byte sequence; the unreserved set
/// (`A-Z` `a-z` `0-9` `-` `_` `.` `~`) passes through unchanged;
/// every other byte (including `/`, `=`, `%`) is escaped as
/// `%XX` with upper-case hex.
#[must_use]
pub fn percent_encode_tenant(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if matches!(
            b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~'
        ) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_nibble(b >> 4));
            out.push(hex_nibble(b & 0x0F));
        }
    }
    out
}

const fn hex_nibble(n: u8) -> char {
    // Callers pass `b >> 4` or `b & 0x0F` from a `u8`, both of
    // which produce values in `0..=15`. The fallback arm is
    // structurally unreachable; `unreachable!()` fails loudly if
    // a future caller breaks that contract rather than emitting
    // a sentinel that would silently produce malformed
    // percent-encoding.
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_record(tenant: &str, ts: u64) -> MinedRecord {
        use ourios_core::record::BodyKind;
        use ourios_core::tenant::TenantId;
        MinedRecord {
            tenant_id: TenantId::new(tenant),
            template_id: 0,
            template_version: 0,
            severity_number: 0,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            time_unix_nano: ts,
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
            // Minimum-valid clean-attach String shape:
            // `separators.len() = params.len() + 1 = 1`. The
            // partition tests don't exercise the writer, but
            // keeping fixtures invariant-consistent prevents
            // accidental reuse breakage if they ever do.
            separators: vec![String::new()],
            body: None,
            confidence: 0.0,
            lossy_flag: false,
        }
    }

    #[test]
    fn percent_encode_passes_unreserved_through() {
        assert_eq!(percent_encode_tenant("tenant-id_42.~"), "tenant-id_42.~");
    }

    #[test]
    fn percent_encode_escapes_path_delimiters() {
        // `/` `=` `%` are the §3.4-named always-escape bytes.
        assert_eq!(percent_encode_tenant("a/b"), "a%2Fb");
        assert_eq!(percent_encode_tenant("k=v"), "k%3Dv");
        assert_eq!(percent_encode_tenant("100%"), "100%25");
    }

    #[test]
    fn percent_encode_escapes_non_ascii_utf8_bytes() {
        // U+00E5 = LATIN SMALL LETTER A WITH RING ABOVE (`å`),
        // UTF-8 = 0xC3 0xA5. Verifies "input is the UTF-8 byte
        // sequence" (§3.4) — no Unicode normalisation, just bytes.
        assert_eq!(percent_encode_tenant("å"), "%C3%A5");
    }

    #[test]
    fn derive_uses_time_unix_nano_when_set() {
        // 2026-04-02T10:58:00Z = 1_775_127_480 s × 1e9 ns
        let ts_ns = 1_775_127_480_000_000_000_u64;
        let key = PartitionKey::derive(&empty_record("t", ts_ns)).unwrap();
        assert_eq!(key.year, 2026);
        assert_eq!(key.month, 4);
        assert_eq!(key.day, 2);
        assert_eq!(key.hour, 10);
    }

    #[test]
    fn derive_falls_back_to_observed_time_when_time_is_zero() {
        let mut rec = empty_record("t", 0);
        rec.observed_time_unix_nano = Some(1_775_127_480_000_000_000);
        let key = PartitionKey::derive(&rec).unwrap();
        assert_eq!(key.year, 2026);
        assert_eq!(key.hour, 10);
    }

    #[test]
    fn derive_lands_on_epoch_when_both_zero() {
        let key = PartitionKey::derive(&empty_record("t", 0)).unwrap();
        assert_eq!(key.year, 1970);
        assert_eq!(key.month, 1);
        assert_eq!(key.day, 1);
        assert_eq!(key.hour, 0);
    }

    #[test]
    fn derive_rejects_time_unix_nano_overflow() {
        // u64::MAX > i64::MAX → reject per RFC 0005 §3.2.
        let err = PartitionKey::derive(&empty_record("t", u64::MAX)).unwrap_err();
        assert_eq!(err.field, "time_unix_nano");
        assert_eq!(err.value, u64::MAX);
    }

    #[test]
    fn data_path_layout_matches_section_3_4() {
        let key = PartitionKey {
            tenant_id: "tenant-x".to_string(),
            year: 2026,
            month: 4,
            day: 2,
            hour: 10,
        };
        let bucket = PathBuf::from("bucket");
        let path = key.data_path(&bucket);
        // Component-wise comparison so the test stays portable
        // across OS path separators (Unix `/` vs Windows `\`).
        let expected: PathBuf = [
            "bucket",
            "data",
            "tenant_id=tenant-x",
            "year=2026",
            "month=04",
            "day=02",
            "hour=10",
        ]
        .iter()
        .collect();
        assert_eq!(path, expected);
    }

    #[test]
    fn audit_path_stops_at_day() {
        let key = PartitionKey {
            tenant_id: "tenant-x".to_string(),
            year: 2026,
            month: 4,
            day: 2,
            hour: 10,
        };
        let bucket = PathBuf::from("bucket");
        let path = key.audit_path(&bucket);
        let expected: PathBuf = [
            "bucket",
            "audit",
            "tenant_id=tenant-x",
            "year=2026",
            "month=04",
            "day=02",
        ]
        .iter()
        .collect();
        assert_eq!(path, expected);
    }
}
