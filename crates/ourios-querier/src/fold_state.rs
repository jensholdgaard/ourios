//! The template-map artifact's fold state (RFC 0033 §3.3 amendment
//! 2026-10-12): what an incremental fold needs to extend a published fold
//! with only the audit files appended since, and nothing a full fold would
//! not reproduce.

use std::time::{SystemTime, UNIX_EPOCH};

use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange};
use serde::{Deserialize, Serialize};

use crate::template_registry::BindingConflicts;

/// Incremental folds allowed since the last full fold. Not needed for
/// correctness; it bounds how far an undetected defect could propagate
/// through persisted incremental steps.
pub const INCREMENTAL_DEPTH_CAP: u32 = 256;

/// The fold state an artifact carries. A full fold always produces one; an
/// artifact published by a pre-amendment writer has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldState {
    high_water_ns: Option<u64>,
    binding_conflicts: BindingConflicts,
    incremental_depth: u32,
}

impl FoldState {
    pub(crate) fn new(
        high_water_ns: Option<u64>,
        binding_conflicts: BindingConflicts,
        incremental_depth: u32,
    ) -> Self {
        Self {
            high_water_ns,
            binding_conflicts,
            incremental_depth,
        }
    }

    /// The greatest fold-relevant event timestamp folded, in nanoseconds
    /// since the epoch; `None` when nothing fold-relevant was folded.
    #[must_use]
    pub fn high_water_ns(&self) -> Option<u64> {
        self.high_water_ns
    }

    /// Every `(template_id, version)` the fold saw bound to two different
    /// templates, ascending.
    #[must_use]
    pub fn binding_conflicts(&self) -> &[(u64, u32)] {
        &self.binding_conflicts
    }

    /// Incremental folds since the last full fold.
    #[must_use]
    pub fn incremental_depth(&self) -> u32 {
        self.incremental_depth
    }

    pub(crate) fn to_json(&self) -> FoldStateJson {
        FoldStateJson {
            high_water_ns: self.high_water_ns,
            binding_conflicts: self
                .binding_conflicts
                .iter()
                .map(|&(template_id, version)| ConflictKey {
                    template_id,
                    version,
                })
                .collect(),
            incremental_depth: self.incremental_depth,
        }
    }

    pub(crate) fn from_json(raw: FoldStateJson) -> Self {
        Self {
            high_water_ns: raw.high_water_ns,
            binding_conflicts: raw
                .binding_conflicts
                .into_iter()
                .map(|key| (key.template_id, key.version))
                .collect(),
            incremental_depth: raw.incremental_depth,
        }
    }
}

/// The `fold_state` object in the artifact's JSON body.
#[derive(Serialize, Deserialize)]
pub(crate) struct FoldStateJson {
    high_water_ns: Option<u64>,
    binding_conflicts: Vec<ConflictKey>,
    incremental_depth: u32,
}

#[derive(Serialize, Deserialize)]
struct ConflictKey {
    template_id: u64,
    version: u32,
}

impl FoldStateJson {
    /// The amendment's validation, beyond §3.2's: conflict keys strictly
    /// sorted and each present in the registry, a `null` high-water only
    /// for an empty fold, and a depth within the cap.
    pub(crate) fn validate(
        &self,
        registry_has: impl Fn(u64, u32) -> bool,
        fold_is_empty: bool,
    ) -> Result<(), String> {
        for pair in self.binding_conflicts.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if (a.template_id, a.version) >= (b.template_id, b.version) {
                return Err("fold_state.binding_conflicts is not strictly sorted".to_string());
            }
        }
        if let Some(key) = self
            .binding_conflicts
            .iter()
            .find(|key| !registry_has(key.template_id, key.version))
        {
            return Err(format!(
                "fold_state.binding_conflicts names ({}, {}), which the registry lacks",
                key.template_id, key.version,
            ));
        }
        if self.high_water_ns.is_none() && !fold_is_empty {
            return Err("fold_state.high_water_ns is null for a non-empty fold".to_string());
        }
        if self.incremental_depth > INCREMENTAL_DEPTH_CAP {
            return Err(format!(
                "fold_state.incremental_depth {} exceeds the cap {INCREMENTAL_DEPTH_CAP}",
                self.incremental_depth,
            ));
        }
        Ok(())
    }
}

/// Whether either fold consumes `event`: a template event that binds a
/// `(template_id, version)` key, or an alias assertion or retraction.
pub(crate) fn is_fold_relevant(event: &AuditEvent) -> bool {
    match &event.payload {
        AuditPayload::Template { change, .. } => {
            !matches!(change, TemplateChange::RejectedDegenerate { .. })
        }
        AuditPayload::AliasAsserted { .. } | AuditPayload::AliasRetracted { .. } => true,
        _ => false,
    }
}

/// `timestamp` as nanoseconds since the epoch, clamped into `u64`. The
/// clamp is monotone, so a strict `>` between clamped values implies the
/// same between the originals and the guard stays sound even for the
/// pre-epoch or far-future timestamps the audit writer never emits.
pub(crate) fn nanos(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The running maximum of fold-relevant event timestamps.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct HighWater(Option<u64>);

impl HighWater {
    pub(crate) fn observe(&mut self, event: &AuditEvent) {
        if is_fold_relevant(event) {
            let ns = nanos(event.timestamp);
            self.0 = Some(self.0.map_or(ns, |held| held.max(ns)));
        }
    }

    pub(crate) fn get(self) -> Option<u64> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ourios_core::audit::hash_triggering_line;
    use ourios_core::tenant::TenantId;

    use super::*;

    fn event(secs: u64, payload: AuditPayload) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new("acme"),
            timestamp: UNIX_EPOCH + Duration::from_secs(secs),
            payload,
        }
    }

    fn template(change: TemplateChange) -> AuditPayload {
        AuditPayload::Template {
            template_id: 1,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change,
        }
    }

    #[test]
    fn high_water_tracks_only_fold_relevant_events() {
        let mut hw = HighWater::default();
        hw.observe(&event(
            50,
            template(TemplateChange::RejectedDegenerate {
                version: 1,
                current_template: "a".into(),
                would_be_template: "<*>".into(),
                would_be_positions: vec![0],
            }),
        ));
        assert_eq!(hw.get(), None, "a rejection binds nothing");
        hw.observe(&event(
            7,
            template(TemplateChange::Created {
                new_template: "a".into(),
            }),
        ));
        hw.observe(&event(
            9,
            AuditPayload::AliasRetracted {
                representative_id: 1,
                member_ids: Vec::new(),
                actor: ourios_core::alias::ActorId::new("op").expect("actor"),
                reason: String::new(),
            },
        ));
        assert_eq!(hw.get(), Some(9_000_000_000));
    }

    #[test]
    fn nanos_clamps_monotonically() {
        assert_eq!(nanos(UNIX_EPOCH), 0);
        assert_eq!(nanos(UNIX_EPOCH - Duration::from_secs(1)), 0);
        assert_eq!(nanos(UNIX_EPOCH + Duration::from_nanos(5)), 5);
    }

    fn json(high_water_ns: Option<u64>, conflicts: &[(u64, u32)], depth: u32) -> FoldStateJson {
        FoldState::new(high_water_ns, conflicts.to_vec(), depth).to_json()
    }

    #[test]
    fn validation_accepts_a_consistent_state() {
        let has = |id, v| (id, v) == (1, 1) || (id, v) == (2, 1);
        assert!(
            json(Some(5), &[(1, 1), (2, 1)], 3)
                .validate(has, false)
                .is_ok()
        );
        assert!(json(None, &[], 0).validate(|_, _| false, true).is_ok());
        assert!(
            json(Some(5), &[], INCREMENTAL_DEPTH_CAP)
                .validate(|_, _| false, false)
                .is_ok()
        );
    }

    #[test]
    fn validation_rejects_each_violation() {
        let has = |_, _| true;
        assert!(
            json(Some(5), &[(2, 1), (1, 1)], 0)
                .validate(has, false)
                .is_err()
        );
        assert!(
            json(Some(5), &[(1, 1), (1, 1)], 0)
                .validate(has, false)
                .is_err()
        );
        assert!(
            json(Some(5), &[(1, 1)], 0)
                .validate(|_, _| false, false)
                .is_err()
        );
        assert!(json(None, &[], 0).validate(has, false).is_err());
        assert!(
            json(Some(5), &[], INCREMENTAL_DEPTH_CAP + 1)
                .validate(has, false)
                .is_err()
        );
    }
}
