//! The Ourios logs query DSL (RFC 0002, Branch B / surface β).
//!
//! Two front-ends, one core (RFC 0002 §6.4): the string DSL ([`parse`]) and
//! the structured JSON surface ([`parse_structured`]) both produce the shared
//! [`ir::Query`]; [`serialize`] renders it back to a canonical single-line β
//! string. Round-tripping (`parse(serialize(q)) == q`, RFC0002.7) holds for any
//! `Query` produced by [`parse`] / [`parse_structured`]: those parsers
//! canonicalise associative `and`/`or` (flatten same-kind nesting, collapse
//! single-element lists), so a hand-built non-canonical IR may serialise to a
//! string that re-parses to a *different* (canonical) shape. The serialised
//! form is a YAML-safe scalar (RFC0002.10).
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

/// The published JSON Schema (draft 2020-12) for the structured query surface
/// (RFC 0002 §6.4). Versioned alongside the parser and snapshot-tested so any
/// drift between the surface the parser accepts and its advertised contract is
/// PR-visible (RFC0002.11 / §6.6). Agents fetch this to constrain or validate
/// requests before sending them; the planner re-validates on receipt.
#[must_use]
pub fn structured_query_schema() -> &'static str {
    include_str!("structured_query.schema.json")
}

pub(crate) use parse::{
    parse_severity_name_pub, parse_time_pub, require_string_operand, validate_sort_key,
};

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
