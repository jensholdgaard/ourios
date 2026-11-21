//! RFC 0056 §3.2: the tenants held after their template events could not
//! be written for good. Shared by the audit sink, which holds them, and the
//! record sink and publisher, which drop what they would publish for them.
//!
//! Holding is opt-in: the local receiver turns it on, while the replicated
//! backend keeps today's drop-and-publish until #1392 designs its hold
//! (RFC 0056 §3.6).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::audit::{AuditEvent, AuditPayload};
use ourios_core::tenant::TenantId;

#[derive(Debug, Default)]
struct Inner {
    enabled: bool,
    tenants: Mutex<BTreeSet<TenantId>>,
}

/// The held tenants, shared between clones. Only a restart clears a hold
/// (RFC 0056 §3.5), so there is no release. The default holds nothing.
#[derive(Debug, Clone, Default)]
pub struct HeldTenants(Arc<Inner>);

impl HeldTenants {
    /// A set that holds tenants.
    #[must_use]
    pub fn enabled() -> Self {
        Self(Arc::new(Inner {
            enabled: true,
            tenants: Mutex::default(),
        }))
    }

    /// Hold `tenant`; `true` if it was not held before. A set that is not
    /// enabled holds nothing.
    #[must_use]
    pub fn hold(&self, tenant: &TenantId) -> bool {
        self.0.enabled && self.lock().insert(tenant.clone())
    }

    #[must_use]
    pub fn is_held(&self, tenant: &TenantId) -> bool {
        self.lock().contains(tenant)
    }

    /// Whether any tenant is held.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.lock().is_empty()
    }

    /// Whether `event` is one a hold drops: a template event of a held
    /// tenant. Denial events stay on their own path, since a restart
    /// cannot regenerate them (#1396).
    #[must_use]
    pub fn drops(&self, event: &AuditEvent) -> bool {
        is_template_event(event) && self.is_held(&event.tenant_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<TenantId>> {
        self.0
            .tenants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `event` describes a template, which replay regenerates.
#[must_use]
pub fn is_template_event(event: &AuditEvent) -> bool {
    matches!(event.payload, AuditPayload::Template { .. })
}
