//! RFC0052.14 — The timer cannot stamp across a concurrent submit.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! A seeded-interleaving test rather than a timing one (RFC 0052 §6):
//! the window is narrow, and a wall-clock test that happens to pass
//! proves nothing. The fallback shape, if seeding proves unreachable,
//! holds the timer artificially between the quiesce and the end of the
//! cut while driving ingest — the lock is never held across store I/O.

/// Scenario RFC0052.14 — no mark passes a frame whose encode had not emitted.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (mark read after the quiesce under the ingest exclusion)"]
fn rfc0052_14_no_checkpoint_passes_an_unemitted_frame_under_any_interleaving() {
    todo!(
        "RFC0052.14 — the reclamation timer firing while ingest submits \
         continuously, under every seeded interleaving: no checkpoint is \
         advanced past a frame whose encode had not finished its sink \
         emit, and the mark used is the one read after the quiesce \
         under the same exclusion, not one read before either"
    );
}

/// Scenario RFC0052.14 — the mark is a turn's own frame offset, never the sync's EOF.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (a flush whose sync covers two turns with a barrier between them)"]
fn rfc0052_14_mark_is_the_turns_frame_offset_not_the_flush_eof() {
    todo!(
        "RFC0052.14 — a flush whose sync covers two turns and a barrier \
         between them: the mark is the first turn's own frame offset, so \
         the later frame made durable by the same flush but not yet \
         mined nor acknowledged is never covered; the post-recovery seed \
         is a delivered offset covered by a successful sync, never \
         max_delivered alone, and a node with no such offset seeds None"
    );
}

/// Scenario RFC0052.14 — a rotation capture past the sink's ceiling parks and advances nothing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (RotationDecision on append_batch hands the cut to the barrier task)"]
fn rfc0052_14_rotation_capture_past_the_ceiling_parks_every_drained_batch() {
    todo!(
        "RFC0052.14 — a rotation capture that would take the pending cut \
         past the sink's ceiling parks every batch it drained and \
         advances neither the mark, the snapshots nor the epoch; the \
         checkpoint that pending cut eventually stamps covers only \
         frames its own batches held, and the parked partitions are \
         covered by the next cut; a rotation-fired cut performs no store \
         I/O inside the ingest turn, and an append admitted after the \
         turn is in neither that cut's checkpoint nor its snapshot"
    );
}

/// Scenario RFC0052.14 — an idle rotation on the barrier tick rotates before the cut.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (Journal::rotate(Discretionary) under the exclusion before the cut)"]
fn rfc0052_14_idle_rotation_on_the_tick_marks_the_last_acked_turn_not_the_boundary() {
    todo!(
        "RFC0052.14 — an idle rotation on the barrier tick rotates before \
         the cut under the exclusion: the mark is the last acknowledged \
         turn's frame offset in the closed segment, never the rotation \
         boundary; a frame appended after the release lands in the new \
         segment above the mark; a frame written but never acknowledged \
         is never a mark"
    );
}

/// Scenario RFC0052.14 — a pre-cut batch that detached mid-batch is fully in the cut.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (quiesce waits for the encode phase, not for a registered publish)"]
fn rfc0052_14_pre_cut_batch_detaching_mid_batch_is_covered_without_waiting_on_the_put() {
    todo!(
        "RFC0052.14 — a pre-cut batch whose first record detached a \
         partition mid-batch has its remaining records in the cut under \
         any interleaving: the quiesce waits for the batch's encode \
         phase, not for a worker to register a publish, and the detached \
         partition's PUT is not waited on; with several partitions \
         detached from one pre-cut batch completing in any order, the \
         batch's shared completion holds the in-flight count until the \
         last finishes"
    );
}

/// Scenario RFC0052.14 — cuts are strictly ordered; a failed A invalidates B.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (pending slot; B neither installs nor stamps before A's outcome)"]
fn rfc0052_14_cut_b_behind_a_held_cut_a_is_invalidated_when_a_fails() {
    todo!(
        "RFC0052.14 — with cut A's flush held at a fault-injection point \
         and cut B captured behind it, B neither installs nor \
         checkpoints before A's outcome; when A fails, B is invalidated \
         — no snapshot of B's installed, no mark of B's stamped, A's and \
         B's records all in the buffers — and the re-captured cut covers \
         them all; when A succeeds, B runs unchanged"
    );
}

/// Scenario RFC0052.14 — an age-sweep publish registered after the cut is neither covered nor lost.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (post-cut publish carries the next epoch; no exclusion around store I/O)"]
fn rfc0052_14_post_cut_publish_failure_fails_no_cut_at_or_below_the_current_one() {
    todo!(
        "RFC0052.14 — an age-sweep publish registered between the \
         barrier's quiesce_publishes and its checkpoint that fails \
         transiently or panics: every frame it holds is above the cut's \
         mark, a transient failure requeues it, and a panic carrying the \
         next epoch fails no cut at or below the current one — which \
         proceeds — and fails every later barrier until restart; the \
         barrier does not re-take the exclusion around its store I/O"
    );
}
