//! Local-backend publication of the RFC 0033 template-map artifact (§3.3
//! amendment 2026-10-12, *Concurrent queriers*).
//!
//! Each publish writes its own uniquely named temp
//! (`<artifact>.<UUIDv7>.tmp`) and renames it into place, so two
//! concurrent publishers can never interleave bytes in one temp: every
//! renamed file is one writer's complete artifact. A publish that fails
//! after creating its temp removes it; temps leaked by a crash are swept
//! by later publishes once they are an hour old.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::QueryError;

/// Age past which a sibling publish temp is a crash leftover. A publish
/// writes at most the 64 MiB decode-bound artifact, which takes seconds;
/// an hour is orders of magnitude above that, so a live publisher's temp
/// never matches. Safety does not rest on it: a temp removed under a live
/// publisher only fails that publisher's rename.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Atomically install `bytes` as `dir/filename` through a uniquely named
/// temp, sweeping stale sibling temps first. Last writer wins.
///
/// # Errors
///
/// [`QueryError::Storage`] if the directory cannot be created or the
/// temp cannot be written or renamed into place. The temp is removed,
/// best-effort, on any failure after it was created.
pub(crate) fn publish_local(dir: &Path, filename: &str, bytes: &[u8]) -> Result<(), QueryError> {
    std::fs::create_dir_all(dir).map_err(|e| io_error("create_dir_all", dir, &e))?;
    sweep_stale_temps(dir, filename, SystemTime::now());
    let tmp = dir.join(format!("{filename}.{}.tmp", uuid::Uuid::now_v7()));
    let result = write_then_rename(&tmp, &dir.join(filename), bytes);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_then_rename(tmp: &Path, target: &Path, bytes: &[u8]) -> Result<(), QueryError> {
    std::fs::write(tmp, bytes).map_err(|e| io_error("write", tmp, &e))?;
    std::fs::rename(tmp, target).map_err(|e| QueryError::Storage {
        detail: format!("rename {} -> {}: {e}", tmp.display(), target.display()),
    })
}

fn io_error(op: &str, path: &Path, e: &std::io::Error) -> QueryError {
    QueryError::Storage {
        detail: format!("{op} {}: {e}", path.display()),
    }
}

/// Remove `dir`'s publish temps for `filename` (the unique
/// `<filename>.*.tmp` names and the legacy fixed `<filename>.tmp`) whose
/// modification time is more than [`STALE_TEMP_AGE`] before `now`. Every
/// failure is ignored: a missing file, a racing remover, an unreadable
/// directory.
fn sweep_stale_temps(dir: &Path, filename: &str, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{filename}.");
    for entry in entries.flatten() {
        let is_temp = entry.file_name().to_str().is_some_and(|name| {
            name.starts_with(&prefix) && Path::new(name).extension().is_some_and(|e| e == "tmp")
        });
        if is_temp && is_stale(&entry, now) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn is_stale(entry: &std::fs::DirEntry, now: SystemTime) -> bool {
    entry
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age > STALE_TEMP_AGE)
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    const NAME: &str = "template_map.v2.json.zst";

    fn plant(dir: &Path, name: &str, age: Duration) {
        let path = dir.join(name);
        std::fs::write(&path, b"leftover").expect("plant temp");
        File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now() - age))
            .expect("age temp");
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn publish_installs_the_bytes_and_leaves_no_temp() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        publish_local(dir.path(), NAME, b"one").expect("publish");
        publish_local(dir.path(), NAME, b"two").expect("republish");
        assert_eq!(std::fs::read(dir.path().join(NAME)).expect("read"), b"two");
        assert_eq!(names(dir.path()), vec![NAME.to_string()]);
    }

    /// RFC0033.12 (local): temps more than an hour old are swept, younger
    /// ones and unrelated files are kept.
    #[test]
    fn publish_sweeps_only_stale_temps() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let hour = Duration::from_secs(3600);
        plant(dir.path(), &format!("{NAME}.old.tmp"), hour * 2);
        plant(dir.path(), &format!("{NAME}.tmp"), hour * 2);
        plant(
            dir.path(),
            &format!("{NAME}.young.tmp"),
            Duration::from_secs(60),
        );
        plant(dir.path(), "other.old.tmp", hour * 2);
        publish_local(dir.path(), NAME, b"map").expect("publish");
        assert_eq!(
            names(dir.path()),
            vec![
                "other.old.tmp".to_string(),
                NAME.to_string(),
                format!("{NAME}.young.tmp"),
            ],
        );
    }

    /// RFC0033.12 (local): a publish that fails after creating its temp
    /// leaves no temp of its own behind. A non-empty directory at the
    /// target name makes the rename fail.
    #[test]
    fn failed_rename_removes_its_own_temp() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let blocker = dir.path().join(NAME);
        std::fs::create_dir(&blocker).expect("blocker dir");
        std::fs::write(blocker.join("keep"), b"x").expect("fill blocker");
        let err = publish_local(dir.path(), NAME, b"map").expect_err("rename must fail");
        assert!(matches!(err, QueryError::Storage { .. }), "got {err:?}");
        assert_eq!(names(dir.path()), vec![NAME.to_string()]);
    }

    /// RFC0033.12 (local): concurrent publishers each use their own temp,
    /// so the installed artifact is always one writer's complete bytes.
    #[test]
    fn concurrent_publishers_install_one_complete_artifact() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let payloads: Vec<Vec<u8>> = (0u8..4).map(|i| vec![i; 256 * 1024]).collect();
        std::thread::scope(|scope| {
            for payload in &payloads {
                let dir = dir.path();
                scope.spawn(move || {
                    for _ in 0..8 {
                        publish_local(dir, NAME, payload).expect("publish");
                    }
                });
            }
        });
        let installed = std::fs::read(dir.path().join(NAME)).expect("read");
        assert!(
            payloads.contains(&installed),
            "an interleaved artifact was installed"
        );
        assert_eq!(names(dir.path()), vec![NAME.to_string()]);
    }
}
