//! The node's vote log, `wal_root/votes.log` (RFC 0065 §4.7, §6.1).
//!
//! Every group on the replica appends its votes here, so a mass election
//! costs one fsync per batch rather than one per group. It sits outside
//! the group WALs, so segment reclamation never touches it.
//!
//! ```text
//! | magic: [u8; 8] = "OURVOTE\x01"                                     |
//! | record*: group: u32_le | term: u64_le | voted_for: [u8; 16] (UUID) |
//! |          | committed: u8 (0 or 1) | crc32c: u32_le (over the 29 B before it) |
//! ```
//!
//! Recovery keeps the last record per group. Only a tail no `save` was
//! answered for is cut off before the next append: a partial record at
//! the end, or complete records that are all zero bytes. Any other record
//! that fails its CRC or holds a `committed` byte other than 0 or 1 may
//! be a damaged acknowledged vote, so the open fails, wherever the record
//! is: the replica rejoins as a new one (RFC 0065 §6.1, B4) rather than
//! restore an older vote. Past a size threshold the log is rewritten with
//! each group's latest vote (tmp, fsync, rename, directory fsync).
//!
//! One writer owns a root at a time, through an OS lock on
//! `wal_root/votes.lock` that its thread holds until it exits.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use openraft::impls::leader_id_std::LeaderId;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::gate::SyncGate;
use super::types::{NodeId, Vote};
use super::{Flush, sync_through};

/// The vote log's name under `wal_root`.
pub const VOTES_FILE: &str = "votes.log";
const MAGIC: [u8; 8] = *b"OURVOTE\x01";
const BODY_LEN: usize = 4 + 8 + 16 + 1;
const RECORD_LEN: usize = BODY_LEN + 4;
/// About 31 000 records; a node's latest votes are a few KiB.
const DEFAULT_REWRITE_BYTES: u64 = 1 << 20;
/// The ownership lock's name under `wal_root`.
pub const LOCK_FILE: &str = "votes.lock";
/// How long an open waits for a retiring writer, whose last fsync may be
/// slow (§4.7 saw drive-cache flushes of seconds), before refusing.
const DEFAULT_OWNER_WAIT: Duration = Duration::from_secs(30);

struct Options {
    rewrite_bytes: u64,
    owner_wait: Duration,
    gate: Arc<SyncGate>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            rewrite_bytes: DEFAULT_REWRITE_BYTES,
            owner_wait: DEFAULT_OWNER_WAIT,
            gate: Arc::default(),
        }
    }
}

type Latest = Arc<Mutex<HashMap<u32, Vote>>>;

struct Request {
    group: u32,
    vote: Vote,
    done: oneshot::Sender<io::Result<()>>,
}

/// A handle on the node's vote log, shared by every group.
#[derive(Clone)]
pub struct VoteLog {
    tx: Sender<Request>,
    latest: Latest,
}

impl VoteLog {
    /// Recover `wal_root/votes.log`, creating it on first start, and
    /// start its writer thread. Blocking.
    ///
    /// # Errors
    ///
    /// A filesystem failure (including the fsyncs of `wal_root`, its
    /// parent and the recovered log), a log whose header is not this
    /// format, a corrupt record, or another writer still owning the root
    /// after 30 seconds.
    pub fn open(wal_root: &Path) -> io::Result<Self> {
        Self::open_with(wal_root, Options::default())
    }

    fn open_with(wal_root: &Path, options: Options) -> io::Result<Self> {
        ourios_wal::prepare_root(wal_root)
            .map_err(|e| io::Error::other(format!("prepare {}: {e:?}", wal_root.display())))?;
        // The writer keeps this lock until its thread exits, after any
        // rewrite it started, so `votes.log` is never read while a retiring
        // writer may still replace it, and two live logs never share a root.
        let owner = take_ownership(wal_root, options.owner_wait)?;
        sync_root(wal_root)?;
        let Options {
            rewrite_bytes,
            gate,
            ..
        } = options;
        let path = wal_root.join(VOTES_FILE);
        let (latest, len) = match fs::read(&path) {
            Ok(bytes) => recover(&path, &bytes, &gate)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_durably(wal_root, &MAGIC)?;
                (HashMap::new(), MAGIC.len() as u64)
            }
            Err(e) => return Err(e),
        };
        let file = OpenOptions::new().append(true).open(&path)?;
        let latest: Latest = Arc::new(Mutex::new(latest));
        let (tx, rx) = mpsc::channel();
        let writer = Writer {
            file,
            dir: wal_root.to_path_buf(),
            len,
            rewrite_bytes,
            latest: Arc::clone(&latest),
            failed: None,
            gate,
            _owner: owner,
        };
        std::thread::Builder::new()
            .name("ourios-votes".to_owned())
            .spawn(move || writer.run(&rx))?;
        Ok(Self { tx, latest })
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

/// Wait up to `wait` for the root's lock, polling: a retiring writer
/// releases it within one fsync, a live one never does.
fn take_ownership(wal_root: &Path, wait: Duration) -> io::Result<File> {
    let path = wal_root.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} is held by another live vote log", path.display()),
                ));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
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

/// The latest vote per group and the length of the valid prefix, which
/// is made the file's length. The file is fsynced either way: a record
/// recovered from the page cache after a process crash may never have
/// reached the disk, and `openraft` grants a retried vote it already
/// holds without saving it again.
fn recover(path: &Path, bytes: &[u8], gate: &SyncGate) -> io::Result<(HashMap<u32, Vote>, u64)> {
    let Some(records) = bytes.strip_prefix(&MAGIC) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not an RFC 0065 vote log", path.display()),
        ));
    };
    let (latest, valid) = scan(records).map_err(|n| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} record {n} is corrupt and may be an acknowledged vote; rejoin this replica \
                 as a new one (RFC 0065 §6.1, B4) rather than restore an older vote",
                path.display()
            ),
        )
    })?;
    let len = (MAGIC.len() + valid * RECORD_LEN) as u64;
    let file = OpenOptions::new().write(true).open(path)?;
    if len < bytes.len() as u64 {
        file.set_len(len)?;
    }
    sync_through(gate, || file.sync_all()).unwrap_or_else(|| Err(super::writer_gone()))?;
    Ok((latest, len))
}

enum Record {
    Vote(u32, Vote),
    /// A crash after the file grew but before its data reached the disk
    /// (ext4 delayed allocation) reads back as zeros: no save was
    /// answered for those bytes.
    Zeroed,
    Corrupt,
}

fn classify(record: &[u8; RECORD_LEN]) -> Record {
    if record.iter().all(|b| *b == 0) {
        return Record::Zeroed;
    }
    decode(record).map_or(Record::Corrupt, |(group, vote)| Record::Vote(group, vote))
}

/// The latest vote per group and the number of valid records, or the
/// number of the first record that cannot be an unanswered tail. A
/// partial final chunk is an append torn before its fsync, and nothing
/// valid may follow a zeroed record.
fn scan(records: &[u8]) -> Result<(HashMap<u32, Vote>, usize), usize> {
    let mut latest = HashMap::new();
    let mut valid = 0;
    let mut zeroed_tail = false;
    let (complete, _torn) = records.as_chunks::<RECORD_LEN>();
    for (n, record) in complete.iter().enumerate() {
        match (classify(record), zeroed_tail) {
            (Record::Zeroed, _) => zeroed_tail = true,
            (Record::Vote(group, vote), false) => {
                latest.insert(group, vote);
                valid += 1;
            }
            _ => return Err(n),
        }
    }
    Ok((latest, valid))
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
    len: u64,
    rewrite_bytes: u64,
    latest: Latest,
    /// Set by the first failed write: after it the file's tail is
    /// unknown, so every later save fails.
    failed: Option<String>,
    gate: Arc<SyncGate>,
    /// Released when the thread exits; see [`VoteLog::open`].
    _owner: File,
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
        let bytes: Vec<u8> = batch
            .iter()
            .flat_map(|r| encode(r.group, &r.vote))
            .collect();
        let written = self.file.write_all(&bytes);
        let file = &mut self.file;
        let Some(synced) = sync_through(&self.gate, || written.and_then(|()| file.sync_data()))
        else {
            return Flush::Crashed;
        };
        match synced {
            Ok(()) => {
                self.len += bytes.len() as u64;
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
        if self.len <= self.rewrite_bytes {
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
        let mut bytes = MAGIC.to_vec();
        bytes.extend(groups.iter().flat_map(|(g, v)| encode(*g, v)));
        write_durably(&self.dir, &bytes)?;
        self.file = OpenOptions::new()
            .append(true)
            .open(self.dir.join(VOTES_FILE))?;
        self.len = bytes.len() as u64;
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
        assert_eq!(gate.passed(), 2, "the 50 queued votes shared one fsync");
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
            assert_eq!(
                len,
                (MAGIC.len() + RECORD_LEN) as u64,
                "{what} tail cut off"
            );
            log.save(2, vote(4, 2, false))
                .await
                .expect("save after the cut");
            drop(log);
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            assert_eq!(log.latest(2), Some(vote(4, 2, false)), "{what}");
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
    fn a_missing_wal_root_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("var").join("wal");
        let log = VoteLog::open(&root).expect("open");
        assert!(root.join(VOTES_FILE).is_file());
        drop(log);
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
        bytes[MAGIC.len() + 6] ^= 0xFF;
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
        let threshold = (MAGIC.len() + 10 * RECORD_LEN) as u64;
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
        let path = dir.path().join(VOTES_FILE);
        let len = fs::metadata(&path).expect("stat").len();
        assert!(len <= threshold, "rewritten to {len} bytes");
        assert!(!dir.path().join(format!("{VOTES_FILE}.tmp")).exists());
        let (log, _) = open(dir.path(), threshold);
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
        drop(VoteLog::open(dir.path()).expect("create"));
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
        let threshold = (MAGIC.len() + 2 * RECORD_LEN) as u64;
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
