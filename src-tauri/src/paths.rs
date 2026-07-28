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
//! Also resolves [`cswap_store_root`], the `cswap` CLI's *own* store location
//! (Linux/WSL: XDG Base Directory Specification, `$XDG_DATA_HOME/claude-swap`;
//! macOS/Windows: legacy `~/.claude-swap-backup`). That is a read-only
//! interop target — [`backup_root`], this app's own vault, is a completely
//! separate directory and never resolves there. See [`backup_root`]'s doc for
//! why the two must never be the same path.
//!
//! References:
//! - claude-code `utils/env.ts` `getGlobalClaudeFile`
//! - claude-code `utils/secureStorage/plainTextStorage.ts` `getStoragePath`
//! - XDG Base Directory Specification:
//!   <https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html>

use std::env;
use std::fs;
use std::io;
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
/// enough for claude-swap's needs:
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

/// POSIX-style `~` expansion, mirroring `posixpath.expanduser` for the one
/// case this module needs it (`XDG_DATA_HOME`, only ever consulted on the
/// Linux/WSL branch where the *real* Python process is itself running on
/// Linux, so `posixpath.expanduser` — not `ntpath.expanduser` — is what
/// applies there).
///
/// Handles `~` and `~/rest`. Does **not** handle `~otheruser` (CPython
/// resolves that via the `pwd` database, which has no portable std
/// equivalent); such input is returned unchanged, same as this module's
/// `home_dir` caveat above.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
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

/// Directory name of the legacy (pre-XDG) backup root.
pub const LEGACY_BACKUP_DIRNAME: &str = ".claude-swap-backup";

/// Return the legacy (pre-XDG) backup root: `~/.claude-swap-backup`.
pub fn legacy_backup_root() -> PathBuf {
    home_dir().join(LEGACY_BACKUP_DIRNAME)
}

/// Return the claude-swap backup root for the current platform.
///
/// Linux/WSL: `$XDG_DATA_HOME/claude-swap` (default
/// `~/.local/share/claude-swap`). macOS/Windows/unknown: `~/.claude-swap-backup`
/// (legacy layout).
///
/// Per the XDG spec, `$XDG_DATA_HOME` is ignored when unset, empty, or
/// non-absolute. A leading `~` is expanded so values like `~/data` set via
/// systemd unit files or Dockerfiles (which don't get shell expansion)
/// still work.
/// Under `cfg(test)`, every resolution is checked to be inside a temp
/// directory. This is not defensive padding: a flaky env-var race once let the
/// suite resolve the developer's real backup root and destroy two registered
/// accounts. Guarding at the single point where the path is produced catches
/// every caller, rather than relying on each test to remember a lock.
///
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
/// **directly into `~/.claude-swap-backup`** — the `cswap` CLI's own directory.
/// That gave interop for free, and a shared blast radius with it: when a bug in
/// this project's test suite corrupted that store, it destroyed the user's
/// `cswap` accounts too, because they were the same bytes.
///
/// So the vault is now always a directory of our own, alongside the settings
/// and history databases we already own — there is no setting or code path
/// that points it at `cswap`'s directory any more. The two tools still
/// interoperate (see [`cswap_store_root`] and `crate::switcher::import_from_cswap`),
/// but interop now means *reading* the other tool's files to copy from them,
/// never sharing the same mutable bytes.
///
/// Idempotent: the first call wins, so a later caller cannot silently move the
/// vault out from under an in-flight operation.
pub fn set_store_root(root: PathBuf) {
    let _ = STORE_ROOT.set(root);
}

/// Where this app's OWN account vault lives. Always this app's directory —
/// never the `cswap` CLI's — regardless of platform, settings, or whether
/// [`set_store_root`] has been called yet.
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
/// shared with `cswap`.
fn default_store_root() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("cc-logins")
        .join("accounts")
}

/// The `cswap` CLI's OWN store location — read-only interop territory. Used
/// for [`crate::switcher::import_from_cswap`] (copy accounts in) and for
/// locking the CLI's own lock file during a switch (see
/// `crate::switcher`'s lock-ordering doc). Never written to as a vault: this
/// app's vault is always [`backup_root`], a completely separate directory.
pub fn cswap_store_root() -> PathBuf {
    let resolved = cswap_store_root_inner();
    #[cfg(test)]
    crate::test_support::guard_real_store(&resolved);
    resolved
}

fn cswap_store_root_inner() -> PathBuf {
    cswap_store_root_for(Platform::detect())
}

/// Core logic behind [`cswap_store_root`], parameterized on the platform.
///
/// Split out from `cswap_store_root` so every branch (Linux/WSL vs.
/// macOS/Windows/Unknown) can be exercised in tests regardless of which OS
/// actually compiles and runs the test suite — unlike [`Platform::detect`],
/// which is pinned to the compiled target and so cannot be driven to every
/// variant on a single host.
fn cswap_store_root_for(platform: Platform) -> PathBuf {
    match platform {
        Platform::Linux | Platform::Wsl => {
            if let Some(xdg) = env_non_empty("XDG_DATA_HOME") {
                let expanded = expand_tilde(&xdg);
                if expanded.is_absolute() {
                    return expanded.join("claude-swap");
                }
            }
            home_dir().join(".local").join("share").join("claude-swap")
        }
        Platform::Macos | Platform::Windows | Platform::Unknown => legacy_backup_root(),
    }
}

// ---------------------------------------------------------------------------
// Compatibility aliases
// ---------------------------------------------------------------------------
//
// `crate::credentials` (`credentials.rs`) already calls these six functions
// under their original `claude_swap/paths.py` names (`get_claude_config_home`,
// `get_global_config_path`, `get_default_global_config_path`,
// `get_credentials_path`, `get_legacy_backup_root`, `get_backup_root`) — see
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

/// Alias for [`legacy_backup_root`] under its original `paths.py` name.
pub fn get_legacy_backup_root() -> PathBuf {
    legacy_backup_root()
}

/// Alias for [`backup_root`] under its original `paths.py` name.
pub fn get_backup_root() -> PathBuf {
    backup_root()
}

// ---------------------------------------------------------------------------
// Legacy backup directory migration
// ---------------------------------------------------------------------------

/// Error migrating the backup directory between layouts (e.g. legacy →
/// XDG). Mirrors Python's `claude_swap.exceptions.MigrationError`; defined
/// locally here since this file may not reach into a sibling error module.
/// Uses `thiserror` for consistency with the rest of the crate's error types
/// (`credentials::CredentialError`, `wsl::WslError`), which is already a
/// workspace dependency.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MigrationError(pub String);

/// Names that any prior claude-swap run may have created in the backup root
/// without user data being present (logger output, update-check + usage
/// cache). The migration treats a target containing only these as
/// effectively empty, since wiping them loses no real state.
const THROWAWAY_NAMES: &[&str] = &["cache"];
const THROWAWAY_PREFIXES: &[&str] = &["claude-swap.log"];

/// `true` if `entry`'s raw (non-symlink-following) file type is a real OS
/// error indicating the path is not a directory. Used to mirror Python's
/// `except (FileNotFoundError, NotADirectoryError)` guards around
/// `Path.iterdir()` without relying on the still-young
/// `io::ErrorKind::NotADirectory` variant.
fn is_not_a_directory(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(20) // ENOTDIR
    }
    #[cfg(windows)]
    {
        // ERROR_DIRECTORY: "The directory name is invalid."
        err.raw_os_error() == Some(267)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = err;
        false
    }
}

/// Return `true` if `target` contains anything beyond throwaway artifacts.
///
/// A missing or non-directory `target` is treated as `Ok(false)` (mirrors
/// Python catching `FileNotFoundError`/`NotADirectoryError`); any other I/O
/// error propagates, mirroring Python letting other `OSError`s escape
/// `_target_has_meaningful_data` to be caught (and re-wrapped) by the outer
/// `migrate_legacy_backup_dir`.
fn target_has_meaningful_data(target: &Path) -> io::Result<bool> {
    let entries = match fs::read_dir(target) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) if is_not_a_directory(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if THROWAWAY_NAMES.contains(&name.as_ref()) {
            continue;
        }
        if THROWAWAY_PREFIXES.iter().any(|p| name.starts_with(*p)) {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// Remove cache dir / log files so a rename/move can land on `target`.
///
/// Same missing/non-directory tolerance as [`target_has_meaningful_data`].
fn wipe_throwaway_artifacts(target: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(target) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) if is_not_a_directory(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        // `DirEntry::file_type` does not follow symlinks (lstat semantics),
        // so `is_dir()` here is true only for a *real* directory — exactly
        // Python's `entry.is_dir() and not entry.is_symlink()` (which
        // follows symlinks for `is_dir()` but excludes symlinks via the
        // second clause; the two conditions combined select the same set
        // of entries this single lstat-based check does).
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    fs::remove_dir(target)?;
    Ok(())
}

/// Best-effort mirror of Python's `Path.resolve(strict=False)`: canonicalize
/// the longest existing ancestor of `path`, then re-append whatever
/// trailing components don't exist yet, unresolved.
///
/// Unlike `std::fs::canonicalize`, this succeeds even when `path` (or its
/// tail) doesn't exist — which matters here because `target` in
/// [`migrate_legacy_backup_dir`] usually doesn't exist yet (creating it is
/// the point of the call), whereas Python's `resolve()` happily returns an
/// absolute, symlink-resolved-so-far path for a nonexistent target.
fn resolve_lenient(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };

    let mut probe = absolute.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match fs::canonicalize(&probe) {
            Ok(canon) => {
                let mut result = canon;
                for component in tail.into_iter().rev() {
                    result.push(component);
                }
                return Ok(result);
            }
            Err(e) => {
                let file_name = probe.file_name().map(|n| n.to_os_string());
                let parent = probe.parent().map(|p| p.to_path_buf());
                match (file_name, parent) {
                    (Some(name), Some(parent)) if parent != probe => {
                        tail.push(name);
                        probe = parent;
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

/// `true` if the OS error indicates a rename failed only because `src` and
/// `dst` are on different filesystems/volumes (`shutil.move`'s trigger for
/// falling back from `os.rename` to copy + delete).
fn is_cross_device(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(18) // EXDEV
    }
    #[cfg(windows)]
    {
        err.raw_os_error() == Some(17) // ERROR_NOT_SAME_DEVICE
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = err;
        false
    }
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let link_target = fs::read_link(src)?;
    std::os::unix::fs::symlink(link_target, dst)
}

#[cfg(windows)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let link_target = fs::read_link(src)?;
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(link_target, dst)
    } else {
        std::os::windows::fs::symlink_file(link_target, dst)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    // No portable symlink primitive off unix/windows; fall back to copying
    // the link's *contents*, which is the closest achievable approximation.
    let link_target = fs::read_link(src)?;
    fs::copy(link_target, dst).map(|_| ())
}

/// Recursively copy a directory tree, used as the cross-filesystem fallback
/// for [`move_path`] (mirrors what `shutil.move` does internally via
/// `shutil.copytree`/`copy2` when `os.rename` can't be used).
///
/// Preserves structure and file contents exactly; file metadata (mode bits,
/// timestamps) is *not* guaranteed to match `shutil.copy2`'s preservation
/// byte-for-byte — see this module's port notes.
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Move `src` to `dst`, mirroring `shutil.move`'s core behavior for this
/// module's use (an atomic rename when possible, falling back to
/// copy-then-delete across filesystems). Both `dst` and `dst`'s parent
/// existing-or-not are the caller's responsibility, same as in
/// [`migrate_legacy_backup_dir`], which always removes/never-creates `dst`
/// before calling this (so `shutil.move`'s "move *into* an existing
/// directory" special case never triggers there and is not reproduced
/// here).
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            copy_dir_recursive(src, dst)?;
            fs::remove_dir_all(src)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn migrating_flag_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let flag_name = format!(".{name}.migrating");
    match target.parent() {
        Some(parent) => parent.join(flag_name),
        None => PathBuf::from(flag_name),
    }
}

fn wrap_migration_io_error(legacy: &Path, target: &Path, err: io::Error) -> MigrationError {
    MigrationError(format!(
        "Migration of {} \u{2192} {} failed: {}",
        legacy.display(),
        target.display(),
        err
    ))
}

/// Move the legacy backup directory to `target` if needed.
///
/// Uses a rename where possible (falling back to copy + delete across
/// filesystems, mirroring `shutil.move`), guarded by a `<target>.migrating`
/// flag file. Touching the flag *before* the move and removing it *after*
/// lets us tell an interrupted migration apart from a foreign collision on
/// the next run:
///
/// * Flag present, legacy still there -> resume (discard any partial target
///   and retry).
/// * Flag present, legacy gone -> previous run completed but didn't get to
///   clean the flag; just remove it.
/// * No flag, both paths exist -> genuine collision, refuse -- *unless* the
///   target only holds throwaway artifacts (cache/, log files) that any
///   prior claude-swap run may have laid down before legacy reappeared
///   (e.g. first run on a fresh box, then legacy synced in from another
///   machine). In that case wipe the artifacts and migrate normally.
///
/// Returns `Ok(true)` if the move ran in this call, `Ok(false)` if it was a
/// no-op.
///
/// # Errors
///
/// Returns [`MigrationError`] on a genuine collision, or when the
/// filesystem operations fail. Note the collision case is raised directly
/// (matching Python raising `MigrationError` from inside the `try` block
/// that only catches `OSError` — `MigrationError` isn't an `OSError`
/// subclass there, so it passes through unwrapped); other I/O failures are
/// wrapped with a `"Migration of {legacy} -> {target} failed: {err}"`
/// message, matching the Python `except OSError as exc: raise
/// MigrationError(...) from exc` path.
pub fn migrate_legacy_backup_dir(target: &Path) -> Result<bool, MigrationError> {
    let legacy = legacy_backup_root();

    let same_path = match (resolve_lenient(&legacy), resolve_lenient(target)) {
        (Ok(l), Ok(t)) => l == t,
        _ => legacy == target,
    };
    if same_path {
        return Ok(false);
    }

    let flag = migrating_flag_path(target);

    if !legacy.exists() {
        // Successful prior run that died before removing the flag.
        if flag.exists() {
            let _ = fs::remove_file(&flag);
        }
        return Ok(false);
    }

    // Genuine-collision check, mirroring the Python `elif target.exists():
    // if _target_has_meaningful_data(target): raise MigrationError(...)`
    // branch. This raises directly rather than going through the generic
    // I/O wrap below, matching Python's `MigrationError` not being an
    // `OSError` and therefore skipping the surrounding `except OSError`.
    if !flag.exists() && target.exists() {
        let has_data = target_has_meaningful_data(target)
            .map_err(|e| wrap_migration_io_error(&legacy, target, e))?;
        if has_data {
            return Err(MigrationError(format!(
                "Both legacy ({}) and new ({}) backup paths exist. Refusing to merge or \
                 overwrite \u{2014} inspect both and remove the stale one manually before \
                 re-running.",
                legacy.display(),
                target.display(),
            )));
        }
    }

    let outcome: io::Result<()> = (|| {
        if flag.exists() {
            // Prior run was interrupted before completion. Discard any
            // (potentially partial) target and retry the move from legacy.
            if target.exists() {
                fs::remove_dir_all(target)?;
            }
        } else if target.exists() {
            // Re-checked (matches Python re-evaluating `target.exists()`
            // rather than caching the earlier check): only throwaway
            // artifacts can remain here, since a meaningful-data collision
            // already returned above.
            wipe_throwaway_artifacts(target)?;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // touch(): create if missing, leave existing (empty marker)
        // content alone otherwise.
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&flag)?;
        move_path(&legacy, target)?;
        fs::remove_file(&flag)?;
        Ok(())
    })();

    outcome.map_err(|e| wrap_migration_io_error(&legacy, target, e))?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};
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

    // -- legacy_backup_root -------------------------------------------------

    #[test]
    fn legacy_backup_root_is_home_dot_claude_swap_backup() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        assert_eq!(
            legacy_backup_root(),
            home.path().join(".claude-swap-backup")
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
        assert_eq!(get_legacy_backup_root(), legacy_backup_root());
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

    #[test]
    fn backup_root_never_equals_the_cswap_location_even_when_home_would_make_them_collide() {
        // Point HOME somewhere that WOULD make cswap_store_root() resolve
        // right where the legacy shared store used to live, and confirm our
        // vault (via an explicit override, as it always is once configured)
        // is still a completely different directory. This is the regression
        // this whole task exists to prevent: the vault silently landing
        // inside the cswap CLI's own directory.
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        // Unset so the Linux branch is deterministic; CI runners set this.
        let _xdg_guard = EnvGuard::unset("XDG_DATA_HOME");
        let vault = TempDir::new().unwrap();
        let _store_guard = crate::test_support::StoreRootGuard::set(vault.path().to_path_buf());

        // The point of the test: the vault is never the CLI's directory.
        assert_ne!(backup_root(), cswap_store_root());

        // cswap's own layout differs per platform — XDG on Linux, a dotdir
        // elsewhere — so the expectation has to as well.
        #[cfg(any(windows, target_os = "macos"))]
        let expected = home.path().join(".claude-swap-backup");
        #[cfg(not(any(windows, target_os = "macos")))]
        let expected = home.path().join(".local").join("share").join("claude-swap");

        assert_eq!(cswap_store_root(), expected);
    }

    // -- cswap_store_root_for (platform-parameterized XDG logic) ------------

    #[test]
    fn cswap_store_root_linux_uses_xdg_data_home_when_set_and_absolute() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let xdg = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _xdg_guard = EnvGuard::set("XDG_DATA_HOME", xdg.path().to_str().unwrap());

        assert_eq!(
            cswap_store_root_for(Platform::Linux),
            xdg.path().join("claude-swap")
        );
        assert_eq!(
            cswap_store_root_for(Platform::Wsl),
            xdg.path().join("claude-swap")
        );
    }

    #[test]
    fn cswap_store_root_linux_falls_back_to_local_share_when_xdg_unset() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _xdg_guard = EnvGuard::unset("XDG_DATA_HOME");

        assert_eq!(
            cswap_store_root_for(Platform::Linux),
            home.path().join(".local").join("share").join("claude-swap")
        );
    }

    #[test]
    fn cswap_store_root_linux_ignores_non_absolute_xdg_data_home() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _xdg_guard = EnvGuard::set("XDG_DATA_HOME", "relative/path");

        assert_eq!(
            cswap_store_root_for(Platform::Linux),
            home.path().join(".local").join("share").join("claude-swap")
        );
    }

    #[test]
    fn cswap_store_root_linux_expands_leading_tilde_in_xdg_data_home() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _xdg_guard = EnvGuard::set("XDG_DATA_HOME", "~/xdgdata");

        assert_eq!(
            cswap_store_root_for(Platform::Linux),
            home.path().join("xdgdata").join("claude-swap")
        );
    }

    #[test]
    fn cswap_store_root_macos_and_windows_use_legacy_layout() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        // Even with XDG_DATA_HOME set, non-Linux platforms must ignore it.
        let xdg = TempDir::new().unwrap();
        let _xdg_guard = EnvGuard::set("XDG_DATA_HOME", xdg.path().to_str().unwrap());

        let expected = home.path().join(".claude-swap-backup");
        assert_eq!(cswap_store_root_for(Platform::Macos), expected);
        assert_eq!(cswap_store_root_for(Platform::Windows), expected);
        assert_eq!(cswap_store_root_for(Platform::Unknown), expected);
    }

    // -- Platform::detect (host-pinned; smoke test only) ---------------------

    #[test]
    fn platform_detect_matches_compiled_target() {
        // Platform::detect() is pinned to the OS this test binary was
        // compiled for (see its doc comment) — it cannot be driven to other
        // variants from a single test run, unlike cswap_store_root_for above.
        // This just confirms it produces a variant consistent with the
        // build, and that on Linux the WSL_DISTRO_NAME env var (when
        // present) is honored.
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

    // -- migrate_legacy_backup_dir -------------------------------------------
    //
    // These exercise the migration state machine directly against real
    // temp-dir filesystem state; they don't depend on CLAUDE_CONFIG_DIR or
    // XDG_DATA_HOME, but do depend on `home_dir()` (via
    // `legacy_backup_root()`), so they still take the env lock and pin HOME.

    #[test]
    fn migrate_no_op_when_legacy_absent() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        // legacy_backup_root() = <home>/.claude-swap-backup, which we never
        // create in this test, so it does not exist.

        let target_parent = TempDir::new().unwrap();
        let target = target_parent.path().join("claude-swap");

        let moved = migrate_legacy_backup_dir(&target).unwrap();
        assert!(!moved);
        assert!(!target.exists());
    }

    #[test]
    fn migrate_moves_legacy_into_target_when_target_absent() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        let legacy = home.path().join(".claude-swap-backup");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("marker.txt"), b"hello").unwrap();

        let target_parent = TempDir::new().unwrap();
        let target = target_parent.path().join("claude-swap");

        let moved = migrate_legacy_backup_dir(&target).unwrap();
        assert!(moved);
        assert!(!legacy.exists());
        assert!(target.join("marker.txt").exists());
        assert_eq!(
            fs::read_to_string(target.join("marker.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn migrate_wipes_throwaway_artifacts_and_proceeds() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        let legacy = home.path().join(".claude-swap-backup");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("marker.txt"), b"payload").unwrap();

        let target_parent = TempDir::new().unwrap();
        let target = target_parent.path().join("claude-swap");
        fs::create_dir_all(target.join("cache")).unwrap();
        fs::write(target.join("claude-swap.log"), b"log").unwrap();

        let moved = migrate_legacy_backup_dir(&target).unwrap();
        assert!(moved);
        assert!(target.join("marker.txt").exists());
        assert!(!target.join("cache").exists());
    }

    #[test]
    fn migrate_refuses_genuine_collision() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        let legacy = home.path().join(".claude-swap-backup");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("marker.txt"), b"payload").unwrap();

        let target_parent = TempDir::new().unwrap();
        let target = target_parent.path().join("claude-swap");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("real_data.txt"), b"already here").unwrap();

        let err = migrate_legacy_backup_dir(&target).unwrap_err();
        assert!(err.0.contains("Refusing to merge or overwrite"));
        // Nothing should have moved.
        assert!(legacy.exists());
        assert!(target.join("real_data.txt").exists());
    }

    #[test]
    fn migrate_is_no_op_when_legacy_and_target_are_the_same_path() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        let legacy = home.path().join(".claude-swap-backup");
        fs::create_dir_all(&legacy).unwrap();

        let moved = migrate_legacy_backup_dir(&legacy).unwrap();
        assert!(!moved);
        assert!(legacy.exists());
    }

    #[test]
    fn migrate_resumes_after_interrupted_run_via_flag_file() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        let legacy = home.path().join(".claude-swap-backup");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("marker.txt"), b"fresh").unwrap();

        let target_parent = TempDir::new().unwrap();
        let target = target_parent.path().join("claude-swap");
        // Simulate a partially-completed prior run: target exists with
        // stale/partial data, and the flag file is present.
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("partial.txt"), b"stale").unwrap();
        let flag = target_parent.path().join(".claude-swap.migrating");
        fs::write(&flag, b"").unwrap();

        let moved = migrate_legacy_backup_dir(&target).unwrap();
        assert!(moved);
        assert!(!flag.exists());
        assert!(target.join("marker.txt").exists());
        assert!(!target.join("partial.txt").exists());
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
