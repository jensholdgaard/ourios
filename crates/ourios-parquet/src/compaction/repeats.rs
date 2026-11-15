//! Dropping the rows that repeat an ingest position (RFC 0065 §6.6) as the
//! sorted rows go out: exactly, and in memory bounded by the sort budget.
//!
//! A repeat is the same row (§6.5), so it has the same RFC 0036 §3.1 key
//! and arrives among the rows of equal key; the positions of the current
//! run of equal keys are all that need remembering. Rows without a
//! timestamp all sort at `(service, 0)`, so one run can span a partition:
//! past its share of the budget, the run's positions spill to local
//! scratch as sorted runs of fixed-width records, searched by bisection
//! and merged once there are more than the fan-in.

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

/// Bytes one position held in memory is charged: the key and the hash
/// table's share around it.
pub(super) const HELD_BYTES: u64 = 32;

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

/// A sorted run of positions on scratch.
struct PositionRun {
    path: PathBuf,
    file: File,
    records: u64,
}

impl PositionRun {
    fn write(path: PathBuf, sorted: impl Iterator<Item = Record>) -> Result<Self, CompactionError> {
        let created = File::create(&path).map_err(|e| io_error("create position run", &path, e))?;
        let mut out = BufWriter::new(created);
        let mut records = 0_u64;
        for record in sorted {
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
        })
    }

    fn record_at(&self, i: u64) -> Result<Record, CompactionError> {
        let mut record = [0; RECORD];
        let mut file = &self.file;
        file.seek(SeekFrom::Start(i * RECORD as u64))
            .and_then(|_| file.read_exact(&mut record))
            .map_err(|e| io_error("read position run", &self.path, e))?;
        Ok(record)
    }

    fn contains(&self, wanted: &Record) -> Result<bool, CompactionError> {
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

    fn reader(
        &self,
    ) -> Result<impl Iterator<Item = std::io::Result<Record>> + use<>, CompactionError> {
        let file =
            File::open(&self.path).map_err(|e| io_error("open position run", &self.path, e))?;
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
pub(super) struct Repeats<'a> {
    keys: ClusterKeys,
    key: Option<(Option<String>, u64)>,
    held: HashSet<IngestPosition>,
    held_max: usize,
    fan_in: usize,
    runs: Vec<PositionRun>,
    scratch: Option<tempfile::TempDir>,
    spilled: usize,
    gauges: Gauges<'a>,
    pub(super) dropped: u64,
}

impl<'a> Repeats<'a> {
    /// Positions are held up to one open run's share of the sort budget,
    /// the share [`cursor_batch_rows`] gives each run in the merge.
    pub(super) fn new(plan: SortPlan<'a>) -> Self {
        let SortTuning {
            in_memory_max_bytes,
            fan_in,
            ..
        } = plan.tuning;
        let share =
            in_memory_max_bytes / u64::try_from(fan_in.saturating_add(1)).unwrap_or(u64::MAX);
        let held_max = usize::try_from(share / HELD_BYTES)
            .unwrap_or(usize::MAX)
            .max(1);
        Self {
            keys: plan.keys,
            key: None,
            held: HashSet::new(),
            held_max,
            fan_in: fan_in.max(2),
            runs: Vec::new(),
            scratch: None,
            spilled: 0,
            gauges: plan.gauges,
            dropped: 0,
        }
    }

    /// The bytes the positions held in memory are charged.
    pub(super) fn held_bytes(&self) -> u64 {
        u64::try_from(self.held.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(HELD_BYTES)
    }

    /// How many positions are held in memory.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.held.len()
    }

    /// Drop from `rows` each one that repeats a position already seen.
    pub(super) fn drop_from(&mut self, rows: &mut Vec<MinedRecord>) -> Result<(), CompactionError> {
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows.drain(..) {
            if self.keep(&row)? {
                kept.push(row);
            }
        }
        *rows = kept;
        Ok(())
    }

    fn keep(&mut self, row: &MinedRecord) -> Result<bool, CompactionError> {
        self.follow_key(row);
        let Some(position) = row.ingest_position else {
            return Ok(true);
        };
        if self.seen(position)? {
            self.dropped = self.dropped.saturating_add(1);
            return Ok(false);
        }
        self.held.insert(position);
        if self.held.len() > self.held_max {
            self.spill()?;
        }
        Ok(true)
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
        if !same {
            self.key = Some((service.map(str::to_owned), row.time_unix_nano));
            self.held.clear();
            self.runs.clear();
        }
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

    /// Write the held positions as a sorted run and clear them; merge the
    /// runs into one once there are more than the fan-in.
    fn spill(&mut self) -> Result<(), CompactionError> {
        let mut sorted: Vec<Record> = self.held.drain().map(encode).collect();
        sorted.sort_unstable();
        let path = self.next_path()?;
        let run = PositionRun::write(path, sorted.into_iter())?;
        self.gauges
            .spilled(run.records.saturating_mul(RECORD as u64));
        self.runs.push(run);
        if self.runs.len() > self.fan_in {
            self.merge()?;
        }
        Ok(())
    }

    /// Merge every run into one, streaming: each run's reader holds one
    /// buffer. Runs never share a position, since a position is held only
    /// once it is unseen.
    fn merge(&mut self) -> Result<(), CompactionError> {
        let path = self.next_path()?;
        let mut readers = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            readers.push(run.reader()?);
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
        let run = PositionRun::write(path, merged)?;
        if let Some(e) = failed {
            return Err(io_error("read position run", &run.path, e));
        }
        self.runs = vec![run];
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
