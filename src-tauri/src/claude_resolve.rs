//! The pure core of locating the Claude Code CLI.
//!
//! Lifted out of `claude_cli.rs` so the `cc-logins-shim` binary can compile
//! this file directly (via `#[path]`) without linking the app library. It
//! depends only on `std` and the sibling `sys_env` module, which both crates
//! declare at their root. See `claude_cli.rs` for why this is more than a
//! `PATH` walk.
//!
//! The shim adds one requirement the app never had: it must never resolve
//! *itself*. [`SearchContext::exclude_dirs`] and
//! [`SearchContext::exclude_files`] let a caller remove its own directory and
//! executable from every stage of the search.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::sys_env::{env_non_empty, home_dir, Platform};

/// Environment variable pointing directly at a `claude` executable, for
/// installations auto-discovery cannot find. Takes precedence over everything.
pub const OVERRIDE_ENV: &str = "CC_LOGINS_CLAUDE_BIN";

/// How many searched directories the failure message lists before eliding the
/// rest. Enough to show the likely ones without producing a banner nobody
/// reads.
pub(crate) const MAX_LISTED_DIRS: usize = 4;

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
pub(crate) fn abbreviate(path: &Path, home: &Path) -> String {
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
    /// Directories never searched, at any stage after the overrides. The
    /// shim puts its own install directory here so a `claude` shim earlier on
    /// `PATH` can never resolve to itself. Compared with [`path_key`], so a
    /// trailing separator or (on Windows) a case difference still matches.
    pub exclude_dirs: Vec<PathBuf>,
    /// Individual executables never returned, for the case where the shim was
    /// started through a path outside `exclude_dirs` (a copy, a symlink
    /// target) but must still skip its own file.
    pub exclude_files: Vec<PathBuf>,
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
            exclude_dirs: Vec::new(),
            exclude_files: Vec::new(),
        }
    }
}

impl SearchContext {
    pub(crate) fn from_env() -> Self {
        Self {
            platform: Platform::detect(),
            home: home_dir(),
            path_var: std::env::var_os("PATH"),
            pathext: env_non_empty("PATHEXT"),
            app_data: env_non_empty("APPDATA").map(PathBuf::from),
            local_app_data: env_non_empty("LOCALAPPDATA").map(PathBuf::from),
            env_override: env_non_empty(OVERRIDE_ENV).map(PathBuf::from),
            settings_override: None,
            exclude_dirs: Vec::new(),
            exclude_files: Vec::new(),
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
pub(crate) enum Base {
    Home,
    Absolute,
    AppData,
    LocalAppData,
}

/// One documented install location. `note` explains which installer puts a
/// binary here, so the table stays auditable against Anthropic's docs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WellKnown {
    base: Base,
    rel: &'static str,
    #[allow(dead_code)]
    note: &'static str,
}

pub(crate) const fn d(base: Base, rel: &'static str, note: &'static str) -> WellKnown {
    WellKnown { base, rel, note }
}

/// Tried on macOS, Linux and WSL alike.
///
/// `~/.local/bin` is deliberately first: it is where the official native
/// installer puts the launcher, and missing it is what produced this module.
pub(crate) const UNIX_COMMON: &[WellKnown] = &[
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

pub(crate) const MACOS_ONLY: &[WellKnown] = &[
    d(
        Base::Absolute,
        "/opt/homebrew/bin",
        "Homebrew, Apple silicon",
    ),
    d(Base::Absolute, "/usr/local/bin", "Homebrew Intel / manual"),
];

pub(crate) const LINUX_ONLY: &[WellKnown] = &[
    d(Base::Absolute, "/usr/bin", "apt / dnf / apk package"),
    d(Base::Absolute, "/usr/local/bin", "manual install"),
    d(
        Base::Absolute,
        "/home/linuxbrew/.linuxbrew/bin",
        "Homebrew on Linux",
    ),
    d(Base::Absolute, "/snap/bin", "snap"),
];

pub(crate) const WINDOWS_ONLY: &[WellKnown] = &[
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
pub(crate) fn well_known_dirs(ctx: &SearchContext) -> Vec<PathBuf> {
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
pub(crate) fn read_dir_names(dir: &Path) -> Vec<String> {
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
pub(crate) fn parse_node_version(name: &str) -> Option<(u64, u64, u64)> {
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
pub(crate) fn nvm_bin_dirs(
    ctx: &SearchContext,
    list: &dyn Fn(&Path) -> Vec<String>,
) -> Vec<PathBuf> {
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
pub(crate) fn exe_candidates(name: &str, platform: Platform, pathext: Option<&str>) -> Vec<String> {
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
pub(crate) fn find_in_dir(
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
pub(crate) const LAUNCHD_MINIMAL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A comparison key for `path` on `platform`: trailing separators dropped,
/// and on Windows `/` folded to `\` and case folded, matching how that
/// filesystem compares names. Never touches the filesystem — canonicalising
/// would turn a Windows path into its `\\?\` form and follow symlinks the
/// caller may have meant literally.
pub(crate) fn path_key(path: &Path, platform: Platform) -> String {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    if platform == Platform::Windows {
        trimmed.replace('/', "\\").to_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// Expand a bare leading `~` component of `path` against `home`.
///
/// Users paste `~/.local/bin/claude` into a text field expecting shell-style
/// expansion; without this, a perfectly good path would be rejected as
/// missing, which is a worse failure than doing nothing — a rejection
/// message claiming a file that exists doesn't exist is actively misleading.
/// Only a bare `~` (or `~/...`) expands: `~user/x` and a bareword like `~x`
/// are left untouched, since resolving another user's home directory is a
/// different, unrelated feature this does not attempt.
pub(crate) fn expand_home(path: &Path, home: &Path) -> PathBuf {
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
    // Exclusions apply to discovery only, never to an explicit override: a
    // user who names a path gets exactly that path.
    let excluded_dirs: Vec<String> = ctx
        .exclude_dirs
        .iter()
        .map(|d| path_key(d, ctx.platform))
        .collect();
    let excluded_files: Vec<String> = ctx
        .exclude_files
        .iter()
        .map(|f| path_key(f, ctx.platform))
        .collect();
    let dir_excluded = |dir: &Path| excluded_dirs.contains(&path_key(dir, ctx.platform));
    let file_excluded = |p: &Path| excluded_files.contains(&path_key(p, ctx.platform));
    let discover_exec = |p: &Path| is_exec(p) && !file_excluded(p);

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
            if dir_excluded(&dir) {
                continue;
            }
            if let Some(found) = find_in_dir(&dir, &candidates, &discover_exec) {
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
        if searched.contains(&dir) || dir_excluded(&dir) {
            continue;
        }
        if let Some(found) = find_in_dir(&dir, &candidates, &discover_exec) {
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
