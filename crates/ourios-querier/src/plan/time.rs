//! The §7 `time` grammar resolution: `now`, signed durations, and
//! RFC 3339 timestamps to absolute nanoseconds. Shared with the
//! RFC 0010 drift path. Split from the flat compile module (epic
//! #745 wave 3).

#[allow(clippy::wildcard_imports)] // parent glue after the file split
use super::*;

/// Resolve a §7 [`Time`] bound to absolute nanoseconds against `now`. Shared
/// with the RFC 0010 drift path (`crate::drift`), which reuses the same `time`
/// grammar for its window (RFC 0010 §6.5).
pub(crate) fn resolve_time(time: &Time, now: u64) -> Result<u64, QueryError> {
    match time {
        Time::Now => Ok(now),
        Time::Duration { neg, literal } => {
            let d = duration_nanos(literal)?;
            if *neg {
                Ok(now.saturating_sub(d))
            } else {
                Ok(now.saturating_add(d))
            }
        }
        Time::Timestamp(s) => timestamp_nanos(s),
    }
}

/// Parse a `<int><unit>` duration lexeme (the parser already validated its
/// shape) into nanoseconds.
pub(super) fn duration_nanos(literal: &str) -> Result<u64, QueryError> {
    let invalid = || QueryError::InvalidQuery {
        detail: format!("duration {literal:?} is not resolvable"),
    };
    let (digits, unit) = literal.split_at(literal.len().checked_sub(1).ok_or_else(invalid)?);
    let n: u64 = digits.parse().map_err(|_| invalid())?;
    let per_unit = match unit {
        "s" => NS_PER_SECOND,
        "m" => 60 * NS_PER_SECOND,
        "h" => 3_600 * NS_PER_SECOND,
        "d" => 86_400 * NS_PER_SECOND,
        "w" => 7 * 86_400 * NS_PER_SECOND,
        _ => return Err(invalid()),
    };
    n.checked_mul(per_unit).ok_or_else(invalid)
}

/// Resolve an RFC 3339 timestamp lexeme to nanoseconds since the epoch.
pub(super) fn timestamp_nanos(s: &str) -> Result<u64, QueryError> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| QueryError::InvalidQuery {
        detail: format!("timestamp {s:?} is not a resolvable RFC 3339 instant"),
    })?;
    let ns = dt
        .timestamp_nanos_opt()
        .ok_or_else(|| QueryError::InvalidQuery {
            detail: format!("timestamp {s:?} is out of the representable range"),
        })?;
    u64::try_from(ns).map_err(|_| QueryError::InvalidQuery {
        detail: format!("timestamp {s:?} predates the epoch"),
    })
}
