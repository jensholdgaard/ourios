//! The sweep's orphan-reclaim pass (RFC0009.4): every planned candidate's
//! orphan GC, run once after the consolidation pass so each partition is
//! reclaimed after its own rewrite (RFC0009.17). The pass is the RFC 0038
//! §3.9 `reclaim orphans` phase; its per-tenant counts feed
//! `ourios.compaction.orphan.reclaimed` (RFC 0009 §3.6).

use ourios_parquet::{CompactionError, OrphanGc, gc_orphans};

use super::{Sweep, Target, TenantPlan, compaction_error_type, hour_label};

/// The `error.type` of an orphan whose delete failed: a store I/O error.
pub(crate) const DELETE_FAILED: &str = "io";

/// One tenant's orphan GC over a sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantOrphanGc {
    /// The tenant whose candidates were GC'd.
    pub tenant: String,
    /// Orphan files deleted.
    pub reclaimed: u64,
    /// Orphan files whose delete failed, left for a later pass.
    pub failed: u64,
}

/// What a run of partition GCs did, and the class of its first failure.
#[derive(Default)]
struct Tally {
    reclaimed: u64,
    failed: u64,
    first_error: Option<&'static str>,
}

impl Tally {
    fn add(&mut self, gc: &Result<OrphanGc, CompactionError>) {
        match gc {
            Ok(gc) => {
                self.reclaimed += gc.reclaimed;
                self.failed += gc.failures;
                if gc.failures > 0 {
                    self.first_error.get_or_insert(DELETE_FAILED);
                }
            }
            Err(e) => {
                self.first_error.get_or_insert(compaction_error_type(e));
            }
        }
    }

    fn merge(&mut self, other: &Self) {
        self.reclaimed += other.reclaimed;
        self.failed += other.failed;
        self.first_error = self.first_error.or(other.first_error);
    }

    /// Set the phase's counts, and mark it `Error` with the first failure's
    /// class: the sweep goes on either way.
    fn record(&self, span: &tracing::Span) {
        // `i64`: tracing-opentelemetry renders a `u64` field as a string.
        let as_count = |count: u64| i64::try_from(count).unwrap_or(i64::MAX);
        span.record(
            ourios_semconv::OURIOS_COMPACTION_ORPHAN_RECLAIMED_COUNT,
            as_count(self.reclaimed),
        );
        span.record(
            ourios_semconv::OURIOS_COMPACTION_ORPHAN_FAILED_COUNT,
            as_count(self.failed),
        );
        if let Some(error_type) = self.first_error {
            ourios_core::telemetry::record_span_error(span, error_type);
        }
    }
}

impl Sweep<'_, '_> {
    /// Reclaim the orphans of every candidate of every successfully planned
    /// tenant, after the consolidation pass: a crashed output written for
    /// the generation a rewrite just committed is dead only once that
    /// commit is in (RFC0009.17).
    pub(super) fn reclaim_all(&mut self, plans: &[TenantPlan<'_>]) {
        let span = tracing::info_span!(
            "reclaim orphans",
            otel.kind = "internal",
            ourios.compaction.orphan.reclaimed_count = tracing::field::Empty,
            ourios.compaction.orphan.failed_count = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let _phase = span.enter();
        let mut pass = Tally::default();
        for (tenant, planned) in plans {
            let Ok(candidates) = planned else { continue };
            let mut tally = Tally::default();
            for partition in candidates {
                tally.add(&self.reclaim_orphans(Target { tenant, partition }));
            }
            pass.merge(&tally);
            self.report.orphans_reclaimed += tally.reclaimed;
            self.report.orphan_gc.push(TenantOrphanGc {
                tenant: (*tenant).to_owned(),
                reclaimed: tally.reclaimed,
                failed: tally.failed,
            });
        }
        pass.record(&span);
    }

    /// GC the partition. Manifest-authoritative, so it never touches a live
    /// file; a scan error is recorded as a sweep error, not fatal.
    fn reclaim_orphans(&mut self, target: Target<'_>) -> Result<OrphanGc, CompactionError> {
        let gc = gc_orphans(self.store, target.partition);
        if let Err(e) = &gc {
            self.report.errors.push(format!(
                "gc-orphans {:?} {}: {e}",
                target.tenant,
                hour_label(target.partition)
            ));
        }
        gc
    }
}
