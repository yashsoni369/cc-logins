//! Cross-process advisory file locking.
//!
//! Ported from claude-swap (MIT) — <https://github.com/realiti4/claude-swap>,
//! `claude_swap/locking.py`. Two processes coordinate only when they lock the
//! *same* path with the *same* OS primitive, so this stays byte-compatible
//! with the lock files Claude Code itself honours (see
//! [`crate::claude_locks`]) and with other instances of this app.
//!
//! Protocol (must match the Python implementation byte-for-byte in
//! behaviour, not just in outcome):
//! - POSIX: `flock(fd, LOCK_EX | LOCK_NB)`, retried in a poll loop.
//! - Windows: `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK |
//!   LOCKFILE_FAIL_IMMEDIATELY` over a single byte — the Rust equivalent of
//!   `msvcrt.locking(fd, LK_NBLCK, 1)`.
//! - Poll every 100ms until `timeout` elapses (default 10s), then report
//!   failure. Never panics on contention.
//!
//! Hard rule inherited from upstream, binding on every caller of this module:
//! **never hold a credential or config lock across a network call.** A lock
//! held here blocks every other process on this machine that honours it; a
//! network call can stall arbitrarily long. Acquire, do the local file I/O,
//! release — then make the network call unlocked.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Default lock timeout, matching Python's `FileLock(timeout: float = 10.0)`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while contended, matching Python's `time.sleep(0.1)`.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Errors from preparing or acquiring a [`FileLock`].
///
/// Note this is deliberately narrow: contention alone is not an error (it is
/// reported via `acquire() -> Ok(false)`, mirroring Python's `bool` return),
/// matching upstream's "return failure rather than panicking" contract. Only
/// failing to create/open the lock file itself is an [`LockingError::Io`].
#[derive(Debug, thiserror::Error)]
pub enum LockingError {
    #[error("failed to prepare lock file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Raised only by [`acquire_or_err`], mirroring Python's
    /// `FileLock.__enter__` raising `LockError` on a failed `with` entry.
    #[error("failed to acquire lock on {path} — another instance may be running")]
    Timeout { path: PathBuf },
}

/// Cross-process file lock using platform-specific advisory locking APIs.
///
/// Mirrors Python's `claude_swap.locking.FileLock` class. The lock is
/// released automatically on [`Drop`], which is the RAII equivalent of
/// Python's `__exit__`/`release()`. Prefer [`acquire_or_err`] for the common
/// "acquire or fail loudly" pattern (Python's `with FileLock(path):`); use
/// [`FileLock::new`] + [`FileLock::acquire`] directly when a caller wants to
/// inspect the timeout outcome instead of treating it as an error.
#[derive(Debug)]
pub struct FileLock {
    lock_path: PathBuf,
    timeout: Duration,
    file: Option<File>,
    locked: bool,
}

impl FileLock {
    /// A lock over `lock_path` with the default 10s timeout.
    pub fn new(lock_path: impl Into<PathBuf>) -> Self {
        Self::with_timeout(lock_path, DEFAULT_TIMEOUT)
    }

    /// A lock over `lock_path` with an explicit default timeout (used when no
    /// timeout is passed to [`acquire`](Self::acquire)).
    pub fn with_timeout(lock_path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            lock_path: lock_path.into(),
            timeout,
            file: None,
            locked: false,
        }
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Acquire the lock using this instance's configured timeout.
    ///
    /// Returns `Ok(true)` once acquired, `Ok(false)` on timeout. Only fails
    /// with `Err` when the lock file itself could not be created/opened
    /// (parent directory creation, permissions, etc.) — contention is never
    /// an `Err`, matching the Python `acquire() -> bool` contract.
    pub fn acquire(&mut self) -> Result<bool, LockingError> {
        let timeout = self.timeout;
        self.acquire_timeout(timeout)
    }

    /// Acquire the lock with an explicit timeout, overriding the one given at
    /// construction (mirrors Python's `acquire(timeout: float | None)`).
    pub fn acquire_timeout(&mut self, timeout: Duration) -> Result<bool, LockingError> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LockingError::Io {
                path: self.lock_path.clone(),
                source,
            })?;
        }

        // Python: `open(self.lock_path, "w")` — create-or-truncate, opened
        // once per acquire() call; the lock call itself is retried on the
        // same open handle below.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.lock_path)
            .map_err(|source| LockingError::Io {
                path: self.lock_path.clone(),
                source,
            })?;

        let start = Instant::now();
        loop {
            if imp::try_lock_exclusive(&file) {
                self.file = Some(file);
                self.locked = true;
                return Ok(true);
            }
            // Python catches (BlockingIOError, OSError) broadly here and
            // just retries on *any* locking failure, not only contention —
            // so we do the same rather than distinguishing error kinds.
            if start.elapsed() > timeout {
                // `file` drops here, closing the fd/handle — mirrors
                // `self._lock_file.close(); self._lock_file = None`.
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Release the lock. Idempotent — a second call is a no-op, matching
    /// Python's `if self._lock_file and self._locked:` guard.
    pub fn release(&mut self) {
        if self.locked {
            if let Some(file) = self.file.take() {
                imp::unlock(&file);
                // `file` drops here, closing the handle — mirrors
                // `self._lock_file.close()`.
            }
            self.locked = false;
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Acquire a lock at `lock_path` or fail — the Rust equivalent of Python's
/// `with FileLock(lock_path, timeout) as lock: ...`. The returned guard
/// releases the lock when dropped.
///
/// # Errors
/// [`LockingError::Timeout`] if another process (this app or Claude Code
/// itself) is holding the lock past `timeout`. [`LockingError::Io`] if the lock file
/// could not be created/opened at all.
pub fn acquire_or_err(
    lock_path: impl Into<PathBuf>,
    timeout: Duration,
) -> Result<FileLock, LockingError> {
    let mut lock = FileLock::with_timeout(lock_path, timeout);
    if lock.acquire()? {
        Ok(lock)
    } else {
        // Clone rather than move: FileLock implements Drop, so its fields
        // cannot be moved out — the guard still has to release on drop.
        Err(LockingError::Timeout {
            path: lock.lock_path.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Platform-specific advisory lock primitives.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    /// `flock(fd, LOCK_EX | LOCK_NB)`. Returns `true` iff the lock was
    /// acquired; any failure (contention or otherwise) returns `false` so
    /// the caller's retry loop treats it uniformly, matching Python's broad
    /// `except (BlockingIOError, OSError)`.
    pub fn try_lock_exclusive(file: &File) -> bool {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid, open file descriptor for the lifetime of
        // this call (borrowed from `file`); `flock` does not retain it.
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        ret == 0
    }

    /// `flock(fd, LOCK_UN)`. Best-effort, mirrors Python swallowing the
    /// (essentially impossible) unlock failure.
    pub fn unlock(file: &File) {
        let fd = file.as_raw_fd();
        // SAFETY: same as above.
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, UnlockFile, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// `LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
    /// ..., nNumberOfBytesToLockLow = 1, ...)` — the documented Rust
    /// equivalent of `msvcrt.locking(fd, LK_NBLCK, 1)`: a single-byte
    /// non-blocking exclusive lock at the current (0) file offset. Any
    /// failure returns `false`, matching Python's broad retry-on-any-OSError
    /// behaviour.
    pub fn try_lock_exclusive(file: &File) -> bool {
        let handle = file.as_raw_handle() as HANDLE;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: `handle` is a valid, open file handle for the lifetime of
        // this call; `overlapped` is a valid, zeroed OVERLAPPED for a
        // synchronous (non-async) lock at offset 0.
        let ok = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1, // nNumberOfBytesToLockLow — lock exactly 1 byte.
                0, // nNumberOfBytesToLockHigh
                &mut overlapped,
            )
        };
        ok != 0
    }

    /// `UnlockFile` over the same single-byte range. Best-effort.
    pub fn unlock(file: &File) {
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: same as above.
        unsafe {
            UnlockFile(handle, 0, 0, 1, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let mut lock = FileLock::new(&path);
        assert!(lock.acquire().unwrap());
        assert!(lock.is_locked());
        lock.release();
        assert!(!lock.is_locked());
    }

    #[test]
    fn second_lock_on_same_file_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let mut first = FileLock::new(&path);
        assert!(first.acquire().unwrap());

        let mut second = FileLock::with_timeout(&path, Duration::from_millis(250));
        let acquired = second.acquire().unwrap();
        assert!(
            !acquired,
            "a held lock must not be acquirable a second time"
        );

        // Releasing the first lock frees it up for a subsequent acquire.
        first.release();
        let mut third = FileLock::with_timeout(&path, Duration::from_millis(250));
        assert!(third.acquire().unwrap());
    }

    #[test]
    fn drop_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        {
            let mut lock = FileLock::new(&path);
            assert!(lock.acquire().unwrap());
        } // dropped here — must release

        let mut second = FileLock::with_timeout(&path, Duration::from_millis(250));
        assert!(second.acquire().unwrap());
    }

    #[test]
    fn acquire_or_err_reports_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let _held = acquire_or_err(&path, Duration::from_secs(5)).unwrap();
        let err = acquire_or_err(&path, Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, LockingError::Timeout { .. }));
    }
}
