//! Path resolution for Claude Code config and credential files.
//!
//! Ported from [`claude-swap`](https://github.com/realiti4/claude-swap) (MIT
//! licensed), module `claude_swap/paths.py`. This file is a line-for-line
//! behavioral port, not a redesign: cc-logins must resolve exactly the
//! same on-disk paths the Python CLI does, or the two tools will silently
//! read and write different files.
//!
//! Mirrors claude-code's own resolution so this app reads and writes the
//! same files claude-code does. Key rules (from claude-code source, as
//! documented by the original Python module):
//!
//! - Config home: `CLAUDE_CONFIG_DIR` if set, else `~/.claude`.
//! - Global config: `<config_home>/.config.json` if it exists (legacy),
//!   otherwise `(CLAUDE_CONFIG_DIR || $HOME)/.claude.json`. Note the
//!   asymmetry: `.claude.json` sits at the home dir by default, not inside
//!   `.claude/`.
//! - Credentials: `<config_home>/.credentials.json`.
//!
//! Also resolves [`backup_root`], this app's own account vault. That is a
//! directory this app alone owns — see its doc for why it is never shared
//! with any other tool's store.
//!
//! References:
//! - claude-code `utils/env.ts` `getGlobalClaudeFile`
//! - claude-code `utils/secureStorage/plainTextStorage.ts` `getStoragePath`

use std::env;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Small env helpers
// ---------------------------------------------------------------------------

/// Read an environment variable, treating an unset *or empty* value as
/// absent. Mirrors Python's `if os.environ.get("X"):` truthiness check,
/// which is falsy for both a missing var and `""`.
fn env_non_empty(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Resolve the current user's home directory.
///
/// Mirrors Python's `Path.home()` (== `os.path.expanduser("~")`) closely
/// enough for this app's needs:
///
/// - macOS/Linux/WSL: `HOME` is authoritative, matching CPython's
///   `posixpath.expanduser`. (CPython additionally falls back to a
///   `pwd`-database lookup when `HOME` is unset; we do not reproduce that
///   fallback — see the crate-free caveat in the module's port notes.)
/// - Windows: CPython's `ntpath.expanduser` prefers `USERPROFILE`, then
///   `HOMEDRIVE`+`HOMEPATH`. We check those in the same order, with `HOME`
///   as a last-resort fallback (e.g. Git Bash / MSYS environments that set
///   only `HOME`).
///
/// Deliberately hand-rolled rather than built on the `dirs` crate (already a
/// workspace dependency, used elsewhere for exactly this kind of lookup):
/// empirically, `dirs::home_dir()` on Windows resolves the profile directory
/// straight from the OS (`SHGetKnownFolderPath`/`FOLDERID_Profile`) and
/// **ignores `USERPROFILE`/`HOME` entirely**, even when both are set. Python's
/// `Path.home()` does honor `USERPROFILE` on Windows (see above), so using
/// `dirs` here would silently diverge from the CLI's behavior for anyone who
/// overrides their profile dir (portable installs, CI, corporate imaging) —
/// exactly the kind of case this module exists to get right.
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(profile) = env_non_empty("USERPROFILE") {
            return PathBuf::from(profile);
        }
        if let (Some(drive), Some(path)) = (env_non_empty("HOMEDRIVE"), env_non_empty("HOMEPATH")) {
            return PathBuf::from(format!("{drive}{path}"));
        }
        if let Some(home) = env_non_empty("HOME") {
            return PathBuf::from(home);
        }
        PathBuf::from(".")
    }
    #[cfg(not(windows))]
    {
        match env_non_empty("HOME") {
            Some(home) => PathBuf::from(home),
            None => PathBuf::from("."),
        }
    }
}

// ---------------------------------------------------------------------------
// Platform detection
// ---------------------------------------------------------------------------

/// Supported platforms, mirroring `claude_swap.models.Platform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
    Wsl,
    Windows,
    Unknown,
}

impl Platform {
    /// Detect the current platform.
    ///
    /// The Python original uses `sys.platform` (a single cross-platform
    /// interpreter deciding at runtime) and treats WSL as Linux plus a
    /// `WSL_DISTRO_NAME` env var check. cc-logins instead ships one
    /// compiled binary per OS, so the OS itself is pinned at compile time
    /// via `#[cfg(target_os = ...)]` — equivalent in effect, since a
    /// Windows-built binary never runs under WSL and vice versa. WSL runs
    /// Linux binaries, so a Linux-target build takes the `target_os =
    /// "linux"` branch there too, and `WSL_DISTRO_NAME` (set by WSL itself)
    /// distinguishes it from bare Linux at runtime, exactly as upstream
    /// does.
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            Platform::Macos
        }
        #[cfg(target_os = "windows")]
        {
            Platform::Windows
        }
        #[cfg(target_os = "linux")]
        {
            if env::var_os("WSL_DISTRO_NAME").is_some() {
                Platform::Wsl
            } else {
                Platform::Linux
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Platform::Unknown
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution (the core of this module)
// ---------------------------------------------------------------------------

/// Return the Claude config home directory (`CLAUDE_CONFIG_DIR` or
/// `~/.claude`).
///
/// Under `cfg(test)` the result is asserted to be inside a temp directory.
/// This path leads to the **live** `.credentials.json` — a test that resolved
/// the real one could log the developer out of Claude Code entirely. Guarding
/// where the path is produced covers every caller, instead of trusting each
/// test to have set its environment override correctly.
pub fn claude_config_home() -> PathBuf {
    let resolved = claude_config_home_inner();
    #[cfg(test)]
    crate::test_support::guard_real_store(&resolved);
    resolved
}

fn claude_config_home_inner() -> PathBuf {
    if let Some(dir) = env_non_empty("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".claude")
}

/// Return the path to the global Claude config file.
///
/// Returns the legacy `<config_home>/.config.json` if it exists, else
/// `(CLAUDE_CONFIG_DIR || $HOME)/.claude.json`. Note the asymmetry versus
/// [`claude_config_home`]: the non-legacy file sits directly at the home
/// dir (or `CLAUDE_CONFIG_DIR`), *not* inside `.claude/`.
pub fn global_config_path() -> PathBuf {
    let legacy = claude_config_home().join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    let base = match env_non_empty("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => home_dir(),
    };
    let resolved = base.join(".claude.json");
    // Same reasoning as `claude_config_home`: this is the live account config.
    #[cfg(test)]
    crate::test_support::guard_real_store(&resolved);
    resolved
}

/// Return the global config path of the *default* profile.
///
/// Same legacy fallback as [`global_config_path`], but deliberately ignores
/// `CLAUDE_CONFIG_DIR`: callers that mirror the user's real profile
/// (session sharing) must not source from another session when invoked from
/// inside one.
pub fn default_global_config_path() -> PathBuf {
    let legacy = home_dir().join(".claude").join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    home_dir().join(".claude.json")
}

/// Return the path to the Claude credentials file.
pub fn credentials_path() -> PathBuf {
    claude_config_home().join(".credentials.json")
}

/// Claude Code 2.1.218+'s primary OAuth refresh directory lock.
pub fn oauth_refresh_lock_dir() -> PathBuf {
    claude_config_home().join(".oauth_refresh.lock")
}

/// Claude Code's legacy credential lock, still acquired after the primary
/// OAuth lock for compatibility with external writers.
pub fn credentials_lock_dir() -> PathBuf {
    let home = claude_config_home();
    let name = home
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(".claude");
    home.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.lock"))
}

/// `proper-lockfile` directory guarding Claude Code's global config.
pub fn global_config_lock_dir() -> PathBuf {
    let config = global_config_path();
    let name = config
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(".claude.json");
    config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.lock"))
}

/// Process-wide override for where this app keeps its own account vault.
///
/// Set once at startup (from Tauri's per-platform `app_data_dir()`). A
/// `OnceLock`, not a `RwLock`/`Mutex`: first call wins, deliberately, so a
/// later caller can never silently move the vault out from under an
/// in-flight operation. Tests do not use this at all — see
/// [`crate::test_support::StoreRootGuard`] for the equivalent they use
/// instead, which supports one fresh override per test.
static STORE_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Point this app's account vault at `root` (this app's own app-data
/// directory — e.g. Tauri's `app_data_dir()/accounts`).
///
/// # Why the vault is always ours
///
/// This app originally wrote its registry, credential backups and lock
/// directly into a directory another tool also owned. That gave interop for
/// free, and a shared blast radius with it: when a bug in this project's test
/// suite corrupted that store, it destroyed the other tool's accounts too,
/// because they were the same bytes.
///
/// So the vault is now always a directory of our own, alongside the settings
/// and history databases we already own — there is no setting or code path
/// that resolves anywhere else.
///
/// Idempotent: the first call wins, so a later caller cannot silently move the
/// vault out from under an in-flight operation.
pub fn set_store_root(root: PathBuf) {
    let _ = STORE_ROOT.set(root);
}

/// Where this app's OWN account vault lives. Always this app's directory,
/// regardless of platform, settings, or whether [`set_store_root`] has been
/// called yet.
///
/// [`set_store_root`]'s value once startup has configured it; otherwise a
/// same-shaped own-directory fallback (see [`default_store_root`]), so any
/// code that runs before startup configuration (notably tests, via
/// [`crate::test_support::StoreRootGuard`]) still resolves to a private
/// location rather than silently falling through to shared state.
pub fn backup_root() -> PathBuf {
    let resolved = backup_root_inner();
    #[cfg(test)]
    crate::test_support::guard_real_store(&resolved);
    resolved
}

fn backup_root_inner() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = crate::test_support::store_root_override() {
            return root;
        }
    }
    match STORE_ROOT.get() {
        Some(root) => root.clone(),
        None => default_store_root(),
    }
}

/// Fallback vault location used only before [`set_store_root`] has run.
///
/// In the shipped app this is never reached: `lib.rs` calls [`set_store_root`]
/// during Tauri `setup()`, before any command can touch the vault. It exists
/// for callers outside that lifecycle (tests without an override, or any
/// future non-Tauri entry point) so they still resolve to a directory this
/// app owns — computed from the OS's own "user data dir" convention (the same
/// `dirs` crate used for this app's log directory), never from anything
/// shared with another tool.
fn default_store_root() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("cc-logins")
        .join("accounts")
}

// ---------------------------------------------------------------------------
// Compatibility aliases
// ---------------------------------------------------------------------------
//
// `crate::credentials` (`credentials.rs`) already calls these five functions
// under their original `claude_swap/paths.py` names (`get_claude_config_home`,
// `get_global_config_path`, `get_default_global_config_path`,
// `get_credentials_path`, `get_backup_root`) — see
// e.g. its `read_global_config`/`write_active_credentials_file`. That file
// predates this port's naming and this task's instructions are not to edit
// it, so rather than choose between "idiomatic names" (what this task asked
// for, and what `crate::wsl` already calls — `claude_config_home`,
// `credentials_path`) and "don't touch other files", these thin aliases
// provide both: the functions above are the real implementation and the
// idiomatic public API, these just forward to them under the legacy name so
// `credentials.rs` keeps compiling unmodified once it is wired into the
// module tree.

/// Alias for [`claude_config_home`] under its original `paths.py` name.
pub fn get_claude_config_home() -> PathBuf {
    claude_config_home()
}

/// Alias for [`global_config_path`] under its original `paths.py` name.
pub fn get_global_config_path() -> PathBuf {
    global_config_path()
}

/// Alias for [`default_global_config_path`] under its original `paths.py`
/// name.
pub fn get_default_global_config_path() -> PathBuf {
    default_global_config_path()
}

/// Alias for [`credentials_path`] under its original `paths.py` name.
pub fn get_credentials_path() -> PathBuf {
    credentials_path()
}

/// Alias for [`backup_root`] under its original `paths.py` name.
pub fn get_backup_root() -> PathBuf {
    backup_root()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};
    use std::fs;
    use tempfile::TempDir;

    // Every test below reads or mutates process-wide environment variables
    // (CLAUDE_CONFIG_DIR, HOME/USERPROFILE, XDG_DATA_HOME). Rust's default
    // test harness runs `#[test]` fns in parallel worker threads within the
    // *same process*, and env vars are process-global state shared by all
    // of them — so without coordination, one test's `set_var` can leak into
    // another running concurrently and produce flaky, order-dependent
    // failures.
    //
    // Every env-touching test acquires `crate::test_support::ENV_LOCK` (via
    // `env_lock()`) for its full body. This is a *crate-wide* lock, shared
    // with `credentials.rs` and `switcher.rs`: a per-module lock here would
    // only serialize this file's tests against each other while a test in a
    // different module raced right past it and mutated the same env vars —
    // which is exactly the bug that used to make this suite flaky under the
    // default parallel `cargo test`.

    /// Point the platform-appropriate "home" env var at `dir`, returning a
    /// guard that restores the previous value on drop.
    #[cfg(windows)]
    fn set_home(dir: &Path) -> EnvGuard {
        EnvGuard::set("USERPROFILE", dir.to_str().expect("utf8 temp path"))
    }

    #[cfg(not(windows))]
    fn set_home(dir: &Path) -> EnvGuard {
        EnvGuard::set("HOME", dir.to_str().expect("utf8 temp path"))
    }

    // -- claude_config_home ---------------------------------------------

    #[test]
    fn claude_config_home_uses_env_var_when_set() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let custom = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", custom.path().to_str().unwrap());

        assert_eq!(claude_config_home(), custom.path());
    }

    #[test]
    fn claude_config_home_defaults_to_home_dot_claude_when_unset() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        assert_eq!(claude_config_home(), home.path().join(".claude"));
    }

    #[test]
    fn claude_config_home_treats_empty_env_var_as_unset() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", "");

        assert_eq!(claude_config_home(), home.path().join(".claude"));
    }

    // -- global_config_path ----------------------------------------------

    #[test]
    fn global_config_path_prefers_existing_legacy_file() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        let config_home = home.path().join(".claude");
        fs::create_dir_all(&config_home).unwrap();
        let legacy = config_home.join(".config.json");
        fs::write(&legacy, "{}").unwrap();

        assert_eq!(global_config_path(), legacy);
    }

    #[test]
    fn global_config_path_falls_back_to_home_dot_claude_json_when_env_unset() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        // No legacy file created, so it must fall through to <home>/.claude.json
        // (deliberately NOT inside .claude/).
        assert_eq!(global_config_path(), home.path().join(".claude.json"));
    }

    #[test]
    fn global_config_path_uses_claude_config_dir_as_base_when_set() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let custom = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", custom.path().to_str().unwrap());

        // No legacy .config.json under CLAUDE_CONFIG_DIR, so falls back to
        // <CLAUDE_CONFIG_DIR>/.claude.json, NOT <home>/.claude.json.
        assert_eq!(global_config_path(), custom.path().join(".claude.json"));
    }

    // -- default_global_config_path ---------------------------------------

    #[test]
    fn default_global_config_path_ignores_claude_config_dir() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let custom = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        // Even with CLAUDE_CONFIG_DIR set, default_global_config_path must
        // ignore it and use the real home dir.
        let _cfg_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", custom.path().to_str().unwrap());

        assert_eq!(
            default_global_config_path(),
            home.path().join(".claude.json")
        );
    }

    #[test]
    fn default_global_config_path_prefers_existing_legacy_file() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        let config_home = home.path().join(".claude");
        fs::create_dir_all(&config_home).unwrap();
        let legacy = config_home.join(".config.json");
        fs::write(&legacy, "{}").unwrap();

        assert_eq!(default_global_config_path(), legacy);
    }

    // -- credentials_path ---------------------------------------------------

    #[test]
    fn credentials_path_uses_config_home_env_var_when_set() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let custom = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", custom.path().to_str().unwrap());

        assert_eq!(credentials_path(), custom.path().join(".credentials.json"));
    }

    #[test]
    fn credentials_path_defaults_under_home_dot_claude_when_unset() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        assert_eq!(
            credentials_path(),
            home.path().join(".claude").join(".credentials.json")
        );
    }

    // -- get_* compatibility aliases -----------------------------------------

    #[test]
    fn get_prefixed_aliases_delegate_to_the_idiomatic_functions() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let custom = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", custom.path().to_str().unwrap());
        let vault = TempDir::new().unwrap();
        let _store_guard = crate::test_support::StoreRootGuard::set(vault.path().to_path_buf());

        // `credentials.rs` calls these three under their original
        // `paths.py` names; assert they agree with the idiomatic functions
        // they alias rather than re-testing the underlying logic.
        assert_eq!(get_claude_config_home(), claude_config_home());
        assert_eq!(get_global_config_path(), global_config_path());
        assert_eq!(get_credentials_path(), credentials_path());

        // default_global_config_path deliberately ignores CLAUDE_CONFIG_DIR;
        // its alias must too.
        assert_eq!(
            get_default_global_config_path(),
            default_global_config_path()
        );
        assert_eq!(get_backup_root(), backup_root());
    }

    // -- backup_root (our own vault — task 1) --------------------------------

    #[test]
    fn backup_root_is_whatever_the_test_override_configures() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let vault = TempDir::new().unwrap();
        let _store_guard = crate::test_support::StoreRootGuard::set(vault.path().to_path_buf());

        assert_eq!(backup_root(), vault.path());
    }

    // -- Platform::detect (host-pinned; smoke test only) ---------------------

    #[test]
    fn platform_detect_matches_compiled_target() {
        // Platform::detect() is pinned to the OS this test binary was
        // compiled for (see its doc comment) — it cannot be driven to other
        // variants from a single test run. This just confirms it produces a
        // variant consistent with the build, and that on Linux the
        // WSL_DISTRO_NAME env var (when present) is honored.
        let _lock = env_lock();

        #[cfg(target_os = "macos")]
        assert_eq!(Platform::detect(), Platform::Macos);

        #[cfg(target_os = "windows")]
        assert_eq!(Platform::detect(), Platform::Windows);

        #[cfg(target_os = "linux")]
        {
            let _wsl_guard = EnvGuard::unset("WSL_DISTRO_NAME");
            assert_eq!(Platform::detect(), Platform::Linux);

            let _wsl_guard = EnvGuard::set("WSL_DISTRO_NAME", "Ubuntu");
            assert_eq!(Platform::detect(), Platform::Wsl);
        }
    }

    // Sanity check that the env lock / guard machinery itself behaves, since
    // every test above depends on it for correctness.
    #[test]
    fn env_guard_restores_previous_value_on_drop() {
        let _lock = env_lock();
        let key = "CC_LOGINS_PATHS_TEST_PROBE_VAR";
        let _outer = EnvGuard::set(key, "outer");
        assert_eq!(env::var(key).unwrap(), "outer");
        {
            let _inner = EnvGuard::set(key, "inner");
            assert_eq!(env::var(key).unwrap(), "inner");
        }
        assert_eq!(env::var(key).unwrap(), "outer");

        // Also verify unset restores correctly when the var wasn't set at all.
        let untouched_key = "CC_LOGINS_PATHS_TEST_UNSET_VAR";
        let _ = env::var(untouched_key); // sanity: no assumption either way
        {
            let _guard = EnvGuard::unset(untouched_key);
            assert!(env::var(untouched_key).is_err());
        }
    }
}
