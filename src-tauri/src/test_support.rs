//! Shared test-only environment-variable lock and RAII guard.
//!
//! `#[test]` functions across `paths.rs`, `credentials.rs`, and `switcher.rs`
//! all read or mutate process-global environment variables
//! (`CLAUDE_CONFIG_DIR`, `HOME`/`USERPROFILE`, `XDG_DATA_HOME`). Rust's default
//! test harness runs `#[test]` fns in parallel worker threads within the
//! *same process*, and env vars are process-global state shared by every one
//! of them.
//!
//! A lock scoped to a single module only serializes the tests *within* that
//! module — env vars have no module boundary, so a test in a different module
//! races right past a per-module mutex and can flip `HOME`/`CLAUDE_CONFIG_DIR`
//! out from under it mid-test. That was exactly the bug: `paths.rs`,
//! `credentials.rs`, and `switcher.rs` each had their own private mutex, which
//! served as a lock on nothing shared. This module is the single, crate-wide
//! lock every env-touching test must acquire instead, so no two of them —
//! regardless of which file they live in — can ever have their env-sensitive
//! section running concurrently.
//!
//! Not compiled into the shipped binary: gated `#![cfg(test)]` here *and*
//! included from `lib.rs` behind `#[cfg(test)] mod test_support;`.

#![cfg(test)]

use std::env;
use std::sync::{Mutex, MutexGuard};

/// The one process-wide lock guarding every env-var-touching test section in
/// this crate. Plain `Mutex::new(())` in a `static` (no `OnceLock` needed):
/// `Mutex::new` is `const fn`, so this initializes at compile time.
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`ENV_LOCK`], recovering from poison rather than propagating it.
///
/// A panic inside one env-touching test (e.g. a failed assertion) would
/// otherwise poison the mutex, and every *other* test that subsequently tries
/// to lock it would itself panic on `.unwrap()` — cascading one real failure
/// into a wall of unrelated ones. The lock here only serializes a side
/// effect (env-var mutation); it protects no invariant a panic could leave
/// broken, so recovering the guard from a poisoned lock is safe.
pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Panic unless `path` is inside a temp directory.
///
/// [`ENV_LOCK`] serialises env mutation, but a lock only helps tests that
/// remember to take it — and "remember to do X" is precisely the guarantee
/// that already failed here. A run of this suite once resolved the real
/// `~/.claude-swap-backup` and destroyed two registered accounts.
///
/// So this is the load-bearing defence, and it is deliberately not opt-in:
/// [`crate::paths::backup_root`] calls it on every resolution under `cfg(test)`,
/// which makes it structurally impossible for a test to reach a real store,
/// whether or not it took the lock.
///
/// If this fires, a test's env override did not take effect. Fix the test.
/// Never relax the assertion to make a run go green — that is exactly the
/// trade that caused the data loss.
pub fn guard_real_store(path: &std::path::Path) {
    let p = path.to_string_lossy().to_ascii_lowercase();

    let temp = env::temp_dir().to_string_lossy().to_ascii_lowercase();
    let looks_temp = (!temp.is_empty() && p.starts_with(temp.trim_end_matches(['\\', '/'])))
        || p.contains("\\temp\\")
        || p.contains("/tmp/")
        || p.contains("\\.tmp")
        || p.contains("/.tmp");

    assert!(
        looks_temp,
        "REFUSING TO RUN: a test resolved a path outside any temp directory:\n  \
         {}\n\
         A previous run destroyed a real ~/.claude-swap-backup this way. The \
         test's environment override did not take effect — it most likely did \
         not hold `test_support::env_lock()` for the whole section, or set the \
         wrong variable for this platform.",
        path.display()
    );
}

/// Test-only override for [`crate::paths::backup_root`].
///
/// Production's `STORE_ROOT` in `paths.rs` is a `OnceLock`: set once at real
/// app startup, first call wins, deliberately not re-settable — a later
/// caller must not be able to move the vault out from under an in-flight
/// operation. Tests need the opposite property: each test wants its *own*
/// fresh vault directory, and the suite runs many of them in one process.
///
/// So this is a second, separate piece of storage that [`crate::paths::backup_root`]
/// consults *before* the real `STORE_ROOT`, purely under `cfg(test)`. Same
/// discipline as [`EnvGuard`]: every caller holds [`env_lock`] for this
/// guard's full lifetime, and the previous value is restored on drop so one
/// test's override can never leak into the next.
static STORE_ROOT_OVERRIDE: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// RAII guard for [`STORE_ROOT_OVERRIDE`]. Construct via [`StoreRootGuard::set`].
pub struct StoreRootGuard {
    previous: Option<std::path::PathBuf>,
}

impl StoreRootGuard {
    /// Point [`crate::paths::backup_root`] at `path` for as long as the
    /// returned guard lives, restoring whatever override (if any) was active
    /// before.
    pub fn set(path: std::path::PathBuf) -> Self {
        let mut guard = STORE_ROOT_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner());
        let previous = guard.clone();
        *guard = Some(path);
        Self { previous }
    }
}

impl Drop for StoreRootGuard {
    fn drop(&mut self) {
        let mut guard = STORE_ROOT_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner());
        *guard = self.previous.clone();
    }
}

/// The active test override, if any. Consulted by [`crate::paths::backup_root`].
pub fn store_root_override() -> Option<std::path::PathBuf> {
    STORE_ROOT_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Saves an environment variable's prior value and restores it on drop, so a
/// test can freely set/unset a var without permanently disturbing the
/// process — or leaking state into the next test, given every caller also
/// holds [`env_lock`] for the guard's full lifetime.
///
/// Local variables in a Rust function drop in the *reverse* of their
/// declaration order, so a test that declares its `env_lock()` guard first
/// and its `EnvGuard`s after gets the right teardown order for free: the
/// `EnvGuard`s restore the environment first, and the shared lock is only
/// released after that — never the other way around, which would let another
/// thread start mutating the same env vars while this test's guards are
/// still being torn down.
pub struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    /// Set `key` to `value`, remembering whatever it was before so it can be
    /// restored on drop.
    pub fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var(key).ok();
        // SAFETY: every caller acquires `ENV_LOCK` (via `env_lock()`) for the
        // full lifetime of this guard, so no other thread observes or
        // mutates the environment concurrently with this call.
        unsafe { env::set_var(key, value) };
        Self { key, previous }
    }

    /// Remove `key`, remembering whatever it was before so it can be restored
    /// on drop.
    pub fn unset(key: &'static str) -> Self {
        let previous = env::var(key).ok();
        // SAFETY: see `set` above.
        unsafe { env::remove_var(key) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `EnvGuard::set` above.
        unsafe {
            match &self.previous {
                Some(v) => env::set_var(self.key, v),
                None => env::remove_var(self.key),
            }
        }
    }
}

// Sanity check for the guard/lock machinery itself, since every env-touching
// test in the crate depends on it for correctness.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_guard_restores_previous_value_on_drop() {
        let _lock = env_lock();
        let key = "CLAUDE_SWAP_GUI_TEST_SUPPORT_PROBE_VAR";
        let _outer = EnvGuard::set(key, "outer");
        assert_eq!(env::var(key).unwrap(), "outer");
        {
            let _inner = EnvGuard::set(key, "inner");
            assert_eq!(env::var(key).unwrap(), "inner");
        }
        assert_eq!(env::var(key).unwrap(), "outer");

        let untouched_key = "CLAUDE_SWAP_GUI_TEST_SUPPORT_UNSET_VAR";
        {
            let _guard = EnvGuard::unset(untouched_key);
            assert!(env::var(untouched_key).is_err());
        }
    }

    #[test]
    fn env_lock_recovers_from_poison() {
        // Simulate a prior test having panicked while holding the lock.
        let result = std::panic::catch_unwind(|| {
            let _guard = ENV_LOCK.lock().unwrap();
            panic!("simulated panic while holding ENV_LOCK");
        });
        assert!(result.is_err());
        assert!(ENV_LOCK.is_poisoned());

        // A subsequent caller must still be able to get a usable guard.
        let _lock = env_lock();
    }
}
