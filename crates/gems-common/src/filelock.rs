//! A single cross-process, exclusive, non-blocking advisory lock on a
//! path, via `rustix::fs::flock`. `gems-engine::Store` uses this to make
//! sure at most one process at a time opens a store directory writable —
//! see its module doc for the corruption scenario two concurrent writers
//! would otherwise cause.
//!
//! Non-blocking: a caller that finds the lock already held gets
//! `Error::AlreadyLocked` back immediately rather than hanging, since a
//! second writer waiting to "take its turn" on the same store isn't a
//! queueing problem this crate solves — it's a configuration mistake the
//! caller needs to know about right away.

use std::path::{Path, PathBuf};

use rustix::fs::{self, FlockOperation, Mode, OFlags};

use crate::{Error, Result};

/// An acquired exclusive lock; releases automatically (via `flock`'s
/// standard "released when the last fd referencing it closes" semantics)
/// when this value is dropped.
pub struct FileLock {
    _fd: std::os::fd::OwnedFd,
}

/// Acquires an exclusive, non-blocking lock on a file at `path`, creating
/// it if it doesn't exist. Returns `Error::AlreadyLocked` if another open
/// file description already holds it — including one held by a different
/// process, which is the scenario this exists to catch.
pub fn acquire_exclusive(path: &Path) -> Result<FileLock> {
    let fd = fs::open(path, OFlags::CREATE | OFlags::RDWR, Mode::RUSR | Mode::WUSR)
        .map_err(std::io::Error::from)?;
    fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|_| Error::AlreadyLocked {
        path: path.to_path_buf(),
    })?;
    Ok(FileLock { _fd: fd })
}

/// Convenience for the common case: the lock file lives at
/// `<dir>/.gems.lock`.
pub fn acquire_exclusive_in_dir(dir: &Path) -> Result<FileLock> {
    acquire_exclusive(&lock_path(dir))
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(".gems.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-common-filelock-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_exclusive_acquire_on_the_same_file_fails() {
        let dir = tmp_dir("second_fails");
        let path = dir.join("lock");
        let first = acquire_exclusive(&path).unwrap();
        let second = acquire_exclusive(&path);
        assert!(matches!(second, Err(Error::AlreadyLocked { .. })));
        drop(first);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_is_released_on_drop_so_a_later_acquire_succeeds() {
        let dir = tmp_dir("release_on_drop");
        let path = dir.join("lock");
        {
            let _first = acquire_exclusive(&path).unwrap();
        }
        assert!(acquire_exclusive(&path).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_exclusive_in_dir_uses_a_dotfile() {
        let dir = tmp_dir("in_dir");
        let _lock = acquire_exclusive_in_dir(&dir).unwrap();
        assert!(lock_path(&dir).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
