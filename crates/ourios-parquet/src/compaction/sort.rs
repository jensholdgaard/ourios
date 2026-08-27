//! RFC 0036 §3.2 external merge sort — phases, spill runs, k-way merge,
//! and the test-only residency gauge. Split from the flat `compaction.rs`
//! (epic #745 wave 1); pure code motion.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path — types, siblings, external crates — resolving
// unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

/// Where the RFC 0036 §3.2 sort currently holds the partition's rows:
/// decoded in memory while the running encoded input total fits
/// [`SortTuning::in_memory_max_bytes`], or spilled to local scratch as
/// sorted runs once it doesn't (scratch is cache, not truth —
/// `CLAUDE.md` §3.6; the `TempDir` tears the runs down when the
/// compaction call ends, success or error).
pub(super) enum SortState {
    /// One decoded-row vec per input, in input-ordinal order.
    Buffering(Vec<Vec<MinedRecord>>),
    /// Sorted runs on scratch, one per input so far, in input-ordinal
    /// order.
    Spilling {
        scratch: tempfile::TempDir,
        runs: Vec<PathBuf>,
    },
}

/// Phases 1–2 of the RFC 0036 §3.2 external merge sort: decode
/// `inputs` (already in sorted-basename order) one at a time, sort by
/// the §3.1 key, and emit every row into `writer` in that key order.
/// Returns `(rows, input bytes read)`.
///
/// Peak decoded-row residency depends on the path (see [`SortTuning`]):
/// - **Spill path** (encoded input total > `in_memory_max_bytes`): one
///   decoded input file during run formation (inputs are decoded
///   strictly one at a time, then spilled), then — via [`reduce_runs`]'s
///   fan-in cap F — F × one decoded batch during the merge. This
///   preserves the pre-sort one-input-file bound.
/// - **In-memory path** (encoded input total ≤ `in_memory_max_bytes`,
///   default 256 MiB = one ingest seal target): all inputs' decoded
///   rows are held at once to sort in place and skip spilling — bounded
///   by one seal-target's worth of input, so no larger than decoding a
///   single worst-case input file (the [`SortTuning`] tradeoff).
#[allow(clippy::too_many_arguments)] // one call site; the tuple of sort inputs is the seam
pub(super) fn sort_inputs_into(
    writer: &mut Writer,
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
    keys: ClusterKeys,
    tuning: SortTuning,
    inputs: &[String],
    hooks: &mut RowHooks<'_>,
) -> Result<(u64, u64, u64), CompactionError> {
    let mut row_count: u64 = 0;
    let mut bytes_read: u64 = 0;
    let mut rows_dropped: u64 = 0;
    let mut state = SortState::Buffering(Vec::new());
    for input in inputs {
        let bytes = store
            .get_blocking(input)
            .map_err(|e| store_io("get", input, e))?;
        bytes_read = bytes_read.saturating_add(bytes.len() as u64);
        let reader = Reader::open_partition_bytes(Bytes::from(bytes), partition.clone(), input)
            .map_err(CompactionError::Read)?;
        let mut records = reader.read_all().map_err(CompactionError::Read)?;
        // RFC 0047 §3.3/§3.6: the graph feed sees every row once (before
        // any drop); an erasure removes its rows before the sort.
        if let Some(observe) = hooks.observe.as_deref_mut() {
            observe(&records);
        }
        if let Some(drop) = hooks.drop {
            let before = records.len();
            records.retain(|record| !drop(record));
            rows_dropped = rows_dropped
                .saturating_add(u64::try_from(before - records.len()).unwrap_or(u64::MAX));
        }
        #[cfg(test)]
        residency::add(records.len());
        // `usize <= u64` on every supported target; saturate rather than panic
        // on a theoretically wider one.
        row_count = row_count.saturating_add(u64::try_from(records.len()).unwrap_or(u64::MAX));
        state = match state {
            SortState::Buffering(mut buffered) if bytes_read <= tuning.in_memory_max_bytes => {
                buffered.push(records);
                SortState::Buffering(buffered)
            }
            // Crossed the in-memory bound: spill mode from here on.
            // Flush the inputs buffered so far as sorted runs first,
            // preserving input-ordinal order (the §3.1 tie-break).
            SortState::Buffering(buffered) => {
                let scratch = tempfile::tempdir().map_err(|source| CompactionError::Io {
                    op: "create scratch",
                    path: PathBuf::from("<scratch>"),
                    source,
                })?;
                let mut runs = Vec::with_capacity(inputs.len());
                for mut prior in buffered {
                    sort_records(keys, &mut prior);
                    runs.push(spill_run(scratch.path(), runs.len(), &prior, promoted)?);
                    #[cfg(test)]
                    residency::sub(prior.len());
                }
                sort_records(keys, &mut records);
                runs.push(spill_run(scratch.path(), runs.len(), &records, promoted)?);
                #[cfg(test)]
                residency::sub(records.len());
                SortState::Spilling { scratch, runs }
            }
            SortState::Spilling { scratch, mut runs } => {
                sort_records(keys, &mut records);
                runs.push(spill_run(scratch.path(), runs.len(), &records, promoted)?);
                #[cfg(test)]
                residency::sub(records.len());
                SortState::Spilling { scratch, runs }
            }
        };
    }
    match state {
        // Whole partition within the one-input-file bound: sort in
        // memory and skip spilling (§3.2 / §7). Concatenation order is
        // (input ordinal, row ordinal), so the stable sort realises the
        // §3.1 tie-break; the single `append_records` call sub-batches
        // exactly as the merge path's chunked emit does (§3.5).
        SortState::Buffering(buffered) => {
            let mut rows: Vec<MinedRecord> = buffered.into_iter().flatten().collect();
            sort_records(keys, &mut rows);
            writer
                .append_records(&rows)
                .map_err(CompactionError::Write)?;
            #[cfg(test)]
            residency::sub(rows.len());
        }
        SortState::Spilling { scratch, runs } => {
            let runs = reduce_runs(scratch.path(), runs, tuning.fan_in, keys, promoted)?;
            merge_runs(&runs, keys, |chunk| {
                writer.append_records(chunk).map_err(CompactionError::Write)
            })?;
            drop(scratch);
        }
    }
    Ok((row_count, bytes_read, rows_dropped))
}

/// Stable §3.1 sort of one input's decoded rows: promoted
/// `service.name` value (lexicographic UTF-8 bytes, absent/null first)
/// then `time_unix_nano` — stability preserves pre-sort row ordinals,
/// the second half of the §3.1 tie-break.
pub(super) fn sort_records(keys: ClusterKeys, records: &mut [MinedRecord]) {
    match keys {
        ClusterKeys::ServiceThenTime => records.sort_by(|a, b| {
            let ka = (
                project_string_value(&a.resource_attributes, SERVICE_NAME_KEY),
                a.time_unix_nano,
            );
            let kb = (
                project_string_value(&b.resource_attributes, SERVICE_NAME_KEY),
                b.time_unix_nano,
            );
            ka.cmp(&kb)
        }),
        ClusterKeys::TimeOnly => records.sort_by_key(|r| r.time_unix_nano),
    }
}

/// A sorted run being written to local scratch (RFC 0036 §3.2): a
/// Parquet file in the data schema with spill-oriented properties —
/// no dictionaries, no statistics, light compression — since a run is
/// write-once read-once cache whose bytes influence the output only
/// through the decoded rows.
pub(super) struct RunWriter<'a> {
    inner: ArrowWriter<File>,
    promoted: &'a PromotedAttributes,
}

impl<'a> RunWriter<'a> {
    fn create(path: &Path, promoted: &'a PromotedAttributes) -> Result<Self, CompactionError> {
        let file = File::create(path).map_err(|source| CompactionError::Io {
            op: "create run",
            path: path.to_path_buf(),
            source,
        })?;
        let zstd = ZstdLevel::try_new(1).map_err(parquet_write)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(zstd))
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .build();
        let inner = ArrowWriter::try_new(file, data_schema_with_promoted(promoted), Some(props))
            .map_err(parquet_write)?;
        Ok(Self { inner, promoted })
    }

    fn append(&mut self, records: &[MinedRecord]) -> Result<(), CompactionError> {
        for chunk in records.chunks(SUB_BATCH_ROWS) {
            // Cap the buffered row group so writing an intermediate
            // merge run never holds the whole merged output encoded in
            // memory (the §3.2 phase-2 bound). Intermediate runs are
            // write-once scratch, so the fixed ceiling is fine here — the
            // adaptive threshold governs only the final consolidated file.
            if self.inner.in_progress_size() >= MAX_COMPACTED_RG_BYTES {
                self.inner.flush().map_err(parquet_write)?;
            }
            let batch = mined_records_to_batch_with_promoted(chunk, self.promoted)
                .map_err(|e| CompactionError::Write(WriterError::Batch(e)))?;
            self.inner.write(&batch).map_err(parquet_write)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<(), CompactionError> {
        self.inner.close().map_err(parquet_write)?;
        Ok(())
    }
}

/// Write one input's sorted rows as run file `index` under `dir`.
pub(super) fn spill_run(
    dir: &Path,
    index: usize,
    records: &[MinedRecord],
    promoted: &PromotedAttributes,
) -> Result<PathBuf, CompactionError> {
    let path = dir.join(format!("run-{index:06}.parquet"));
    let mut run = RunWriter::create(&path, promoted)?;
    run.append(records)?;
    run.finish()?;
    Ok(path)
}

/// Collapse `runs` hierarchically until at most `fan_in` remain
/// (RFC 0036 §3.2's cap F): each pass merges consecutive groups of
/// `fan_in` runs into one intermediate run, preserving run order so
/// the §3.1 tie-break (input ordinal) survives every level.
pub(super) fn reduce_runs(
    scratch: &Path,
    mut runs: Vec<PathBuf>,
    fan_in: usize,
    keys: ClusterKeys,
    promoted: &PromotedAttributes,
) -> Result<Vec<PathBuf>, CompactionError> {
    let fan_in = fan_in.max(2);
    let mut next_index = runs.len();
    while runs.len() > fan_in {
        let mut merged = Vec::with_capacity(runs.len().div_ceil(fan_in));
        for group in runs.chunks(fan_in) {
            if let [single] = group {
                merged.push(single.clone());
                continue;
            }
            let path = scratch.join(format!("run-{next_index:06}.parquet"));
            next_index += 1;
            let mut out = RunWriter::create(&path, promoted)?;
            merge_runs(group, keys, |chunk| out.append(chunk))?;
            out.finish()?;
            merged.push(path);
            for consumed in group {
                // Best-effort: the TempDir reclaims scratch either way;
                // early removal just bounds peak scratch-disk use.
                let _ = std::fs::remove_file(consumed);
            }
        }
        runs = merged;
    }
    Ok(runs)
}

/// K-way merge of sorted `runs` in §3.1 key order, emitting
/// [`SUB_BATCH_ROWS`]-sized chunks to `emit` — exactly the
/// sub-batching `Writer::append_records` applies itself, so the spill
/// path and the in-memory path drive the Parquet writer with an
/// identical call sequence (§3.5).
///
/// Peak memory is one decoded batch per run: each [`RunCursor`]
/// streams its file batch-by-batch, and [`reduce_runs`] caps the run
/// count at F, so this holds ≤ F × batch bytes no matter how many
/// inputs the partition accrued — far below phase 1's
/// one-decoded-input bound.
pub(super) fn merge_runs<F>(
    runs: &[PathBuf],
    keys: ClusterKeys,
    mut emit: F,
) -> Result<(), CompactionError>
where
    F: FnMut(&[MinedRecord]) -> Result<(), CompactionError>,
{
    let mut cursors = Vec::with_capacity(runs.len());
    for path in runs {
        cursors.push(RunCursor::open(path)?);
    }
    let mut heap = BinaryHeap::with_capacity(cursors.len());
    for (run, cursor) in cursors.iter_mut().enumerate() {
        if let Some(record) = cursor.next_record()? {
            heap.push(Reverse(MergeEntry::new(keys, run, record)));
        }
    }
    let mut out: Vec<MinedRecord> = Vec::with_capacity(SUB_BATCH_ROWS);
    while let Some(Reverse(entry)) = heap.pop() {
        let run = entry.run;
        out.push(entry.record);
        if out.len() == SUB_BATCH_ROWS {
            emit(&out)?;
            out.clear();
        }
        if let Some(record) = cursors[run].next_record()? {
            heap.push(Reverse(MergeEntry::new(keys, run, record)));
        }
    }
    if !out.is_empty() {
        emit(&out)?;
    }
    Ok(())
}

/// One run's head row in the merge heap, ordered by (§3.1 key, run
/// ordinal): equal-key rows pop in run order — which is input-ordinal
/// order — and a run holds one input's equal-key rows in pre-sort row
/// order (the stable phase-1 sort), so the pop sequence realises the
/// full §3.1 tie-break.
pub(super) struct MergeEntry {
    /// Promoted `service.name` value, precomputed once so heap
    /// comparisons don't rescan `resource_attributes`. `None` under
    /// [`ClusterKeys::TimeOnly`] regardless of the row.
    service: Option<String>,
    time: u64,
    run: usize,
    record: MinedRecord,
}

impl MergeEntry {
    fn new(keys: ClusterKeys, run: usize, record: MinedRecord) -> Self {
        let service = match keys {
            ClusterKeys::ServiceThenTime => {
                project_string_value(&record.resource_attributes, SERVICE_NAME_KEY)
                    .map(str::to_owned)
            }
            ClusterKeys::TimeOnly => None,
        };
        Self {
            service,
            time: record.time_unix_nano,
            run,
            record,
        }
    }

    fn key(&self) -> (Option<&str>, u64, usize) {
        (self.service.as_deref(), self.time, self.run)
    }
}

impl Ord for MergeEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for MergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for MergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for MergeEntry {}

/// A sorted run streamed batch-by-batch off scratch — the phase-2
/// merge holds exactly one decoded batch per open run.
pub(super) struct RunCursor {
    reader: Reader,
    batch: std::vec::IntoIter<MinedRecord>,
    /// Rows in the batch this cursor currently holds decoded, for the
    /// RFC0036.3 residency gauge: the merge keeps ≤ one batch resident
    /// per open run, so the gauge peaks at `F × batch`, not the whole
    /// partition. The count is charged for the whole batch's lifetime
    /// (a small over-count as its rows drain into the merge heap), and
    /// released when the next batch loads or the run is exhausted.
    #[cfg(test)]
    batch_len: usize,
}

impl RunCursor {
    fn open(path: &Path) -> Result<Self, CompactionError> {
        Ok(Self {
            reader: Reader::open_streaming_file(path).map_err(CompactionError::Read)?,
            batch: Vec::new().into_iter(),
            #[cfg(test)]
            batch_len: 0,
        })
    }

    fn next_record(&mut self) -> Result<Option<MinedRecord>, CompactionError> {
        loop {
            if let Some(record) = self.batch.next() {
                return Ok(Some(record));
            }
            if let Some(batch) = self.reader.next_batch().map_err(CompactionError::Read)? {
                #[cfg(test)]
                {
                    residency::sub(self.batch_len);
                    residency::add(batch.len());
                    self.batch_len = batch.len();
                }
                self.batch = batch.into_iter();
            } else {
                #[cfg(test)]
                {
                    residency::sub(self.batch_len);
                    self.batch_len = 0;
                }
                return Ok(None);
            }
        }
    }
}

/// Map an `ArrowWriter` failure on a run file onto the same
/// [`CompactionError::Write`] channel the consolidated writer uses.
pub(super) fn parquet_write(e: parquet::errors::ParquetError) -> CompactionError {
    CompactionError::Write(WriterError::Parquet(e))
}

/// Test-only decoded-row residency gauge (RFC 0036 §3.2 / RFC0036.3).
/// Counts the `MinedRecord`s the sort holds decoded in RAM on the
/// current thread, exposing the peak so the forced-spill memory test
/// can assert the one-input-plus-`F × batch` bound rather than
/// whole-partition residency (RFC 0036 §6, "an instrumentation counter
/// inside `sort_inputs_into`/`merge_runs`"). Thread-local because a
/// `compact_*` call runs entirely on its caller's thread (blocking I/O
/// throughout), so parallel tests never pollute each other's peak — the
/// property a process-global gauge (or a tracking allocator) cannot
/// offer under `cargo test`'s in-process parallelism.
#[cfg(test)]
pub(in crate::compaction) mod residency {
    use std::cell::Cell;

    thread_local! {
        static CURRENT: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    /// Zero both the running count and the high-water mark before a
    /// measured compaction.
    pub(in crate::compaction) fn reset() {
        CURRENT.with(|c| c.set(0));
        PEAK.with(|p| p.set(0));
    }

    /// The peak concurrently-live decoded-row count since the last
    /// [`reset`].
    pub(in crate::compaction) fn peak() -> usize {
        PEAK.with(Cell::get)
    }

    /// `n` decoded rows entered residency.
    pub(in crate::compaction) fn add(n: usize) {
        let now = CURRENT.with(|c| {
            let now = c.get() + n;
            c.set(now);
            now
        });
        PEAK.with(|p| {
            if now > p.get() {
                p.set(now);
            }
        });
    }

    /// `n` decoded rows left residency (spilled and dropped, or emitted).
    /// Underflow means the instrumentation's add/sub calls are unbalanced
    /// — a bug in the gauge the RFC0036.3 bound relies on — so panic
    /// rather than saturate and silently under-report the peak.
    pub(in crate::compaction) fn sub(n: usize) {
        CURRENT.with(|c| {
            let now = c
                .get()
                .checked_sub(n)
                .expect("residency gauge underflow: unbalanced add/sub");
            c.set(now);
        });
    }
}
