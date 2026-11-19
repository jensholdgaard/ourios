//! RFC 0068: the torn tail recovery cuts off the newest segment.

use std::fs::File;
use std::io::ErrorKind;
use std::path::PathBuf;

use crate::RecoveryError;

/// Which torn-tail shape recovery healed (RFC 0068 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailShape {
    /// T1: a frame cut short at EOF (RFC0008.4).
    PartialFrame,
    /// T2: zeros from the last valid frame to EOF.
    ZeroTail,
}

/// A torn tail [`Wal::replay`](crate::Wal::replay) cut off the newest segment (RFC 0068 §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailHeal {
    /// The segment that was cut.
    pub segment: PathBuf,
    pub shape: TailShape,
    /// `B`, the end of its last valid frame and its length after the cut.
    pub offset: u64,
    /// `E`, its length before the cut.
    pub size: u64,
}

/// Whether every byte of `file` from `from` to its end is zero (RFC 0068
/// §3.1 T2). Reads only the tail the scan stopped in.
pub(crate) fn zero_to_eof(file: &mut File, from: u64) -> Result<bool, RecoveryError> {
    use std::io::{Read, Seek, SeekFrom};
    let io = |source| RecoveryError::Io {
        op: "read(zero tail of newest segment)",
        source,
    };
    file.seek(SeekFrom::Start(from)).map_err(io)?;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(n) if chunk[..n].iter().all(|&b| b == 0) => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(io(e)),
        }
    }
}
