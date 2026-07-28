//! Cross-process coordination for mutations of Claude Code's live state.
//!
//! The first four locks mirror current `cswap` exactly: its account-store
//! file lock, Claude Code's primary and legacy credential directory locks,
//! and Claude Code's global-config directory lock. This GUI owns a separate
//! vault, so its private vault lock is acquired last. Keeping the guards in
//! one value makes both acquisition order and lifetime structural.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum LiveStateLockError {
    #[error(transparent)]
    CswapOrVault(#[from] crate::locking::LockingError),
    #[error(transparent)]
    Claude(#[from] crate::claude_locks::ClaudeLockError),
}

/// Every lock required while mutating the live credential, global config,
/// and this GUI's switch sequence. Fields intentionally remain private so a
/// caller cannot release only part of the lock set before a live write.
pub struct LiveStateLocks {
    _cswap: Option<crate::locking::FileLock>,
    _claude_credentials: crate::claude_locks::ClaudeCredentialLocks,
    _claude_config: crate::claude_locks::DirectoryLock,
    _vault: crate::locking::FileLock,
}

/// Acquire the complete live-state lock set in the canonical order.
///
/// The cswap lock is optional and is never allowed to create a fake cswap
/// store. All work performed while this guard exists must be local I/O; the
/// caller must complete network refreshes before entering this boundary.
pub fn acquire_live_state_locks(timeout: Duration) -> Result<LiveStateLocks, LiveStateLockError> {
    let cswap_root = crate::paths::cswap_store_root();
    let cswap = if cswap_root.exists() {
        Some(crate::locking::acquire_or_err(
            cswap_root.join(".lock"),
            timeout,
        )?)
    } else {
        None
    };
    let claude_credentials = crate::claude_locks::acquire_credential_locks(timeout)?;
    let claude_config = crate::claude_locks::acquire_config_lock(timeout)?;
    let vault = crate::locking::acquire_or_err(crate::paths::backup_root().join(".lock"), timeout)?;

    Ok(LiveStateLocks {
        _cswap: cswap,
        _claude_credentials: claude_credentials,
        _claude_config: claude_config,
        _vault: vault,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard, StoreRootGuard};
    use std::fs;
    use tempfile::TempDir;

    struct TestEnv {
        _home: EnvGuard,
        _userprofile: EnvGuard,
        _config: EnvGuard,
        _xdg: EnvGuard,
        _wsl: EnvGuard,
        _store: StoreRootGuard,
        _home_dir: TempDir,
        _config_dir: TempDir,
        vault: TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn setup() -> TestEnv {
        let lock = env_lock();
        let home = TempDir::new().unwrap();
        let config = TempDir::new().unwrap();
        let vault = TempDir::new().unwrap();
        let home_text = home.path().to_string_lossy().into_owned();
        let config_text = config.path().to_string_lossy().into_owned();
        let home_guard = EnvGuard::set("HOME", &home_text);
        let userprofile = EnvGuard::set("USERPROFILE", &home_text);
        let config_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", &config_text);
        let xdg = EnvGuard::unset("XDG_DATA_HOME");
        let wsl = EnvGuard::unset("WSL_DISTRO_NAME");
        let store = StoreRootGuard::set(vault.path().to_path_buf());
        TestEnv {
            _home: home_guard,
            _userprofile: userprofile,
            _config: config_guard,
            _xdg: xdg,
            _wsl: wsl,
            _store: store,
            _home_dir: home,
            _config_dir: config,
            vault,
            _lock: lock,
        }
    }

    #[test]
    fn locks_missing_cswap_store_without_creating_it() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        assert!(!cswap.exists());
        let locks = acquire_live_state_locks(Duration::from_millis(100)).unwrap();
        assert!(!cswap.exists());
        assert!(crate::paths::oauth_refresh_lock_dir().is_dir());
        assert!(crate::paths::credentials_lock_dir().is_dir());
        assert!(crate::paths::global_config_lock_dir().is_dir());
        assert!(env.vault.path().join(".lock").exists());
        drop(locks);
    }

    #[test]
    fn locks_cswap_contention_touches_no_later_lock() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        let _held =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_secs(1)).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        assert!(!env.vault.path().join(".lock").exists());
    }

    #[test]
    fn locks_primary_contention_releases_cswap_and_touches_no_later_lock() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        fs::create_dir(crate::paths::oauth_refresh_lock_dir()).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        assert!(!env.vault.path().join(".lock").exists());
    }

    #[test]
    fn locks_legacy_contention_releases_primary_and_cswap() {
        let _env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        fs::create_dir(crate::paths::credentials_lock_dir()).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
    }

    #[test]
    fn locks_config_contention_releases_credentials_and_cswap() {
        let _env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        fs::create_dir(crate::paths::global_config_lock_dir()).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
    }

    #[test]
    fn locks_vault_contention_releases_every_external_lock() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        let _held =
            crate::locking::acquire_or_err(env.vault.path().join(".lock"), Duration::from_secs(1))
                .unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
    }
}
