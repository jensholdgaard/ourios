//! RFC 0036 §3.2 external merge sort — budgeted run formation, spill
//! runs, k-way merge, and the test-only residency gauge.

// The parent scope IS this module's import surface: the split was
// mechanical code motion, and gluing back through `super` keeps every
// pre-split path — types, siblings, external crates — resolving
// unchanged (epic #745 wave 1).
#[allow(clippy::wildcard_imports)]
use super::*;

use ourios_core::otlp::{AnyValue, KeyValue, any_value};
use ourios_core::record::Param;

/// The fixed inputs of one RFC 0036 §3.2 sort of a partition.
#[derive(Clone, Copy)]
pub(super) struct SortPlan<'a> {
    pub(super) store: &'a Store,
    pub(super) partition: &'a PartitionKey,
    pub(super) promoted: &'a PromotedAttributes,
    pub(super) keys: ClusterKeys,
    pub(super) tuning: SortTuning,
}

/// What one sort carried: rows written, input bytes read, and rows an
/// erasure dropped.
#[derive(Debug, Default)]
pub(super) struct SortTotals {
    pub(super) rows: u64,
    pub(super) bytes_read: u64,
    pub(super) rows_dropped: u64,
}

/// Phases 1–2 of the RFC 0036 §3.2 external merge sort: stream
/// `inputs` (already in sorted-basename order) batch by batch, sort by
/// the §3.1 key, and emit every row into `writer` in that key order.
///
/// Decoded rows buffer until their [`decoded_footprint`] exceeds
/// [`SortTuning::in_memory_max_bytes`]; a partition that never crosses
/// it sorts wholly in memory, and one that does spills the buffer as a
/// sorted run at the end of the input that filled it. Peak residency is
/// therefore the budget plus one decoded input in phase 1 and, in
/// phase 2, one decoded batch per open run — sized so the open runs
/// together hold at most the budget, or one row each when a row is wider
/// than its share (see [`cursor_batch_rows`]) — plus the merge's
/// [`SUB_BATCH_ROWS`] output
/// chunk; independent of how many inputs the partition holds or how
/// well they compress.
pub(super) fn sort_inputs_into(
    writer: &mut Writer,
    plan: SortPlan<'_>,
    inputs: &[String],
    hooks: &mut RowHooks<'_>,
) -> Result<SortTotals, CompactionError> {
    let mut formation = RunFormation::new(plan);
    let mut totals = SortTotals::default();
    for input in inputs {
        let mut reader = open_input(plan, input, &mut totals)?;
        while let Some(batch) = reader.next_batch().map_err(CompactionError::Read)? {
            formation.push(apply_hooks(hooks, batch, &mut totals));
        }
        formation.end_input()?;
    }
    formation.finish(writer)?;
    Ok(totals)
}

/// Fetch one input and open it under the partition, so every row is
/// validated against the partition as it decodes (RFC 0005 §3.9 /
/// RFC0009.5).
fn open_input(
    plan: SortPlan<'_>,
    input: &str,
    totals: &mut SortTotals,
) -> Result<Reader, CompactionError> {
    let bytes = plan
        .store
        .get_blocking(input)
        .map_err(|e| store_io("get", input, e))?;
    totals.bytes_read = totals.bytes_read.saturating_add(bytes.len() as u64);
    Reader::open_partition_bytes(Bytes::from(bytes), plan.partition.clone(), input)
        .map_err(CompactionError::Read)
}

/// RFC 0047 §3.3/§3.6: the graph feed sees every row once, before any
/// drop; an erasure removes its rows before the sort.
fn apply_hooks(
    hooks: &mut RowHooks<'_>,
    mut batch: Vec<MinedRecord>,
    totals: &mut SortTotals,
) -> Vec<MinedRecord> {
    if let Some(observe) = hooks.observe.as_deref_mut() {
        observe(&batch);
    }
    if let Some(drop) = hooks.drop {
        let before = batch.len();
        batch.retain(|record| !drop(record));
        totals.rows_dropped = totals
            .rows_dropped
            .saturating_add(widen(before - batch.len()));
    }
    totals.rows = totals.rows.saturating_add(widen(batch.len()));
    batch
}

/// `usize <= u64` on every supported target; saturate rather than panic
/// on a theoretically wider one.
fn widen(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Phase 1 of the sort: the decoded-row buffer and, once it has
/// overflowed the budget, the sorted runs spilled to local scratch
/// (scratch is cache, not truth — `CLAUDE.md` §3.6; the `TempDir`
/// tears the runs down when the compaction call ends, success or
/// error).
///
/// The buffer holds rows in (input ordinal, row ordinal) order and is
/// sorted stably, and runs are spilled in buffer order, so the merge's
/// run-ordinal tie-break realises the §3.1 total order however the
/// rows were cut into runs.
struct RunFormation<'a> {
    plan: SortPlan<'a>,
    buffer: Vec<MinedRecord>,
    buffered_bytes: u64,
    /// The widest buffered row's [`decoded_footprint`], carried onto the
    /// run the buffer spills as.
    widest_row: u64,
    spill: Option<Spill>,
}

impl<'a> RunFormation<'a> {
    fn new(plan: SortPlan<'a>) -> Self {
        Self {
            plan,
            buffer: Vec::new(),
            buffered_bytes: 0,
            widest_row: 0,
            spill: None,
        }
    }

    fn push(&mut self, batch: Vec<MinedRecord>) {
        #[cfg(test)]
        residency::add(batch.len());
        let mut bytes = 0_u64;
        for record in &batch {
            let footprint = decoded_footprint(record);
            bytes = bytes.saturating_add(footprint);
            self.widest_row = self.widest_row.max(footprint);
        }
        #[cfg(test)]
        residency::add_bytes(bytes);
        self.buffered_bytes = self.buffered_bytes.saturating_add(bytes);
        self.buffer.extend(batch);
    }

    /// Runs are cut only between inputs, so a run never splits one
    /// input's rows and the budget overshoots by at most one input.
    fn end_input(&mut self) -> Result<(), CompactionError> {
        if self.buffered_bytes > self.plan.tuning.in_memory_max_bytes {
            self.spill_buffer()?;
        }
        Ok(())
    }

    fn spill_buffer(&mut self) -> Result<(), CompactionError> {
        let spill = match self.spill.take() {
            Some(spill) => spill,
            None => Spill::new()?,
        };
        let spill = self.spill.insert(spill);
        sort_records(self.plan.keys, &mut self.buffer);
        let path = spill_run(
            spill.scratch.path(),
            spill.runs.len(),
            &self.buffer,
            self.plan.promoted,
        )?;
        spill.runs.push(Run {
            path,
            widest_row: self.widest_row,
        });
        #[cfg(test)]
        {
            residency::sub(self.buffer.len());
            residency::sub_bytes(self.buffered_bytes);
        }
        self.buffer.clear();
        self.buffered_bytes = 0;
        self.widest_row = 0;
        Ok(())
    }

    /// Emit every buffered row in §3.1 order: sorted in place when the
    /// partition never left memory, else merged from the spilled runs.
    /// Both drive the writer with the same [`SUB_BATCH_ROWS`] chunking,
    /// so the output is byte-identical either way (§3.5).
    fn finish(mut self, writer: &mut Writer) -> Result<(), CompactionError> {
        if self.spill.is_some() && !self.buffer.is_empty() {
            self.spill_buffer()?;
        }
        let Self {
            plan,
            buffer,
            spill,
            ..
        } = self;
        match spill {
            None => emit_in_memory(plan.keys, buffer, writer),
            Some(spill) => {
                // `clear()` kept the phase-1 allocation; release it before
                // the merge so phase 2 holds only (F + 1) × one batch.
                drop(buffer);
                spill.merge_into(writer, plan)
            }
        }
    }
}

fn emit_in_memory(
    keys: ClusterKeys,
    mut rows: Vec<MinedRecord>,
    writer: &mut Writer,
) -> Result<(), CompactionError> {
    sort_records(keys, &mut rows);
    writer
        .append_records(&rows)
        .map_err(CompactionError::Write)?;
    #[cfg(test)]
    {
        residency::sub(rows.len());
        residency::sub_bytes(rows.iter().map(decoded_footprint).sum());
    }
    Ok(())
}

/// Sorted runs on local scratch, in spill order.
struct Spill {
    scratch: tempfile::TempDir,
    runs: Vec<Run>,
}

/// A sorted run on local scratch and the widest decoded row it holds —
/// what sizes its reader's batches in the merge.
pub(super) struct Run {
    pub(super) path: PathBuf,
    pub(super) widest_row: u64,
}

/// Rows per decoded batch for each of `open_runs` cursors: the budget's
/// (F + 1)-th share per run, divided by the run's widest row, so the
/// cursors together hold at most `budget` [`decoded_footprint`] bytes. At
/// most [`SUB_BATCH_ROWS`], the batch narrow rows have always used. At
/// least one row, because a merge cannot advance a run without its head
/// row: a run whose widest row exceeds its share holds that one row, so
/// the cursors' bound is the larger of `budget` and one widest row per
/// open run.
pub(super) fn cursor_batch_rows(budget: u64, open_runs: usize, widest_row: u64) -> usize {
    let share = budget / widen(open_runs.saturating_add(1));
    let rows = share / widest_row.max(1);
    usize::try_from(rows)
        .unwrap_or(usize::MAX)
        .clamp(1, SUB_BATCH_ROWS)
}

impl Spill {
    fn new() -> Result<Self, CompactionError> {
        let scratch = tempfile::tempdir().map_err(|source| CompactionError::Io {
            op: "create scratch",
            path: PathBuf::from("<scratch>"),
            source,
        })?;
        Ok(Self {
            scratch,
            runs: Vec::new(),
        })
    }

    fn merge_into(self, writer: &mut Writer, plan: SortPlan<'_>) -> Result<(), CompactionError> {
        let budget = plan.tuning.in_memory_max_bytes;
        let runs = reduce_runs(
            self.scratch.path(),
            self.runs,
            plan.tuning.fan_in,
            budget,
            plan.keys,
            plan.promoted,
        )?;
        merge_runs(&runs, plan.keys, budget, |chunk| {
            writer.append_records(chunk).map_err(CompactionError::Write)
        })
    }
}

/// Approximate heap bytes one decoded row holds: the record itself plus
/// its strings, params, separators and attribute trees. It sizes the
/// sort's in-memory budget, so it tracks what decoding allocates rather
/// than what the row costs on disk — small, well-compressed inputs
/// decode to many times their encoded size.
pub(super) fn decoded_footprint(record: &MinedRecord) -> u64 {
    let strings: usize = [
        &record.severity_text,
        &record.scope_name,
        &record.scope_version,
        &record.resource_schema_url,
        &record.scope_schema_url,
        &record.event_name,
        &record.body,
    ]
    .into_iter()
    .flatten()
    .map(String::len)
    .sum();
    let params: usize = record
        .params
        .iter()
        .map(|p| size_of::<Param>() + p.value.len())
        .sum();
    let separators: usize = record
        .separators
        .iter()
        .map(|s| size_of::<String>() + s.len())
        .sum();
    let attributes: usize = [
        &record.attributes,
        &record.resource_attributes,
        &record.scope_attributes,
    ]
    .into_iter()
    .map(|kvs| key_values_footprint(kvs))
    .sum();
    let fixed = size_of::<MinedRecord>() + record.tenant_id.as_str().len();
    widen(fixed + strings + params + separators + attributes)
}

fn key_values_footprint(kvs: &[KeyValue]) -> usize {
    kvs.iter()
        .map(|kv| {
            size_of::<KeyValue>() + kv.key.len() + kv.value.as_ref().map_or(0, any_value_footprint)
        })
        .sum()
}

fn any_value_footprint(value: &AnyValue) -> usize {
    match &value.value {
        Some(any_value::Value::StringValue(s)) => s.len(),
        Some(any_value::Value::BytesValue(b)) => b.len(),
        Some(any_value::Value::ArrayValue(array)) => array
            .values
            .iter()
            .map(|v| size_of::<AnyValue>() + any_value_footprint(v))
            .sum(),
        Some(any_value::Value::KvlistValue(list)) => key_values_footprint(&list.values),
        _ => 0,
    }
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
/// the §3.1 tie-break (input ordinal) survives every level. Each merge
/// reads its runs within `budget` (see [`merge_runs`]).
pub(super) fn reduce_runs(
    scratch: &Path,
    mut runs: Vec<Run>,
    fan_in: usize,
    budget: u64,
    keys: ClusterKeys,
    promoted: &PromotedAttributes,
) -> Result<Vec<Run>, CompactionError> {
    let fan_in = fan_in.max(2);
    let mut next_index = runs.len();
    while runs.len() > fan_in {
        let mut merged = Vec::with_capacity(runs.len().div_ceil(fan_in));
        for group in runs.chunks(fan_in) {
            if let [single] = group {
                merged.push(Run {
                    path: single.path.clone(),
                    widest_row: single.widest_row,
                });
                continue;
            }
            let path = scratch.join(format!("run-{next_index:06}.parquet"));
            next_index += 1;
            let mut out = RunWriter::create(&path, promoted)?;
            merge_runs(group, keys, budget, |chunk| out.append(chunk))?;
            out.finish()?;
            let widest_row = group.iter().map(|run| run.widest_row).max().unwrap_or(0);
            merged.push(Run { path, widest_row });
            for consumed in group {
                // Best-effort: the TempDir reclaims scratch either way;
                // early removal just bounds peak scratch-disk use.
                let _ = std::fs::remove_file(&consumed.path);
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
/// Peak memory is one decoded batch per run plus the output chunk
/// being filled: each [`RunCursor`] streams its file in batches
/// [`cursor_batch_rows`] sizes so the open runs hold at most `budget`
/// bytes together (or one row each, when a row is wider than its share),
/// and the chunk holds at most
/// [`SUB_BATCH_ROWS`] rows — no matter how many inputs the partition
/// accrued.
pub(super) fn merge_runs<F>(
    runs: &[Run],
    keys: ClusterKeys,
    budget: u64,
    mut emit: F,
) -> Result<(), CompactionError>
where
    F: FnMut(&[MinedRecord]) -> Result<(), CompactionError>,
{
    let mut cursors = Vec::with_capacity(runs.len());
    for run in runs {
        let batch_rows = cursor_batch_rows(budget, runs.len(), run.widest_row);
        cursors.push(RunCursor::open(&run.path, batch_rows)?);
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
        #[cfg(test)]
        {
            residency::add(1);
            residency::add_bytes(decoded_footprint(&entry.record));
        }
        out.push(entry.record);
        if out.len() == SUB_BATCH_ROWS {
            emit_chunk(&mut out, &mut emit)?;
        }
        if let Some(record) = cursors[run].next_record()? {
            heap.push(Reverse(MergeEntry::new(keys, run, record)));
        }
    }
    if !out.is_empty() {
        emit_chunk(&mut out, &mut emit)?;
    }
    Ok(())
}

fn emit_chunk<F>(out: &mut Vec<MinedRecord>, emit: &mut F) -> Result<(), CompactionError>
where
    F: FnMut(&[MinedRecord]) -> Result<(), CompactionError>,
{
    emit(out)?;
    #[cfg(test)]
    {
        residency::sub(out.len());
        residency::sub_bytes(out.iter().map(decoded_footprint).sum());
    }
    out.clear();
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
    /// per open run, and the merge charges its output chunk on top, so
    /// the gauge peaks at `(F + 1) × batch`, not the whole partition.
    /// The count is charged for the whole batch's lifetime (an
    /// over-count while its rows drain into the heap and the output
    /// chunk), and released when the next batch loads or the run is
    /// exhausted.
    #[cfg(test)]
    batch_len: usize,
    /// The same batch's [`decoded_footprint`] bytes, for the byte gauge.
    #[cfg(test)]
    batch_bytes: u64,
}

impl RunCursor {
    fn open(path: &Path, batch_rows: usize) -> Result<Self, CompactionError> {
        Ok(Self {
            reader: Reader::open_streaming_file(path, batch_rows).map_err(CompactionError::Read)?,
            batch: Vec::new().into_iter(),
            #[cfg(test)]
            batch_len: 0,
            #[cfg(test)]
            batch_bytes: 0,
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
                    residency::sub_bytes(self.batch_bytes);
                    self.batch_bytes = batch.iter().map(decoded_footprint).sum();
                    residency::add_bytes(self.batch_bytes);
                }
                self.batch = batch.into_iter();
            } else {
                #[cfg(test)]
                {
                    residency::sub(self.batch_len);
                    self.batch_len = 0;
                    residency::sub_bytes(self.batch_bytes);
                    self.batch_bytes = 0;
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
/// can assert the phase bounds (one input, then `(F + 1) × batch`) rather than
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
        static CURRENT_BYTES: Cell<u64> = const { Cell::new(0) };
        static PEAK_BYTES: Cell<u64> = const { Cell::new(0) };
    }

    /// Zero both the running counts and the high-water marks before a
    /// measured compaction.
    pub(in crate::compaction) fn reset() {
        CURRENT.with(|c| c.set(0));
        PEAK.with(|p| p.set(0));
        CURRENT_BYTES.with(|c| c.set(0));
        PEAK_BYTES.with(|p| p.set(0));
    }

    /// The peak concurrently-live decoded bytes ([`super::decoded_footprint`])
    /// since the last [`reset`] — the same residency as [`peak`], weighed
    /// by row width.
    pub(in crate::compaction) fn peak_bytes() -> u64 {
        PEAK_BYTES.with(Cell::get)
    }

    /// `n` decoded bytes entered residency.
    pub(in crate::compaction) fn add_bytes(n: u64) {
        let now = CURRENT_BYTES.with(|c| {
            let now = c.get() + n;
            c.set(now);
            now
        });
        PEAK_BYTES.with(|p| {
            if now > p.get() {
                p.set(now);
            }
        });
    }

    /// `n` decoded bytes left residency; underflow panics, as [`sub`].
    pub(in crate::compaction) fn sub_bytes(n: u64) {
        CURRENT_BYTES.with(|c| {
            let now = c
                .get()
                .checked_sub(n)
                .expect("residency byte gauge underflow: unbalanced add/sub");
            c.set(now);
        });
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
