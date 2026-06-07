//! The Ourios logs query DSL (RFC 0002, Branch B / surface β).
//!
//! Two front-ends, one core (RFC 0002 §6.4): the string DSL ([`parse`]) and
//! the structured JSON surface ([`parse_structured`]) both produce the shared
//! [`ir::Query`]; [`serialize`] renders it back to a canonical single-line β
//! string that round-trips (`parse(serialize(q)) == q`, RFC0002.7) and is a
//! YAML-safe scalar (RFC0002.10).
//!
//! No `datafusion`/`arrow`/SQL type or message crosses this surface — the
//! whole point of the DSL (hazard `CLAUDE.md` §4.6). Errors are the
//! Ourios-owned [`DslError`].

pub mod ir;

mod display;
mod parse;
mod structured;

pub use display::serialize;
pub use ir::Query;
pub use parse::parse;
pub use structured::parse_structured;

pub(crate) use parse::{parse_severity_name_pub, parse_time_pub};

/// An error from parsing the logs DSL (either front-end). Hand-rolled (no
/// `thiserror`, matching the repo's `QueryError`/`TokenizeError` style); the
/// message cites the offending token/clause and never names a
/// `datafusion`/`arrow`/SQL construct (hazard `CLAUDE.md` §4.6 / RFC0002.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DslError {
    message: String,
}

impl DslError {
    pub(crate) fn new(message: String) -> Self {
        Self { message }
    }

    /// The operator-facing message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for DslError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid query: {}", self.message)
    }
}

impl std::error::Error for DslError {}
