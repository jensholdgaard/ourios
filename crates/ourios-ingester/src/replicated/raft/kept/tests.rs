use super::super::mining::tests::{batch_at, block, configured, corpus, events_of, miner_of, time};
use super::super::types::GroupEntry;
use super::*;

/// A configuration, an id block and `batches` batches of five corpus
/// lines from index 3, alternating two tenants.
fn entries(batches: usize) -> Vec<GroupEntry> {
    let lines = corpus();
    let mut entries = vec![configured(0.7), block(0, 10_000)];
    for (n, chunk) in (1..).zip(lines.chunks(5).take(batches)) {
        let tenant = if n % 2 == 0 { "globex" } else { "acme" };
        entries.push(batch_at(time(n), tenant, chunk));
    }
    entries
}

/// What a tail replica mines from `entries`, with the index of each
/// entry.
fn mined_log(entries: &[GroupEntry]) -> Vec<(u64, Mined)> {
    let mut miner = miner_of(entries);
    events_of(entries, 64)
        .into_iter()
        .map(|applied| {
            let index = applied.at.index;
            (index, miner.apply(applied).expect("mined"))
        })
        .collect()
}

const UNBOUNDED: Bounds = Bounds {
    audit: usize::MAX,
    rows: usize::MAX,
};

fn kept_all(log: &[(u64, Mined)], bounds: Bounds) -> Kept {
    let mut kept = Kept::new(bounds);
    for (index, mined) in log {
        kept.keep(*index, mined.clone());
    }
    kept
}

fn events_after(log: &[(u64, Mined)], cut: u64) -> Vec<AuditEvent> {
    log.iter()
        .filter(|(index, _)| *index > cut)
        .flat_map(|(_, mined)| mined.audit.clone())
        .collect()
}

fn rows_after(log: &[(u64, Mined)], cut: u64) -> Vec<MinedRow> {
    log.iter()
        .flat_map(|(_, mined)| mined.rows.clone())
        .filter(|row| row.position.seq > cut)
        .collect()
}

/// RFC 0065 §6.7: a replica keeps the rows and audit events above the
/// checkpoint it applied, in log order, and none at or below the cut.
#[test]
fn a_checkpoint_drops_what_is_at_or_below_its_cut() {
    let log = mined_log(&entries(10));
    let mut kept = kept_all(&log, UNBOUNDED);
    assert_eq!(
        kept.rows().cloned().collect::<Vec<_>>(),
        rows_after(&log, 0)
    );
    assert_eq!(
        kept.audit().cloned().collect::<Vec<_>>(),
        events_after(&log, 0)
    );

    let cut = 6;
    kept.checkpoint(cut);
    let audit: Vec<_> = kept.audit().cloned().collect();
    assert_eq!(
        kept.rows().cloned().collect::<Vec<_>>(),
        rows_after(&log, cut)
    );
    assert_eq!(audit, events_after(&log, cut));
    assert!(!audit.is_empty(), "events above the cut survive");
    let bytes: usize = audit.iter().map(audit_bytes).sum();
    assert_eq!(kept.audit_bytes(), bytes);
    assert_eq!((kept.gap(), kept.dropped()), (None, Dropped::default()));
}

/// An event's estimate counts its template text, so a longer template
/// costs more.
#[test]
fn an_audit_event_costs_more_with_a_longer_template() {
    let log = mined_log(&entries(10));
    let created = |event: &AuditEvent| match &event.payload {
        AuditPayload::Template {
            change: TemplateChange::Created { new_template },
            ..
        } => Some(new_template.len()),
        _ => None,
    };
    let mut events: Vec<_> = log
        .iter()
        .flat_map(|(_, m)| m.audit.clone())
        .filter(|event| created(event).is_some())
        .collect();
    events.sort_by_key(created);
    let (shortest, longest) = (&events[0], &events[events.len() - 1]);
    assert!(audit_bytes(shortest) > 0);
    assert!(audit_bytes(longest) > audit_bytes(shortest));
}

/// Past the audit ceiling a replica drops every event it keeps, counts
/// them, records the entry as its gap, and keeps later entries' events.
#[test]
fn passing_the_audit_ceiling_drops_every_kept_event_and_records_a_gap() {
    let log = mined_log(&entries(30));
    let largest = log
        .iter()
        .map(|(_, mined)| mined.audit.iter().map(audit_bytes).sum::<usize>())
        .max()
        .expect("entries");
    let bounds = Bounds {
        audit: largest * 2,
        rows: usize::MAX,
    };
    let mut kept = Kept::new(bounds);
    let mut last_drop = None;
    for (index, mined) in &log {
        let before = kept.dropped().audit;
        kept.keep(*index, mined.clone());
        assert!(kept.audit_bytes() <= bounds.audit, "within the ceiling");
        if kept.dropped().audit > before {
            last_drop = Some(*index);
        }
    }
    let gap = last_drop.expect("the ceiling was passed");
    let total = log.iter().map(|(_, m)| m.audit.len() as u64).sum::<u64>();
    assert_eq!(kept.gap(), Some(gap));
    assert_eq!(kept.audit().count() as u64 + kept.dropped().audit, total);
    assert_eq!(
        kept.audit().cloned().collect::<Vec<_>>(),
        events_after(&log, gap)
    );
    assert_eq!(
        kept.rows().count(),
        rows_after(&log, 0).len(),
        "rows are kept"
    );
    assert_eq!(kept.dropped().rows, 0);
}

/// Rows that do not fit their bound are dropped as they arrive and
/// counted, the entry recorded as the gap; earlier rows stay, and later
/// ones are kept once there is room.
#[test]
fn rows_past_their_bound_are_dropped_and_recorded_as_a_gap() {
    let log = mined_log(&entries(10));
    let entry_bytes = |mined: &Mined| -> usize {
        mined
            .rows
            .iter()
            .map(|row| estimate_bytes(&row.record))
            .sum()
    };
    let batches: Vec<_> = log.iter().filter(|(_, m)| !m.rows.is_empty()).collect();
    let bound = batches[..3].iter().map(|(_, m)| entry_bytes(m)).sum();
    let bounds = Bounds {
        audit: usize::MAX,
        rows: bound,
    };
    let mut kept = Kept::new(bounds);
    for (index, mined) in log.iter().take_while(|(index, _)| *index <= batches[3].0) {
        kept.keep(*index, mined.clone());
    }
    let first_three = rows_after(&log, 0)[..15].to_vec();
    assert_eq!(kept.rows().cloned().collect::<Vec<_>>(), first_three);
    assert_eq!(kept.gap(), Some(batches[3].0));
    assert_eq!(kept.dropped().rows, batches[3].1.rows.len() as u64);
    assert_eq!(kept.dropped().audit, 0, "audit events are not rows");

    kept.checkpoint(batches[2].0);
    assert_eq!(kept.gap(), Some(batches[3].0), "a cut below the gap");
    assert_eq!(kept.rows().count(), 0);
    assert!(entry_bytes(&batches[4].1) <= bound);
    kept.keep(batches[4].0, batches[4].1.clone());
    let later = batches[4].1.rows.clone();
    assert_eq!(
        kept.rows().cloned().collect::<Vec<_>>(),
        later,
        "room again"
    );
}

/// A checkpoint clears the gap only when its cut is at or above it.
#[test]
fn a_cut_at_or_above_the_gap_clears_it() {
    let log = mined_log(&entries(10));
    let mut kept = kept_all(
        &log,
        Bounds {
            audit: 1,
            rows: usize::MAX,
        },
    );
    let gap = kept.gap().expect("every event dropped");
    assert_eq!(kept.audit().count(), 0);
    kept.checkpoint(gap - 1);
    assert_eq!(kept.gap(), Some(gap));
    kept.checkpoint(gap);
    assert_eq!(kept.gap(), None);
    let dropped = log.iter().map(|(_, m)| m.audit.len() as u64).sum::<u64>();
    assert_eq!(kept.dropped().audit, dropped, "counts outlive the gap");
}
