//! Environment and platform helpers with no dependencies beyond `std`.
//!
//! Split out of `paths.rs` so the `cc-logins-shim` binary can compile this
//! same file (via `#[path]`) without linking the app library, which pulls in
//! Tauri and the webview. Anything added here must stay `std`-only and must
//! not reference `crate::` items other than sibling shared modules.

use std::env;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Small env helpers
// ---------------------------------------------------------------------------

/// Read an environment variable, treating an unset *or empty* value as
/// absent. Mirrors Python's `if os.environ.get("X"):` truthiness check,
/// which is falsy for both a missing var and `""`.
pub(crate) fn env_non_empty(name: &str) -> Option<String> {
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
pub(crate) fn home_dir() -> PathBuf {
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
// Well-known directories shared with the shim
// ---------------------------------------------------------------------------

/// Overrides where cc-logins keeps the files the `claude` shim reads
/// (`~/.cc-logins` by default). Tests and portable setups point this
/// elsewhere; the app and the shim must always agree on it, which is why it
/// lives here rather than in `paths.rs`.
pub const CC_LOGINS_HOME_ENV: &str = "CC_LOGINS_HOME";

/// `~/.cc-logins`, or [`CC_LOGINS_HOME_ENV`] when set.
///
/// Deliberately outside the app's own data directory: profile folders hold
/// each account's Claude Code history, and uninstalling the app must never
/// take that with it.
pub fn cc_logins_home_dir() -> PathBuf {
    match env_non_empty(CC_LOGINS_HOME_ENV) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir().join(".cc-logins"),
    }
}

/// Claude Code's own default config directory, `~/.claude`. The shim never
/// sets `CLAUDE_CONFIG_DIR` to this path: Claude Code keys its macOS Keychain
/// item on whether the variable is set at all, so setting it to the default
/// would point at a different, empty login.
pub fn default_claude_config_dir() -> PathBuf {
    home_dir().join(".claude")
}
