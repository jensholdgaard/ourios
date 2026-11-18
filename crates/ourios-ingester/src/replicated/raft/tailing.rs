//! A tail replica's side of `Tail` and `TailScan` (RFC 0065 §6.7): the
//! read point it confirms, and the pages of a tenant's unpublished rows it
//! has kept above the reader's checkpoint, each row carrying its position
//! under the group's log id and each page the definitions of the templates
//! its rows reference.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use ourios_core::record::{IngestPosition, MinedRecord};
use ourios_core::tenant::TenantId;
use tokio::time::Instant;

use super::host::{GroupHost, Tailed};
use super::kept::{Definitions, Kept, RowPage, RowScan, TemplateKey};
use super::read_point::{caught_up, read_point_of};
use super::transport::Peers;
use super::transport::tail::{
    PAGE_BYTES, Refusal, ScanPage, ScanQuery, Scanning, TailPoint, TailQuery, TailTarget, Tailing,
    TemplateDefinition, WirePosition,
};

/// How long a read waits for its read point, or for the replica to catch
/// up to one: a starting value.
pub const READ_PATIENCE: Duration = Duration::from_secs(5);

/// The groups a node mines, as `Tail` and `TailScan` reach them.
pub struct TailReads {
    groups: Arc<BTreeMap<u32, Tailed>>,
    peers: Peers,
    patience: Duration,
    page_bytes: usize,
}

impl TailReads {
    /// The groups `host` mines, asking their leaders through `peers`.
    #[must_use]
    pub fn new(host: &GroupHost, peers: Peers) -> Self {
        let groups = host
            .groups()
            .filter_map(|group| Some((group, host.tailed(group)?)))
            .collect();
        Self {
            groups: Arc::new(groups),
            peers,
            patience: READ_PATIENCE,
            page_bytes: PAGE_BYTES,
        }
    }

    /// As [`Self::new`], with pages of at most `page_bytes`.
    #[cfg(test)]
    pub(super) fn with_page_bytes(self, page_bytes: usize) -> Self {
        Self { page_bytes, ..self }
    }

    fn group(&self, group: u32) -> Result<Tailed, Refusal> {
        self.groups.get(&group).cloned().ok_or(Refusal::NotHosted)
    }
}

impl TailTarget for TailReads {
    fn tail(&self, query: TailQuery) -> Tailing {
        let (group, peers, patience) = (self.group(query.group), self.peers.clone(), self.patience);
        Box::pin(async move {
            let tailed = group?;
            let read = read_point_of(&tailed, &peers, query.group, patience)
                .await
                .map_err(|e| Refusal::Unavailable(e.to_string()))?;
            tailed
                .read_kept(|kept| servable(kept, query.from))
                .ok_or_else(stopped)??;
            // Published keys come with the published entries that name them.
            Ok(TailPoint {
                read,
                published: Vec::new(),
            })
        })
    }

    fn scan(&self, query: ScanQuery) -> Scanning {
        let (group, patience, page_bytes) =
            (self.group(query.group), self.patience, self.page_bytes);
        Box::pin(async move {
            let tailed = group?;
            caught_up(&tailed, query.read, Instant::now() + patience)
                .await
                .map_err(|e| Refusal::Unavailable(e.to_string()))?;
            let log = tailed.log().await.map_err(Refusal::Unavailable)?;
            let tenant = TenantId::try_new(query.tenant.clone())
                .map_err(|e| Refusal::Unavailable(format!("tenant: {e}")))?;
            let page = read_page(&tailed, &tenant, &query, page_bytes / 2)?;
            // Asked after the page is read, so any widening since is in
            // what the task answers from: its kept events bind the version
            // the widening replaced.
            let definitions = tailed
                .definitions(&tenant, referenced(&page.rows))
                .await
                .ok_or_else(stopped)?;
            encoded(page, &definitions, (log, page_bytes))
        })
    }
}

/// The refusal of a read whose group's miner has stopped.
fn stopped() -> Refusal {
    Refusal::Unavailable("the miner stopped".to_owned())
}

/// The page `query` reads of what `tailed` keeps, within `budget`
/// estimated bytes.
fn read_page(
    tailed: &Tailed,
    tenant: &TenantId,
    query: &ScanQuery,
    budget: usize,
) -> Result<RowPage, Refusal> {
    let scan = RowScan {
        tenant,
        from: query.from,
        read: query.read,
        range: query.range,
        after: query.after.map(|after| (after.seq, after.index)),
    };
    tailed
        .read_kept(|kept| {
            servable(kept, query.from)?;
            Ok(kept.page(&scan, budget))
        })
        .ok_or_else(stopped)?
}

/// The template versions `rows` use.
fn referenced(rows: &[super::mining::MinedRow]) -> BTreeSet<TemplateKey> {
    rows.iter()
        .map(|row| (row.record.template_id, row.record.template_version))
        .collect()
}

/// `page` as served: its rows under the group's `log` id, encoded to at
/// most `page_bytes` by giving the rest to the next page, with the
/// definitions of the templates the rows served reference.
fn encoded(
    mut page: RowPage,
    definitions: &Definitions,
    (log, page_bytes): (u32, usize),
) -> Result<ScanPage, Refusal> {
    loop {
        let records: Vec<MinedRecord> = page
            .rows
            .iter()
            .map(|row| MinedRecord {
                ingest_position: Some(IngestPosition {
                    group: log,
                    seq: row.position.seq,
                    index: row.position.index,
                }),
                ..row.record.clone()
            })
            .collect();
        let rows = ourios_parquet::ipc::encode(&records)
            .map_err(|e| Refusal::Unavailable(e.to_string()))?;
        if rows.len() > page_bytes && page.rows.len() > 1 {
            page.rows.truncate(page.rows.len() / 2);
            page.next = page
                .rows
                .last()
                .map(|row| (row.position.seq, row.position.index));
            continue;
        }
        let templates = templates_of(&referenced(&page.rows), definitions)?;
        let next = page.next.map(|(seq, index)| WirePosition { seq, index });
        return Ok(ScanPage {
            rows,
            templates,
            next,
        });
    }
}

/// The definitions of `wanted`; a version neither the kept events nor the
/// miner define leaves the page unservable, as rows it could not render.
fn templates_of(
    wanted: &BTreeSet<TemplateKey>,
    definitions: &Definitions,
) -> Result<Vec<TemplateDefinition>, Refusal> {
    wanted
        .iter()
        .map(|&(template_id, template_version)| {
            let template = definitions
                .get(&(template_id, template_version))
                .ok_or_else(|| {
                    Refusal::Unavailable(format!(
                        "no definition of template {template_id} version {template_version}"
                    ))
                })?;
            Ok(TemplateDefinition {
                template_id,
                template_version,
                template: template.clone(),
            })
        })
        .collect()
}

/// Why `kept` cannot serve a reader holding the checkpoint cut `from`:
/// a newer cut has dropped rows above `from`, which the reader then finds
/// in the store from that cut, or a gap has, whose rows only a flusher's
/// replay or a later checkpoint publishes.
pub(super) fn servable(kept: &Kept, from: u64) -> Result<(), Refusal> {
    match (kept.cut(), kept.gap()) {
        (cut, _) if cut > from => Err(Refusal::TooOld { oldest: cut }),
        (_, Some(gap)) if gap > from => Err(Refusal::Unavailable(format!(
            "rows through {gap} were dropped from the tail"
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
