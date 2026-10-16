//! Test seams over the per-tenant template-map single-flight (`map_flight`),
//! for integration tests through the `testing` feature.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use crate::Querier;

/// Test seams over the per-tenant template-map single-flight, for
/// integration tests that must steer it deterministically (RFC0033.7's
/// `joined` outcome). Not part of the query surface.
impl Querier {
    /// Run `gate` on the acquiring thread just before each template-map
    /// acquisition this querier starts; `None` removes it.
    #[doc(hidden)]
    pub fn set_template_map_acquire_gate(&self, gate: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self
            .template_maps
            .hooks
            .before_acquire
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = gate;
    }

    /// Run `gate` on the acquiring thread right after each tenant listing a
    /// template-map acquisition takes, before anything is decided from it;
    /// `None` removes it.
    #[doc(hidden)]
    pub fn set_template_map_listing_gate(&self, gate: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self
            .template_maps
            .hooks
            .after_listing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = gate;
    }

    /// How many queries have so far found an acquisition in flight and
    /// waited on it.
    #[doc(hidden)]
    #[must_use]
    pub fn template_map_waits(&self) -> usize {
        self.template_maps.hooks.waits.load(Ordering::SeqCst)
    }

    /// How many queries have so far joined another acquisition.
    #[doc(hidden)]
    #[must_use]
    pub fn template_map_joins(&self) -> usize {
        self.template_maps.hooks.joins.load(Ordering::SeqCst)
    }

    /// Every acquisition started so far, in order: its tenant, and how many
    /// acquisitions (itself included) were in flight as it started.
    #[doc(hidden)]
    #[must_use]
    pub fn template_map_acquisitions(&self) -> Vec<(String, usize)> {
        self.template_maps
            .hooks
            .started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(tenant, in_flight)| (tenant.as_str().to_owned(), *in_flight))
            .collect()
    }
}
