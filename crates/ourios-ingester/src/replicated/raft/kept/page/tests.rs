//! A page of kept rows, and the template definitions a tail replica sends
//! with it (RFC 0065 §6.7).

use std::collections::{BTreeMap, BTreeSet};

use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange};
use ourios_core::tenant::TenantId;
use ourios_parquet::partition::effective_time_unix_nano;

use super::super::super::mining::tests::{events_of, miner_of};
use super::super::super::mining::{GroupMiner, Mined, MinedRow};
use super::super::super::types::GroupEntry;
use super::super::tests::entries;
use super::super::{Bounds, Kept};
use super::{Definitions, RowPage, RowScan, TemplateKey};
use crate::record_sink::estimate_bytes;

const UNBOUNDED: Bounds = Bounds {
    audit: usize::MAX,
    rows: usize::MAX,
};

/// What a tail replica keeps of `entries` above `cut`, its miner after
/// mining them, and everything they mined.
fn mined(entries: &[GroupEntry], cut: u64) -> (Kept, GroupMiner, Vec<(u64, Mined)>) {
    let mut miner = miner_of(entries);
    let mut kept = Kept::new(UNBOUNDED);
    let mut log = Vec::new();
    for applied in events_of(entries, 64) {
        let index = applied.at.index;
        let out = miner.apply(applied).expect("mined");
        kept.keep(index, out.clone());
        log.push((index, out));
    }
    kept.checkpoint(cut);
    (kept, miner, log)
}

fn acme() -> TenantId {
    TenantId::new("acme")
}

fn scan(tenant: &TenantId, from: u64, read: u64) -> RowScan<'_> {
    RowScan {
        tenant,
        from,
        read,
        range: None,
        after: None,
    }
}

/// Every row `scan` should select from `log`, in log order.
fn selected(log: &[(u64, Mined)], scan: &RowScan<'_>) -> Vec<MinedRow> {
    log.iter()
        .flat_map(|(_, mined)| mined.rows.iter().cloned())
        .filter(|row| {
            let at = row.position.seq;
            let time = effective_time_unix_nano(&row.record).ok();
            let in_range = scan.range.is_none_or(|(start, end)| {
                time.and_then(|t| u64::try_from(t).ok())
                    .is_some_and(|t| start <= t && t < end)
            });
            &row.record.tenant_id == scan.tenant && at > scan.from && at <= scan.read && in_range
        })
        .collect()
}

#[test]
fn a_page_holds_one_tenants_rows_above_from_through_the_read_point() {
    let (kept, _, log) = mined(&entries(10), 0);
    let tenant = acme();
    let scan = scan(&tenant, 4, 9);
    let expected = selected(&log, &scan);
    assert!(!expected.is_empty());
    assert!(
        expected.len() < selected(&log, &self::scan(&tenant, 0, u64::MAX)).len(),
        "from and read exclude rows"
    );
    assert_eq!(
        kept.page(&scan, usize::MAX),
        RowPage {
            rows: expected,
            next: None
        }
    );
}

/// Pages within a budget read the scan once, in order: each ends where
/// the next starts after, and none passes the budget unless it holds a
/// single row.
#[test]
fn pages_resume_after_their_last_position_and_read_the_scan_once() {
    let (kept, _, log) = mined(&entries(10), 0);
    let tenant = acme();
    let whole = selected(&log, &scan(&tenant, 0, u64::MAX));
    let budget = whole
        .iter()
        .take(3)
        .map(|row| estimate_bytes(&row.record))
        .sum();
    let mut read = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = kept.page(
            &RowScan {
                after,
                ..scan(&tenant, 0, u64::MAX)
            },
            budget,
        );
        let bytes: usize = page
            .rows
            .iter()
            .map(|row| estimate_bytes(&row.record))
            .sum();
        assert!(
            bytes <= budget || page.rows.len() == 1,
            "{bytes} > {budget}"
        );
        assert!(!page.rows.is_empty());
        pages += 1;
        let last = page
            .rows
            .last()
            .map(|row| (row.position.seq, row.position.index));
        read.extend(page.rows);
        match page.next {
            Some(next) => {
                assert_eq!(Some(next), last);
                after = Some(next);
            }
            None => break,
        }
    }
    assert!(pages > 2, "{pages} pages");
    assert_eq!(read, whole);
}

/// `log` with each row's time distinct: its position, in microseconds.
fn timed(log: &[(u64, Mined)]) -> Vec<(u64, Mined)> {
    log.iter()
        .map(|(index, mined)| {
            let mut mined = mined.clone();
            for row in &mut mined.rows {
                row.record.time_unix_nano =
                    row.position.seq * 1_000_000 + u64::from(row.position.index);
            }
            (*index, mined)
        })
        .collect()
}

#[test]
fn a_range_selects_rows_by_their_effective_time() {
    let (_, _, log) = mined(&entries(10), 0);
    let log = timed(&log);
    let mut kept = Kept::new(UNBOUNDED);
    for (index, mined) in &log {
        kept.keep(*index, mined.clone());
    }
    let tenant = acme();
    let times: BTreeSet<u64> = selected(&log, &scan(&tenant, 0, u64::MAX))
        .iter()
        .filter_map(|row| effective_time_unix_nano(&row.record).ok())
        .filter_map(|t| u64::try_from(t).ok())
        .collect();
    let (&start, &end) = (
        times.iter().nth(1).expect("times"),
        times.iter().nth_back(1).expect("times"),
    );
    let ranged = RowScan {
        range: Some((start, end)),
        ..scan(&tenant, 0, u64::MAX)
    };
    let expected = selected(&log, &ranged);
    assert!(!expected.is_empty());
    assert_eq!(kept.page(&ranged, usize::MAX).rows, expected);
}

/// The definitions the audit stream records: what the querier's registry
/// folds from it.
fn recorded(log: &[(u64, Mined)], tenant: &TenantId) -> Definitions {
    let mut held = BTreeMap::new();
    for event in log.iter().flat_map(|(_, mined)| &mined.audit) {
        for (key, template) in bound(event, tenant) {
            held.insert(key, template);
        }
    }
    held
}

fn bound(event: &AuditEvent, tenant: &TenantId) -> Vec<(TemplateKey, String)> {
    let AuditPayload::Template {
        template_id: id,
        change,
        ..
    } = &event.payload
    else {
        return Vec::new();
    };
    if &event.tenant_id != tenant {
        return Vec::new();
    }
    match change {
        TemplateChange::Created { new_template } => vec![((*id, 1), new_template.clone())],
        TemplateChange::Widened {
            new_version,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            new_version,
            new_template,
            ..
        } => vec![((*id, *new_version), new_template.clone())],
        TemplateChange::Adopted {
            template_version,
            new_template,
        } => vec![((*id, *template_version), new_template.clone())],
        TemplateChange::RejectedDegenerate { .. } => Vec::new(),
    }
}

/// RFC 0065 §6.7: whatever cut a replica has applied, what it keeps and
/// what its miner holds define every template version its kept rows use,
/// exactly as the audit stream records them, without the store.
#[test]
fn kept_and_mined_definitions_cover_every_kept_row_as_recorded() {
    let entries = entries(40);
    let tenant = acme();
    let mut from_kept = 0;
    for cut in [0, 4, 9, 15] {
        let (kept, miner, log) = mined(&entries, cut);
        let wanted: BTreeSet<TemplateKey> = kept
            .rows()
            .filter(|row| row.record.tenant_id == tenant)
            .map(|row| (row.record.template_id, row.record.template_version))
            .collect();
        assert!(!wanted.is_empty(), "cut {cut}");
        let mut got = miner.definitions(&tenant, &wanted);
        let older = kept.definitions(&tenant, &wanted);
        from_kept += older.keys().filter(|key| !got.contains_key(key)).count();
        got.extend(older);
        let recorded = recorded(&log, &tenant);
        let expected: Definitions = wanted
            .iter()
            .map(|key| (*key, recorded.get(key).cloned().expect("recorded")))
            .collect();
        assert_eq!(got, expected, "cut {cut}");
    }
    assert!(from_kept > 0, "some version only the kept events define");
}

/// RFC 0065 §6.7: once a checkpoint has dropped a template's creation
/// event, kept rows may still use its first version after the miner has
/// widened it; one lookup of the replica's state, the miner's and what it
/// keeps together, still defines that version from the widening's event.
#[test]
fn a_version_widened_after_its_creation_was_checkpointed_is_still_defined() {
    let entries = entries(40);
    let tenant = acme();
    let (_, _, log) = mined(&entries, 0);
    let recorded = recorded(&log, &tenant);
    let mut covered = 0;
    for cut in 0..=log.last().map_or(0, |(index, _)| *index) {
        let (kept, miner, _) = mined(&entries, cut);
        let wanted: BTreeSet<TemplateKey> = kept
            .rows()
            .filter(|row| row.record.tenant_id == tenant)
            .map(|row| (row.record.template_id, row.record.template_version))
            .collect();
        let replaced: BTreeSet<TemplateKey> = wanted
            .iter()
            .copied()
            .filter(|key| {
                !miner
                    .definitions(&tenant, &BTreeSet::from([*key]))
                    .contains_key(key)
            })
            .collect();
        if replaced.is_empty() {
            continue;
        }
        covered += 1;
        let defined = miner.definitions_with(&kept, &tenant, &wanted);
        for key in &wanted {
            assert_eq!(defined.get(key), recorded.get(key), "cut {cut}, {key:?}");
        }
    }
    assert!(
        covered > 0,
        "a cut where a kept row uses a replaced version"
    );
}
