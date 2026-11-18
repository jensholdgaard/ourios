//! When a tail replica's kept rows can serve a reader's checkpoint.

use super::super::kept::tests::{entries, mined_log};
use super::super::kept::{Bounds, Kept};
use super::servable;
use crate::replicated::raft::transport::tail::Refusal;

fn kept(bounds: Bounds, cut: u64) -> Kept {
    let mut kept = Kept::new(bounds);
    for (index, mined) in mined_log(&entries(10)) {
        kept.keep(index, mined);
    }
    kept.checkpoint(cut);
    kept
}

/// A reader at the replica's cut, or past it, is served; one below it is
/// too old, since the rows between were published and dropped, and the
/// refusal names the cut to restart from.
#[test]
fn a_reader_below_the_cut_is_too_old() {
    let kept = kept(Bounds::default(), 6);
    assert_eq!(servable(&kept, 6), Ok(()));
    assert_eq!(servable(&kept, 8), Ok(()));
    assert_eq!(servable(&kept, 5), Err(Refusal::TooOld { oldest: 6 }));
}

/// Rows the replica dropped for its bound leave a gap no reader below it
/// can be served across; a reader at or past it can.
#[test]
fn a_gap_above_the_reader_leaves_the_tail_unavailable() {
    let bounds = Bounds {
        rows: 1,
        ..Bounds::default()
    };
    let kept = kept(bounds, 0);
    let gap = kept.gap().expect("rows dropped");
    assert!(matches!(servable(&kept, 0), Err(Refusal::Unavailable(_))));
    assert_eq!(servable(&kept, gap), Ok(()));
}
