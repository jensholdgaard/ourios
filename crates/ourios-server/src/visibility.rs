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
use ourios_querier::{ScopedIds, SelfMatch, Visibility};

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
    let decision = match resolver
        .visibility(graph.principal(), graph.groups(), tenant)
        .await
    {
        Ok(decision) => decision,
        Err(e) => {
            // A scoped enumeration that failed closed still happened —
            // count it, so `scoped` on `ourios.query.visibility` is exactly
            // "a stream was issued".
            if matches!(
                e,
                OpenFgaError::BoundExceeded { .. } | OpenFgaError::Incomplete
            ) {
                metrics.record_visibility(BRANCH_SCOPED);
                tracing::Span::current().record("ourios.query.visibility.branch", BRANCH_SCOPED);
            }
            return Err(reject(
                &e,
                tenant,
                resolver.visibility_config().max_objects(),
            ));
        }
    };
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
            // No bound conversation object ⇒ nothing to enumerate (the
            // resolver returned no ids) — represented as such, never as an
            // empty column name.
            let conversations = config
                .objects()
                .iter()
                .find(|object| object.object_type() == CONVERSATION_TYPE)
                .map(|object| ScopedIds {
                    column: object.column().to_string(),
                    ids: conversations.into_iter().collect(),
                });
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
                    conversations,
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
        // Never produced by the two-step (an erasure-only outcome); mapped
        // like an unanswerable graph so a future caller stays fail-closed.
        OpenFgaError::Unavailable(_) | OpenFgaError::EraseIncomplete { .. } => {
            VisibilityRejection {
                status: StatusCode::SERVICE_UNAVAILABLE,
                kind: "auth_unavailable",
                message: "the authorization resolver is unavailable; retry later".to_string(),
                error_type: "upstream_unavailable",
            }
        }
        // A tenant no graph object can name: nothing can have been granted
        // on it, so nothing in it is readable — and the operator should hear
        // why.
        OpenFgaError::InvalidTenant => VisibilityRejection {
            status: StatusCode::FORBIDDEN,
            kind: "tenant_unaddressable",
            message: format!(
                "tenant `{tenant}` cannot be named in the authorization graph (a tenant id \
                 is 1-128 bytes of ASCII graphic characters excluding ':', '#' and '/' — \
                 RFC 0048)"
            ),
            error_type: "permission_denied",
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use ourios_core::auth::openfga::OpenFgaError;
    use ourios_querier::Visibility;

    use super::{reject, require_tenant_wide};

    /// The refusal contract per resolver error: status, stable kind,
    /// `error.type` — pinned so a reword cannot change a class unnoticed.
    #[test]
    fn rejections_map_each_error_class() {
        let cases = [
            (
                OpenFgaError::BoundExceeded { bound: 3 },
                StatusCode::FORBIDDEN,
                "visibility_bound",
                "visibility_bound",
            ),
            (
                OpenFgaError::Incomplete,
                StatusCode::SERVICE_UNAVAILABLE,
                "visibility_incomplete",
                "visibility_incomplete",
            ),
            (
                OpenFgaError::Unavailable("down".to_string()),
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_unavailable",
                "upstream_unavailable",
            ),
            (
                OpenFgaError::EraseIncomplete { rounds: 8 },
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_unavailable",
                "upstream_unavailable",
            ),
            (
                OpenFgaError::InvalidTenant,
                StatusCode::FORBIDDEN,
                "tenant_unaddressable",
                "permission_denied",
            ),
            (
                OpenFgaError::TooManyContextualTuples { count: 101 },
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "unauthenticated",
            ),
            (
                OpenFgaError::InvalidGroup { index: 0 },
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "unauthenticated",
            ),
            (
                OpenFgaError::InvalidPrincipal,
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "unauthenticated",
            ),
        ];
        for (error, status, kind, error_type) in cases {
            let rejection = reject(&error, "acme", 100);
            assert_eq!(rejection.status, status, "{error:?}");
            assert_eq!(rejection.kind, kind, "{error:?}");
            assert_eq!(rejection.error_type, error_type, "{error:?}");
        }
        let bound = reject(&OpenFgaError::BoundExceeded { bound: 3 }, "acme", 100);
        assert!(bound.message.contains("exceeds 100 objects in tenant acme"));
        assert!(bound.message.contains("ask for tenant-wide read"));
    }

    /// Template-level surfaces: open mode and tenant-wide pass; masked and
    /// scoped principals are refused with the stable kind.
    #[test]
    fn template_level_queries_need_tenant_wide_read() {
        assert!(require_tenant_wide(None).is_none());
        assert!(require_tenant_wide(Some(&Visibility::TenantWide)).is_none());
        for visibility in [
            Visibility::Masked {
                content_columns: vec!["body".to_string()],
            },
            Visibility::Scoped {
                conversations: None,
                self_match: None,
            },
        ] {
            let rejection = require_tenant_wide(Some(&visibility)).expect("refused");
            assert_eq!(rejection.status, StatusCode::FORBIDDEN);
            assert_eq!(rejection.kind, "visibility_scoped");
            assert_eq!(rejection.error_type, "permission_denied");
        }
    }
}
