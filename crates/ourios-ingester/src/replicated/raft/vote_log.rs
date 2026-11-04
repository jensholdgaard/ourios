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
//! Recovery keeps the last record per group. A bad record (short, CRC
//! mismatch, or a `committed` byte other than 0 or 1) ends the log only
//! when no valid record follows it: that is a torn tail no `save` was
//! answered for, and it is cut off before the next append. A valid record
//! after a bad one is corruption, and the open fails rather than forget
//! an acknowledged vote. Past a size threshold the log is rewritten with
//! each group's latest vote (tmp, fsync, rename, directory fsync).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use openraft::impls::leader_id_std::LeaderId;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::gate::SyncGate;
use super::types::{NodeId, Vote};

/// The vote log's name under `wal_root`.
pub const VOTES_FILE: &str = "votes.log";
const MAGIC: [u8; 8] = *b"OURVOTE\x01";
const BODY_LEN: usize = 4 + 8 + 16 + 1;
const RECORD_LEN: usize = BODY_LEN + 4;
/// About 31 000 records; a node's latest votes are a few KiB.
const DEFAULT_REWRITE_BYTES: u64 = 1 << 20;

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
    /// A filesystem failure, a log whose header is not this format, or a
    /// corrupt record that a valid one follows.
    pub fn open(wal_root: &Path) -> io::Result<Self> {
        Self::open_with(wal_root, DEFAULT_REWRITE_BYTES, Arc::default())
    }

    fn open_with(wal_root: &Path, rewrite_bytes: u64, gate: Arc<SyncGate>) -> io::Result<Self> {
        let path = wal_root.join(VOTES_FILE);
        let (latest, len) = match fs::read(&path) {
            Ok(bytes) => recover(&path, &bytes)?,
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
/// is made the file's length.
fn recover(path: &Path, bytes: &[u8]) -> io::Result<(HashMap<u32, Vote>, u64)> {
    let Some(records) = bytes.strip_prefix(&MAGIC) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not an RFC 0065 vote log", path.display()),
        ));
    };
    let mut latest = HashMap::new();
    let mut chunks = records.chunks(RECORD_LEN);
    let mut valid = 0;
    for (group, vote) in chunks.by_ref().map_while(decode) {
        latest.insert(group, vote);
        valid += 1;
    }
    if chunks.any(|c| decode(c).is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} has a corrupt record {valid} before valid ones; a replica never restarts \
                 under a vote it may have lost (RFC 0065 §4.5)",
                path.display()
            ),
        ));
    }
    let len = (MAGIC.len() + valid * RECORD_LEN) as u64;
    if len < bytes.len() as u64 {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(len)?;
        file.sync_all()?;
    }
    Ok((latest, len))
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

enum Outcome {
    Synced,
    Failed(io::Error),
    /// The test gate stopped the writer before its fsync.
    Crashed,
}

impl Outcome {
    fn reply(&self) -> io::Result<()> {
        match self {
            Self::Synced => Ok(()),
            Self::Failed(e) => Err(io::Error::new(e.kind(), e.to_string())),
            Self::Crashed => Err(super::writer_gone()),
        }
    }
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
        if let Outcome::Synced = outcome {
            let mut latest = lock(&self.latest);
            latest.extend(batch.iter().map(|r| (r.group, r.vote)));
        }
        for r in batch {
            let _ = r.done.send(outcome.reply());
        }
        match outcome {
            Outcome::Synced => {
                self.rewrite_if_due();
                true
            }
            Outcome::Failed(_) => true,
            Outcome::Crashed => false,
        }
    }

    fn write(&mut self, batch: &[Request]) -> Outcome {
        if let Some(reason) = &self.failed {
            return Outcome::Failed(io::Error::other(format!(
                "the vote log failed earlier: {reason}"
            )));
        }
        let bytes: Vec<u8> = batch
            .iter()
            .flat_map(|r| encode(r.group, &r.vote))
            .collect();
        let written = self.file.write_all(&bytes);
        if !self.gate.pass() {
            return Outcome::Crashed;
        }
        match written.and_then(|()| self.file.sync_data()) {
            Ok(()) => {
                self.len += bytes.len() as u64;
                Outcome::Synced
            }
            Err(e) => {
                self.failed = Some(e.to_string());
                Outcome::Failed(e)
            }
        }
    }

    fn rewrite_if_due(&mut self) {
        if self.len <= self.rewrite_bytes {
            return;
        }
        if let Err(e) = self.rewrite() {
            self.failed = Some(format!("rewrite {VOTES_FILE}: {e}"));
        }
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
        let log = VoteLog::open_with(dir, rewrite_bytes, Arc::clone(&gate)).expect("open");
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

    #[tokio::test]
    async fn a_torn_tail_record_is_ignored_and_cut_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(VOTES_FILE);
        {
            let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
            log.save(1, vote(1, 1, false)).await.expect("save");
        }
        let mut torn = encode(1, &vote(9, 9, true)).to_vec();
        torn.truncate(RECORD_LEN - 3);
        let mut garbage = encode(2, &vote(9, 9, true));
        garbage[5] ^= 0xFF;
        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        file.write_all(&garbage).expect("write");
        file.write_all(&torn).expect("write");
        drop(file);
        let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
        assert_eq!(log.latest(1), Some(vote(1, 1, false)));
        assert_eq!(log.latest(2), None);
        let len = fs::metadata(&path).expect("stat").len();
        assert_eq!(
            len,
            (MAGIC.len() + RECORD_LEN) as u64,
            "the torn tail is cut off"
        );
        log.save(2, vote(4, 2, false))
            .await
            .expect("save after the cut");
        drop(log);
        let (log, _) = open(dir.path(), DEFAULT_REWRITE_BYTES);
        assert_eq!(log.latest(2), Some(vote(4, 2, false)));
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
}
