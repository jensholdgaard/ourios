#![no_main]

//! RFC0015.8 / RFC0015.15 — the RFC 0002 DSL from untrusted query text
//! down to the `DataFusion` logical plan. `/v1/query` (RFC 0016) and the
//! MCP surface (RFC 0027) hand the parsers raw client bytes, so garbage
//! must come back as a typed `DslError`, never a panic, abort, or UB. Both
//! grammars share the oracle: the pipe-text parser and the structured-JSON
//! variant get the same input (each rejects the other's happy path as
//! garbage, which is exactly the point). The statement entry points are
//! used so the RFC 0010 `drift` head is covered too.
//!
//! What either parser accepts must round-trip through the canonical
//! serialiser (RFC0002.7): `parse_statement(serialize_statement(s)) == s`.
//! The parsers canonicalise associative `and`/`or` on the way in, so the
//! IR they produce is exactly the shape the serialiser promises to
//! reproduce (the proptest in `dsl/structured.rs` pins the same contract
//! over generated trees).
//!
//! An accepted log query then goes where `run_query` takes it before any
//! IO (the querier's `fuzzing` feature): `plan::validate`, then the
//! compile, the filter and aggregation lowering and `DataFusion`'s
//! analyzer and optimizer over an empty in-memory scan, against the
//! current writer's schema (one promoted key per RFC 0042 class) and a
//! scan with every OPTIONAL column absent (RFC0007.4). Nothing executes.
//! RFC0015.15 asserts:
//!
//! - validation accepts or rejects with `InvalidQuery`, the DSL's own
//!   error (RFC 0002 §6.5);
//! - lowering returns exactly validation's rejection, and lowers what
//!   validation accepts to a plan or an `InvalidQuery`. A `Storage` error
//!   there is a `DataFusion` planning error the compiler let through:
//!   hazard `CLAUDE.md` §4.6, the engine deciding what the DSL accepts,
//!   reported to the user as a failed storage read;
//! - the canonical re-parse validates and lowers to the same verdict and
//!   an equal plan: lowering is a function of the query alone.
//!
//! Lowering only runs on accepted queries, a small share of the
//! executions, so it shares this target with the parsers (one seed corpus,
//! one dictionary) instead of costing a binary of its own.

use libfuzzer_sys::fuzz_target;
use ourios_querier::QueryError;
use ourios_querier::dsl::{
    Query, Statement, parse_statement, parse_structured_statement, serialize_statement,
};
use ourios_querier::fuzzing::{self, LoweredPlan, ScanSchema};

#[path = "../common/telemetry.rs"]
mod telemetry;

type Lowering = Result<Option<LoweredPlan>, QueryError>;

fn round_trip(scenario: &str, statement: &Statement) -> Statement {
    let canonical = serialize_statement(statement);
    let reparsed = parse_statement(&canonical).unwrap_or_else(|e| {
        panic!("{scenario} / RFC0002.7: canonical form {canonical:?} does not parse: {e}")
    });
    assert!(
        &reparsed == statement,
        "{scenario} / RFC0002.7: canonical form {canonical:?} re-parses to a different query\n  \
         parsed:   {statement:?}\n  reparsed: {reparsed:?}",
    );
    reparsed
}

fn assert_dsl_error(scenario: &str, query: &Query, error: &QueryError) {
    assert!(
        matches!(error, QueryError::InvalidQuery { .. }),
        "RFC0015.15 / CLAUDE.md §4.6 ({scenario}): {query:?} is rejected with {error:?}, \
         not the DSL's own InvalidQuery",
    );
}

fn same_lowering(a: &Lowering, b: &Lowering) -> bool {
    match (a, b) {
        (Ok(a), Ok(b)) => a == b,
        (Err(a), Err(b)) => format!("{a:?}") == format!("{b:?}"),
        _ => false,
    }
}

fn assert_lowers(query: &Query, verdict: &Result<(), QueryError>, schema: ScanSchema) -> Lowering {
    let lowered = fuzzing::lower(query, schema);
    match (verdict, &lowered) {
        (Err(rejected), Ok(_)) => panic!(
            "RFC0015.15 ({schema:?}): validation rejects {query:?} with {rejected:?}, \
             but it lowers"
        ),
        (Err(rejected), Err(error)) => assert!(
            format!("{rejected:?}") == format!("{error:?}"),
            "RFC0015.15 ({schema:?}): validation rejects {query:?} with {rejected:?}, \
             lowering with {error:?}",
        ),
        (Ok(()), Err(error)) => assert_dsl_error("lowering", query, error),
        (Ok(()), Ok(_)) => {}
    }
    lowered
}

fn assert_plans(query: &Query, reparsed: &Query) {
    let verdict = fuzzing::validate(query);
    if let Err(error) = &verdict {
        assert_dsl_error("validation", query, error);
    }
    let again = fuzzing::validate(reparsed);
    assert!(
        format!("{verdict:?}") == format!("{again:?}"),
        "RFC0015.15: {query:?} validates to {verdict:?}, its canonical re-parse to {again:?}",
    );
    for schema in [ScanSchema::Current, ScanSchema::RequiredOnly] {
        let lowered = assert_lowers(query, &verdict, schema);
        let relowered = fuzzing::lower(reparsed, schema);
        assert!(
            same_lowering(&lowered, &relowered),
            "RFC0015.15 ({schema:?}): {query:?} and its canonical re-parse lower differently\n  \
             lowered:   {lowered:?}\n  relowered: {relowered:?}",
        );
    }
}

fn check(scenario: &str, statement: &Statement) {
    let reparsed = round_trip(scenario, statement);
    if let (Statement::Logs(query), Statement::Logs(reparsed)) = (statement, &reparsed) {
        assert_plans(query, reparsed);
    }
}

// A typed `&str` target: libFuzzer produces valid UTF-8 directly, so no
// execution is wasted on byte sequences the real boundary cannot deliver
// (axum/MCP hand the parsers `String`s, so invalid UTF-8 is rejected
// upstream of them in production).
fuzz_target!(init: telemetry::init(), |text: &str| {
    if let Ok(statement) = parse_statement(text) {
        check("RFC0015.8 (pipe text)", &statement);
    }
    if let Ok(statement) = parse_structured_statement(text) {
        check("RFC0015.8 (structured JSON)", &statement);
    }
});
