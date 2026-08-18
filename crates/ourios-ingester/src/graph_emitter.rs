//! RFC 0047 §3.3 / §3.6 — feeding the authorization graph from the data,
//! and taking a conversation back out of it.
//!
//! The emitter derives relationship tuples from stored rows — the promoted
//! `gen_ai.conversation.id`, `user.hash` / `enduser.pseudo.id` and
//! `gen_ai.agent.id` values — and writes them to `OpenFGA` in ≤ 100-tuple
//! idempotent batches. It is fed by the compaction sweep (every row it
//! rewrites) and by the receiver's flush cadence (every batch it publishes),
//! so a conversation is visible to fine-grained principals seconds after it
//! is stored. Erasure reads a conversation object's tuples and deletes them
//! — after the Parquet rewrite that dropped the rows, never before.
//!
//! Object naming is [`TenantObjects`] — the one place the rule lives — so
//! the emitter and the planner can never disagree.

use std::collections::BTreeSet;
use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;
use ourios_core::auth::openfga::{
    MCP_TOOL_NAMES, OpenFgaClient, OpenFgaConfig, OpenFgaError, PrincipalKind, TenantObjects,
    TupleKey, is_object_id,
};
use ourios_core::record::MinedRecord;
use ourios_parquet::promoted::{self, project_string_value};
use ourios_semconv as semconv;

/// `OpenFGA`'s cap on tuples per transactional `Write` (RFC 0047 §3.3).
pub const WRITE_CHUNK: usize = 100;
/// The user-identity attribute keys the emitter reads (RFC 0047 §3.3).
pub const USER_KEYS: [&str; 2] = ["user.hash", "enduser.pseudo.id"];
/// The agent-identity attribute key the emitter reads (RFC 0047 §3.3).
pub const AGENT_KEY: &str = "gen_ai.agent.id";

const OPERATION_WRITE: &str = "write";
const OPERATION_DELETE: &str = "delete";
const ERROR_TYPE: &str = "error.type";
const ERROR_TYPE_UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";

/// The graph emitter (RFC 0047 §3.3): derives tuples from rows and writes
/// them; erases a conversation's tuples (§3.6).
pub struct GraphEmitter {
    client: OpenFgaClient,
    /// Which attribute family and key carries the conversation id — the
    /// same column the planner filters on.
    conversation: ConversationKey,
    tuples: Counter<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationKey {
    Log(String),
    Resource(String),
}

impl std::fmt::Debug for GraphEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphEmitter")
            .field("conversation", &self.conversation)
            .finish_non_exhaustive()
    }
}

/// What one emitter call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Emitted {
    /// Tuples sent (idempotent — an existing tuple is not an error).
    pub tuples: usize,
    /// `Write` calls issued (≤ 100 tuples each).
    pub batches: usize,
}

impl GraphEmitter {
    /// An emitter for the configured graph, or `None` when no
    /// `conversation` object type is bound (`auth.openfga.visibility.objects`)
    /// — nothing to derive tuples for.
    ///
    /// # Errors
    ///
    /// When the HTTP client cannot be built (a startup error).
    pub fn from_config(config: &OpenFgaConfig) -> Result<Option<Self>, String> {
        let Some(object) =
            config.visibility().objects().iter().find(|object| {
                object.object_type() == ourios_core::auth::openfga::CONVERSATION_TYPE
            })
        else {
            return Ok(None);
        };
        let conversation = if let Some(key) = object.column().strip_prefix(promoted::ATTR_PREFIX) {
            ConversationKey::Log(key.to_string())
        } else if let Some(key) = object.column().strip_prefix(promoted::RESOURCE_PREFIX) {
            ConversationKey::Resource(key.to_string())
        } else {
            return Err(format!(
                "auth.openfga.visibility.objects: conversation column `{}` is not a promoted \
                 column name",
                object.column()
            ));
        };
        Ok(Some(Self {
            client: OpenFgaClient::new(config)?,
            conversation,
            tuples: global::meter("ourios.graph")
                .u64_counter(semconv::OURIOS_GRAPH_TUPLES)
                .with_unit("{tuple}")
                .build(),
        }))
    }

    /// Whether `record` belongs to conversation `id` — the erasure filter
    /// (RFC 0047 §3.6), reading the same column the tuples were derived
    /// from, on the **raw** value: a conversation whose id can never be a
    /// graph object is still erasable from the rows.
    #[must_use]
    pub fn conversation_matches(&self, record: &MinedRecord, id: &str) -> bool {
        self.raw_conversation_id(record) == Some(id)
    }

    /// The conversation id of `record` as stored, if any.
    fn raw_conversation_id<'a>(&self, record: &'a MinedRecord) -> Option<&'a str> {
        match &self.conversation {
            ConversationKey::Log(key) => project_string_value(&record.attributes, key),
            ConversationKey::Resource(key) => {
                project_string_value(&record.resource_attributes, key)
            }
        }
    }

    /// The conversation id of `record`, when it carries one that can be a
    /// graph object id.
    fn conversation_id<'a>(&self, record: &'a MinedRecord) -> Option<&'a str> {
        let id = self.raw_conversation_id(record)?;
        is_object_id(id).then_some(id)
    }

    /// The RFC 0047 §3.3 tuples of `records` in `tenant`: for every distinct
    /// conversation `conversation:T/<id>#parent@tenant:T`; per
    /// (conversation, user) `#participant@user:<hash>` plus the
    /// `tenant:T#scoped_reader@user:<hash>` binding tuple; per (conversation,
    /// agent) `#actor@agent:<id>` plus its binding tuple. Deduplicated;
    /// values that cannot be object ids are skipped. Empty when the tenant
    /// itself cannot be a graph object.
    #[must_use]
    pub fn derive(&self, tenant: &str, records: &[MinedRecord]) -> BTreeSet<TupleKey> {
        let mut tuples = BTreeSet::new();
        let Some(objects) = TenantObjects::new(tenant) else {
            return tuples;
        };
        for record in records {
            let Some(id) = self.conversation_id(record) else {
                continue;
            };
            if !objects.conversation_fits(id) {
                continue;
            }
            let conversation = objects.conversation(id);
            tuples.insert(TupleKey::new(objects.tenant(), "parent", &conversation));
            for key in USER_KEYS {
                if let Some(user) = project_string_value(&record.attributes, key)
                    && is_object_id(user)
                {
                    let user = format!("{}:{user}", PrincipalKind::User.type_name());
                    tuples.insert(TupleKey::new(&user, "participant", &conversation));
                    tuples.insert(TupleKey::new(&user, "scoped_reader", objects.tenant()));
                }
            }
            if let Some(agent) = project_string_value(&record.attributes, AGENT_KEY)
                && is_object_id(agent)
            {
                let agent = format!("{}:{agent}", PrincipalKind::Agent.type_name());
                tuples.insert(TupleKey::new(&agent, "actor", &conversation));
                tuples.insert(TupleKey::new(&agent, "scoped_reader", objects.tenant()));
            }
        }
        tuples
    }

    /// The per-tenant tool objects (RFC 0047 §3.5): `tool:T/<name>#parent@
    /// tenant:T` for every MCP tool, so operators grant `caller` only.
    #[must_use]
    pub fn tool_tuples(tenant: &str) -> BTreeSet<TupleKey> {
        let Some(objects) = TenantObjects::new(tenant) else {
            return BTreeSet::new();
        };
        MCP_TOOL_NAMES
            .iter()
            .map(|tool| TupleKey::new(objects.tenant(), "parent", objects.tool(tool)))
            .collect()
    }

    /// Write `tuples` in ≤ 100-tuple idempotent batches. Stops at the first
    /// failed batch (later batches are retried by the next sweep — every
    /// write is idempotent).
    ///
    /// # Errors
    ///
    /// [`OpenFgaError`] from the failed batch.
    pub async fn emit(&self, tuples: &BTreeSet<TupleKey>) -> Result<Emitted, OpenFgaError> {
        let all: Vec<TupleKey> = tuples.iter().cloned().collect();
        let mut emitted = Emitted::default();
        for chunk in all.chunks(WRITE_CHUNK) {
            match self.client.write(chunk, &[]).await {
                Ok(()) => {
                    self.record(OPERATION_WRITE, chunk.len(), None);
                    emitted.tuples += chunk.len();
                    emitted.batches += 1;
                }
                Err(e) => {
                    self.record(
                        OPERATION_WRITE,
                        chunk.len(),
                        Some(ERROR_TYPE_UPSTREAM_UNAVAILABLE),
                    );
                    return Err(e);
                }
            }
        }
        Ok(emitted)
    }

    /// Erase a conversation from the graph (RFC 0047 §3.6): read the
    /// object's tuples, delete them in ≤ 100-tuple batches. Returns the
    /// number deleted. Call **after** the Parquet rewrite that dropped the
    /// rows — a dangling tuple is harmless, a dangling row is a leak.
    ///
    /// # Errors
    ///
    /// [`OpenFgaError`] from the read or a failed batch; the erasure is
    /// retried by the next sweep (deletes are idempotent).
    pub async fn erase_conversation(&self, tenant: &str, id: &str) -> Result<usize, OpenFgaError> {
        let objects = TenantObjects::new(tenant).ok_or(OpenFgaError::InvalidTenant)?;
        // A conversation the emitter could never have named has no tuples
        // — nothing to read or delete; the rows were still dropped.
        if !objects.conversation_fits(id) {
            return Ok(0);
        }
        let tuples = self
            .client
            .read_by_object(&objects.conversation(id))
            .await?;
        let mut deleted = 0;
        for chunk in tuples.chunks(WRITE_CHUNK) {
            match self.client.write(&[], chunk).await {
                Ok(()) => {
                    self.record(OPERATION_DELETE, chunk.len(), None);
                    deleted += chunk.len();
                }
                Err(e) => {
                    self.record(
                        OPERATION_DELETE,
                        chunk.len(),
                        Some(ERROR_TYPE_UPSTREAM_UNAVAILABLE),
                    );
                    return Err(e);
                }
            }
        }
        Ok(deleted)
    }

    fn record(&self, operation: &'static str, count: usize, error_type: Option<&'static str>) {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        match error_type {
            None => self.tuples.add(
                count,
                &[KeyValue::new(
                    semconv::OURIOS_GRAPH_TUPLE_OPERATION,
                    operation,
                )],
            ),
            Some(error_type) => self.tuples.add(
                count,
                &[
                    KeyValue::new(semconv::OURIOS_GRAPH_TUPLE_OPERATION, operation),
                    KeyValue::new(ERROR_TYPE, error_type),
                ],
            ),
        }
    }
}

/// A shared emitter handle for the roles that feed it.
pub type SharedGraphEmitter = Arc<GraphEmitter>;

#[cfg(test)]
mod tests {
    use ourios_core::auth::openfga::{OpenFgaSpec, TupleKey, build_openfga_config};
    use ourios_core::otlp::any_value::Value;
    use ourios_core::otlp::{AnyValue, KeyValue};
    use ourios_core::record::{BodyKind, MinedRecord};
    use ourios_core::tenant::TenantId;

    use super::GraphEmitter;

    fn kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
            ..Default::default()
        }
    }

    fn record(attrs: Vec<KeyValue>) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("acme"),
            template_id: 1,
            template_version: 1,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 1,
            observed_time_unix_nano: None,
            attributes: attrs,
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::Absent,
            params: Vec::new(),
            separators: Vec::new(),
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    fn emitter() -> GraphEmitter {
        use ourios_core::auth::openfga::{VisibilityObjectSpec, VisibilitySpec};
        let config = build_openfga_config(&OpenFgaSpec {
            api_url: Some("http://openfga.invalid:8080".to_string()),
            store_id: Some("s".to_string()),
            visibility: VisibilitySpec {
                objects: vec![VisibilityObjectSpec {
                    object_type: Some("conversation".to_string()),
                    column: Some("attr.gen_ai.conversation.id".to_string()),
                }],
                ..VisibilitySpec::default()
            },
            ..OpenFgaSpec::default()
        })
        .expect("config");
        GraphEmitter::from_config(&config)
            .expect("client")
            .expect("conversation bound")
    }

    fn t(user: &str, relation: &str, object: &str) -> TupleKey {
        TupleKey::new(user, relation, object)
    }

    /// RFC0047.10 (derivation): the §3.3 table — parent per conversation,
    /// participant + binding per user, actor + binding per agent —
    /// deduplicated across rows, tenant-prefixed, with values that cannot
    /// be object ids skipped and rows without a conversation ignored.
    #[test]
    fn derives_the_section_3_3_tuples() {
        let emitter = emitter();
        let rows = vec![
            record(vec![
                kv("gen_ai.conversation.id", "c-1"),
                kv("user.hash", "alice"),
                kv("gen_ai.agent.id", "bot"),
            ]),
            record(vec![
                kv("gen_ai.conversation.id", "c-1"),
                kv("user.hash", "alice"),
            ]),
            record(vec![
                kv("gen_ai.conversation.id", "c-2"),
                kv("enduser.pseudo.id", "bob"),
            ]),
            // no conversation → nothing
            record(vec![kv("user.hash", "carol")]),
            // a user value that cannot be an object id → skipped, the
            // conversation itself still gets its parent tuple
            record(vec![
                kv("gen_ai.conversation.id", "c-3"),
                kv("user.hash", "has space"),
            ]),
        ];
        let tuples = emitter.derive("acme", &rows);
        let expected = [
            t("tenant:acme", "parent", "conversation:acme/c-1"),
            t("user:alice", "participant", "conversation:acme/c-1"),
            t("user:alice", "scoped_reader", "tenant:acme"),
            t("agent:bot", "actor", "conversation:acme/c-1"),
            t("agent:bot", "scoped_reader", "tenant:acme"),
            t("tenant:acme", "parent", "conversation:acme/c-2"),
            t("user:bob", "participant", "conversation:acme/c-2"),
            t("user:bob", "scoped_reader", "tenant:acme"),
            t("tenant:acme", "parent", "conversation:acme/c-3"),
        ];
        assert_eq!(tuples, expected.into_iter().collect());
        assert!(emitter.derive("bad tenant", &rows).is_empty());
        assert_eq!(GraphEmitter::tool_tuples("acme").len(), 3);
        assert!(GraphEmitter::tool_tuples("acme").contains(&t(
            "tenant:acme",
            "parent",
            "tool:acme/query_logs"
        )));
    }
}
