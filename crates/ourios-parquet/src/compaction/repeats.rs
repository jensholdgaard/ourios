//! Dropping the rows that repeat an ingest position (RFC 0065 §6.6) as the
//! sorted rows go out: exactly, and in memory bounded by the sort budget.
//!
//! A repeat is the same row (§6.5), so it has the same RFC 0036 §3.1 key
//! and arrives among the rows of equal key; the positions of the current
//! run of equal keys are all that need remembering. Rows without a
//! timestamp all sort at `(service, 0)`, so one run can span a partition:
//! past its share of the budget, the run's positions spill to local
//! scratch as sorted runs of fixed-width records. A lookup skips every
//! run whose range cannot hold the position and bisects the rest. Runs
//! merge in tiers of [`POSITION_FAN_IN`], so a position is rewritten once
//! per tier and fewer than `POSITION_FAN_IN` runs are open per tier.

// The parent scope is this module's import surface, as for its siblings.
#[allow(clippy::wildcard_imports)]
use super::*;

use std::cmp::Reverse;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};

use ourios_core::record::IngestPosition;

/// Bytes one position takes in a run file: `group`, `seq` and `index`,
/// big-endian, so the bytes order as the positions do.
const RECORD: usize = 16;

/// Bytes one slot of the in-memory set is charged: the key and the hash
/// table's share around it.
const HELD_BYTES: u64 = 32;

/// Runs of one tier merged into one of the next. Small, so the runs a
/// lookup searches and the files held open stay few beside the sort's
/// own merge cursors.
const POSITION_FAN_IN: usize = 8;

/// A set holding more slots than this is released, not cleared, when the
/// key moves on, so one wide run of keys does not keep its allocation.
const KEPT_SLOTS: usize = 1024;

type Record = [u8; RECORD];

fn encode(position: IngestPosition) -> Record {
    let mut record = [0; RECORD];
    record[..4].copy_from_slice(&position.group.to_be_bytes());
    record[4..12].copy_from_slice(&position.seq.to_be_bytes());
    record[12..].copy_from_slice(&position.index.to_be_bytes());
    record
}

fn io_error(op: &'static str, path: &Path, source: std::io::Error) -> CompactionError {
    CompactionError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

fn widen(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A sorted run of positions on scratch, its tier and its range.
struct PositionRun {
    path: PathBuf,
    file: File,
    records: u64,
    tier: usize,
    first: Record,
    last: Record,
}

impl PositionRun {
    /// Write `sorted`, which is ascending.
    fn write(
        path: PathBuf,
        tier: usize,
        sorted: impl Iterator<Item = Record>,
    ) -> Result<Self, CompactionError> {
        let created = File::create(&path).map_err(|e| io_error("create position run", &path, e))?;
        let mut out = BufWriter::new(created);
        let (mut records, mut first, mut last) = (0_u64, [0; RECORD], [0; RECORD]);
        for record in sorted {
            if records == 0 {
                first = record;
            }
            last = record;
            out.write_all(&record)
                .map_err(|e| io_error("write position run", &path, e))?;
            records += 1;
        }
        out.flush()
            .map_err(|e| io_error("write position run", &path, e))?;
        let file = File::open(&path).map_err(|e| io_error("open position run", &path, e))?;
        Ok(Self {
            path,
            file,
            records,
            tier,
            first,
            last,
        })
    }

    fn bytes(&self) -> u64 {
        self.records.saturating_mul(widen(RECORD))
    }

    fn record_at(&self, i: u64) -> Result<Record, CompactionError> {
        let mut record = [0; RECORD];
        let mut file = &self.file;
        file.seek(SeekFrom::Start(i.saturating_mul(widen(RECORD))))
            .and_then(|_| file.read_exact(&mut record))
            .map_err(|e| io_error("read position run", &self.path, e))?;
        Ok(record)
    }

    /// Whether `wanted` falls within the run's range.
    fn may_hold(&self, wanted: &Record) -> bool {
        self.records > 0 && (self.first..=self.last).contains(wanted)
    }

    fn contains(&self, wanted: &Record) -> Result<bool, CompactionError> {
        if !self.may_hold(wanted) {
            return Ok(false);
        }
        let (mut low, mut high) = (0, self.records);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.record_at(mid)?.cmp(wanted) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(true),
            }
        }
        Ok(false)
    }

    /// Its records in order, read through the handle lookups use.
    fn records(
        &self,
    ) -> Result<impl Iterator<Item = std::io::Result<Record>> + '_, CompactionError> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| io_error("read position run", &self.path, e))?;
        let mut reader = BufReader::new(file);
        Ok(std::iter::from_fn(move || {
            let mut record = [0; RECORD];
            match reader.read_exact(&mut record) {
                Ok(()) => Some(Ok(record)),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => None,
                Err(e) => Some(Err(e)),
            }
        }))
    }
}

impl Drop for PositionRun {
    fn drop(&mut self) {
        // Scratch: a run left behind goes with its directory.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The positions of the current run of equal keys, and how many rows
/// repeated one of them.
pub(super) struct Repeats<'c> {
    keys: ClusterKeys,
    key: Option<(Option<String>, u64)>,
    held: HashSet<IngestPosition>,
    held_max: usize,
    runs: Vec<PositionRun>,
    scratch: Option<tempfile::TempDir>,
    spilled: usize,
    charge: &'c WriteCharge<'c>,
    pub(super) dropped: u64,
}

impl<'c> Repeats<'c> {
    /// Positions are held up to one open run's share of the sort budget,
    /// the share [`cursor_batch_rows`] gives each run in the merge, and
    /// charged to `charge` as the set grows and shrinks.
    pub(super) fn new(plan: SortPlan<'_>, charge: &'c WriteCharge<'c>) -> Self {
        let SortTuning {
            in_memory_max_bytes,
            fan_in,
            ..
        } = plan.tuning;
        let share = in_memory_max_bytes / widen(fan_in.saturating_add(1));
        let held_max = usize::try_from(share / HELD_BYTES)
            .unwrap_or(usize::MAX)
            .max(1);
        Self {
            keys: plan.keys,
            key: None,
            held: HashSet::new(),
            held_max,
            runs: Vec::new(),
            scratch: None,
            spilled: 0,
            charge,
            dropped: 0,
        }
    }

    /// How many positions are held in memory.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.held.len()
    }

    /// Drop from `rows`, in place, each one that repeats a position
    /// already seen.
    pub(super) fn drop_from(&mut self, rows: &mut Vec<MinedRecord>) -> Result<(), CompactionError> {
        let mut failed = None;
        rows.retain(|row| {
            if failed.is_some() {
                return true;
            }
            self.keep(row).unwrap_or_else(|e| {
                failed = Some(e);
                true
            })
        });
        failed.map_or(Ok(()), Err)
    }

    /// Whether `row` is the first with its position among its equal keys.
    pub(super) fn keep(&mut self, row: &MinedRecord) -> Result<bool, CompactionError> {
        self.follow_key(row);
        let Some(position) = row.ingest_position else {
            return Ok(true);
        };
        if self.seen(position)? {
            self.dropped = self.dropped.saturating_add(1);
            return Ok(false);
        }
        self.held.insert(position);
        self.publish();
        if self.held.len() > self.held_max {
            self.spill()?;
        }
        Ok(true)
    }

    /// Charge the set's slots, allocated or in use.
    fn publish(&self) {
        let slots = widen(self.held.capacity());
        self.charge.positions(slots.saturating_mul(HELD_BYTES));
    }

    /// Forget every position once `row`'s key differs from the last.
    fn follow_key(&mut self, row: &MinedRecord) {
        let service = match self.keys {
            ClusterKeys::ServiceThenTime => {
                project_string_value(&row.resource_attributes, SERVICE_NAME_KEY)
            }
            ClusterKeys::TimeOnly => None,
        };
        let same = self
            .key
            .as_ref()
            .is_some_and(|(s, time)| *time == row.time_unix_nano && s.as_deref() == service);
        if same {
            return;
        }
        self.key = Some((service.map(str::to_owned), row.time_unix_nano));
        if self.held.capacity() > KEPT_SLOTS {
            self.held = HashSet::new();
            self.publish();
        } else {
            self.held.clear();
        }
        self.runs.clear();
    }

    fn seen(&self, position: IngestPosition) -> Result<bool, CompactionError> {
        if self.held.contains(&position) {
            return Ok(true);
        }
        let record = encode(position);
        for run in &self.runs {
            if run.contains(&record)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn next_path(&mut self) -> Result<PathBuf, CompactionError> {
        let scratch = match self.scratch.take() {
            Some(scratch) => scratch,
            None => tempfile::tempdir()
                .map_err(|e| io_error("create scratch", Path::new("<scratch>"), e))?,
        };
        let path = scratch
            .path()
            .join(format!("positions-{:06}.run", self.spilled));
        self.scratch = Some(scratch);
        self.spilled += 1;
        Ok(path)
    }

    /// Keep `run`, counting it as a spill.
    fn written(&mut self, run: PositionRun) {
        self.charge.gauges().spilled(run.bytes());
        self.runs.push(run);
    }

    /// Write the held positions as a tier-0 run, then merge each tier that
    /// has filled into one run of the next.
    fn spill(&mut self) -> Result<(), CompactionError> {
        let mut sorted: Vec<Record> = self.held.drain().map(encode).collect();
        sorted.sort_unstable();
        let path = self.next_path()?;
        let run = PositionRun::write(path, 0, sorted.into_iter())?;
        self.written(run);
        let mut tier = 0;
        while self.runs.iter().filter(|run| run.tier == tier).count() >= POSITION_FAN_IN {
            self.merge_tier(tier)?;
            tier += 1;
        }
        Ok(())
    }

    /// Merge the runs of `tier` into one run of the next, streaming
    /// through the runs' own handles. Runs never share a position, since
    /// a position is held only once it is unseen.
    fn merge_tier(&mut self, tier: usize) -> Result<(), CompactionError> {
        let (group, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.runs)
            .into_iter()
            .partition(|run| run.tier == tier);
        self.runs = rest;
        let path = self.next_path()?;
        let mut readers = Vec::with_capacity(group.len());
        for run in &group {
            readers.push(run.records()?);
        }
        let mut heap = BinaryHeap::with_capacity(readers.len());
        let mut failed = None;
        for (i, reader) in readers.iter_mut().enumerate() {
            push_next(reader, i, &mut heap, &mut failed);
        }
        let merged = std::iter::from_fn(|| {
            let Reverse((record, i)) = heap.pop()?;
            push_next(&mut readers[i], i, &mut heap, &mut failed);
            Some(record)
        });
        let run = PositionRun::write(path, tier + 1, merged)?;
        if let Some(e) = failed {
            return Err(io_error("read position run", &run.path, e));
        }
        self.written(run);
        Ok(())
    }
}

/// Push run `i`'s next record onto `heap`, noting a read that failed.
fn push_next(
    reader: &mut impl Iterator<Item = std::io::Result<Record>>,
    i: usize,
    heap: &mut BinaryHeap<Reverse<(Record, usize)>>,
    failed: &mut Option<std::io::Error>,
) {
    match reader.next() {
        Some(Ok(record)) => heap.push(Reverse((record, i))),
        Some(Err(e)) => *failed = Some(e),
        None => {}
    }
}
