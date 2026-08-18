//! RFC 0047 §3.4 — layer-2 visibility as the engine sees it: a decision the
//! caller already made against the authorization graph, applied here as
//! **query rewrite at plan time** — an extra predicate over a promoted
//! column, or column masking on the returned rows — never as per-record
//! checks. The engine knows nothing about `OpenFGA`; it receives one of
//! three shapes and applies it.

use datafusion::common::Column;
use datafusion::dataframe::DataFrame;
use datafusion::logical_expr::Expr;
use datafusion::prelude::lit;

use crate::dsl::ir::{Call, Field, GroupTerm, Predicate, Query, Stage};
use crate::log_row::{LogBody, LogRow};
use crate::{QueryError, has_column};
use ourios_parquet::promoted;

/// What the principal may see inside the tenant (RFC 0047 §3.4). Column
/// names are DSL names: `body`, `attr.<key>`, `resource.<key>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// Step 1 allowed: the tenant predicate only — today's plan, unchanged.
    TenantWide,
    /// Step 2 allowed: every row of the tenant, with `content_columns`
    /// masked on the way out; a query that filters or aggregates on one of
    /// them is rejected (`403`, named column) rather than answered.
    Masked {
        /// The columns a metadata-only reader may not read.
        content_columns: Vec<String>,
    },
    /// Step 3: only rows whose conversation is one the principal may read
    /// — OR'd with the §3.3 self fast path when configured. No bound
    /// object type, or an empty id set, and no self match ⇒ an empty
    /// result, not an error.
    Scoped {
        /// The enumerated conversations over their promoted column; `None`
        /// when no object type is bound (nothing to enumerate).
        conversations: Option<ScopedIds>,
        /// The self fast path: rows whose `column` equals `value`.
        self_match: Option<SelfMatch>,
    },
}

/// The enumerated object ids of a scoped principal and the promoted
/// column they are matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedIds {
    /// The promoted column carrying the object ids (`attr.<key>`).
    pub column: String,
    /// The ids the principal may read (prefix stripped).
    pub ids: Vec<String>,
}

/// The §3.3 self fast path: `<column> == <value>`, where `value` is a
/// `user:` principal's subject with the prefix stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfMatch {
    /// The promoted column carrying the principal identity.
    pub column: String,
    /// The principal's subject.
    pub value: String,
}

impl Visibility {
    /// Reject a query that *reads* a masked column (RFC0047.8): any content
    /// column in the predicate, an aggregation path, or a `by`-list. A
    /// projection is not a read — a projected content column comes back
    /// masked, like every returned row's. Never an oracle about the data —
    /// the columns are configuration.
    ///
    /// # Errors
    ///
    /// [`QueryError::Forbidden`] naming the first masked column referenced.
    pub(crate) fn validate(&self, query: &Query) -> Result<(), QueryError> {
        let Self::Masked { content_columns } = self else {
            return Ok(());
        };
        let mut fields: Vec<&Field> = Vec::new();
        collect_predicate_fields(&query.predicate, &mut fields);
        for stage in &query.stages {
            match stage {
                Stage::Count { by } => collect_group_fields(by, &mut fields),
                Stage::Agg { path, by, .. } => {
                    fields.push(path);
                    collect_group_fields(by, &mut fields);
                }
                Stage::Range(..)
                | Stage::Sort { .. }
                | Stage::Limit(_)
                | Stage::Project(_)
                | Stage::Render => {}
            }
        }
        for field in fields {
            if let Some(name) = dsl_name(field)
                && content_columns.contains(&name)
            {
                return Err(QueryError::Forbidden { column: name });
            }
        }
        Ok(())
    }

    /// The plan-time filter for a scoped principal: `column IN (ids)` OR
    /// the self fast path, over the promoted columns. [`VisibilityFilter::Nothing`]
    /// when the principal can see no row at all (nothing to enumerate and no
    /// fast path, or the promoted columns are absent from the scanned schema —
    /// an absent column carries no id, so it matches nothing);
    /// [`VisibilityFilter::Everything`] for the two branches that add no
    /// predicate.
    ///
    /// # Errors
    ///
    /// [`QueryError::InvalidQuery`] when a configured column is not a
    /// promoted-column name.
    pub(crate) fn filter(&self, df: &DataFrame) -> Result<VisibilityFilter, QueryError> {
        let Self::Scoped {
            conversations,
            self_match,
        } = self
        else {
            return Ok(VisibilityFilter::Everything);
        };
        let mut arms: Vec<Expr> = Vec::new();
        if let Some(ScopedIds { column, ids }) = conversations
            && !ids.is_empty()
            && let Some(promoted) = promoted_expr(df, column)?
        {
            arms.push(promoted.in_list(ids.iter().map(|id| lit(id.clone())).collect(), false));
        }
        if let Some(SelfMatch { column, value }) = self_match
            && let Some(promoted) = promoted_expr(df, column)?
        {
            arms.push(promoted.eq(lit(value.clone())));
        }
        let mut arms = arms.into_iter();
        let Some(first) = arms.next() else {
            return Ok(VisibilityFilter::Nothing);
        };
        Ok(VisibilityFilter::Only(arms.fold(first, Expr::or)))
    }

    /// Mask the content columns of returned rows (RFC0047.8): the body
    /// becomes [`LogBody::Masked`], masked attributes keep their key with
    /// the value unset — the OTLP null (`"value": null` on the JSON API).
    /// The column vocabulary (`body`, `attr.<key>`, `resource.<key>`) is
    /// validated where it is configured (`auth.openfga.visibility`), so a
    /// name of any other shape cannot reach here.
    pub(crate) fn mask(&self, rows: &mut [LogRow]) {
        let Self::Masked { content_columns } = self else {
            return;
        };
        let body = content_columns.iter().any(|column| column == "body");
        let attrs: Vec<&str> = content_columns
            .iter()
            .filter_map(|column| column.strip_prefix(promoted::ATTR_PREFIX))
            .collect();
        let resources: Vec<&str> = content_columns
            .iter()
            .filter_map(|column| column.strip_prefix(promoted::RESOURCE_PREFIX))
            .collect();
        for row in rows {
            if body {
                row.body = LogBody::Masked;
            }
            for kv in &mut row.attributes {
                if attrs.contains(&kv.key.as_str()) {
                    kv.value = None;
                }
            }
            for kv in &mut row.resource_attributes {
                if resources.contains(&kv.key.as_str()) {
                    kv.value = None;
                }
            }
        }
    }
}

/// What [`Visibility::filter`] adds to the plan.
pub(crate) enum VisibilityFilter {
    /// No visibility predicate — every row the query matches.
    Everything,
    /// The principal can see no row: the plan short-circuits to empty.
    Nothing,
    /// Rows must additionally satisfy this predicate.
    Only(Expr),
}

/// The DSL name of a field the masking vocabulary can name; `None` for
/// fields that are never content.
fn dsl_name(field: &Field) -> Option<String> {
    match field {
        Field::Body => Some("body".to_string()),
        Field::Attr(key) => Some(format!("{}{key}", promoted::ATTR_PREFIX)),
        Field::Resource(key) => Some(format!("{}{key}", promoted::RESOURCE_PREFIX)),
        _ => None,
    }
}

fn collect_predicate_fields<'a>(predicate: &'a Predicate, out: &mut Vec<&'a Field>) {
    match predicate {
        Predicate::Bool(_) | Predicate::Severity { .. } => {}
        Predicate::Comparison { field, .. } => out.push(field),
        Predicate::Call(call) => match call {
            Call::Matches { field, .. }
            | Call::Contains { field, .. }
            | Call::StartsWith { field, .. }
            | Call::EndsWith { field, .. } => out.push(field),
            Call::ResolvesTo(_) => {}
        },
        Predicate::Not(inner) => collect_predicate_fields(inner, out),
        Predicate::And(terms) | Predicate::Or(terms) => {
            for term in terms {
                collect_predicate_fields(term, out);
            }
        }
    }
}

fn collect_group_fields<'a>(by: &'a [GroupTerm], out: &mut Vec<&'a Field>) {
    for term in by {
        if let GroupTerm::Field(field) = term {
            out.push(field);
        }
    }
}

/// The unqualified column expression for a DSL `attr.<key>` /
/// `resource.<key>` name when the promoted column is in the scanned
/// schema; `Ok(None)` when absent (it matches nothing).
fn promoted_expr(df: &DataFrame, name: &str) -> Result<Option<Expr>, QueryError> {
    if !(name.starts_with(promoted::ATTR_PREFIX) || name.starts_with(promoted::RESOURCE_PREFIX)) {
        return Err(QueryError::InvalidQuery {
            detail: format!("visibility column `{name}` is not a promoted attribute column"),
        });
    }
    // The promoted column is literally named `attr.<key>` — the same string
    // as the DSL name — and must be addressed as an unqualified `Column`
    // (`col()` would parse the dots as a qualifier).
    Ok(has_column(df, name).then(|| Expr::Column(Column::new_unqualified(name))))
}

#[cfg(test)]
mod tests {
    use super::{ScopedIds, SelfMatch, Visibility};
    use crate::QueryError;
    use crate::dsl::ir::{Query, Statement};
    use crate::dsl::parse_statement;
    use crate::log_row::{LogBody, LogRow};

    fn logs(statement: &str) -> Query {
        match parse_statement(statement).expect("parses") {
            Statement::Logs(query) => query,
            Statement::Drift(_) => panic!("not a logs query"),
        }
    }

    fn masked() -> Visibility {
        Visibility::Masked {
            content_columns: vec!["body".to_string(), "attr.gen_ai.input.messages".to_string()],
        }
    }

    /// RFC0047.8: a masked column in the predicate, an aggregation path, or
    /// a by-list is rejected naming the column; anything else passes, and
    /// the other branches never reject.
    #[test]
    fn masked_columns_are_forbidden_in_filters_and_aggregations() {
        for statement in [
            "attr.gen_ai.input.messages == \"hi\"",
            "contains(body, \"x\")",
            "not (severity >= 9 or attr.gen_ai.input.messages == \"a\")",
            "true | sum(attr.gen_ai.input.messages) by attr.model",
            "true | count by attr.gen_ai.input.messages",
        ] {
            let query = logs(statement);
            match masked().validate(&query) {
                Err(QueryError::Forbidden { column }) => assert!(
                    column == "body" || column == "attr.gen_ai.input.messages",
                    "{statement}: names the column, got {column}"
                ),
                other => panic!("{statement}: expected Forbidden, got {other:?}"),
            }
        }
        for statement in [
            "true",
            "attr.model == \"gpt\" | sum(attr.cost_usd) by attr.model",
            "severity >= 9 | count by service",
        ] {
            let query = logs(statement);
            masked().validate(&query).expect("not a content column");
            Visibility::TenantWide
                .validate(&query)
                .expect("never rejects");
        }
        let query = logs("attr.gen_ai.input.messages == \"hi\"");
        Visibility::Scoped {
            conversations: None,
            self_match: None,
        }
        .validate(&query)
        .expect("scoped principals read their content");
    }

    /// RFC0047.8: masking sets the body to `Masked` and unsets the value of
    /// masked attributes, leaving keys (and every other column) intact.
    #[test]
    fn masking_nulls_content_columns_only() {
        use ourios_core::otlp::{AnyValue, KeyValue, any_value};
        let kv = |key: &str, value: &str| KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value.to_string())),
            }),
            ..Default::default()
        };
        let mut row = LogRow::test_row();
        row.body = LogBody::Rendered {
            line: b"secret prompt".to_vec(),
            reconstruction: ourios_miner::reconstruct::Reconstruction::Faithful,
        };
        row.attributes = vec![kv("gen_ai.input.messages", "hi"), kv("model", "gpt")];
        let mut rows = vec![row];
        masked().mask(&mut rows);
        assert_eq!(rows[0].body, LogBody::Masked);
        assert_eq!(rows[0].attributes[0].key, "gen_ai.input.messages");
        assert!(
            rows[0].attributes[0].value.is_none(),
            "value unset (OTLP null)"
        );
        assert!(
            rows[0].attributes[1].value.is_some(),
            "other columns intact"
        );

        let mut rows = vec![LogRow::test_row()];
        Visibility::Scoped {
            conversations: Some(ScopedIds {
                column: "attr.gen_ai.conversation.id".to_string(),
                ids: vec!["c-1".to_string()],
            }),
            self_match: Some(SelfMatch {
                column: "attr.user.hash".to_string(),
                value: "bob".to_string(),
            }),
        }
        .mask(&mut rows);
        assert_ne!(rows[0].body, LogBody::Masked, "scoped rows are not masked");
    }
}
