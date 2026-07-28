//! Claude Code `proper-lockfile`-compatible directory locks.
//!
//! Current Claude Code takes the primary OAuth lock first, then the legacy
//! credential lock, both with 60-second staleness. Global-config writes use a
//! separate 10-second-stale lock. This mirrors current cswap's compatibility
//! implementation and keeps the paths/timings explicit and test-pinned.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub const CREDENTIAL_STALENESS: Duration = Duration::from_secs(60);
pub const CONFIG_STALENESS: Duration = Duration::from_secs(10);
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(3);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(9);
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum ClaudeLockError {
    #[error("could not acquire Claude Code lock {path} within {timeout:?}")]
    Timeout { path: PathBuf, timeout: Duration },
    #[error("I/O error while managing Claude Code lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl ClaudeLockError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// An owned directory lock with a cooperative background mtime toucher.
#[derive(Debug)]
pub struct DirectoryLock {
    path: PathBuf,
    stop: Arc<(Mutex<bool>, Condvar)>,
    toucher: Option<JoinHandle<()>>,
}

impl DirectoryLock {
    pub fn acquire(
        path: impl Into<PathBuf>,
        timeout: Duration,
        staleness: Duration,
    ) -> Result<Self, ClaudeLockError> {
        Self::acquire_with_touch_interval(path.into(), timeout, staleness, TOUCH_INTERVAL)
    }

    fn acquire_with_touch_interval(
        path: PathBuf,
        timeout: Duration,
        staleness: Duration,
        touch_interval: Duration,
    ) -> Result<Self, ClaudeLockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| ClaudeLockError::io(&path, error))?;
        }
        let started = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(ClaudeLockError::io(&path, error)),
            }

            if started.elapsed() > timeout {
                return Err(ClaudeLockError::Timeout { path, timeout });
            }
            match fs::metadata(&path) {
                Ok(metadata) => {
                    let stale = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > staleness);
                    if stale {
                        match fs::remove_dir(&path) {
                            Ok(()) => continue,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(_) => {}
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(ClaudeLockError::io(&path, error)),
            }
            thread::sleep(RETRY_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
        }

        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_stop = Arc::clone(&stop);
        let thread_path = path.clone();
        let toucher = thread::Builder::new()
            .name("claude-lock-toucher".to_string())
            .spawn(move || {
                let (flag, wake) = &*thread_stop;
                let mut stopped = flag.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                loop {
                    let waited = wake
                        .wait_timeout_while(stopped, touch_interval, |value| !*value)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    stopped = waited.0;
                    if *stopped {
                        return;
                    }
                    drop(stopped);
                    if set_directory_modified(&thread_path, SystemTime::now()).is_err() {
                        return;
                    }
                    stopped = flag.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            })
            .map_err(|error| ClaudeLockError::io(&path, error))?;

        Ok(Self {
            path,
            stop,
            toucher: Some(toucher),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let (flag, wake) = &*self.stop;
        *flag.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        wake.notify_all();
        if let Some(toucher) = self.toucher.take() {
            if toucher.join().is_err() {
                log::warn!("Claude lock toucher panicked for {}", self.path.display());
            }
        }
        match fs::remove_dir(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log::warn!("Claude lock vanished while held: {}", self.path.display());
            }
            Err(error) => log::warn!(
                "could not release Claude lock {}: {error}",
                self.path.display()
            ),
        }
    }
}

pub struct ClaudeCredentialLocks {
    _primary: DirectoryLock,
    _legacy: DirectoryLock,
}

pub fn acquire_credential_locks(
    timeout: Duration,
) -> Result<ClaudeCredentialLocks, ClaudeLockError> {
    let primary = DirectoryLock::acquire(
        crate::paths::oauth_refresh_lock_dir(),
        timeout,
        CREDENTIAL_STALENESS,
    )?;
    let legacy = DirectoryLock::acquire(
        crate::paths::credentials_lock_dir(),
        timeout,
        CREDENTIAL_STALENESS,
    )?;
    Ok(ClaudeCredentialLocks {
        _primary: primary,
        _legacy: legacy,
    })
}

pub fn acquire_config_lock(timeout: Duration) -> Result<DirectoryLock, ClaudeLockError> {
    DirectoryLock::acquire(
        crate::paths::global_config_lock_dir(),
        timeout,
        CONFIG_STALENESS,
    )
}

#[cfg(not(windows))]
fn set_directory_modified(path: &Path, time: SystemTime) -> io::Result<()> {
    fs::File::open(path)?.set_modified(time)
}

#[cfg(test)]
pub(crate) fn age_lock_for_test(path: &Path, age: Duration) -> io::Result<()> {
    set_directory_modified(path, SystemTime::now() - age)
}

#[cfg(windows)]
fn set_directory_modified(path: &Path, time: SystemTime) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetFileTime, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let unix = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let ticks = unix.as_nanos() / 100 + 116_444_736_000_000_000_u128;
    let file_time = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let result = unsafe { SetFileTime(handle, std::ptr::null(), std::ptr::null(), &file_time) };
    unsafe { CloseHandle(handle) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};

    #[test]
    fn acquire_touches_and_release_removes_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target.lock");
        let lock = DirectoryLock::acquire_with_touch_interval(
            path.clone(),
            Duration::from_secs(1),
            CONFIG_STALENESS,
            Duration::from_millis(20),
        )
        .unwrap();
        let old = SystemTime::now() - Duration::from_secs(30);
        set_directory_modified(&path, old).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while fs::metadata(&path).unwrap().modified().unwrap() <= old {
            assert!(
                Instant::now() < deadline,
                "toucher did not update lock mtime"
            );
            thread::yield_now();
        }
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn fresh_lock_times_out_without_being_stolen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target.lock");
        fs::create_dir(&path).unwrap();
        let error =
            DirectoryLock::acquire(path.clone(), Duration::from_millis(40), CONFIG_STALENESS)
                .unwrap_err();
        assert!(matches!(error, ClaudeLockError::Timeout { .. }));
        assert!(path.is_dir());
    }

    #[test]
    fn credential_staleness_is_sixty_seconds_and_config_is_ten() {
        let root = tempfile::tempdir().unwrap();
        let credential = root.path().join("credential.lock");
        fs::create_dir(&credential).unwrap();
        set_directory_modified(&credential, SystemTime::now() - Duration::from_secs(30)).unwrap();
        assert!(matches!(
            DirectoryLock::acquire(
                credential.clone(),
                Duration::from_millis(40),
                CREDENTIAL_STALENESS
            ),
            Err(ClaudeLockError::Timeout { .. })
        ));
        set_directory_modified(&credential, SystemTime::now() - Duration::from_secs(61)).unwrap();
        drop(
            DirectoryLock::acquire(
                credential.clone(),
                Duration::from_secs(1),
                CREDENTIAL_STALENESS,
            )
            .unwrap(),
        );

        let config = root.path().join("config.lock");
        fs::create_dir(&config).unwrap();
        set_directory_modified(&config, SystemTime::now() - Duration::from_secs(11)).unwrap();
        drop(DirectoryLock::acquire(config, Duration::from_secs(1), CONFIG_STALENESS).unwrap());
    }

    #[test]
    fn primary_contention_never_creates_legacy() {
        let _env = env_lock();
        let root = tempfile::tempdir().unwrap();
        let root_text = root.path().to_string_lossy().into_owned();
        let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", &root_text);
        fs::create_dir(crate::paths::oauth_refresh_lock_dir()).unwrap();
        assert!(acquire_credential_locks(Duration::from_millis(40)).is_err());
        assert!(!crate::paths::credentials_lock_dir().exists());
    }

    #[test]
    fn legacy_contention_releases_primary() {
        let _env = env_lock();
        let root = tempfile::tempdir().unwrap();
        let root_text = root.path().to_string_lossy().into_owned();
        let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", &root_text);
        fs::create_dir(crate::paths::credentials_lock_dir()).unwrap();
        assert!(acquire_credential_locks(Duration::from_millis(40)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
    }

    #[test]
    fn configured_paths_match_claude_code_protocol() {
        let _env = env_lock();
        let root = tempfile::tempdir().unwrap();
        let config_home = root.path().join("custom-claude");
        let config_text = config_home.to_string_lossy().into_owned();
        let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", &config_text);
        assert_eq!(
            crate::paths::oauth_refresh_lock_dir(),
            config_home.join(".oauth_refresh.lock")
        );
        assert_eq!(
            crate::paths::credentials_lock_dir(),
            root.path().join("custom-claude.lock")
        );
        assert_eq!(
            crate::paths::global_config_lock_dir(),
            config_home.join(".claude.json.lock")
        );
    }
}
