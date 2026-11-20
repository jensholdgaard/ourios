//! RFC 0065 §6.4: a group's writer retries a transient rotation fault in
//! place instead of stopping the replica.

use std::io;
use std::time::Duration;

use ourios_wal::{AppendError, FrameKind, SyncError, Wal, WalOffset};

use super::WalFailure;

/// The waits between attempts at a write RFC 0052 §3.3 reports as a
/// transient rotation fault, the last repeated: the WAL's own budget
/// (`rotation_retry_attempts`) decides when the retries end, by reporting
/// `RotationTerminal`.
const ROTATION_BACKOFF: [Duration; 4] = [
    Duration::from_millis(50),
    Duration::from_millis(200),
    Duration::from_millis(800),
    Duration::from_secs(2),
];

/// `write`, retried after [`ROTATION_BACKOFF`]'s waits while it fails
/// `transient`ly, and reported as I/O otherwise (RFC 0065 §6.4).
/// Nothing is acknowledged meanwhile: the write's callback fires only
/// after this returns and the frames are synced.
fn retrying<T, E: WalFailure>(
    mut write: impl FnMut() -> Result<T, E>,
    transient: impl Fn(&E) -> bool,
) -> io::Result<T> {
    let last = ROTATION_BACKOFF[ROTATION_BACKOFF.len() - 1];
    let mut waits = ROTATION_BACKOFF.into_iter().chain(std::iter::repeat(last));
    loop {
        match write() {
            Ok(done) => return Ok(done),
            Err(e) if transient(&e) => {
                std::thread::sleep(waits.next().unwrap_or(last));
            }
            Err(e) => return Err(e.to_io()),
        }
    }
}

/// Append `frame` as a replicated entry, retrying a transient rotation
/// fault in place.
pub(super) fn append(wal: &mut Wal, frame: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    injected::arm(wal);
    let transient = |e: &AppendError| matches!(e, AppendError::RotationRetrying(_));
    retrying(|| wal.append(FrameKind::ReplicatedEntry, frame), transient).map(|_| ())
}

/// Sync `wal`, retrying a transient rotation fault in place.
pub(super) fn sync(wal: &mut Wal) -> io::Result<WalOffset> {
    #[cfg(test)]
    injected::arm(wal);
    let transient = |e: &SyncError| matches!(e, SyncError::RotationRetrying(_));
    retrying(|| wal.sync(), transient)
}

/// RFC 0052's rotation faults, armed on a group's WAL by its writer
/// thread's next write. Keyed by the writer thread's name, which carries
/// the group, so tests that arm faults use groups no other test does.
#[cfg(test)]
pub(super) mod injected {
    use std::collections::HashMap;
    use std::sync::{Mutex, PoisonError};

    use ourios_wal::{RotationFaults, Wal};

    static ARMED: Mutex<Option<HashMap<String, RotationFaults>>> = Mutex::new(None);

    /// Arm `faults` on group `group`'s WAL from its writer's next write.
    pub(in crate::replicated::raft) fn on_group(group: u32, faults: RotationFaults) {
        let mut armed = ARMED.lock().unwrap_or_else(PoisonError::into_inner);
        armed
            .get_or_insert_with(HashMap::new)
            .insert(format!("ourios-wal-g{group}"), faults);
    }

    pub(super) fn arm(wal: &mut Wal) {
        let Some(thread) = std::thread::current().name().map(str::to_owned) else {
            return;
        };
        let mut armed = ARMED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(faults) = armed.as_mut().and_then(|armed| armed.remove(&thread)) {
            wal.arm_rotation_faults(faults);
        }
    }
}
