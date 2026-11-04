//! SPIKE (RFC 0068 §4.3, not for merge): the write-side cost of a durable
//! synced offset. Recovery does not read it; only the hot-path cost is
//! measured.
//!
//! A record is `segment uuid (16) | byte (8) | seq (8) | crc32c (4)`, written
//! into one of two 512-byte slots chosen by `seq % 2`. Alternating slots means
//! a torn record write can only damage the slot being written, while the other
//! still holds the previous (lower, still valid) bound, so the design does not
//! lean on sector atomicity; recovery takes the valid slot with the highest
//! `seq`.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

const SLOT_LEN: u64 = 512;
const RECORD_LEN: usize = 36;
const SIDECAR_NAME: &str = "SYNCED";

/// Where and how the synced offset is made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncedOffsetMode {
    /// Segment `fdatasync`, then the `SYNCED` sidecar record + its own
    /// `fdatasync`. Two ordered flushes: the recorded offset never names
    /// bytes that are not already durable.
    SidecarSequential,
    /// Segment `fdatasync` on this thread while a second thread writes and
    /// flushes the sidecar record. Both must succeed before `sync` returns.
    SidecarConcurrent,
    /// Record pwritten into a fixed slot inside the live segment (stand-in:
    /// the 24-byte segment header is rewritten in place with its own bytes),
    /// covered by the one segment `fdatasync`.
    InSegment,
}

#[derive(Debug)]
pub(crate) struct SyncedOffset {
    pub(crate) mode: SyncedOffsetMode,
    sidecar: Option<File>,
    in_segment: Option<(uuid::Uuid, File, [u8; crate::SEGMENT_HEADER_LEN])>,
    seq: u64,
}

impl SyncedOffset {
    pub(crate) fn new(root: &Path, mode: SyncedOffsetMode) -> std::io::Result<Self> {
        let sidecar = match mode {
            SyncedOffsetMode::InSegment => None,
            SyncedOffsetMode::SidecarSequential | SyncedOffsetMode::SidecarConcurrent => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(root.join(SIDECAR_NAME))?;
                // Preallocate both slots once so a steady-state record write
                // is an in-place overwrite with no size or extent change.
                file.set_len(2 * SLOT_LEN)?;
                file.write_all_at(&[0u8; 1024], 0)?;
                file.sync_all()?;
                File::open(root)?.sync_all()?;
                Some(file)
            }
        };
        Ok(Self {
            mode,
            sidecar,
            in_segment: None,
            seq: 0,
        })
    }

    fn next_record(&mut self, segment: uuid::Uuid, byte: u64) -> ([u8; RECORD_LEN], u64) {
        self.seq += 1;
        let mut rec = [0u8; RECORD_LEN];
        rec[..16].copy_from_slice(segment.as_bytes());
        rec[16..24].copy_from_slice(&byte.to_le_bytes());
        rec[24..32].copy_from_slice(&self.seq.to_le_bytes());
        let crc = crc32c::crc32c(&rec[..32]);
        rec[32..].copy_from_slice(&crc.to_le_bytes());
        (rec, (self.seq % 2) * SLOT_LEN)
    }

    /// The sidecar handle plus the record to write at `pos`, for the
    /// sidecar modes.
    pub(crate) fn sidecar_write(
        &mut self,
        segment: uuid::Uuid,
        byte: u64,
    ) -> Option<(&File, [u8; RECORD_LEN], u64)> {
        let (rec, pos) = self.next_record(segment, byte);
        self.sidecar.as_ref().map(|f| (f, rec, pos))
    }

    pub(crate) fn write_sidecar(
        file: &File,
        rec: &[u8; RECORD_LEN],
        pos: u64,
        full: bool,
    ) -> std::io::Result<()> {
        file.write_all_at(rec, pos)?;
        crate::sync_file_data(file, full)
    }

    /// `InSegment`: pwrite the slot through a second, non-`O_APPEND` handle
    /// (an `O_APPEND` fd ignores the pwrite offset on Linux). The caller's
    /// segment `fdatasync` then covers it: fsync is per inode, not per fd.
    pub(crate) fn write_in_segment(
        &mut self,
        segment: uuid::Uuid,
        path: &Path,
    ) -> std::io::Result<()> {
        let stale = !matches!(&self.in_segment, Some((id, _, _)) if *id == segment);
        if stale {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            let mut header = [0u8; crate::SEGMENT_HEADER_LEN];
            file.read_exact_at(&mut header, 0)?;
            self.in_segment = Some((segment, file, header));
        }
        self.seq += 1;
        match &self.in_segment {
            Some((_, file, header)) => file.write_all_at(header, 0),
            None => Ok(()),
        }
    }
}
