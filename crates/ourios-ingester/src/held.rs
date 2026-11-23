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
use ourios_wal::{HeldFloor, HeldFloors, WalOffset};

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

    /// The tenants held now.
    #[must_use]
    pub fn tenants(&self) -> BTreeSet<TenantId> {
        self.lock().clone()
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

/// The floors a checkpoint carries for the tenants `held` now (RFC 0056
/// §3.4). A tenant that already has a floor in `kept` keeps it: a floor
/// never rises, or it would lift over records that never published. A
/// tenant held for the first time is durable through the later of the
/// last stamped `checkpoint` and its installed snapshot `horizon`, or
/// from the start of the log when it has neither. A tenant no longer
/// held has no floor.
#[must_use]
pub fn floors_for(
    held: &BTreeSet<TenantId>,
    kept: &HeldFloors,
    checkpoint: Option<WalOffset>,
    horizon: impl Fn(&TenantId) -> Option<WalOffset>,
) -> HeldFloors {
    held.iter()
        .map(|tenant| {
            let floor = kept.get(tenant).copied().unwrap_or_else(|| {
                checkpoint
                    .max(horizon(tenant))
                    .map_or(HeldFloor::StartOfLog, HeldFloor::At)
            });
            (tenant.clone(), floor)
        })
        .collect()
}

/// Whether `event` describes a template, which replay regenerates.
#[must_use]
pub fn is_template_event(event: &AuditEvent) -> bool {
    matches!(event.payload, AuditPayload::Template { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(byte: u64) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::nil(),
            byte,
        }
    }

    fn held(tenants: &[&str]) -> BTreeSet<TenantId> {
        tenants.iter().map(|t| TenantId::new(*t)).collect()
    }

    #[test]
    fn a_new_floor_is_the_later_of_the_checkpoint_and_the_horizon() {
        let horizon = |t: &TenantId| (t.as_str() == "ahead").then(|| at(30));
        let floors = floors_for(
            &held(&["ahead", "behind"]),
            &HeldFloors::new(),
            Some(at(20)),
            horizon,
        );
        assert_eq!(
            floors,
            HeldFloors::from([
                (TenantId::new("ahead"), HeldFloor::At(at(30))),
                (TenantId::new("behind"), HeldFloor::At(at(20))),
            ])
        );
    }

    #[test]
    fn a_tenant_with_no_checkpoint_or_horizon_floors_at_the_start_of_the_log() {
        let floors = floors_for(&held(&["acme"]), &HeldFloors::new(), None, |_| None);
        assert_eq!(
            floors,
            HeldFloors::from([(TenantId::new("acme"), HeldFloor::StartOfLog)])
        );
    }

    #[test]
    fn a_kept_floor_never_rises_and_an_unheld_tenant_loses_its_floor() {
        let kept = HeldFloors::from([
            (TenantId::new("acme"), HeldFloor::StartOfLog),
            (TenantId::new("repaired"), HeldFloor::At(at(5))),
        ]);
        let floors = floors_for(&held(&["acme"]), &kept, Some(at(90)), |_| Some(at(80)));
        assert_eq!(
            floors,
            HeldFloors::from([(TenantId::new("acme"), HeldFloor::StartOfLog)])
        );
    }
}
