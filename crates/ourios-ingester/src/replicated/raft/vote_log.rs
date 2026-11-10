//! The node's vote log, `wal_root/votes.log` (RFC 0065 §4.7, §6.1).
//!
//! Every group on the replica appends its votes here, so a mass election
//! costs one fsync per batch rather than one per group. It sits outside
//! the group WALs, so segment reclamation never touches it.
//!
//! The file is a sequence of 512-byte sectors:
//!
//! ```text
//! | sector 0: magic: [u8; 8] = "OURVOTE\x02" | 504 zero bytes                    |
//! | sector n: slot * 15 | 17 zero bytes (pad)                                    |
//! | slot:     group: u32_le | term: u64_le | voted_for: [u8; 16] (UUID)         |
//! |           | committed: u8 (0 or 1) | crc32c: u32_le (over the 29 B before it) |
//! ```
//!
//! Every fsync batch starts on a fresh sector and ends padded to one, so
//! a sector may hold fewer than 15 slots, zero-filled after them.
//! Recovery keeps the last record per group. Only a tail no `save` was
//! answered for is cut off before the next append: sectors that are all
//! zero bytes, and a partial slot at the end of the file. Any other slot
//! that fails its CRC or holds a `committed` byte other than 0 or 1, a
//! valid slot after a zero one in the same sector, anything non-zero after
//! an all-zero sector, or a non-zero pad byte may be a damaged
//! acknowledged vote, so the open fails, wherever it is: the replica
//! rejoins as a new one (RFC 0065 §6.1, B4) rather than restore an older
//! vote. Past a size threshold the log is rewritten with each group's
//! latest vote (tmp, fsync, rename, directory fsync).
//!
//! [`create`] makes the empty log in the same first-start step that
//! writes the replica's identity, before it; a later open that finds no
//! log fails, since the node's votes are lost. One writer owns a root at
//! a time, through an OS lock on `wal_root/votes.lock` that its thread
//! holds until it exits. The WAL root must be on a local filesystem.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openraft::impls::leader_id_std::LeaderId;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::gate::SyncGate;
use super::telemetry::{Component, Storage};
use super::types::{NodeId, Vote};
use super::{Flush, OWNER_WAIT, Owner, sync_through, take_ownership};

/// The vote log's name under `wal_root`.
pub const VOTES_FILE: &str = "votes.log";
const MAGIC: [u8; 8] = *b"OURVOTE\x02";
const BODY_LEN: usize = 4 + 8 + 16 + 1;
const RECORD_LEN: usize = BODY_LEN + 4;
// No slot straddles a sector, and every fsync batch starts on a fresh
// sector and zero-pads its last one before the fsync, so an fsynced sector
// is never written again. A torn write, which loses whole sectors, can then
// only zero sectors of a batch no save was answered for: a slot that is
// partly valid is damage.
const SECTOR: usize = 512;
const SLOTS_PER_SECTOR: usize = SECTOR / RECORD_LEN;
/// The header sector, which holds no slot.
const HEADER_LEN: usize = SECTOR;
/// About 31 000 records; a node's latest votes are a few KiB.
const DEFAULT_REWRITE_BYTES: u64 = 1 << 20;
/// The ownership lock's name under `wal_root`.
pub const LOCK_FILE: &str = "votes.lock";

struct Options {
    rewrite_bytes: u64,
    owner_wait: Duration,
    gate: Arc<SyncGate>,
    storage: Storage,
}

impl Options {
    fn storing(storage: Storage) -> Self {
        Self {
            rewrite_bytes: DEFAULT_REWRITE_BYTES,
            owner_wait: OWNER_WAIT,
            gate: Arc::default(),
            storage,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::storing(Storage::global())
    }
}

type Latest = Arc<Mutex<HashMap<u32, Vote>>>;
/// The groups whose logs the node opened over this vote log.
type Hosted = Arc<Mutex<BTreeSet<u32>>>;

struct Request {
    group: u32,
    vote: Vote,
    done: oneshot::Sender<io::Result<()>>,
}

/// Create an empty `wal_root/votes.log` unless one exists. The replica's
/// first start calls this before it publishes `replica.id`.
///
/// # Errors
///
/// A filesystem failure, or another writer owning the root.
pub fn create(wal_root: &Path) -> io::Result<()> {
    ourios_wal::prepare_root(wal_root)
        .map_err(|e| io::Error::other(format!("prepare {}: {e:?}", wal_root.display())))?;
    let _owner = take_ownership(&wal_root.join(LOCK_FILE), OWNER_WAIT, "vote log")?;
    // A retry of an interrupted first start finds the root already made,
    // which `prepare_root` then fsyncs alone; its entry in the parent must
    // be durable before `replica.id` is published beside the log.
    sync_root(wal_root)?;
    if wal_root.join(VOTES_FILE).try_exists()? {
        return Ok(());
    }
    write_durably(wal_root, &header())
}

/// The magic, zero-padded to its own sector.
fn header() -> Vec<u8> {
    let mut sector = vec![0u8; HEADER_LEN];
    sector[..MAGIC.len()].copy_from_slice(&MAGIC);
    sector
}

/// Append one batch of `records` to `out`, which continues a file `len`
/// bytes long, `len` a whole number of sectors: slots packed 15 to a
/// sector, the last sector zero-padded. Returns the new file length.
fn lay_out(
    out: &mut Vec<u8>,
    mut len: usize,
    records: impl IntoIterator<Item = [u8; RECORD_LEN]>,
) -> usize {
    for record in records {
        let room = SECTOR - len % SECTOR;
        if room < RECORD_LEN {
            out.resize(out.len() + room, 0);
            len += room;
        }
        out.extend_from_slice(&record);
        len += RECORD_LEN;
    }
    let end = len.next_multiple_of(SECTOR);
    out.resize(out.len() + (end - len), 0);
    end
}

fn lost(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} is missing on a replica that has started before; rejoin it as a new replica \
             (RFC 0065 §6.1, B4) rather than vote again under the same identity",
            path.display()
        ),
    )
}

/// A handle on the node's vote log, shared by every group.
#[derive(Clone)]
pub struct VoteLog {
    tx: Sender<Request>,
    latest: Latest,
    hosted: Hosted,
}

impl VoteLog {
    /// Recover `wal_root/votes.log`, which [`create`] made on the
    /// replica's first start, and start its writer thread. Blocking.
    ///
    /// # Errors
    ///
    /// A filesystem failure (including the fsyncs of `wal_root`, its
    /// parent and the recovered log), a missing log, a log whose header
    /// is not this format, a corrupt record, or another writer still
    /// owning the root after 30 seconds.
    pub fn open(wal_root: &Path) -> io::Result<Self> {
        Self::open_with(wal_root, Options::default())
    }

    /// [`VoteLog::open`], recording to `storage`.
    pub(super) fn open_storing(wal_root: &Path, storage: Storage) -> io::Result<Self> {
        Self::open_with(wal_root, Options::storing(storage))
    }

    #[cfg(test)]
    pub(super) fn open_metered(
        wal_root: &Path,
        rewrite_bytes: u64,
        gate: Arc<SyncGate>,
        storage: Storage,
    ) -> io::Result<Self> {
        let options = Options {
            rewrite_bytes,
            gate,
            ..Options::storing(storage)
        };
        Self::open_with(wal_root, options)
    }

    /// Note that `group`'s log is open over this vote log, so a failure
    /// of the vote log counts as a stop of the group.
    pub(super) fn host(&self, group: u32) {
        lock(&self.hosted).insert(group);
    }

    fn open_with(wal_root: &Path, options: Options) -> io::Result<Self> {
        ourios_wal::prepare_root(wal_root)
            .map_err(|e| io::Error::other(format!("prepare {}: {e:?}", wal_root.display())))?;
        // The writer keeps this lock until its thread exits, after any
        // rewrite it started, so `votes.log` is never read while a retiring
        // writer may still replace it, and two live logs never share a root.
        let owner = take_ownership(&wal_root.join(LOCK_FILE), options.owner_wait, "vote log")?;
        sync_root(wal_root)?;
        let Options {
            rewrite_bytes,
            gate,
            storage,
            ..
        } = options;
        let path = wal_root.join(VOTES_FILE);
        // `create` runs before `replica.id` is published, so a log missing
        // here was lost after this replica's first start, not never made.
        let (latest, len) = match fs::read(&path) {
            Ok(bytes) => recover(&path, &bytes, &gate)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(lost(&path)),
            Err(e) => return Err(e),
        };
        let file = OpenOptions::new().append(true).open(&path)?;
        let latest: Latest = Arc::new(Mutex::new(latest));
        let hosted = Hosted::default();
        let (tx, rx) = mpsc::channel();
        let writer = Writer {
            file,
            dir: wal_root.to_path_buf(),
            len,
            rewrite_bytes,
            latest: Arc::clone(&latest),
            failed: None,
            gate,
            stops: Stops {
                storage,
                hosted: Arc::clone(&hosted),
            },
            _owner: owner,
        };
        std::thread::Builder::new()
            .name("ourios-votes".to_owned())
            .spawn(move || writer.run(&rx))?;
        Ok(Self { tx, latest, hosted })
    }

    /// The group's latest durable vote.
    #[must_use]
    pub fn latest(&self, group: u32) -> Option<Vote> {
        lock(&self.latest).get(&group).copied()
    }

    /// Queue the group's vote; the future resolves once it is fsynced.
    /// The record is queued before the future is first polled.
    ///
    /// # Errors
    ///
    /// The write or fsync failed, now or in an earlier batch, or the
    /// writer has stopped.
    pub fn save(&self, group: u32, vote: Vote) -> impl Future<Output = io::Result<()>> + use<> {
        let (done, rx) = oneshot::channel();
        let sent = self.tx.send(Request { group, vote, done });
        async move {
            sent.map_err(|_| super::writer_gone())?;
            rx.await.map_err(|_| super::writer_gone())?
        }
    }
}

/// Fsync `wal_root` and its parent, so the `votes.log` entry read next is
/// durable even if a rewrite renamed it in and crashed before its own
/// directory fsync.
fn sync_root(wal_root: &Path) -> io::Result<()> {
    File::open(wal_root)?.sync_all()?;
    match wal_root.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn encode(group: u32, vote: &Vote) -> [u8; RECORD_LEN] {
    let mut out = [0u8; RECORD_LEN];
    out[..4].copy_from_slice(&group.to_le_bytes());
    out[4..12].copy_from_slice(&vote.leader_id.term.to_le_bytes());
    out[12..28].copy_from_slice(vote.leader_id.voted_for.uuid().as_bytes());
    out[28] = u8::from(vote.committed);
    let crc = crc32c::crc32c(&out[..BODY_LEN]);
    out[BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
    out
}

fn decode(record: &[u8]) -> Option<(u32, Vote)> {
    let record: &[u8; RECORD_LEN] = record.try_into().ok()?;
    let (body, crc) = record.split_at(BODY_LEN);
    if crc32c::crc32c(body).to_le_bytes() != crc {
        return None;
    }
    let committed = match body[28] {
        0 => false,
        1 => true,
        _ => return None,
    };
    let group = u32::from_le_bytes(body[..4].try_into().ok()?);
    let term = u64::from_le_bytes(body[4..12].try_into().ok()?);
    let voted_for = NodeId::from(Uuid::from_bytes(body[12..28].try_into().ok()?));
    let leader_id = LeaderId { term, voted_for };
    Some((
        group,
        Vote {
            leader_id,
            committed,
        },
    ))
}

/// The latest vote per group and the end of the last valid slot, which
/// is made the file's length. The file is fsynced either way: a record
/// recovered from the page cache after a process crash may never have
/// reached the disk, and `openraft` grants a retried vote it already
/// holds without saving it again.
fn recover(path: &Path, bytes: &[u8], gate: &SyncGate) -> io::Result<(HashMap<u32, Vote>, usize)> {
    if bytes.get(..HEADER_LEN) != Some(header().as_slice()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not an RFC 0065 vote log", path.display()),
        ));
    }
    let Scan {
        latest,
        len,
        slots_end,
    } = scan(bytes).map_err(|at| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is corrupt at byte {at} and may hold an acknowledged vote; rejoin this \
                 replica as a new one (RFC 0065 §6.1, B4) rather than restore an older vote",
                path.display()
            ),
        )
    })?;
    let file = OpenOptions::new().write(true).open(path)?;
    if len != bytes.len() {
        // Cut back to the last valid slot first: a torn slot after it in
        // the same sector would otherwise survive the zero extension and
        // be read as corrupt on the next open.
        file.set_len(slots_end as u64)?;
        file.set_len(len as u64)?;
    }
    sync_through(gate, || file.sync_all()).unwrap_or_else(|| Err(super::writer_gone()))?;
    Ok((latest, len))
}

enum Slot {
    Vote(u32, Vote),
    Zeroed,
    Corrupt,
}

fn classify(slot: &[u8]) -> Slot {
    if slot.iter().all(|b| *b == 0) {
        return Slot::Zeroed;
    }
    decode(slot).map_or(Slot::Corrupt, |(group, vote)| Slot::Vote(group, vote))
}

/// What recovery keeps of a vote log.
struct Scan {
    latest: HashMap<u32, Vote>,
    /// The end of the last sector holding a valid slot: the file's length.
    len: usize,
    /// The end of the last valid slot.
    slots_end: usize,
}

/// The latest vote per group and where the valid slots end, or the
/// offset of the first byte that cannot be an unanswered tail. Nothing
/// but zeros may follow an all-zero sector.
fn scan(bytes: &[u8]) -> Result<Scan, usize> {
    let mut latest = HashMap::new();
    let mut valid_end = HEADER_LEN;
    let mut slots_end = HEADER_LEN;
    let mut ended = false;
    for (n, sector) in bytes[HEADER_LEN..].chunks(SECTOR).enumerate() {
        let start = HEADER_LEN + n * SECTOR;
        if ended {
            if let Some(i) = sector.iter().position(|b| *b != 0) {
                return Err(start + i);
            }
            continue;
        }
        match scan_sector(sector, start, &mut latest)? {
            Some(end) => {
                valid_end = start + SECTOR;
                slots_end = end;
            }
            None => ended = true,
        }
    }
    Ok(Scan {
        latest,
        len: valid_end,
        slots_end,
    })
}

/// One sector: valid slots, then only zeros. Returns the end of its last
/// valid slot, if it holds one. A partial slot can only be the end of the
/// file, an append torn before its fsync.
fn scan_sector(
    sector: &[u8],
    start: usize,
    latest: &mut HashMap<u32, Vote>,
) -> Result<Option<usize>, usize> {
    let (slots, pad) = sector.split_at(sector.len().min(SLOTS_PER_SECTOR * RECORD_LEN));
    let mut valid = None;
    let mut zeroed = false;
    let (slots, _torn) = slots.as_chunks::<RECORD_LEN>();
    for (i, slot) in slots.iter().enumerate() {
        let at = start + i * RECORD_LEN;
        match (classify(slot), zeroed) {
            (Slot::Zeroed, _) => zeroed = true,
            (Slot::Vote(group, vote), false) => {
                latest.insert(group, vote);
                valid = Some(at + RECORD_LEN);
            }
            _ => return Err(at),
        }
    }
    match pad.iter().position(|b| *b != 0) {
        Some(i) => Err(start + SLOTS_PER_SECTOR * RECORD_LEN + i),
        None => Ok(valid),
    }
}

/// Replace `dir/votes.log` with `bytes`: tmp, fsync, rename, directory fsync.
fn write_durably(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!("{VOTES_FILE}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(VOTES_FILE))?;
    File::open(dir)?.sync_all()
}

struct Writer {
    file: File,
    dir: PathBuf,
    len: usize,
    rewrite_bytes: u64,
    latest: Latest,
    /// Set by the first failed write: after it the file's tail is
    /// unknown, so every later save fails.
    failed: Option<String>,
    gate: Arc<SyncGate>,
    stops: Stops,
    /// Released when the thread exits; see [`VoteLog::open`].
    _owner: Owner,
}

/// Where the vote log reports its writes, and the groups its failure
/// stops.
struct Stops {
    storage: Storage,
    hosted: Hosted,
}

impl Writer {
    fn run(mut self, rx: &Receiver<Request>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            batch.extend(rx.try_iter());
            if !self.commit(batch) {
                return;
            }
        }
    }

    /// One fsync for the whole batch; `false` when the writer must stop.
    fn commit(&mut self, batch: Vec<Request>) -> bool {
        let outcome = self.write(&batch);
        if let Flush::Done = outcome {
            let mut latest = lock(&self.latest);
            latest.extend(batch.iter().map(|r| (r.group, r.vote)));
        }
        for r in batch {
            let _ = r.done.send(outcome.reply());
        }
        match outcome {
            Flush::Done => self.rewrite_if_due(),
            Flush::Failed(_) => true,
            Flush::Crashed => false,
        }
    }

    fn write(&mut self, batch: &[Request]) -> Flush {
        if let Some(reason) = &self.failed {
            return Flush::Failed(io::Error::other(format!(
                "the vote log failed earlier: {reason}"
            )));
        }
        let mut bytes = Vec::new();
        let records = batch.iter().map(|r| encode(r.group, &r.vote));
        let len = lay_out(&mut bytes, self.len, records);
        let written = self.file.write_all(&bytes);
        let file = &mut self.file;
        let Some(synced) = sync_through(&self.gate, || written.and_then(|()| file.sync_data()))
        else {
            return Flush::Crashed;
        };
        match synced {
            Ok(()) => {
                self.len = len;
                Flush::Done
            }
            Err(e) => {
                self.failed = Some(e.to_string());
                Flush::Failed(e)
            }
        }
    }

    /// `false` when the gate crashed the writer before the rewrite.
    fn rewrite_if_due(&mut self) -> bool {
        if self.len as u64 <= self.rewrite_bytes {
            return true;
        }
        let gate = Arc::clone(&self.gate);
        let Some(rewritten) = sync_through(&gate, || self.rewrite()) else {
            return false;
        };
        if let Err(e) = rewritten {
            self.failed = Some(format!("rewrite {VOTES_FILE}: {e}"));
        }
        true
    }

    fn rewrite(&mut self) -> io::Result<()> {
        let mut groups: Vec<(u32, Vote)> =
            lock(&self.latest).iter().map(|(g, v)| (*g, *v)).collect();
        groups.sort_unstable_by_key(|(g, _)| *g);
        let mut bytes = header();
        let len = lay_out(
            &mut bytes,
            HEADER_LEN,
            groups.iter().map(|(g, v)| encode(*g, v)),
        );
        write_durably(&self.dir, &bytes)?;
        self.file = OpenOptions::new()
            .append(true)
            .open(self.dir.join(VOTES_FILE))?;
        self.len = len;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(term: u64, node: u64, committed: bool) -> Vote {
        Vote {
            leader_id: LeaderId {
                term,
                voted_for: NodeId::from(node),
            },
            committed,
        }
    }

    fn open(dir: &Path, rewrite_bytes: u64) -> (VoteLog, Arc<SyncGate>) {
        create(dir).expect("create");
        let gate = Arc::new(SyncGate::default());
        let options = Options {
            rewrite_bytes,
            gate: Arc::clone(&gate),
            ..Options::default()
        };
        let log = VoteLog::open_with(dir, options).expect("open");
        (log, gate)
    }

    #[test]
    fn a_record_encodes_to_the_documented_bytes() {
        let v = vote(0x0102_0304_0506_0708, 9, true);
        let bytes = encode(7, &v);
        assert_eq!(&bytes[..4], &[7, 0, 0, 0]);
        assert_eq!(&bytes[4..12], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&bytes[12..28], Uuid::from_u64_pair(0, 9).as_bytes());
        assert_eq!(bytes[28], 1);
        assert_eq!(&bytes[29..], &crc32c::crc32c(&bytes[..29]).to_le_bytes());
        assert_eq!(decode(&bytes), Some((7, v)));
    }

    #[tokio::test]
    async fn one_fsync_covers_every_vote_queued_while_the_previous_ran() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (log, gate) = open(dir.path(), DEFAULT_REWRITE_BYTES);
        let opened = gate.passed();
        gate.close();
        let first = log.save(0, vote(1, 1, false));
        gate.wait_for_writer();
        let queued: Vec<_> = (1..=50).map(|g| log.save(g, vote(2, 2, true))).collect();
        assert_eq!(log.latest(1), None, "no vote is visible before its fsync");
        gate.open();
        first.await.expect("first");
        for saved in queued {
            saved.await.expect("queued");
        }
        assert_eq!(
            gate.passed() - opened,
            2,
            "the 50 queued votes shared one fsync"
        );
        assert_eq!(log.latest(50), Some(vote(2, 2, true)));
    }

    #[tokio::test]
    async fn recovery_keeps_the_last_vote_per_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
            log.save(2, vote(1, 2, false)).await.expect("save");
            log.save(1, vote(3, 1, true)).await.expect("save");
        }
        let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
        assert_eq!(log.latest(1), Some(vote(3, 1, true)));
        assert_eq!(log.latest(2), Some(vote(1, 2, false)));
        assert_eq!(log.latest(3), None);
    }

    /// One durable vote, then `tail` appended as a crash might leave it.
    async fn with_tail(dir: &Path, tail: &[u8]) {
        let (log, _) = open(dir, DEFAULT_REWRITE_BYTES);
        log.save(1, vote(1, 1, false)).await.expect("save");
        drop(log);
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(VOTES_FILE))
            .expect("open");
        file.write_all(tail).expect("write");
    }

    #[tokio::test]
    async fn an_unanswered_tail_is_ignored_and_cut_off() {
        let mut partial = encode(2, &vote(9, 9, true)).to_vec();
        partial.truncate(RECORD_LEN - 3);
        let mut zero_filled = vec![0u8; 2 * RECORD_LEN];
        zero_filled.extend_from_slice(&partial[..5]);
        for (what, tail) in [("partial", partial), ("zero-filled", zero_filled)] {
            let dir = tempfile::tempdir().expect("tempdir");
            with_tail(dir.path(), &tail).await;
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            assert_eq!(log.latest(1), Some(vote(1, 1, false)), "{what}");
            assert_eq!(log.latest(2), None, "{what}");
            let len = fs::metadata(dir.path().join(VOTES_FILE))
                .expect("stat")
                .len();
            assert_eq!(len, (2 * SECTOR) as u64, "{what} tail cut off");
            log.save(2, vote(4, 2, false))
                .await
                .expect("save after the cut");
            drop(log);
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            assert_eq!(log.latest(2), Some(vote(4, 2, false)), "{what}");
        }
    }

    #[test]
    fn a_torn_slot_in_a_short_final_sector_is_cut_before_the_sector_is_padded() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path()).expect("root");
        let mut torn = header();
        torn.extend_from_slice(&encode(1, &vote(1, 1, false)));
        torn.extend_from_slice(&encode(2, &vote(9, 9, true))[..20]);
        fs::write(dir.path().join(VOTES_FILE), &torn).expect("write");
        for reopen in 1..=2 {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            assert_eq!(log.latest(1), Some(vote(1, 1, false)), "reopen {reopen}");
            assert_eq!(log.latest(2), None, "reopen {reopen}");
            drop(log);
            let bytes = fs::read(dir.path().join(VOTES_FILE)).expect("read");
            assert_eq!(bytes.len(), 2 * SECTOR, "reopen {reopen}");
            assert!(
                bytes[HEADER_LEN + RECORD_LEN..].iter().all(|b| *b == 0),
                "reopen {reopen}: the torn slot's bytes survived the padding"
            );
        }
    }

    #[tokio::test]
    async fn a_bit_flip_in_the_last_complete_record_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut flipped = encode(2, &vote(9, 9, true));
        flipped[5] ^= 0x01;
        with_tail(dir.path(), &flipped).await;
        let err = VoteLog::open(dir.path()).err().expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("new one"), "{err}");
        assert!(err.to_string().contains(VOTES_FILE), "{err}");
    }

    #[tokio::test]
    async fn a_valid_record_after_a_zero_filled_one_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tail = vec![0u8; RECORD_LEN];
        tail.extend_from_slice(&encode(2, &vote(9, 9, true)));
        with_tail(dir.path(), &tail).await;
        assert!(VoteLog::open(dir.path()).is_err());
    }

    #[test]
    fn a_first_start_creates_the_root_and_an_empty_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("var").join("wal");
        create(&root).expect("create");
        assert_eq!(fs::read(root.join(VOTES_FILE)).expect("read"), header());
        let log = VoteLog::open(&root).expect("open");
        assert_eq!(log.latest(0), None);
    }

    #[tokio::test]
    async fn create_keeps_an_existing_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
        }
        create(dir.path()).expect("create again");
        let log = VoteLog::open(dir.path()).expect("open");
        assert_eq!(log.latest(1), Some(vote(1, 1, false)));
    }

    #[test]
    fn a_lost_log_after_the_first_start_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        create(dir.path()).expect("create");
        fs::remove_file(dir.path().join(VOTES_FILE)).expect("lose it");
        let err = VoteLog::open(dir.path()).err().expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("new replica"), "{err}");
    }

    /// A log of `n` valid records, laid out as the writer lays them out.
    fn log_bytes(n: u32) -> Vec<u8> {
        let mut bytes = header();
        let records = (0..n).map(|g| encode(g, &vote(u64::from(g), 1, false)));
        let len = lay_out(&mut bytes, HEADER_LEN, records);
        assert_eq!(len, bytes.len());
        bytes
    }

    fn reopen_with(dir: &Path, bytes: &[u8]) -> io::Result<VoteLog> {
        create(dir).expect("create");
        fs::write(dir.join(VOTES_FILE), bytes).expect("write");
        VoteLog::open(dir)
    }

    #[test]
    fn slots_are_packed_fifteen_to_a_sector_and_never_straddle_one() {
        let bytes = log_bytes(16);
        assert_eq!(&bytes[..8], b"OURVOTE\x02");
        assert!(bytes[8..HEADER_LEN].iter().all(|b| *b == 0));
        let first = &bytes[HEADER_LEN..HEADER_LEN + RECORD_LEN];
        assert_eq!(first, encode(0, &vote(0, 1, false)));
        let pad = HEADER_LEN + 15 * RECORD_LEN;
        assert_eq!(pad, 1007);
        assert!(
            bytes[pad..2 * SECTOR].iter().all(|b| *b == 0),
            "17-byte pad"
        );
        let sixteenth = &bytes[2 * SECTOR..2 * SECTOR + RECORD_LEN];
        assert_eq!(sixteenth, encode(15, &vote(15, 1, false)));
        assert_eq!(bytes.len(), 3 * SECTOR);
    }

    #[tokio::test]
    async fn appends_and_rewrites_keep_the_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let threshold = (HEADER_LEN + 20 * RECORD_LEN) as u64;
        {
            let (log, _) = open(dir.path(), threshold);
            for group in 0..40 {
                let v = vote(u64::from(group), 1, false);
                log.save(group, v).await.expect("save");
            }
        }
        // The reopen waits for the retiring writer's last rewrite.
        let log = VoteLog::open(dir.path()).expect("reopen");
        let bytes = fs::read(dir.path().join(VOTES_FILE)).expect("read");
        assert_eq!(bytes, log_bytes(40), "the rewrite laid every slot out");
        assert_eq!(log.latest(39), Some(vote(39, 1, false)));
    }

    #[test]
    fn a_zero_extended_tail_is_cut() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bytes = log_bytes(16);
        let valid = bytes.len();
        bytes.resize(5 * SECTOR, 0);
        let log = reopen_with(dir.path(), &bytes).expect("open");
        assert_eq!(log.latest(15), Some(vote(15, 1, false)));
        let len = fs::metadata(dir.path().join(VOTES_FILE))
            .expect("stat")
            .len();
        assert_eq!(len, valid as u64);
    }

    #[test]
    fn a_partially_zeroed_slot_fails_the_open() {
        let slot = HEADER_LEN + 3 * RECORD_LEN;
        for from in [1, 16, RECORD_LEN - 1] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut bytes = log_bytes(4);
            bytes[slot + from..slot + RECORD_LEN].fill(0);
            let err = reopen_with(dir.path(), &bytes).err().expect("must refuse");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "zeroed from {from}");
        }
    }

    #[test]
    fn a_crc_byte_flipped_to_zero_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bytes = log_bytes(4);
        let last_crc_byte = HEADER_LEN + 4 * RECORD_LEN - 1;
        bytes[last_crc_byte] = u8::from(bytes[last_crc_byte] == 0);
        assert!(reopen_with(dir.path(), &bytes).is_err());
    }

    #[tokio::test]
    async fn each_fsync_batch_starts_a_fresh_sector() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
            log.save(2, vote(2, 1, true)).await.expect("save");
        }
        let mut expected = header();
        for (group, v) in [(1, vote(1, 1, false)), (2, vote(2, 1, true))] {
            let mut sector = vec![0u8; SECTOR];
            sector[..RECORD_LEN].copy_from_slice(&encode(group, &v));
            expected.extend_from_slice(&sector);
        }
        let bytes = fs::read(dir.path().join(VOTES_FILE)).expect("read");
        assert_eq!(bytes, expected);
        let log = VoteLog::open(dir.path()).expect("reopen");
        assert_eq!(log.latest(2), Some(vote(2, 1, true)));
    }

    #[tokio::test]
    async fn a_crash_that_zeroes_a_later_batch_keeps_the_earlier_vote() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
            log.save(1, vote(2, 1, false)).await.expect("save");
        }
        let path = dir.path().join(VOTES_FILE);
        let mut bytes = fs::read(&path).expect("read");
        bytes[2 * SECTOR..].fill(0);
        fs::write(&path, &bytes).expect("zero the second batch's sector");
        let log = VoteLog::open(dir.path()).expect("open");
        assert_eq!(
            log.latest(1),
            Some(vote(1, 1, false)),
            "the first vote survives"
        );
        let len = fs::metadata(&path).expect("stat").len();
        assert_eq!(len, (2 * SECTOR) as u64);
    }

    #[test]
    fn a_valid_sector_after_an_all_zero_one_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bytes = log_bytes(1);
        bytes.extend_from_slice(&[0u8; SECTOR]);
        let mut later = vec![0u8; SECTOR];
        later[..RECORD_LEN].copy_from_slice(&encode(5, &vote(5, 1, false)));
        bytes.extend_from_slice(&later);
        let err = reopen_with(dir.path(), &bytes).err().expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_non_zero_pad_byte_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bytes = log_bytes(16);
        bytes[HEADER_LEN + 15 * RECORD_LEN + 3] = 1;
        let err = reopen_with(dir.path(), &bytes).err().expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_corrupt_record_before_a_valid_one_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
            log.save(2, vote(1, 2, false)).await.expect("save");
        }
        let path = dir.path().join(VOTES_FILE);
        let mut bytes = fs::read(&path).expect("read");
        bytes[HEADER_LEN + 6] ^= 0xFF;
        fs::write(&path, &bytes).expect("write");
        let err = VoteLog::open(dir.path()).err().expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_file_without_the_header_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(VOTES_FILE), b"not a vote log").expect("write");
        let err = VoteLog::open(dir.path()).err().expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn past_the_threshold_the_log_is_rewritten_to_each_groups_latest_vote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let threshold = (4 * SECTOR) as u64;
        {
            let (log, _) = open(dir.path(), threshold);
            for term in 1..=20u64 {
                let group = u32::try_from(term % 3).expect("below 3");
                log.save(group, vote(term, term, false))
                    .await
                    .expect("save");
            }
            log.save(0, vote(99, 7, true)).await.expect("save");
        }
        // The reopen waits for the retiring writer's last rewrite.
        let (log, _) = open(dir.path(), threshold);
        let path = dir.path().join(VOTES_FILE);
        let len = fs::metadata(&path).expect("stat").len();
        assert!(len <= threshold, "rewritten to {len} bytes");
        assert!(!dir.path().join(format!("{VOTES_FILE}.tmp")).exists());
        assert_eq!(log.latest(0), Some(vote(99, 7, true)));
        assert_eq!(log.latest(1), Some(vote(19, 19, false)));
        assert_eq!(log.latest(2), Some(vote(20, 20, false)));
    }

    #[tokio::test]
    async fn a_writer_crashed_before_its_fsync_answers_with_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (log, gate) = open(dir.path(), DEFAULT_REWRITE_BYTES);
        gate.close();
        let saved = log.save(1, vote(1, 1, false));
        gate.wait_for_writer();
        gate.crash();
        assert!(saved.await.is_err());
        assert_eq!(log.latest(1), None);
    }

    #[tokio::test]
    async fn a_recovered_log_is_fsynced_before_it_is_trusted() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
        }
        let (log, gate) = open(dir.path(), DEFAULT_REWRITE_BYTES);
        assert_eq!(gate.passed(), 1, "recovery fsynced the log it read");
        assert_eq!(log.latest(1), Some(vote(1, 1, false)));
    }

    #[test]
    fn a_failed_recovery_fsync_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        create(dir.path()).expect("create");
        let gate = Arc::new(SyncGate::default());
        gate.fail();
        let options = Options {
            gate,
            ..Options::default()
        };
        assert!(VoteLog::open_with(dir.path(), options).is_err());
    }

    #[tokio::test]
    async fn a_reopen_waits_for_a_retiring_writer_to_finish_its_rewrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let threshold = (3 * SECTOR) as u64;
        let (log, gate) = open(dir.path(), threshold);
        gate.close_after(3);
        for group in 1..=3 {
            log.save(group, vote(u64::from(group), 1, false))
                .await
                .expect("save");
        }
        gate.wait_for_writer();
        drop(log);
        let root = dir.path().to_path_buf();
        let (opened_tx, opened) = mpsc::channel();
        let reopen = std::thread::spawn(move || {
            let reopened = VoteLog::open(&root);
            let _ = opened_tx.send(());
            reopened
        });
        assert!(
            opened.recv_timeout(Duration::from_millis(200)).is_err(),
            "the reopen waits while the retiring writer's rewrite is paused"
        );
        gate.open();
        let log = reopen.join().expect("join").expect("reopen");
        for group in 1..=3 {
            assert_eq!(log.latest(group), Some(vote(u64::from(group), 1, false)));
        }
        log.save(4, vote(4, 1, true))
            .await
            .expect("save after the reopen");
        drop(log);
        let log = VoteLog::open(dir.path()).expect("reopen again");
        assert_eq!(
            log.latest(4),
            Some(vote(4, 1, true)),
            "no vote went to an unlinked file"
        );
    }

    #[test]
    fn a_second_live_vote_log_on_one_root_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        create(dir.path()).expect("create");
        let live = VoteLog::open(dir.path()).expect("open");
        let options = Options {
            owner_wait: Duration::from_millis(50),
            ..Options::default()
        };
        let err = VoteLog::open_with(dir.path(), options)
            .err()
            .expect("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(live);
    }
}
