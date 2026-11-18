//! A tail replica's side of `Tail` and `TailScan` (RFC 0065 §6.7): the
//! read point it confirms, and the pages of a tenant's unpublished rows it
//! has kept above the reader's checkpoint, each row carrying its position
//! under the group's log id and each page the definitions of the templates
//! its rows reference.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use ourios_core::record::{BodyKind, IngestPosition, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::NO_TEMPLATE;
use tokio::time::Instant;

use super::host::{GroupHost, Tailed};
use super::kept::{Definitions, Kept, RowPage, RowScan, TemplateKey};
use super::mining::MinedRow;
use super::read_point::{caught_up, read_point_of};
use super::transport::Peers;
use super::transport::tail::{
    PAGE_BYTES, Refusal, ScanPage, ScanQuery, Scanning, TailPoint, TailQuery, TailTarget, Tailing,
    TemplateDefinition, WirePosition,
};
use super::transport::wire::MAX_MESSAGE_BYTES;

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

/// The template versions `rows` render from: those of rows with a string
/// body and a template. A structured or absent body, or a parse failure,
/// renders without one.
pub(super) fn referenced(rows: &[MinedRow]) -> BTreeSet<TemplateKey> {
    rows.iter()
        .map(|row| &row.record)
        .filter(|record| record.body_kind == BodyKind::String && record.template_id != NO_TEMPLATE)
        .map(|record| (record.template_id, record.template_version))
        .collect()
}

/// `page` as served: its rows under the group's `log` id, encoded to at
/// most `page_bytes` and the whole answer to within the message limit by
/// giving the rest to the next page, with the definitions of the templates
/// the rows served reference. A row that does not fit alone is refused, so
/// no answer passes the bound.
pub(super) fn encoded(
    mut page: RowPage,
    definitions: &Definitions,
    (log, page_bytes): (u32, usize),
) -> Result<ScanPage, Refusal> {
    loop {
        let rows = ourios_parquet::ipc::encode(&stamped(&page.rows, log))
            .map_err(|e| Refusal::Unavailable(e.to_string()))?;
        let templates = templates_of(&referenced(&page.rows), definitions)?;
        let fits = rows.len() <= page_bytes && answer_bytes(&rows, &templates) <= MAX_MESSAGE_BYTES;
        match (fits, page.rows.len()) {
            (true, _) => {
                let next = page.next.map(|(seq, index)| WirePosition { seq, index });
                return Ok(ScanPage {
                    rows,
                    templates,
                    next,
                });
            }
            (false, 0 | 1) => {
                return Err(Refusal::Unavailable(format!(
                    "a row encodes to {} bytes, past the page bound",
                    rows.len()
                )));
            }
            (false, n) => {
                page.rows.truncate(n / 2);
                page.next = page
                    .rows
                    .last()
                    .map(|row| (row.position.seq, row.position.index));
            }
        }
    }
}

/// `rows`' records, each carrying its position under the group's `log` id.
fn stamped(rows: &[MinedRow], log: u32) -> Vec<MinedRecord> {
    rows.iter()
        .map(|row| MinedRecord {
            ingest_position: Some(IngestPosition {
                group: log,
                seq: row.position.seq,
                index: row.position.index,
            }),
            ..row.record.clone()
        })
        .collect()
}

/// What a `TailScan` answer of `rows` and `templates` encodes to, its
/// framing included.
fn answer_bytes(rows: &[u8], templates: &[TemplateDefinition]) -> usize {
    use prost::Message as _;

    // Each field's tag and length prefix, and the message's.
    const FRAMING: usize = 16;
    let templates: usize = templates
        .iter()
        .map(|template| template.encoded_len() + FRAMING)
        .sum();
    rows.len() + templates + 2 * FRAMING
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
/// in the store from that cut; a replay has published and dropped rows
/// above it that the reader's listing may predate; or a gap has dropped
/// rows only a flusher's replay or a later checkpoint publishes.
pub(super) fn servable(kept: &Kept, from: u64) -> Result<(), Refusal> {
    match (kept.cut(), kept.gap()) {
        (cut, _) if cut > from => Err(Refusal::TooOld { oldest: cut }),
        // Restarting from the checkpoint would not help until a cut covers
        // what the replay published.
        _ if kept
            .replayed_through()
            .is_some_and(|through| through > from) =>
        {
            Err(Refusal::Unavailable(
                "rows a replay published are not yet behind a checkpoint".to_owned(),
            ))
        }
        (_, Some(gap)) if gap > from => Err(Refusal::Unavailable(format!(
            "rows through {gap} were dropped from the tail"
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
