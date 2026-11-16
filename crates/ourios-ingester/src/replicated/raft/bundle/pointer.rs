//! `checkpoint.json` (RFC 0065 §6.5): the latest cut a flusher completed
//! a bundle for, which a querier starts from. Recovery never reads it.

use std::fmt;

use serde::Deserialize;

use super::CutId;

const FORMAT: u64 = 1;

/// `checkpoint.json`'s key, under the log id rather than the group
/// number, so a new log incarnation never loses to an old one's `seq`.
#[must_use]
pub fn key(log: u32) -> String {
    format!("wal/replicated/logs/{log}/checkpoint.json")
}

/// The object's fields, all of them required and no others allowed: a
/// writer that replaced an object with a field it does not know would
/// drop it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    // Read through `Header`; kept here so the object must carry it.
    #[expect(dead_code, reason = "required field, checked through the header")]
    format: u64,
    log: u32,
    seq: u64,
    term: u64,
    bundle: String,
}

/// Why a stored `checkpoint.json` is unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PointerError {
    /// Not the object's JSON: unparsable, a field missing, unknown or
    /// repeated, or a number out of range.
    Malformed(String),
    /// A format this release does not read.
    Format(u64),
    /// The object of another log.
    Log { found: u32, expected: u32 },
    /// A `bundle` that is not the prefix `log` and `seq` name.
    Bundle(String),
}

impl fmt::Display for PointerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "checkpoint.json malformed: {detail}"),
            Self::Format(format) => write!(f, "checkpoint.json format {format}, not {FORMAT}"),
            Self::Log { found, expected } => {
                write!(f, "checkpoint.json of log {found}, not {expected}")
            }
            Self::Bundle(bundle) => {
                write!(
                    f,
                    "checkpoint.json bundle {bundle:?} is not its cut's prefix"
                )
            }
        }
    }
}

impl std::error::Error for PointerError {}

/// The object naming `cut` and, by it, the bundle's prefix.
#[must_use]
pub fn encode(cut: CutId) -> Vec<u8> {
    let CutId { log, seq, term } = cut;
    format!(
        r#"{{"format":{FORMAT},"log":{log},"seq":{seq},"term":{term},"bundle":"{}"}}"#,
        super::prefix(log, seq)
    )
    .into_bytes()
}

/// The cut `bytes`, log `log`'s stored object, names.
///
/// # Errors
///
/// The [`PointerError`] naming why the object is unreadable.
pub fn decode(bytes: &[u8], log: u32) -> Result<CutId, PointerError> {
    // The format is read on its own first: another format may hold other
    // fields, which is a version mismatch, not a malformed object.
    #[derive(Deserialize)]
    struct Header {
        format: u64,
    }
    let malformed = |e: serde_json::Error| PointerError::Malformed(e.to_string());
    // Derived structs also read a JSON array positionally.
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(malformed)?;
    if !value.is_object() {
        return Err(PointerError::Malformed("not a JSON object".to_owned()));
    }
    let header: Header = serde_json::from_slice(bytes).map_err(malformed)?;
    if header.format != FORMAT {
        return Err(PointerError::Format(header.format));
    }
    let object: Object = serde_json::from_slice(bytes).map_err(malformed)?;
    if object.log != log {
        return Err(PointerError::Log {
            found: object.log,
            expected: log,
        });
    }
    if object.bundle != super::prefix(object.log, object.seq) {
        return Err(PointerError::Bundle(object.bundle));
    }
    Ok(CutId {
        log: object.log,
        seq: object.seq,
        term: object.term,
    })
}
