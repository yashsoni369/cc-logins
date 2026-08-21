//! Locating the Claude Code CLI (`claude`) on the user's machine.
//!
//! # Why this is not just a `PATH` walk
//!
//! A GUI application does not inherit a shell's environment. On macOS an
//! `.app` launched from Finder, the Dock, or Spotlight is started by
//! **launchd**, whose default `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin` —
//! nothing more. The official Claude Code installer puts its launcher at
//! `~/.local/bin/claude`, a directory that is on `PATH` only because the
//! user's shell rc (`~/.zshrc`, `~/.bashrc`, …) puts it there. A shipped
//! build therefore cannot see a perfectly good installation, while
//! `tauri dev` — launched *from a terminal*, inheriting that shell's
//! environment — can. The same gap affects Linux `.desktop` launches and
//! Homebrew on Apple silicon.
//!
//! So `PATH` is necessary but not sufficient. This module layers an explicit
//! override and a table of documented install locations around it.
//!
//! # Why not resolve the login shell's environment
//!
//! The usual fix (VS Code's `shellEnv.ts`, `sindresorhus/fix-path`, and
//! Tauri's own `fix-path-env` crate) spawns `$SHELL -ilc` and scrapes its
//! environment back. That is the right tool when you must resolve an
//! environment for *arbitrary* user tooling. We need exactly one binary,
//! whose install locations Anthropic documents exhaustively, so a static
//! table gets the same coverage with none of the costs: no process spawn, no
//! hang on a slow or interactive rc file (this app can start at login), and
//! no `std::env::set_var`, which is unsound in a process with live threads.
//!
//! Anything the table cannot reach is covered by `CC_LOGINS_CLAUDE_BIN`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::paths::{env_non_empty, home_dir, Platform};

/// Environment variable pointing directly at a `claude` executable, for
/// installations auto-discovery cannot find. Takes precedence over everything.
pub const OVERRIDE_ENV: &str = "CC_LOGINS_CLAUDE_BIN";

/// How many searched directories the failure message lists before eliding the
/// rest. Enough to show the likely ones without producing a banner nobody
/// reads.
const MAX_LISTED_DIRS: usize = 4;

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Which strategy produced the binary. Logged on every successful resolution:
/// on a machine with more than one installation (which Anthropic's own
/// troubleshooting docs treat as a normal situation) this is what tells a bug
/// report *which* `claude` the app actually ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The `CC_LOGINS_CLAUDE_BIN` environment variable.
    EnvOverride,
    /// The persisted `claudeBinaryPath` setting.
    Setting,
    /// A directory on the inherited `PATH`.
    Path,
    /// A documented install location — see [`well_known_dirs`].
    WellKnown,
}

impl Source {
    /// A short, stable label for logs.
    pub fn label(self) -> &'static str {
        match self {
            Source::EnvOverride => "env-override",
            Source::Setting => "setting",
            Source::Path => "path",
            Source::WellKnown => "well-known",
        }
    }
}

/// A located `claude` executable and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

/// Why discovery failed, with enough detail for the user to act on it.
///
/// Carries the directories actually examined rather than a hardcoded list, so
/// the message can never drift from what the code really did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NotFound {
    /// Directories examined, in the order examined: the `PATH` entries first,
    /// then the well-known install locations.
    pub searched: Vec<PathBuf>,
    /// How many leading entries of `searched` came from `PATH`.
    ///
    /// The message summarises those as "on PATH" and names the well-known
    /// locations individually: on the machine that reported this bug `PATH`
    /// held a dozen irrelevant directories, and listing those while eliding
    /// `~/.local/bin` behind "and 9 more" hid the one line a user needs to see.
    pub path_dir_count: usize,
    /// Set when an override was configured but did not point at an executable
    /// file, together with which override it was. Never silently ignored: an
    /// override that quietly degrades to "not installed" is impossible to
    /// debug from the message. One field rather than one per source — a
    /// both-rejected state would be meaningless, since the env override is a
    /// hard stop before the setting is even consulted.
    pub rejected_override: Option<(Source, PathBuf)>,
    /// True when this looks like the macOS GUI-launch case — the single most
    /// confusing part of this bug for whoever hits it, and something only the
    /// backend can detect.
    pub launchd_minimal_path: bool,
}

/// Replace a leading home directory with `~` so the message stays readable
/// and does not print the user's account name back at them.
fn abbreviate(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => path.display().to_string(),
    }
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((source, bad)) = &self.rejected_override {
            return match source {
                Source::Setting => write!(
                    f,
                    "This app's Settings point at {} for the `claude` binary, but there's no \
                     executable file there. Fix or clear the Claude binary path in Settings, \
                     then try again.",
                    bad.display()
                ),
                _ => write!(
                    f,
                    "{OVERRIDE_ENV} is set to {}, but there's no executable file there. \
                     Fix or unset it, then try again.",
                    bad.display()
                ),
            };
        }

        let home = home_dir();
        let well_known = self.searched.iter().skip(self.path_dir_count);
        let shown: Vec<String> = well_known
            .clone()
            .take(MAX_LISTED_DIRS)
            .map(|p| abbreviate(p, &home))
            .collect();

        write!(f, "Couldn't find the `claude` command.")?;
        match (self.path_dir_count > 0, shown.is_empty()) {
            (true, false) => write!(f, " Looked on PATH and in {}", shown.join(", "))?,
            (true, true) => write!(f, " Looked on PATH.")?,
            (false, false) => write!(f, " Looked in {}", shown.join(", "))?,
            (false, true) => {}
        }
        if !shown.is_empty() {
            let rest = well_known.count().saturating_sub(shown.len());
            if rest > 0 {
                write!(f, " (and {rest} more)")?;
            }
            write!(f, ".")?;
        }
        if self.launchd_minimal_path {
            write!(
                f,
                " Apps opened from the Dock don't see PATH changes made in your \
                 shell's startup files."
            )?;
        }
        write!(
            f,
            " If Claude Code is installed somewhere else, set its full path in this \
             app's Settings, or set {OVERRIDE_ENV} to the full path of the binary. If \
             it isn't installed, install it and try again."
        )
    }
}

// ---------------------------------------------------------------------------
// Search context — every machine-specific input, injectable
// ---------------------------------------------------------------------------

/// Everything about *this* machine that the resolver reads.
///
/// Built from the real environment by [`SearchContext::from_env`], or
/// literal-by-literal in tests. This exists because [`Platform::detect`] is
/// pinned at compile time: without injecting the platform, the Windows and
/// Linux tables would be unreachable dead code on a macOS CI runner.
#[derive(Debug, Clone)]
pub(crate) struct SearchContext {
    pub platform: Platform,
    pub home: PathBuf,
    pub path_var: Option<OsString>,
    /// Only consulted when `platform` is [`Platform::Windows`].
    pub pathext: Option<String>,
    pub app_data: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub env_override: Option<PathBuf>,
    /// The persisted `claudeBinaryPath` setting, if any. Always `None` from
    /// [`SearchContext::from_env`] — this module never reads the settings
    /// store itself; [`resolve`] injects it from its caller so `resolve_in`
    /// stays pure and this module stays dependency-free of `crate::settings`.
    pub settings_override: Option<PathBuf>,
}

/// `Platform::Unknown` and an empty home: a base for tests to override
/// field-by-field, never a description of a real machine.
impl Default for SearchContext {
    fn default() -> Self {
        Self {
            platform: Platform::Unknown,
            home: PathBuf::new(),
            path_var: None,
            pathext: None,
            app_data: None,
            local_app_data: None,
            env_override: None,
            settings_override: None,
        }
    }
}

impl SearchContext {
    fn from_env() -> Self {
        Self {
            platform: Platform::detect(),
            home: home_dir(),
            path_var: std::env::var_os("PATH"),
            pathext: env_non_empty("PATHEXT"),
            app_data: env_non_empty("APPDATA").map(PathBuf::from),
            local_app_data: env_non_empty("LOCALAPPDATA").map(PathBuf::from),
            env_override: env_non_empty(OVERRIDE_ENV).map(PathBuf::from),
            settings_override: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Well-known install locations
// ---------------------------------------------------------------------------

/// What a [`WellKnown`] entry's relative path hangs off. Kept as an enum so
/// the tables stay plain constants and every machine-specific base comes from
/// [`SearchContext`] rather than a second environment lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Base {
    Home,
    Absolute,
    AppData,
    LocalAppData,
}

/// One documented install location. `note` explains which installer puts a
/// binary here, so the table stays auditable against Anthropic's docs.
#[derive(Debug, Clone, Copy)]
struct WellKnown {
    base: Base,
    rel: &'static str,
    #[allow(dead_code)]
    note: &'static str,
}

const fn d(base: Base, rel: &'static str, note: &'static str) -> WellKnown {
    WellKnown { base, rel, note }
}

/// Tried on macOS, Linux and WSL alike.
///
/// `~/.local/bin` is deliberately first: it is where the official native
/// installer puts the launcher, and missing it is what produced this module.
const UNIX_COMMON: &[WellKnown] = &[
    d(Base::Home, ".local/bin", "official native installer"),
    d(Base::Home, ".claude/local", "legacy local install"),
    d(
        Base::Home,
        ".claude/local/node_modules/.bin",
        "legacy local install's npm bin",
    ),
    d(Base::Home, ".npm-global/bin", "npm prefix override"),
    d(Base::Home, ".local/share/pnpm", "pnpm global"),
    d(Base::Home, ".bun/bin", "bun global"),
    d(Base::Home, ".volta/bin", "volta shims"),
];

const MACOS_ONLY: &[WellKnown] = &[
    d(
        Base::Absolute,
        "/opt/homebrew/bin",
        "Homebrew, Apple silicon",
    ),
    d(Base::Absolute, "/usr/local/bin", "Homebrew Intel / manual"),
];

const LINUX_ONLY: &[WellKnown] = &[
    d(Base::Absolute, "/usr/bin", "apt / dnf / apk package"),
    d(Base::Absolute, "/usr/local/bin", "manual install"),
    d(
        Base::Absolute,
        "/home/linuxbrew/.linuxbrew/bin",
        "Homebrew on Linux",
    ),
    d(Base::Absolute, "/snap/bin", "snap"),
];

const WINDOWS_ONLY: &[WellKnown] = &[
    d(Base::Home, ".local\\bin", "official native installer"),
    d(
        Base::LocalAppData,
        "Microsoft\\WinGet\\Links",
        "winget shim directory",
    ),
    d(Base::AppData, "npm", "npm -g shims"),
    d(Base::LocalAppData, "Volta\\bin", "volta shims"),
    d(Base::LocalAppData, "pnpm", "pnpm global"),
    d(Base::Home, ".bun\\bin", "bun global"),
    d(Base::Home, ".claude\\local", "legacy local install"),
];

/// Resolve the well-known directories for `ctx`, in search order.
///
/// Pure: no I/O and no environment reads — everything comes from `ctx`.
/// Entries whose base is unset on this machine (e.g. `%APPDATA%` on unix) are
/// skipped rather than guessed at.
fn well_known_dirs(ctx: &SearchContext) -> Vec<PathBuf> {
    let table: Vec<&WellKnown> = match ctx.platform {
        Platform::Macos => UNIX_COMMON.iter().chain(MACOS_ONLY).collect(),
        Platform::Linux | Platform::Wsl => UNIX_COMMON.iter().chain(LINUX_ONLY).collect(),
        Platform::Windows => WINDOWS_ONLY.iter().collect(),
        Platform::Unknown => UNIX_COMMON.iter().collect(),
    };

    let mut out: Vec<PathBuf> = table
        .into_iter()
        .filter_map(|e| {
            let base = match e.base {
                Base::Home => Some(ctx.home.clone()),
                Base::Absolute => return Some(PathBuf::from(e.rel)),
                Base::AppData => ctx.app_data.clone(),
                Base::LocalAppData => ctx.local_app_data.clone(),
            }?;
            Some(base.join(e.rel))
        })
        .collect();

    // npm `-g` under a Node version manager is one of the documented install
    // sources, but nvm's bin directory is versioned, so it cannot be a
    // constant. Appended last so a stale Node never outranks a real install.
    out.extend(nvm_bin_dirs(ctx, &read_dir_names));
    out
}

/// Directory entry names of `dir`, or empty when it cannot be read. The only
/// directory listing this module performs.
fn read_dir_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `vX.Y.Z` nvm directory name into comparable numbers. `None` for
/// anything that is not a version directory (`alias`, stray files).
fn parse_node_version(name: &str) -> Option<(u64, u64, u64)> {
    let mut parts = name.strip_prefix('v')?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// nvm's global bin directories, newest Node first.
///
/// `list` is injected so this is unit-testable without an nvm installation.
/// Newest-first matters: an old Node left behind by a version manager must not
/// shadow the install the user actually uses.
fn nvm_bin_dirs(ctx: &SearchContext, list: &dyn Fn(&Path) -> Vec<String>) -> Vec<PathBuf> {
    if ctx.platform == Platform::Windows {
        return Vec::new();
    }
    let root = ctx.home.join(".nvm/versions/node");
    let mut versions: Vec<(u64, u64, u64, String)> = list(&root)
        .into_iter()
        .filter_map(|name| parse_node_version(&name).map(|(a, b, c)| (a, b, c, name)))
        .collect();
    versions.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    versions
        .into_iter()
        .map(|(_, _, _, name)| root.join(name).join("bin"))
        .collect()
}

// ---------------------------------------------------------------------------
// Executable probing
// ---------------------------------------------------------------------------

/// A regular file the current user could actually exec. On unix a
/// non-executable `claude` earlier in the search order must not shadow the
/// real one.
#[cfg(unix)]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows has no executable bit — extension matching (`PATHEXT`) already
/// does this job in [`exe_candidates`].
#[cfg(not(unix))]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Filenames to try for `name`, honoring `PATHEXT` when `platform` is Windows
/// (so `claude` resolves to an npm-shimmed `claude.cmd` or the native
/// installer's `claude.exe`, not just a literal extension-less `claude`,
/// which rarely exists on Windows).
///
/// Takes the platform and `PATHEXT` rather than reading them, so the Windows
/// behavior is unit-testable from any host — a `#[cfg(windows)]` version
/// never could be.
fn exe_candidates(name: &str, platform: Platform, pathext: Option<&str>) -> Vec<String> {
    if platform != Platform::Windows {
        return vec![name.to_string()];
    }
    if Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let pathext = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
    let mut out = vec![name.to_string()];
    for ext in pathext.split(';') {
        if ext.is_empty() {
            continue;
        }
        out.push(format!("{name}{}", ext.to_ascii_lowercase()));
    }
    out
}

/// First executable named `name` inside `dir`, if any.
fn find_in_dir(
    dir: &Path,
    candidates: &[String],
    is_exec: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|c| dir.join(c))
        .find(|full| is_exec(full))
}

/// Locate an executable named `name` on `PATH`, honoring `PATHEXT` on Windows.
///
/// `PATH`-only, with no fallbacks — `login.rs` uses it for its Linux terminal
/// emulator search, which genuinely wants `PATH` semantics and nothing else.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let ctx = SearchContext::from_env();
    let candidates = exe_candidates(name, ctx.platform, ctx.pathext.as_deref());
    let path_var = ctx.path_var?;
    std::env::split_paths(&path_var)
        .find_map(|dir| find_in_dir(&dir, &candidates, &is_executable_file))
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// launchd's default `PATH` for a GUI-launched process. Recognizing it lets
/// the failure message explain the actual cause instead of leaving the user
/// to wonder why a working `claude` is invisible.
const LAUNCHD_MINIMAL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Expand a bare leading `~` component of `path` against `home`.
///
/// Users paste `~/.local/bin/claude` into a text field expecting shell-style
/// expansion; without this, a perfectly good path would be rejected as
/// missing, which is a worse failure than doing nothing — a rejection
/// message claiming a file that exists doesn't exist is actively misleading.
/// Only a bare `~` (or `~/...`) expands: `~user/x` and a bareword like `~x`
/// are left untouched, since resolving another user's home directory is a
/// different, unrelated feature this does not attempt.
fn expand_home(path: &Path, home: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == "~" => {
            home.join(components.as_path())
        }
        _ => path.to_path_buf(),
    }
}

/// The whole resolution chain, pure apart from the injected `is_exec` probe.
///
/// `is_exec` is injected for the same reason `find_linux_terminal` takes an
/// `exists` predicate: the selection logic must be testable without depending
/// on what happens to be installed on the machine running the tests.
///
/// Order: override, then `PATH`, then well-known locations. `PATH` stays ahead
/// of the table so a deliberately-configured environment always wins and
/// today's behavior is unchanged wherever it already worked.
pub(crate) fn resolve_in(
    ctx: &SearchContext,
    is_exec: &dyn Fn(&Path) -> bool,
) -> Result<Resolved, NotFound> {
    // 1. Explicit overrides, most specific first. A configured-but-unusable
    //    override is a hard stop, never a fallthrough: the user (or this
    //    app's Settings) named this path specifically, and silently searching
    //    elsewhere would hide a typo. The env var is checked first and stops
    //    the search on rejection before the setting is even consulted — the
    //    most-specific configured thing wins and fails loudly.
    for (source, configured) in [
        (Source::EnvOverride, &ctx.env_override),
        (Source::Setting, &ctx.settings_override),
    ] {
        if let Some(override_path) = configured {
            let expanded = expand_home(override_path, &ctx.home);
            if is_exec(&expanded) {
                return Ok(Resolved {
                    path: expanded,
                    source,
                });
            }
            return Err(NotFound {
                // Verbatim typed path, not the tilde-expanded form: the user
                // should see back what they wrote.
                rejected_override: Some((source, override_path.clone())),
                ..NotFound::default()
            });
        }
    }

    let candidates = exe_candidates("claude", ctx.platform, ctx.pathext.as_deref());
    let mut searched = Vec::new();

    // 2. PATH, exactly as before.
    if let Some(path_var) = &ctx.path_var {
        for dir in std::env::split_paths(path_var) {
            if let Some(found) = find_in_dir(&dir, &candidates, is_exec) {
                return Ok(Resolved {
                    path: found,
                    source: Source::Path,
                });
            }
            searched.push(dir);
        }
    }

    // Everything appended from here on is a well-known location, not a PATH
    // entry — the boundary the failure message splits on.
    let path_dir_count = searched.len();

    // 3. Documented install locations, skipping any already covered by PATH.
    for dir in well_known_dirs(ctx) {
        if searched.contains(&dir) {
            continue;
        }
        if let Some(found) = find_in_dir(&dir, &candidates, is_exec) {
            return Ok(Resolved {
                path: found,
                source: Source::WellKnown,
            });
        }
        searched.push(dir);
    }

    let launchd_minimal_path = ctx.platform == Platform::Macos
        && ctx
            .path_var
            .as_ref()
            .is_some_and(|p| p == LAUNCHD_MINIMAL_PATH);

    Err(NotFound {
        searched,
        path_dir_count,
        rejected_override: None,
        launchd_minimal_path,
    })
}

/// Locate the Claude Code CLI on this machine.
///
/// `settings_override` is the persisted `claudeBinaryPath` setting, injected
/// by the caller — this module never reads the settings store itself. There
/// is deliberately no zero-argument variant: a shorter name would let a
/// future call site reach for it and silently ignore the setting.
pub fn resolve(settings_override: Option<PathBuf>) -> Result<Resolved, NotFound> {
    let mut ctx = SearchContext::from_env();
    ctx.settings_override = settings_override;
    resolve_in(&ctx, &is_executable_file)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- resolve_in: precedence -----------------------------------------------
    //
    // These tests build a `SearchContext` literally and fake `is_exec` with a
    // closure, so they touch neither the real environment nor the real
    // filesystem. That is the whole point of injecting both.

    #[test]
    fn override_beats_path_even_when_path_also_has_a_claude() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(OsString::from("/on/path")),
            env_override: Some(PathBuf::from("/override/claude")),
            ..SearchContext::default()
        };
        // Both the override and a PATH entry resolve to something
        // executable; the override must win regardless.
        let is_exec =
            |p: &Path| p == Path::new("/override/claude") || p == Path::new("/on/path/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: PathBuf::from("/override/claude"),
                source: Source::EnvOverride,
            })
        );
    }

    #[test]
    fn path_beats_well_known() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(OsString::from("/on/path")),
            ..SearchContext::default()
        };
        // A well-known directory (~/.local/bin) also has a hit, but PATH is
        // searched first and must be the one that wins.
        let is_exec = |p: &Path| {
            p == Path::new("/on/path/claude") || p == Path::new("/home/u/.local/bin/claude")
        };

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: PathBuf::from("/on/path/claude"),
                source: Source::Path,
            })
        );
    }

    #[test]
    fn rejected_override_is_a_hard_stop_not_a_fallthrough() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(OsString::from("/on/path")),
            env_override: Some(PathBuf::from("/bad/claude")),
            ..SearchContext::default()
        };
        // A real `claude` sits on PATH, but the override never matches it —
        // a configured-and-broken override must fail loudly rather than
        // silently searching elsewhere, or a typo would be undebuggable.
        let is_exec = |p: &Path| p == Path::new("/on/path/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Err(NotFound {
                rejected_override: Some((Source::EnvOverride, PathBuf::from("/bad/claude"))),
                ..NotFound::default()
            })
        );
    }

    #[test]
    fn setting_override_is_used_when_env_override_is_absent() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(OsString::from("/on/path")),
            settings_override: Some(PathBuf::from("/from/setting/claude")),
            ..SearchContext::default()
        };
        // A real `claude` sits on PATH too, but the setting is more specific
        // and must win.
        let is_exec =
            |p: &Path| p == Path::new("/from/setting/claude") || p == Path::new("/on/path/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: PathBuf::from("/from/setting/claude"),
                source: Source::Setting,
            })
        );
    }

    #[test]
    fn env_override_beats_setting_override_even_when_both_are_executable() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            env_override: Some(PathBuf::from("/from/env/claude")),
            settings_override: Some(PathBuf::from("/from/setting/claude")),
            ..SearchContext::default()
        };
        let is_exec =
            |p: &Path| p == Path::new("/from/env/claude") || p == Path::new("/from/setting/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: PathBuf::from("/from/env/claude"),
                source: Source::EnvOverride,
            })
        );
    }

    #[test]
    fn rejected_setting_is_a_hard_stop_not_a_fallthrough() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(OsString::from("/on/path")),
            settings_override: Some(PathBuf::from("/bad/setting/claude")),
            ..SearchContext::default()
        };
        // A real `claude` sits on PATH, but a configured-and-broken setting
        // must fail loudly rather than silently falling through to PATH.
        let is_exec = |p: &Path| p == Path::new("/on/path/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Err(NotFound {
                rejected_override: Some((Source::Setting, PathBuf::from("/bad/setting/claude"))),
                ..NotFound::default()
            })
        );
    }

    #[test]
    fn env_override_rejection_wins_before_the_setting_is_consulted() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            env_override: Some(PathBuf::from("/bad/env/claude")),
            settings_override: Some(PathBuf::from("/good/setting/claude")),
            ..SearchContext::default()
        };
        // The setting would resolve fine on its own, but the env override is
        // checked first and hard-stops on rejection before the setting is
        // ever looked at.
        let is_exec = |p: &Path| p == Path::new("/good/setting/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Err(NotFound {
                rejected_override: Some((Source::EnvOverride, PathBuf::from("/bad/env/claude"))),
                ..NotFound::default()
            })
        );
    }

    #[test]
    fn tilde_in_an_override_expands_against_the_context_home() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            settings_override: Some(PathBuf::from("~/.local/bin/claude")),
            ..SearchContext::default()
        };
        let expanded = PathBuf::from("/home/u/.local/bin/claude");
        let is_exec = |p: &Path| p == expanded;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: expanded,
                source: Source::Setting,
            })
        );
    }

    #[test]
    fn macos_launchd_minimal_path_still_finds_the_native_installer_location() {
        // The regression test for the bug this module exists to fix: a
        // GUI-launched macOS process sees only launchd's bare PATH, but
        // ~/.local/bin/claude — where the official installer puts the
        // binary — must still be found via the well-known table.
        let ctx = SearchContext {
            platform: Platform::Macos,
            home: PathBuf::from("/Users/tester"),
            path_var: Some(OsString::from("/usr/bin:/bin:/usr/sbin:/sbin")),
            ..SearchContext::default()
        };
        let planted = PathBuf::from("/Users/tester/.local/bin/claude");
        let is_exec = |p: &Path| p == planted;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: planted,
                source: Source::WellKnown,
            })
        );
    }

    #[test]
    fn local_bin_wins_over_homebrew_when_both_have_a_claude() {
        // Table order matters: ~/.local/bin is the official installer's
        // location and is listed ahead of Homebrew's directory on purpose.
        let ctx = SearchContext {
            platform: Platform::Macos,
            home: PathBuf::from("/Users/tester"),
            ..SearchContext::default()
        };
        let local_bin = PathBuf::from("/Users/tester/.local/bin/claude");
        let homebrew = PathBuf::from("/opt/homebrew/bin/claude");
        let is_exec = |p: &Path| p == local_bin || p == homebrew;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: local_bin,
                source: Source::WellKnown,
            })
        );
    }

    // -- resolve_in: Windows table ---------------------------------------------
    //
    // `platform` is a `SearchContext` field precisely so these can run (and
    // pass) on the macOS machine building this crate — a `#[cfg(windows)]`
    // version of these tests would never execute in CI at all.

    fn windows_ctx() -> SearchContext {
        SearchContext {
            platform: Platform::Windows,
            home: PathBuf::from("C:\\Users\\tester"),
            app_data: Some(PathBuf::from("C:\\Users\\tester\\AppData\\Roaming")),
            local_app_data: Some(PathBuf::from("C:\\Users\\tester\\AppData\\Local")),
            ..SearchContext::default()
        }
    }

    #[test]
    fn windows_resolves_native_installer_under_userprofile() {
        let ctx = windows_ctx();
        let expected = ctx.home.join(".local\\bin").join("claude.exe");
        let is_exec = |p: &Path| p == expected;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: expected,
                source: Source::WellKnown,
            })
        );
    }

    #[test]
    fn windows_resolves_winget_shim_under_local_app_data() {
        let ctx = windows_ctx();
        let expected = ctx
            .local_app_data
            .clone()
            .unwrap()
            .join("Microsoft\\WinGet\\Links")
            .join("claude.exe");
        let is_exec = |p: &Path| p == expected;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: expected,
                source: Source::WellKnown,
            })
        );
    }

    #[test]
    fn windows_resolves_npm_global_shim_under_app_data() {
        let ctx = windows_ctx();
        let expected = ctx.app_data.clone().unwrap().join("npm").join("claude.cmd");
        let is_exec = |p: &Path| p == expected;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: expected,
                source: Source::WellKnown,
            })
        );
    }

    #[test]
    fn wsl_uses_the_linux_table_not_the_macos_one() {
        // /usr/bin is on the Linux/WSL table but not the macOS one — proves
        // Wsl selects LINUX_ONLY rather than accidentally falling into
        // MACOS_ONLY or missing a table entirely.
        let ctx = SearchContext {
            platform: Platform::Wsl,
            home: PathBuf::from("/home/tester"),
            ..SearchContext::default()
        };
        let expected = PathBuf::from("/usr/bin/claude");
        let is_exec = |p: &Path| p == expected;

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: expected,
                source: Source::WellKnown,
            })
        );
    }

    // -- exe_candidates ---------------------------------------------------------

    #[test]
    fn exe_candidates_windows_uses_injected_pathext() {
        assert_eq!(
            exe_candidates("claude", Platform::Windows, Some(".XYZ;.ABC")),
            vec!["claude", "claude.xyz", "claude.abc"]
        );
    }

    #[test]
    fn exe_candidates_windows_falls_back_to_documented_default_when_pathext_absent() {
        assert_eq!(
            exe_candidates("claude", Platform::Windows, None),
            vec![
                "claude",
                "claude.com",
                "claude.exe",
                "claude.bat",
                "claude.cmd"
            ]
        );
    }

    #[test]
    fn exe_candidates_does_not_expand_a_name_that_already_has_an_extension() {
        // Otherwise a caller passing "claude.exe" explicitly would end up
        // probing "claude.exe.exe" and friends.
        assert_eq!(
            exe_candidates("claude.exe", Platform::Windows, Some(".COM;.EXE")),
            vec!["claude.exe"]
        );
    }

    #[test]
    fn exe_candidates_non_windows_platforms_return_just_the_bare_name() {
        for platform in [
            Platform::Macos,
            Platform::Linux,
            Platform::Wsl,
            Platform::Unknown,
        ] {
            assert_eq!(
                exe_candidates("claude", platform, Some(".EXE")),
                vec!["claude"]
            );
        }
    }

    // -- nvm_bin_dirs -------------------------------------------------------------

    #[test]
    fn nvm_bin_dirs_orders_newest_version_first() {
        // A stale Node left behind by nvm must never shadow the real
        // install, so the newest version has to sort first.
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/t"),
            ..SearchContext::default()
        };
        let list = |_: &Path| vec!["v18.19.0".to_string(), "v20.11.0".to_string()];

        assert_eq!(
            nvm_bin_dirs(&ctx, &list),
            vec![
                ctx.home.join(".nvm/versions/node/v20.11.0/bin"),
                ctx.home.join(".nvm/versions/node/v18.19.0/bin"),
            ]
        );
    }

    #[test]
    fn nvm_bin_dirs_ignores_non_version_entries() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/t"),
            ..SearchContext::default()
        };
        let list = |_: &Path| vec!["alias".to_string(), "v18.19.0".to_string()];

        assert_eq!(
            nvm_bin_dirs(&ctx, &list),
            vec![ctx.home.join(".nvm/versions/node/v18.19.0/bin")]
        );
    }

    #[test]
    fn nvm_bin_dirs_empty_listing_is_empty() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/t"),
            ..SearchContext::default()
        };
        let list = |_: &Path| Vec::new();

        assert!(nvm_bin_dirs(&ctx, &list).is_empty());
    }

    #[test]
    fn nvm_bin_dirs_windows_is_always_empty() {
        // nvm-windows lays out its directories differently; this module
        // does not attempt to model it, so Windows must short-circuit
        // regardless of what the lister would have returned.
        let ctx = SearchContext {
            platform: Platform::Windows,
            home: PathBuf::from("C:\\Users\\t"),
            ..SearchContext::default()
        };
        let list = |_: &Path| vec!["v20.11.0".to_string()];

        assert!(nvm_bin_dirs(&ctx, &list).is_empty());
    }

    // -- parse_node_version -------------------------------------------------------

    #[test]
    fn parse_node_version_parses_full_and_partial_versions() {
        assert_eq!(parse_node_version("v20.11.0"), Some((20, 11, 0)));
        assert_eq!(parse_node_version("v18"), Some((18, 0, 0)));
    }

    #[test]
    fn parse_node_version_rejects_non_version_names() {
        assert_eq!(parse_node_version("alias"), None);
        assert_eq!(parse_node_version("20.11.0"), None); // missing the `v` prefix
    }

    // -- Display for NotFound -----------------------------------------------------

    #[test]
    fn not_found_display_names_the_override_env_var() {
        let err = NotFound::default();
        let msg = err.to_string();
        assert!(msg.contains("CC_LOGINS_CLAUDE_BIN"));
        assert!(msg.starts_with("Couldn't find the `claude` command."));
    }

    #[test]
    fn not_found_display_rejected_override_has_distinctly_different_wording() {
        let err = NotFound {
            rejected_override: Some((Source::EnvOverride, PathBuf::from("/bad/claude"))),
            ..NotFound::default()
        };
        let msg = err.to_string();
        // Different situation, different message: this is not "not
        // installed", it's "you told me exactly where and it's wrong".
        assert!(msg.contains("CC_LOGINS_CLAUDE_BIN is set to"));
        assert!(msg.contains("no executable file there"));
        assert!(!msg.contains("Couldn't find the `claude` command."));
    }

    #[test]
    fn not_found_display_rejected_setting_points_at_the_settings_screen() {
        let err = NotFound {
            rejected_override: Some((Source::Setting, PathBuf::from("/bad/claude"))),
            ..NotFound::default()
        };
        let msg = err.to_string();
        assert!(msg.contains("Settings"));
        assert!(msg.contains("no executable file there"));
        // Distinct from the env-var wording — a Settings-configured path
        // must never be told to "unset a variable".
        assert!(!msg.contains("is set to"));
    }

    #[test]
    fn not_found_display_generic_message_names_settings_before_the_env_var() {
        // Settings is the fix that works from the Dock; naming it first
        // matters for whoever this message is actually written for.
        let msg = NotFound::default().to_string();
        let settings_pos = msg.find("Settings").expect("mentions Settings");
        let env_pos = msg.find(OVERRIDE_ENV).expect("mentions the env var");
        assert!(settings_pos < env_pos, "got {msg}");
    }

    #[test]
    fn not_found_display_includes_launchd_sentence_only_when_flagged() {
        let with_flag = NotFound {
            launchd_minimal_path: true,
            ..NotFound::default()
        };
        let without_flag = NotFound::default();

        assert!(with_flag.to_string().contains("don't see PATH changes"));
        assert!(!without_flag.to_string().contains("don't see PATH changes"));
    }

    #[test]
    fn not_found_display_abbreviates_home_directory_to_tilde() {
        // Env-touching: `Display for NotFound` calls the real `home_dir()`
        // (it has no `SearchContext` to read from), so the home it
        // abbreviates against must be controlled via the real env var.
        // `env_lock()` is declared first so it drops last, after the
        // `EnvGuard` below has restored HOME — see test_support's doc.
        let _lock = crate::test_support::env_lock();
        let home = tempfile::TempDir::new().unwrap();
        let _home_guard = set_home(home.path());

        let err = NotFound {
            searched: vec![home.path().join(".local").join("bin")],
            ..NotFound::default()
        };
        let msg = err.to_string();

        assert!(msg.contains(&format!(
            "~{}.local{}bin",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        )));
    }

    /// Point the platform-appropriate "home" env var at `dir`, mirroring the
    /// helper in `paths.rs`'s own test module.
    #[cfg(windows)]
    fn set_home(dir: &Path) -> crate::test_support::EnvGuard {
        crate::test_support::EnvGuard::set("USERPROFILE", dir.to_str().expect("utf8 temp path"))
    }

    #[cfg(not(windows))]
    fn set_home(dir: &Path) -> crate::test_support::EnvGuard {
        crate::test_support::EnvGuard::set("HOME", dir.to_str().expect("utf8 temp path"))
    }

    // -- find_on_path: reads the real PATH ---------------------------------------
    //
    // Ported from the pre-refactor `login.rs` (where `claude_binary()` was a
    // thin wrapper over this same function). These are the one place in this
    // module's suite that legitimately touches the real environment, since
    // `find_on_path` itself reads real `PATH` rather than a `SearchContext`.

    /// Write an executable (unix: with `mode`) named `name` inside `dir`.
    fn plant_binary(dir: &Path, name: &str, _mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(_mode)).unwrap();
        }
        path
    }

    #[test]
    fn find_on_path_locates_a_planted_executable() {
        let _lock = crate::test_support::env_lock();
        let dir = tempfile::TempDir::new().unwrap();
        #[cfg(windows)]
        let file_name = "claude.cmd";
        #[cfg(not(windows))]
        let file_name = "claude";
        let planted = plant_binary(dir.path(), file_name, 0o755);

        let _path_guard = crate::test_support::EnvGuard::set(
            "PATH",
            dir.path().to_str().expect("utf8 temp path"),
        );

        assert_eq!(find_on_path("claude"), Some(planted));
    }

    #[cfg(unix)]
    #[test]
    fn find_on_path_skips_a_non_executable_file() {
        let _lock = crate::test_support::env_lock();
        let dir = tempfile::TempDir::new().unwrap();
        // A readable but non-executable `claude` must not shadow the real
        // one — spawning it would fail opaquely with EACCES.
        plant_binary(dir.path(), "claude", 0o644);

        let _path_guard = crate::test_support::EnvGuard::set(
            "PATH",
            dir.path().to_str().expect("utf8 temp path"),
        );

        assert_eq!(find_on_path("claude"), None);
    }

    #[cfg(unix)]
    #[test]
    fn find_on_path_skips_non_executable_and_finds_the_later_real_one() {
        let _lock = crate::test_support::env_lock();
        let shadow = tempfile::TempDir::new().unwrap();
        let real = tempfile::TempDir::new().unwrap();
        plant_binary(shadow.path(), "claude", 0o644);
        let planted = plant_binary(real.path(), "claude", 0o755);

        let joined = std::env::join_paths([shadow.path(), real.path()]).unwrap();
        let _path_guard =
            crate::test_support::EnvGuard::set("PATH", joined.to_str().expect("utf8 temp path"));

        assert_eq!(find_on_path("claude"), Some(planted));
    }

    #[test]
    fn find_on_path_none_when_absent() {
        let _lock = crate::test_support::env_lock();
        let dir = tempfile::TempDir::new().unwrap();
        // Empty directory: nothing named `claude*` in it.
        let _path_guard = crate::test_support::EnvGuard::set(
            "PATH",
            dir.path().to_str().expect("utf8 temp path"),
        );

        assert_eq!(find_on_path("claude"), None);
    }
    /// PATH is summarised, well-known locations are named. On the machine that
    /// reported this bug PATH held a dozen unrelated directories; listing those
    /// while hiding `~/.local/bin` behind "and N more" buried the only line
    /// that would have told the user what to do.
    #[test]
    fn display_summarises_path_but_names_the_well_known_dirs() {
        let not_found = NotFound {
            searched: vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ],
            path_dir_count: 2,
            rejected_override: None,
            launchd_minimal_path: false,
        };
        let msg = not_found.to_string();
        assert!(msg.contains("Looked on PATH and in"), "got {msg}");
        assert!(msg.contains("/opt/homebrew/bin"), "got {msg}");
        // The PATH entries are covered by the summary, not enumerated.
        assert!(!msg.contains("/usr/bin,"), "got {msg}");
    }

    /// Nothing but PATH was searched (every well-known dir was already on it),
    /// so there is no list to print and the sentence must still read cleanly.
    #[test]
    fn display_handles_path_only_search_without_a_dangling_list() {
        let not_found = NotFound {
            searched: vec![PathBuf::from("/usr/bin")],
            path_dir_count: 1,
            rejected_override: None,
            launchd_minimal_path: false,
        };
        let msg = not_found.to_string();
        assert!(msg.contains("Looked on PATH."), "got {msg}");
        assert!(!msg.contains("and 0 more"), "got {msg}");
    }
}
