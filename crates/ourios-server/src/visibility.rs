//! RFC 0047 §3.4 — the two-step, at the query surfaces. Runs once per
//! request after the tenant gate: asks the graph resolver which branch the
//! principal takes inside the tenant and hands the engine the matching
//! [`Visibility`], recording the branch on `ourios.query.visibility` and
//! the request span. Every failure is fail-closed and named — never a
//! partial predicate, never an open door.

use axum::http::StatusCode;
use ourios_core::auth::openfga::{
    CONVERSATION_TYPE, OpenFgaError, PrincipalKind, Visibility as GraphVisibility,
};
use ourios_ingester::receiver::{AuthBinding, AuthResolver};
use ourios_querier::{SelfMatch, Visibility};

use crate::querier::QuerierMetrics;

/// The `ourios.query.visibility.branch` values.
pub(crate) const BRANCH_TENANT_WIDE: &str = "tenant_wide";
pub(crate) const BRANCH_METADATA_MASKED: &str = "metadata_masked";
pub(crate) const BRANCH_SCOPED: &str = "scoped";

/// Why a query was refused before the engine ran: the HTTP status, a stable
/// `kind`, the message, and the `error.type` for the duration histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibilityRejection {
    pub(crate) status: StatusCode,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    pub(crate) error_type: &'static str,
}

/// The layer-2 decision for `binding` in `tenant`: `Ok(None)` when no graph
/// resolver bound this session (open mode, static/OIDC-only deployments —
/// today's plan), else the branch the engine applies.
///
/// # Errors
///
/// [`VisibilityRejection`]: the bound (`403`, "ask for tenant-wide read"),
/// an incomplete enumeration or unreachable `OpenFGA` (`503`), a
/// credential defect (`401`).
pub(crate) async fn resolve(
    auth: &AuthResolver,
    binding: Option<&AuthBinding>,
    tenant: &str,
    metrics: &QuerierMetrics,
) -> Result<Option<Visibility>, VisibilityRejection> {
    let (Some(graph), Some(resolver)) = (binding.and_then(AuthBinding::graph), auth.openfga())
    else {
        return Ok(None);
    };
    let decision = resolver
        .visibility(graph.principal(), graph.groups(), tenant)
        .await
        .map_err(|e| reject(&e, tenant, resolver.visibility_config().max_objects()))?;
    let config = resolver.visibility_config();
    let (branch, visibility) = match decision {
        GraphVisibility::TenantWide => (BRANCH_TENANT_WIDE, Visibility::TenantWide),
        GraphVisibility::MetadataOnly => (
            BRANCH_METADATA_MASKED,
            Visibility::Masked {
                content_columns: config.content_columns().to_vec(),
            },
        ),
        GraphVisibility::Scoped { conversations } => {
            let column = config
                .objects()
                .iter()
                .find(|object| object.object_type() == CONVERSATION_TYPE)
                .map(|object| object.column().to_string())
                .unwrap_or_default();
            // The self fast path is for `user:` principals only (§3.3):
            // agents and service accounts never get it.
            let self_match = match (graph.principal().kind(), config.self_principal_column()) {
                (PrincipalKind::User, Some(self_column)) => Some(SelfMatch {
                    column: self_column.to_string(),
                    value: graph.principal().id().to_string(),
                }),
                _ => None,
            };
            (
                BRANCH_SCOPED,
                Visibility::Scoped {
                    column,
                    ids: conversations.into_iter().collect(),
                    self_match,
                },
            )
        }
    };
    metrics.record_visibility(branch);
    tracing::Span::current().record("ourios.query.visibility.branch", branch);
    Ok(Some(visibility))
}

fn reject(error: &OpenFgaError, tenant: &str, bound: usize) -> VisibilityRejection {
    match error {
        OpenFgaError::BoundExceeded { .. } => VisibilityRejection {
            status: StatusCode::FORBIDDEN,
            kind: "visibility_bound",
            message: format!(
                "visibility set exceeds {bound} objects in tenant {tenant}; ask for tenant-wide read"
            ),
            error_type: "visibility_bound",
        },
        OpenFgaError::Incomplete => VisibilityRejection {
            status: StatusCode::SERVICE_UNAVAILABLE,
            kind: "visibility_incomplete",
            message: "visibility enumeration incomplete; retry later".to_string(),
            error_type: "visibility_incomplete",
        },
        OpenFgaError::Unavailable(_) => VisibilityRejection {
            status: StatusCode::SERVICE_UNAVAILABLE,
            kind: "auth_unavailable",
            message: "the authorization resolver is unavailable; retry later".to_string(),
            error_type: "upstream_unavailable",
        },
        OpenFgaError::TooManyContextualTuples { .. }
        | OpenFgaError::InvalidGroup { .. }
        | OpenFgaError::InvalidPrincipal => VisibilityRejection {
            status: StatusCode::UNAUTHORIZED,
            kind: "unauthenticated",
            message: "a valid bearer token is required".to_string(),
            error_type: "unauthenticated",
        },
    }
}

/// A principal that is not a tenant-wide content reader may not run
/// template-level queries (`drift`, the registry) — templates are mined
/// from bodies, so listing them would leak content past the row-level
/// enforcement (RFC 0047 §3.4). `None` = allowed.
pub(crate) fn require_tenant_wide(visibility: Option<&Visibility>) -> Option<VisibilityRejection> {
    match visibility {
        None | Some(Visibility::TenantWide) => None,
        Some(Visibility::Masked { .. } | Visibility::Scoped { .. }) => Some(VisibilityRejection {
            status: StatusCode::FORBIDDEN,
            kind: "visibility_scoped",
            message: "template-level queries require tenant-wide content read".to_string(),
            error_type: "permission_denied",
        }),
    }
}
