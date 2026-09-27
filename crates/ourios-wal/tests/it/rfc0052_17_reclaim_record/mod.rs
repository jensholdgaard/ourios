//! RFC0052.17 — The reclaim record is the only startup witness, and it
//! is fail-closed.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The §6 fixture matrix — the four record states (absent, empty,
//! valid with entries, corrupt) crossed with a restorable and an
//! undecodable snapshot — plus the crash points, the legacy-root
//! migration rows, the header-flag rows, the consumer-mode rows and
//! the uncertain-deletion rows. Split by what each row reads: the
//! file's own bytes, the open-time matrix across both sidecars and the
//! segment headers, or a pass.

mod format;
mod matrix;
mod pass;
