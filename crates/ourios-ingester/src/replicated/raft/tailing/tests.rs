//! When a tail replica's kept rows can serve a reader's checkpoint, which
//! rows need template definitions, and how a page fits the message limit.

use ourios_core::record::BodyKind;

use super::super::kept::tests::{entries, mined_log};
use super::super::kept::{Bounds, Definitions, Kept, RowPage};
use super::super::mining::MinedRow;
use super::{encoded, referenced, servable};
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

/// A flusher's replay publishes the rows it rebuilt and drops them without
/// moving the cut: a reader below them, whose listing may predate the
/// replay's files, cannot be served until a checkpoint covers them, so
/// the tail is unavailable to it rather than short.
#[test]
fn a_reader_below_rows_a_replay_published_is_unavailable() {
    let mut kept = kept(Bounds::default(), 0);
    kept.replayed(6);
    assert!(matches!(servable(&kept, 0), Err(Refusal::Unavailable(_))));
    assert_eq!(servable(&kept, 6), Ok(()));
    kept.checkpoint(8);
    assert_eq!(servable(&kept, 6), Err(Refusal::TooOld { oldest: 8 }));
    assert_eq!(servable(&kept, 8), Ok(()));
}

fn rows() -> Vec<MinedRow> {
    mined_log(&entries(10))
        .into_iter()
        .flat_map(|(_, mined)| mined.rows)
        .collect()
}

/// A row with no template, a structured or absent body or a parse
/// failure, renders without tokens, so it asks for no definition.
#[test]
fn only_string_rows_with_a_template_need_a_definition() {
    let mut rows = rows();
    let templated = referenced(&rows);
    rows[0].record.body_kind = BodyKind::Structured;
    rows[1].record.template_id = ourios_miner::cluster::NO_TEMPLATE;
    let keys = |row: &MinedRow| (row.record.template_id, row.record.template_version);
    let others = referenced(&rows[2..]);
    let expected: std::collections::BTreeSet<_> = templated
        .into_iter()
        .filter(|key| others.contains(key) || (*key != keys(&rows[0]) && *key != keys(&rows[1])))
        .collect();
    assert_eq!(referenced(&rows), expected);
    assert!(!referenced(&rows).contains(&keys(&rows[1])));
}

/// The definitions of every template `rows` reference.
fn defined(rows: &[MinedRow]) -> Definitions {
    referenced(rows)
        .into_iter()
        .map(|key| (key, "t <*>".to_owned()))
        .collect()
}

/// A page whose rows pass the page bound is halved until they fit, the
/// rest left to the next page; a single row that alone does not fit is
/// refused, never sent past the bound.
#[test]
fn a_page_is_halved_to_fit_and_a_row_too_large_alone_is_refused() {
    let rows = rows();
    let one = encoded(page(&rows[..1]), &defined(&rows), (9, usize::MAX)).expect("one row");
    let whole = encoded(page(&rows), &defined(&rows), (9, usize::MAX)).expect("every row");
    let bound = whole.rows.len() - 1;
    assert!(bound > one.rows.len());
    let fitted = encoded(page(&rows), &defined(&rows), (9, bound)).expect("a fitted page");
    assert!(
        fitted.rows.len() <= bound,
        "{} > {bound}",
        fitted.rows.len()
    );
    assert!(fitted.next.is_some(), "the rest is left to the next page");

    let refused = encoded(page(&rows[..1]), &defined(&rows), (9, one.rows.len() - 1));
    assert!(
        matches!(refused, Err(Refusal::Unavailable(_))),
        "{refused:?}"
    );
}

fn page(rows: &[MinedRow]) -> RowPage {
    RowPage {
        rows: rows.to_vec(),
        next: None,
    }
}
