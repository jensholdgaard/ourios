//! RFC 0052 §3.3 — rotation under a bounded retry budget.
//!
//! The quiesce RFC 0008 §6.5 introduced was permanent until a restart
//! (#791). This module carries what replaces it: which step failed, how
//! much of the budget is left, and whether the WAL is still retrying or
//! has given up. The state is typed rather than a `bool`, because
//! "retrying" and "given up" are reported differently all the way out to
//! the transports (RFC0052.15) and a flag cannot say which one holds.

/// Why a rotation was asked for (RFC 0052 §3.7).
///
/// RFC 0053 §3.1's segment cap defers only a [`Self::Discretionary`]
/// rotation; an [`Self::Owed`] one always proceeds. The distinction is
/// already observable here: a discretionary rotation of a segment
/// holding no frame is a no-op, since there is no recovery window to
/// bound and nothing to seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationKind {
    /// The barrier task's idle rotation (§3.2) — wanted, not required.
    Discretionary,
    /// A seal, or a recovery requirement: the caller needs the current
    /// segment closed whatever its contents.
    Owed,
}

/// One of §3.3's five rotation steps, as a fault-injection site.
///
/// The `.wal.partial` name means the first four leave nothing a
/// subsequent [`crate::Wal::open`] can select; the fifth leaves a
/// complete, installed segment whose directory entry may not be durable,
/// which is why it is retried by `sync` and never unlinked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationSite {
    /// The closing segment's final data sync.
    CloseSync,
    /// Creating `<uuid>.wal.partial`.
    Create,
    /// Fsyncing the fresh segment's header, before the rename.
    HeaderSync,
    /// `rename(<uuid>.wal.partial, <uuid>.wal)`.
    Rename,
    /// The parent-directory fsync after the rename — the one site whose
    /// retry is `sync`'s, not a second `rotate`.
    ParentFsync,
}

impl RotationSite {
    /// Every site, for the property tests that must not cover only one.
    pub const ALL: [Self; 5] = [
        Self::CloseSync,
        Self::Create,
        Self::HeaderSync,
        Self::Rename,
        Self::ParentFsync,
    ];

    /// The `op` string the site's failures are reported under.
    #[must_use]
    pub fn op(self) -> &'static str {
        match self {
            Self::CloseSync => "sync(rotation: close segment)",
            Self::Create => "create(rotation: fresh segment partial)",
            Self::HeaderSync => "sync(rotation: fresh segment header)",
            Self::Rename => "rename(rotation: install fresh segment)",
            Self::ParentFsync => "fsync(wal_root after rotation)",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::CloseSync => 0,
            Self::Create => 1,
            Self::HeaderSync => 2,
            Self::Rename => 3,
            Self::ParentFsync => 4,
        }
    }
}

/// How many more attempts at one site the seam should fail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Budget {
    #[default]
    Never,
    Times(u32),
    Always,
}

impl Budget {
    /// Consume one attempt, reporting whether it should fail.
    fn take(&mut self) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Times(0) => {
                *self = Self::Never;
                false
            }
            Self::Times(left) => {
                *left -= 1;
                true
            }
        }
    }
}

/// The RFC 0052 §6 fault-injection seam: which rotation steps fail, and
/// how many times each.
///
/// Inert by construction — a default [`RotationFaults`] fails nothing —
/// and reachable only through the `fault-injection` feature's
/// [`crate::Wal::arm_rotation_faults`], which no production wiring
/// enables. The five sites cannot be driven from outside the process any
/// other way: four of them are `fsync`/`rename` calls a directory
/// permission cannot single out.
#[derive(Debug, Clone, Default)]
pub struct RotationFaults {
    sites: [Budget; 5],
}

impl RotationFaults {
    /// Fail the next `times` attempts at `site`.
    #[must_use]
    pub fn failing(site: RotationSite, times: u32) -> Self {
        Self::default().and_failing(site, times)
    }

    /// Fail every attempt at `site`.
    #[must_use]
    pub fn always(site: RotationSite) -> Self {
        let mut faults = Self::default();
        faults.sites[site.index()] = Budget::Always;
        faults
    }

    /// Add another site to an existing arming.
    #[must_use]
    pub fn and_failing(mut self, site: RotationSite, times: u32) -> Self {
        self.sites[site.index()] = Budget::Times(times);
        self
    }

    /// The error a `site` attempt should fail with, if it should.
    pub(crate) fn take(&mut self, site: RotationSite) -> Option<std::io::Error> {
        if self.sites[site.index()].take() {
            return Some(std::io::Error::other(format!(
                "RFC 0052 §6 injected rotation fault at {}",
                site.op()
            )));
        }
        None
    }
}

/// The first failure of the rotation obligation currently outstanding,
/// with how much of the budget it has consumed.
///
/// The first error is kept, not the latest: RFC0052.5 requires it to
/// still be recoverable from the reported state once the budget is
/// exhausted, rather than replaced by a generic "quiesced" message. It
/// is rendered rather than held as a `std::io::Error` because the state
/// is reported on every later append and `io::Error` is not `Clone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationFault {
    op: &'static str,
    kind: std::io::ErrorKind,
    detail: String,
    attempts: u32,
    budget: u32,
}

impl RotationFault {
    fn first(op: &'static str, source: &std::io::Error, budget: u32) -> Self {
        Self {
            op,
            kind: source.kind(),
            detail: source.to_string(),
            attempts: 0,
            budget,
        }
    }

    /// Build a fault describing `attempts` failures out of `budget` at
    /// `site`.
    ///
    /// The rotation path builds its own through [`RotationState`]; this
    /// exists so a caller's *reporting* of the state — the transports'
    /// status mapping, the group-commit broadcast — can be tested
    /// without a disk that fails on command. It carries no capability:
    /// a fault is a rendered description, and constructing one changes
    /// no WAL state.
    #[must_use]
    pub fn new(site: RotationSite, source: &std::io::Error, attempts: u32, budget: u32) -> Self {
        Self {
            attempts,
            ..Self::first(site.op(), source, budget)
        }
    }

    /// The rotation step the *first* failure happened at.
    #[must_use]
    pub fn op(&self) -> &'static str {
        self.op
    }

    /// The first failure's `ErrorKind`.
    #[must_use]
    pub fn kind(&self) -> std::io::ErrorKind {
        self.kind
    }

    /// The first failure's rendered cause.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Failed attempts charged against the budget so far.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// `rotation_retry_attempts` at the time the obligation began.
    #[must_use]
    pub fn budget(&self) -> u32 {
        self.budget
    }
}

impl std::fmt::Display for RotationFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (attempt {} of {}): {}",
            self.op, self.attempts, self.budget, self.detail
        )
    }
}

/// RFC 0052 §3.3's rotation state, as §3.5 exports it.
///
/// A failed `rotate` and a failed `Rotation`-origin directory-fsync
/// discharge charge the same budget; nothing else does, which is what
/// keeps RFC0052.15's reclassification narrow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RotationState {
    /// No rotation obligation is outstanding.
    #[default]
    Healthy,
    /// A step failed and the budget still holds: a later `append` (or,
    /// for the post-rename site, a later `sync`) can still succeed.
    Retrying(RotationFault),
    /// The budget is exhausted. Appends are refused until an operator
    /// intervenes; the only exit today is a restart.
    Terminal(RotationFault),
}

impl RotationState {
    /// Charge one unit of the budget for a failed `op`, returning the
    /// fault the caller reports.
    ///
    /// The first failure's cause is the one kept: a later attempt
    /// failing at a different step does not overwrite the diagnosis the
    /// operator needs.
    pub(crate) fn charge(
        &mut self,
        op: &'static str,
        source: &std::io::Error,
        budget: u32,
    ) -> RotationFault {
        let mut fault = match self {
            Self::Healthy => RotationFault::first(op, source, budget),
            Self::Retrying(outstanding) | Self::Terminal(outstanding) => outstanding.clone(),
        };
        fault.attempts = fault.attempts.saturating_add(1);
        *self = if fault.attempts >= budget {
            Self::Terminal(fault.clone())
        } else {
            Self::Retrying(fault.clone())
        };
        fault
    }

    /// The obligation succeeded: the budget resets. Only the rotation
    /// itself or the directory-fsync discharge may call this — an
    /// unrelated successful operation must not clear the count, or a
    /// parent fsync failing behind succeeding data syncs would never
    /// reach the terminal state.
    pub(crate) fn discharged(&mut self) {
        *self = Self::Healthy;
    }

    /// The terminal fault, when the budget is exhausted.
    #[must_use]
    pub fn terminal(&self) -> Option<&RotationFault> {
        match self {
            Self::Terminal(fault) => Some(fault),
            Self::Healthy | Self::Retrying(_) => None,
        }
    }
}

/// Whether the parent directory still owes an `fsync`, and why.
///
/// `dir_fsync_pending` starts `true` on every [`crate::Wal::open`], so a
/// flag alone cannot say where the obligation came from — and §3.3 makes
/// the origin decide whether a failed discharge charges the rotation
/// budget or is an ordinary retryable sync failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirFsync {
    /// Every directory entry this WAL depends on is durable.
    Clean,
    /// Owed by `open`: an ordinary retryable sync failure, outside the
    /// rotation budget, discharged by the next `sync`.
    PendingOpen,
    /// Owed by a rotation whose rename landed: a failed discharge
    /// charges the rotation budget, and the installed segment is never
    /// unlinked — it is the live append target.
    PendingRotation,
}

#[cfg(test)]
mod tests {
    use super::{Budget, RotationFaults, RotationSite, RotationState};

    fn io() -> std::io::Error {
        std::io::Error::other("disk")
    }

    #[test]
    fn a_budget_of_one_makes_the_first_failure_terminal() {
        let mut state = RotationState::default();
        let fault = state.charge(RotationSite::CloseSync.op(), &io(), 1);
        assert_eq!(fault.attempts(), 1);
        assert!(state.terminal().is_some(), "{state:?}");
    }

    #[test]
    fn the_first_cause_survives_later_attempts_at_other_sites() {
        let mut state = RotationState::default();
        state.charge(RotationSite::CloseSync.op(), &io(), 3);
        state.charge(
            RotationSite::Rename.op(),
            &std::io::Error::other("something else"),
            3,
        );
        let fault = state.charge(RotationSite::ParentFsync.op(), &io(), 3);
        assert_eq!(fault.op(), RotationSite::CloseSync.op());
        assert!(fault.detail().contains("disk"), "{fault}");
        assert_eq!(fault.attempts(), 3);
        let terminal = state.terminal().expect("the third failure is terminal");
        assert_eq!(terminal.op(), RotationSite::CloseSync.op());
    }

    #[test]
    fn a_discharge_resets_the_count() {
        let mut state = RotationState::default();
        state.charge(RotationSite::CloseSync.op(), &io(), 3);
        state.discharged();
        let fault = state.charge(RotationSite::CloseSync.op(), &io(), 3);
        assert_eq!(fault.attempts(), 1, "the budget restarts after a success");
    }

    #[test]
    fn an_unarmed_seam_fails_nothing() {
        let mut faults = RotationFaults::default();
        for site in RotationSite::ALL {
            assert!(faults.take(site).is_none(), "{site:?}");
        }
    }

    #[test]
    fn a_times_arming_runs_out_and_an_always_arming_does_not() {
        let mut faults =
            RotationFaults::failing(RotationSite::Create, 2).and_failing(RotationSite::Rename, 0);
        assert!(faults.take(RotationSite::Create).is_some());
        assert!(faults.take(RotationSite::Create).is_some());
        assert!(faults.take(RotationSite::Create).is_none());
        assert!(faults.take(RotationSite::Rename).is_none());
        assert!(
            faults.take(RotationSite::HeaderSync).is_none(),
            "arming one site must not arm another",
        );

        let mut always = RotationFaults::always(RotationSite::ParentFsync);
        for _ in 0..64 {
            assert!(always.take(RotationSite::ParentFsync).is_some());
        }
    }

    #[test]
    fn a_zero_times_budget_settles_to_never() {
        let mut budget = Budget::Times(0);
        assert!(!budget.take());
        assert_eq!(budget, Budget::Never);
    }
}
