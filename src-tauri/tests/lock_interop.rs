//! Proves the central technical claim of this project: our [`FileLock`]
//! (`src/locking.rs`) genuinely interoperates with the *real*
//! `claude_swap.locking.FileLock` used by the `cswap` Python CLI — not just
//! "an in-process mutex that would also pass a naive same-process test."
//!
//! # Why cross-process, not in-process
//!
//! `src/locking.rs` already has unit tests that prove two `FileLock`s *in the
//! same process* exclude each other. That is necessary but not sufficient:
//! it would pass even if `locking.rs` used a plain in-process `Mutex` with no
//! real OS-level file lock underneath at all. The only way to prove the
//! actual interop contract — "a `cswap` process in a terminal and this app in
//! the tray can run at the same time without corrupting each other's on-disk
//! state" (see `src/locking.rs`'s module doc) — is to have a *separate OS
//! process*, running the genuine, installed `claude_swap` package, contend
//! for the same lock file using its own platform primitive
//! (`msvcrt.locking` / `fcntl.flock`), and observe real exclusion.
//!
//! That's what this file does. `PY_HELPER` below is a small driver script
//! that imports `claude_swap.locking.FileLock` — the actual class read from
//! `<uv tool venv>\Lib\site-packages\claude_swap\locking.py` — and calls its
//! real `acquire()`/`release()` methods, exactly as `cswap` itself does. A
//! reimplementation of the Python side here would defeat the point of this
//! test.
//!
//! # Environment isolation
//!
//! Every lock path used here lives under a `tempfile::tempdir()` created by
//! the test itself and is passed *directly* to [`FileLock`]/[`acquire_or_err`]
//! — this module never calls `paths::backup_root()` or any other
//! path-resolution function, so there is no `HOME`/`CLAUDE_CONFIG_DIR`
//! environment state to redirect and no risk of ever touching the real
//! `<backup_root>/.lock`. (`test_support::env_lock`/`EnvGuard` exist for
//! tests that *do* resolve real paths via `paths.rs`; they are also a
//! `#[cfg(test)]`-only module private to the lib crate and are not reachable
//! from this integration-test binary at all.) `cswap` is invoked only via
//! read-only, lock-only helper calls — never `switch`/`add`/`remove`/etc.
//!
//! # Skip behaviour
//!
//! [`find_python`] looks for a Python interpreter that actually has
//! `claude_swap` importable — first `CSWAP_PYTHON` if set, then the uv tool
//! venv `cswap` itself ships from. If none is found, every cross-process test
//! prints a clear skip message and returns rather than failing the suite.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use claude_swap_gui_lib::locking::{acquire_or_err, FileLock, LockingError};

// ---------------------------------------------------------------------------
// The Python side: a thin CLI driver around the REAL claude_swap.locking
// module, not a reimplementation of it.
// ---------------------------------------------------------------------------

const PY_HELPER: &str = r#"
import sys
import time
from pathlib import Path

from claude_swap.locking import FileLock

def main() -> None:
    path = Path(sys.argv[1])
    mode = sys.argv[2]
    timeout = float(sys.argv[3]) if len(sys.argv) > 3 else 10.0

    lock = FileLock(path, timeout=timeout)
    acquired = lock.acquire()
    print("ACQUIRED" if acquired else "TIMEOUT", flush=True)
    if not acquired:
        return

    if mode == "hold-until-stdin":
        sys.stdin.readline()
        lock.release()
        print("RELEASED", flush=True)
    elif mode == "hold-forever":
        # Simulates a crashed cswap: never releases, never exits on its own.
        # The test kills this process out from under the OS to prove the
        # lock is released by the OS, not by any cooperative cleanup here.
        while True:
            time.sleep(3600)

if __name__ == "__main__":
    main()
"#;

/// Locate a Python interpreter with the real, installed `claude_swap`
/// package importable. Checks `CSWAP_PYTHON` first (explicit override), then
/// the uv tool venv `cswap`/`claude-swap.exe` themselves run from on Windows
/// and the equivalent uv layout on POSIX. Returns `None` — never panics —
/// when nothing usable is found, so a machine without the CLI installed
/// skips cleanly instead of failing the suite.
fn find_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CSWAP_PYTHON") {
        let p = PathBuf::from(p);
        if python_has_claude_swap(&p) {
            return Some(p);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    // Windows: the exact uv tool venv this project's `cswap` install lives
    // in — `%APPDATA%\uv\tools\claude-swap\Scripts\python.exe`.
    if let Ok(appdata) = std::env::var("APPDATA") {
        candidates.push(
            PathBuf::from(&appdata)
                .join("uv")
                .join("tools")
                .join("claude-swap")
                .join("Scripts")
                .join("python.exe"),
        );
    }

    // POSIX equivalent of the same uv tool layout, in case this suite is
    // ever run on Linux/macOS with `cswap` installed the same way.
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        candidates.push(
            PathBuf::from(&home)
                .join(".local")
                .join("share")
                .join("uv")
                .join("tools")
                .join("claude-swap")
                .join("bin")
                .join("python3"),
        );
    }

    // Last resort: whatever `python3`/`python` resolve to on PATH, IF it
    // happens to have `claude_swap` importable (e.g. installed some other
    // way). Never falls back to a bare interpreter without the package —
    // that would silently test nothing.
    candidates.push(PathBuf::from("python3"));
    candidates.push(PathBuf::from("python"));

    candidates.into_iter().find(|c| python_has_claude_swap(c))
}

fn python_has_claude_swap(python: &Path) -> bool {
    Command::new(python)
        .args(["-c", "import claude_swap.locking"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn write_helper_script(dir: &Path) -> PathBuf {
    let path = dir.join("lock_helper.py");
    std::fs::write(&path, PY_HELPER).expect("write python helper script");
    path
}

// ---------------------------------------------------------------------------
// Process handle: spawns the helper, lets the test read status lines with a
// hard timeout (so a stuck/buggy child can never hang the suite) and send it
// commands, and guarantees the child is killed on drop even if a test
// assertion panics mid-way.
// ---------------------------------------------------------------------------

struct PyLockHolder {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    _stdout_reader: thread::JoinHandle<()>,
    _stderr_reader: thread::JoinHandle<()>,
}

impl PyLockHolder {
    fn spawn(python: &Path, script: &Path, lock_path: &Path, mode: &str, timeout_secs: f64) -> Self {
        let mut child = Command::new(python)
            .arg(script)
            .arg(lock_path)
            .arg(mode)
            .arg(timeout_secs.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn python lock helper");

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take().expect("piped stdin");

        let (tx, rx) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line.trim_end().to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let stderr_reader = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => eprint!("[python stderr] {line}"),
                }
            }
        });

        Self {
            child,
            stdin,
            rx,
            _stdout_reader: stdout_reader,
            _stderr_reader: stderr_reader,
        }
    }

    /// Read one line of the child's stdout, waiting at most `timeout`.
    /// `None` means the child produced no line in time (dead, hung, or
    /// buggy) — never blocks the suite indefinitely.
    fn read_line(&mut self, timeout: Duration) -> Option<String> {
        self.rx.recv_timeout(timeout).ok()
    }

    fn send_line(&mut self, s: &str) {
        writeln!(self.stdin, "{s}").expect("write to python helper stdin");
        self.stdin.flush().expect("flush python helper stdin");
    }
}

impl Drop for PyLockHolder {
    fn drop(&mut self) {
        // Best-effort: this must never itself fail a test. If the child
        // already exited (the common case — we told it to release and it
        // did), `kill` just errors and we ignore it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// 1. Self-consistency: our lock excludes itself, releases on drop, and is
//    re-acquirable afterwards.
// ---------------------------------------------------------------------------

#[test]
fn self_consistency_second_lock_times_out_then_reacquires_after_drop() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join(".lock");

    let mut first = FileLock::new(&lock_path);
    assert!(first.acquire().unwrap(), "first acquire must succeed uncontended");

    // A second FileLock over the same path must NOT succeed while the first
    // is held, and must fail via a clean, bounded timeout rather than hang.
    let mut second = FileLock::with_timeout(&lock_path, Duration::from_millis(300));
    let start = Instant::now();
    let acquired = second.acquire().unwrap();
    let elapsed = start.elapsed();
    assert!(!acquired, "a held lock must exclude a second in-process FileLock on the same path");
    assert!(elapsed < Duration::from_secs(5), "must fail via timeout, not hang (took {elapsed:?})");

    // acquire_or_err must surface this as LockingError::Timeout.
    let err = acquire_or_err(&lock_path, Duration::from_millis(200)).unwrap_err();
    assert!(matches!(err, LockingError::Timeout { .. }));

    // Dropping the guard must release the OS-level lock...
    drop(first);

    // ...so a fresh acquire immediately succeeds.
    let mut third = FileLock::with_timeout(&lock_path, Duration::from_millis(300));
    assert!(third.acquire().unwrap(), "lock must be re-acquirable once the holder drops");
}

// ---------------------------------------------------------------------------
// 2 & 3. Cross-process, both directions: a real second OS process running
//    the genuine claude_swap.locking.FileLock must mutually exclude our
//    FileLock — in both directions — over the same lock file.
// ---------------------------------------------------------------------------

#[test]
fn cross_process_python_holds_then_our_lock_is_excluded_then_succeeds_after_release() {
    let Some(python) = find_python() else {
        eprintln!(
            "skipping cross_process_python_holds_...: no Python with `claude_swap` \
             importable was found (checked CSWAP_PYTHON and the uv tool venv)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join(".lock");
    let script = write_helper_script(dir.path());

    let mut holder = PyLockHolder::spawn(&python, &script, &lock_path, "hold-until-stdin", 10.0);
    let status = holder
        .read_line(Duration::from_secs(10))
        .expect("python helper must print a status line promptly");
    assert_eq!(status, "ACQUIRED", "the real claude_swap.locking.FileLock must acquire uncontended");

    // Direction A + timeout behaviour: our lock must be excluded by the
    // Python holder, failing at approximately the configured timeout — not
    // hanging, and not silently succeeding (which would mean our
    // LockFileEx byte-range/flags disagree with msvcrt.locking's).
    let configured_timeout = Duration::from_millis(500);
    let start = Instant::now();
    let mut ours = FileLock::with_timeout(&lock_path, configured_timeout);
    let acquired = ours.acquire().unwrap();
    let elapsed = start.elapsed();
    assert!(
        !acquired,
        "our FileLock must be excluded while the real cswap Python FileLock holds the same lock file"
    );
    assert!(
        elapsed >= configured_timeout,
        "must not report failure before the configured timeout elapsed (took {elapsed:?}, configured {configured_timeout:?})"
    );
    assert!(
        elapsed < configured_timeout + Duration::from_secs(3),
        "must fail promptly at ~the configured timeout, not hang (took {elapsed:?})"
    );

    // Release the Python side...
    holder.send_line("release");
    let released = holder.read_line(Duration::from_secs(10));
    assert_eq!(released.as_deref(), Some("RELEASED"), "python must confirm it released the lock");

    // ...and now our lock must acquire cleanly: this is the proof that both
    // sides agree on exactly which byte, at which offset, of which file is
    // locked — a one-directional test alone could not catch a mismatch here.
    let mut ours2 = FileLock::with_timeout(&lock_path, Duration::from_secs(5));
    assert!(ours2.acquire().unwrap(), "our lock must succeed once the python holder releases");
}

#[test]
fn cross_process_our_lock_excludes_python_then_python_succeeds_after_we_release() {
    let Some(python) = find_python() else {
        eprintln!(
            "skipping cross_process_our_lock_excludes_python_...: no Python with `claude_swap` \
             importable was found (checked CSWAP_PYTHON and the uv tool venv)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join(".lock");
    let script = write_helper_script(dir.path());

    let mut ours = FileLock::new(&lock_path);
    assert!(ours.acquire().unwrap(), "our lock must acquire uncontended");

    // Direction B: the real Python FileLock, given a short bounded timeout,
    // must be excluded while we hold the lock.
    let mut holder = PyLockHolder::spawn(&python, &script, &lock_path, "hold-until-stdin", 1.5);
    let status = holder
        .read_line(Duration::from_secs(10))
        .expect("python helper must print a status line promptly");
    assert_eq!(
        status, "TIMEOUT",
        "the real claude_swap.locking.FileLock must be excluded while our Rust FileLock holds the file"
    );

    // Release our side...
    drop(ours);

    // ...and now python must be able to acquire — proving the exclusion just
    // observed was real contention over the shared OS lock, not a
    // coincidence, and that our release is visible to another process.
    let mut holder2 = PyLockHolder::spawn(&python, &script, &lock_path, "hold-until-stdin", 10.0);
    let status2 = holder2
        .read_line(Duration::from_secs(10))
        .expect("python helper must print a status line promptly");
    assert_eq!(status2, "ACQUIRED", "python must acquire once our lock releases");
    holder2.send_line("release");
    let released2 = holder2.read_line(Duration::from_secs(10));
    assert_eq!(released2.as_deref(), Some("RELEASED"));
}

// ---------------------------------------------------------------------------
// 5. Crash safety: if the lock-holding process dies without releasing, the
//    OS drops the lock and a subsequent acquire succeeds.
// ---------------------------------------------------------------------------

#[test]
fn crash_safety_os_releases_lock_when_holder_is_killed_without_releasing() {
    let Some(python) = find_python() else {
        eprintln!(
            "skipping crash_safety_...: no Python with `claude_swap` importable was found \
             (checked CSWAP_PYTHON and the uv tool venv)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join(".lock");
    let script = write_helper_script(dir.path());

    let mut holder = PyLockHolder::spawn(&python, &script, &lock_path, "hold-forever", 10.0);
    let status = holder
        .read_line(Duration::from_secs(10))
        .expect("python helper must print a status line promptly");
    assert_eq!(status, "ACQUIRED", "python must actually hold the lock before we simulate its crash");

    // Simulate a crash: forcibly terminate the process (TerminateProcess on
    // Windows, SIGKILL on POSIX via std::process::Child::kill) so it gets NO
    // opportunity to run `lock.release()`, `__exit__`, or any other
    // cooperative cleanup. If our lock only worked because cswap always
    // releases cleanly, this is exactly the scenario that would wedge this
    // app permanently.
    holder.child.kill().expect("terminate python holder to simulate a crash");
    holder.child.wait().expect("reap the killed child");

    // The OS — not the process — owns releasing msvcrt.locking / flock locks
    // on handle close or process exit, clean or not. A subsequent acquire
    // must succeed, and promptly (well under the configured timeout), not
    // eventually via the timeout/retry path.
    let start = Instant::now();
    let mut lock = FileLock::with_timeout(&lock_path, Duration::from_secs(5));
    let acquired = lock.acquire().unwrap();
    let elapsed = start.elapsed();
    assert!(acquired, "lock must be acquirable once the holder crashes without releasing");
    assert!(
        elapsed < Duration::from_secs(2),
        "acquire should succeed promptly once the OS drops the crashed holder's lock, \
         not via the timeout/retry path (took {elapsed:?})"
    );
}
