//! Isolated interactive login: opens a *visible* terminal running
//! `claude auth login`, captures the credential it produces, and hands it
//! back — all without ever touching the caller's own active Claude Code
//! login.
//!
//! # Why this is safe
//!
//! `claude auth login` writes to `(CLAUDE_CONFIG_DIR || ~/.claude)/.credentials.json`.
//! Run naively that would overwrite whatever login this process/user is
//! currently signed in as. So the child terminal is always launched with
//! `CLAUDE_CONFIG_DIR` pointed at a fresh [`tempfile::TempDir`] created here:
//! the new credential lands in `<temp>/.credentials.json`, the real
//! `~/.claude` is never opened for writing, and the temp directory (which
//! briefly holds a real, usable credential) is removed once this function
//! returns, on every path — success, failure, timeout, or cancellation —
//! because it lives in a local variable in [`interactive_login`] and is
//! dropped (deleted) when that function's scope ends.
//!
//! This module never resolves, reads, or writes [`crate::paths::credentials_path`],
//! [`crate::paths::global_config_path`], or [`crate::paths::backup_root`] — the
//! real stores those functions point at. Every filesystem access here is
//! scoped to the [`tempfile::TempDir`] this function creates for itself.
//!
//! # Cleanup on every path
//!
//! The release profile unwinds rather than aborts (see `Cargo.toml`), so local
//! variables — including [`tempfile::TempDir`] — drop during a panic as well as
//! on every ordinary path: `?`, early `return`, and the end of the function.
//! That is deliberate: it was `panic = "abort"` at one point, which runs no
//! destructors at all, and this is the one temp directory in the crate that
//! briefly holds a live credential.
//!
//! [`sweep_stale_login_dirs`] remains as a backstop for the cases unwinding
//! still cannot cover — a hard kill, a power loss — and runs at startup.
//!
//! # Never logged
//!
//! No line in this module ever formats a credential body, an access token, or
//! a refresh token into a `log::` call, an error message, or a panic message.
//! Only the temp path and coarse outcome kinds are logged.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::claude_cli::{self, NotFound};
use crate::paths::Platform;

/// How often the poll loop checks for the credential file / terminal exit.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A browser OAuth round trip can be slow (SSO, MFA, a user who steps away).
/// Ten minutes matches the ceiling documented for the design this module
/// implements.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A "tracking" child that exits faster than this never really hosted the
/// window (unsupported flag, or an undocumented hand-off), so its exit is not
/// a cancellation: fall back to polling for the credential instead.
const MIN_WINDOW_LIFETIME: Duration = Duration::from_secs(3);

/// A Linux terminal emulator, its documented "run this argv" flags, and
/// whether the process we spawn actually lives as long as the window.
#[derive(Debug, Clone, Copy)]
struct LinuxTerminal {
    /// Binary name looked up on `PATH`.
    bin: &'static str,
    /// Options ending in the flag that takes `program args...` as the
    /// remainder of the command line.
    prefix: &'static [&'static str],
    /// `true` only when the spawned process *is* the window, so its exit
    /// means "the user closed it". `false` when it forks or hands off.
    tracks_window_lifetime: bool,
}

/// Linux terminal emulators to try, in preference order. The first one found
/// on `PATH` is used; if none is, the caller gets [`LoginError::NoTerminalAvailable`]
/// so the UI can fall back to the paste-a-token path instead of failing
/// obscurely.
const LINUX_TERMINALS: &[LinuxTerminal] = &[
    // `--wait` blocks until the terminal's child exits (it otherwise hands
    // off to gnome-terminal-server); `--` replaces the deprecated `-e`.
    LinuxTerminal {
        bin: "gnome-terminal",
        prefix: &["--wait", "--"],
        tracks_window_lifetime: true,
    },
    // Konsole forks by default; `--nofork`/`--separate` keeps it in the
    // foreground, and `-e` catches every following argument.
    LinuxTerminal {
        bin: "konsole",
        prefix: &["--nofork", "-e"],
        tracks_window_lifetime: true,
    },
    // `-e` here takes a single command *string*; `-x` takes the remainder as
    // an argv. `--disable-server` stops it attaching to an existing instance.
    LinuxTerminal {
        bin: "xfce4-terminal",
        prefix: &["--disable-server", "-x"],
        tracks_window_lifetime: true,
    },
    // `-e` is program + argv and must be last; the xterm process is the window.
    LinuxTerminal {
        bin: "xterm",
        prefix: &["-e"],
        tracks_window_lifetime: true,
    },
    // Last resort: an alternatives symlink to an unknown terminal. Only the
    // Debian Policy `-e command [args]` form is safe, and its exit is not.
    LinuxTerminal {
        bin: "x-terminal-emulator",
        prefix: &["-e"],
        tracks_window_lifetime: false,
    },
];

/// Shape used for a terminal name not in [`LINUX_TERMINALS`]: the most
/// conservative one, trusting neither extra flags nor the child's exit.
const UNKNOWN_LINUX_TERMINAL: LinuxTerminal = LinuxTerminal {
    bin: "",
    prefix: &["-e"],
    tracks_window_lifetime: false,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A completed interactive login.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    /// The full `.credentials.json` contents captured from the isolated temp
    /// config dir — a JSON object with a `claudeAiOauth` key, the same shape
    /// Claude Code itself writes and the same shape [`crate::credentials::CredentialStore`]
    /// expects for a stored account.
    pub credentials: String,
    /// Stable account UUID from the resolved profile. Replacement flows use
    /// this to prove an isolated login belongs to the selected existing slot.
    pub uuid: Option<String>,
    /// Best-effort identity resolved via [`crate::oauth::fetch_oauth_profile`].
    /// `None` if the profile lookup failed or was unreachable — the login
    /// itself still succeeded; the caller should fall back to a placeholder
    /// label rather than treat this as an error.
    pub email: Option<String>,
    /// Best-effort organization UUID from the same profile lookup. This is an
    /// identifier, not a human-readable org name — [`crate::oauth::TokenAccount`]
    /// (what `fetch_oauth_profile` resolves) does not carry a display name.
    pub organization_uuid: Option<String>,
}

/// Failure modes for [`interactive_login`]. The UI is expected to branch on
/// these — see each variant's doc for the intended user-facing meaning.
#[derive(Debug, Error)]
pub enum LoginError {
    /// No `claude` binary could be found — not on `PATH`, not in any
    /// documented install location, and not at a configured override. The
    /// payload names what was searched so the UI can tell the user where to
    /// look and how to point the app at their install; its `Display` renders
    /// that sentence.
    ///
    /// Filesystem paths are not credentials, so carrying them here does not
    /// touch this module's never-log-secrets rule.
    #[error("{0}")]
    ClaudeNotInstalled(NotFound),

    /// No terminal emulator could be launched (Linux only — Windows and
    /// macOS always have one). The UI should fall back to the paste-a-token
    /// flow.
    #[error("no terminal emulator is available on this system")]
    NoTerminalAvailable,

    /// The terminal window closed before a valid credential appeared. A
    /// normal outcome (the user changed their mind), not an error to alarm
    /// over.
    #[error("login was cancelled")]
    Cancelled,

    /// No valid credential appeared within [`LOGIN_TIMEOUT`].
    #[error("login timed out after 10 minutes")]
    TimedOut,

    /// A filesystem or process-spawn failure unrelated to the login flow
    /// itself (creating the temp dir, launching the terminal, etc).
    #[error("I/O error during login: {0}")]
    Io(#[from] io::Error),

    /// A credential file appeared but did not parse as JSON, or lacked a
    /// non-empty `claudeAiOauth.accessToken`. The message is a fixed,
    /// content-free description — never the file's own bytes.
    #[error("received credential could not be validated: {0}")]
    BadCredential(&'static str),
}

// ---------------------------------------------------------------------------
// Per-platform launch-command construction (pure — no I/O beyond the Linux
// terminal *search*, which only calls the injectable `find_on_path`).
// ---------------------------------------------------------------------------

/// How to launch the isolated login terminal, and how to interpret its
/// process handle afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchPlan {
    /// Windows: `cmd.exe` with a single raw command-line tail (bypassing
    /// Rust's own argv-quoting, which would otherwise mangle the
    /// `start "title" cmd /c "..."` idiom — see [`windows_command_line`]).
    /// `cmd.exe /c start ...` hands off to a detached window and returns
    /// almost immediately, so its own exit carries no information about
    /// whether the user finished, cancelled, or is still typing.
    WindowsRaw { program: String, raw_args: String },
    /// macOS or Linux: a normal argv vector.
    Argv {
        program: String,
        args: Vec<String>,
        /// `true` when the spawned child *is* the terminal window, so its
        /// exit means the window closed (per-terminal on Linux — see
        /// [`LINUX_TERMINALS`]). `false` when it only hands off to a window
        /// whose lifetime it does not own (macOS `osascript`, a forking or
        /// client/server terminal), where its exit says nothing at all.
        tracks_window_lifetime: bool,
    },
}

/// POSIX single-quote a string for embedding in a shell command line:
/// wraps in `'...'`, escaping any embedded `'` as `'\''`. Used for both the
/// macOS (`osascript` → `do script` → `sh`) and Linux (`bash -c`) command
/// strings.
fn shell_quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a string for embedding inside an AppleScript double-quoted string
/// literal (`\` and `"` are the only two characters AppleScript string
/// literals need escaped).
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The raw text appended after `cmd.exe` on Windows (i.e. everything but the
/// program name itself), reproducing:
///
/// ```text
/// cmd.exe /c start "Add Claude account" cmd /c "set "CLAUDE_CONFIG_DIR=<dir>" && claude auth login"
/// ```
///
/// `/c` (not `/k`) on both the outer `start` and the inner `cmd` so the
/// window closes itself once `claude auth login` finishes, instead of
/// leaving a dead shell prompt behind. The temp path is wrapped as
/// `set "VAR=value"` (quoting the whole assignment, not just the value) —
/// the standard idiom for a `set` value containing spaces without the
/// quote characters themselves becoming part of the stored value.
fn windows_command_line(claude: &Path, config_dir: &Path) -> String {
    // Why prepend the directory instead of substituting the absolute path:
    // inlining it would add a *third* level of quoting inside
    // `cmd /c "set "..." && ..."`, whose parsing rules are conditional on
    // `/s`, on leading-quote stripping, and on the total count of quote
    // characters. The `set "VAR=value"` idiom is already proven here, so
    // reuse it — the child still resolves *our* binary first, and cmd's own
    // `PATHEXT` picks the right `.cmd`/`.exe` extension, which a hardcoded
    // absolute filename would get wrong.
    let bin_dir = windows_parent_dir(claude);
    format!(
        "/c start \"Add Claude account\" cmd /c \"set \"CLAUDE_CONFIG_DIR={}\" && set \"PATH={};%PATH%\" && claude auth login\"",
        config_dir.display(),
        bin_dir
    )
}

/// The parent directory of a Windows-style path, as a string.
///
/// Deliberately *not* `Path::parent()`: `std::path::Path`'s separator
/// handling is selected by the compile *target*, not by the path string's
/// own style, so on every non-Windows leg of this crate's CI (which runs
/// `cargo test` on ubuntu/macOS too — see `.github/workflows/ci.yml`) a
/// backslash-separated Windows path is one opaque `Normal` component and
/// `.parent()` silently returns an empty path. Splitting on the last `\` or
/// `/` byte ourselves gives the same, correct answer on every host,
/// matching what a real Windows target would do with `Path::parent()`.
fn windows_parent_dir(path: &Path) -> String {
    // Hand-rolled rather than `Path::parent()` because `std`'s separator set
    // is chosen by the *compile target*, not by the path's own style: on a
    // unix build a backslash-separated Windows path is one opaque component,
    // so `parent()` yields "" and this would emit `PATH=;%PATH%`. That only
    // matters because the CI matrix runs these tests on Linux and macOS too
    // (`.github/workflows/ci.yml`), and a Windows assertion that quietly means
    // something else off-Windows is worse than no assertion. Same reason
    // `claude_cli`'s platform is an injected field rather than `detect()`.
    let s = path.to_string_lossy();
    match s.rfind(['\\', '/']) {
        // Keep the separator on a drive root: `C:\\claude.exe` must yield
        // `C:\\`, since bare `C:` means "current directory on C:" to cmd, not
        // the root of the drive.
        Some(idx) if s[..idx].ends_with(':') => s[..=idx].to_string(),
        Some(idx) => s[..idx].to_string(),
        None => ".".to_string(),
    }
}

fn windows_launch_plan(claude: &Path, config_dir: &Path) -> LaunchPlan {
    LaunchPlan::WindowsRaw {
        program: "cmd.exe".to_string(),
        raw_args: windows_command_line(claude, config_dir),
    }
}

/// The `osascript -e '<script>'` invocation for macOS, as an argv pair.
fn macos_launch_plan(claude: &Path, config_dir: &Path) -> LaunchPlan {
    let shell_cmd = format!(
        "export CLAUDE_CONFIG_DIR={}; {} auth login",
        shell_quote_single(&config_dir.display().to_string()),
        shell_quote_single(&claude.display().to_string())
    );
    let script = format!(
        "tell application \"Terminal\" to do script \"{}\"",
        applescript_escape(&shell_cmd)
    );
    LaunchPlan::Argv {
        program: "osascript".to_string(),
        args: vec!["-e".to_string(), script],
        tracks_window_lifetime: false,
    }
}

/// The launch argv for a specific Linux terminal binary (already resolved to
/// exist on `PATH` by the caller). Each terminal's flags come from its own
/// [`LINUX_TERMINALS`] entry; the tail is always `bash -c <shell command>`,
/// an argv rather than one shell string.
fn linux_launch_plan(terminal: &str, claude: &Path, config_dir: &Path) -> LaunchPlan {
    let spec = LINUX_TERMINALS
        .iter()
        .copied()
        .find(|t| t.bin == terminal)
        .unwrap_or(UNKNOWN_LINUX_TERMINAL);
    let shell_cmd = format!(
        "export CLAUDE_CONFIG_DIR={}; {} auth login",
        shell_quote_single(&config_dir.display().to_string()),
        shell_quote_single(&claude.display().to_string())
    );
    let mut args: Vec<String> = spec.prefix.iter().map(|s| (*s).to_string()).collect();
    args.push("bash".to_string());
    args.push("-c".to_string());
    args.push(shell_cmd);
    LaunchPlan::Argv {
        program: terminal.to_string(),
        args,
        tracks_window_lifetime: spec.tracks_window_lifetime,
    }
}

/// Find the first [`LINUX_TERMINALS`] entry for which `exists` returns
/// `true`. `exists` is injected so this selection logic is unit-testable
/// without depending on what is actually installed on the machine running
/// the tests.
fn find_linux_terminal(exists: impl Fn(&str) -> bool) -> Option<&'static str> {
    LINUX_TERMINALS
        .iter()
        .find(|t| exists(t.bin))
        .map(|t| t.bin)
}

/// Build the launch plan for the current platform. `Err(NoTerminalAvailable)`
/// on Linux/WSL when none of [`LINUX_TERMINALS`] is on `PATH`, and (out of
/// caution, though unreached on this crate's supported targets) on any
/// platform this crate doesn't otherwise recognize.
fn build_launch_plan(claude: &Path, config_dir: &Path) -> Result<LaunchPlan, LoginError> {
    match Platform::detect() {
        Platform::Windows => Ok(windows_launch_plan(claude, config_dir)),
        Platform::Macos => Ok(macos_launch_plan(claude, config_dir)),
        Platform::Linux | Platform::Wsl => {
            let terminal = find_linux_terminal(|name| claude_cli::find_on_path(name).is_some())
                .ok_or(LoginError::NoTerminalAvailable)?;
            Ok(linux_launch_plan(terminal, claude, config_dir))
        }
        Platform::Unknown => Err(LoginError::NoTerminalAvailable),
    }
}

// ---------------------------------------------------------------------------
// Spawning (the one part of this module that is genuinely platform-`cfg`d)
// ---------------------------------------------------------------------------

/// Spawn `plan`, returning the child and whether its exit is meaningful for
/// cancellation detection (mirrors [`LaunchPlan::Argv`]'s
/// `tracks_window_lifetime`; always `false` for [`LaunchPlan::WindowsRaw`]).
fn spawn_launch(plan: &LaunchPlan) -> io::Result<(std::process::Child, bool)> {
    match plan {
        LaunchPlan::WindowsRaw { program, raw_args } => {
            let mut cmd = std::process::Command::new(program);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.raw_arg(raw_args);
            }
            #[cfg(not(windows))]
            {
                // Unreachable in practice — `WindowsRaw` is only constructed
                // by `windows_launch_plan`, which `build_launch_plan` only
                // calls under `Platform::Windows`. Kept so this function
                // still compiles on every host target.
                cmd.arg(raw_args);
            }
            let child = cmd.spawn()?;
            Ok((child, false))
        }
        LaunchPlan::Argv {
            program,
            args,
            tracks_window_lifetime,
        } => {
            let child = std::process::Command::new(program).args(args).spawn()?;
            Ok((child, *tracks_window_lifetime))
        }
    }
}

// ---------------------------------------------------------------------------
// Credential validation (pure)
// ---------------------------------------------------------------------------

/// Read `path` and return its contents only if they parse as JSON containing
/// a non-empty `claudeAiOauth.accessToken`. `None` on any read/parse/shape
/// failure — including a partially-written file mid-flush, which is expected
/// and simply means "keep polling", not an error.
fn try_read_valid_credential(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let token = crate::oauth::extract_access_token(&text)?;
    if token.is_empty() {
        return None;
    }
    Some(text)
}

/// Run `claude auth status` against `config_dir` as the authoritative
/// success check (rather than trusting the credential file's existence
/// alone, per this module's design). `false` on any spawn failure or non-zero
/// exit.
fn verify_login_status(claude_path: &Path, config_dir: &Path) -> bool {
    let mut cmd = std::process::Command::new(claude_path);
    cmd.args(["auth", "status"])
        .env("CLAUDE_CONFIG_DIR", config_dir);
    // A silent check, unlike the sign-in terminal above, which is meant to be
    // seen — so it must not flash a console of its own.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Final determination once the terminal window has closed and no more
/// waiting will happen. Distinguishes three outcomes the UI should treat
/// differently:
///
/// - No credential file (or an empty one) ever appeared: [`LoginError::Cancelled`]
///   — the ordinary "the user closed the window" case, not an error.
/// - A file appeared but doesn't hold a usable token, or `claude auth status`
///   doesn't confirm it: [`LoginError::BadCredential`] — something landed but
///   it isn't trustworthy, worth telling the user about distinctly from a
///   plain cancellation.
/// - A file appeared, validates, and `claude auth status` confirms it: `Ok`.
fn classify_window_closed(claude_path: &Path, config_dir: &Path) -> Result<String, LoginError> {
    let credentials_path = config_dir.join(".credentials.json");
    let text = match std::fs::read_to_string(&credentials_path) {
        Ok(t) if !t.trim().is_empty() => t,
        _ => return Err(LoginError::Cancelled),
    };

    let has_token = crate::oauth::extract_access_token(&text)
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    if !has_token {
        return Err(LoginError::BadCredential(
            "credential file did not contain a usable access token",
        ));
    }

    if !verify_login_status(claude_path, config_dir) {
        return Err(LoginError::BadCredential(
            "claude auth status did not confirm the captured credential",
        ));
    }

    Ok(text)
}

// ---------------------------------------------------------------------------
// The blocking poll loop
// ---------------------------------------------------------------------------

/// Spawn the terminal and poll until a validated credential appears, the
/// window closes with nothing valid captured, or [`LOGIN_TIMEOUT`] elapses.
///
/// Runs on a blocking thread (see [`interactive_login`]) — every step here
/// (`spawn`, `try_wait`, `fs::read_to_string`, `Command::output`) is
/// synchronous I/O, deliberately not `tokio::fs`/`tokio::process` (neither is
/// among this crate's enabled `tokio` features).
fn run_login_blocking(
    claude_path: &Path,
    config_dir: &Path,
    plan: LaunchPlan,
) -> Result<String, LoginError> {
    let (child, tracks_window_lifetime) = spawn_launch(&plan)?;

    // Only a genuine "this child *is* the terminal window" handle is worth
    // polling for exit. The detached-launcher case (Windows `start`, macOS
    // `osascript`) exits almost immediately regardless of user action; reap
    // it so it doesn't linger as a zombie, but treat it as gone from here on.
    let mut window_child = if tracks_window_lifetime {
        Some(child)
    } else {
        let mut child = child;
        let _ = child.wait();
        None
    };

    let credentials_path = config_dir.join(".credentials.json");
    let started = Instant::now();
    let deadline = started + LOGIN_TIMEOUT;

    loop {
        if let Some(creds) = try_read_valid_credential(&credentials_path) {
            if verify_login_status(claude_path, config_dir) {
                return Ok(creds);
            }
            // File present and parses, but `claude auth status` doesn't
            // (yet) agree — could be a write race with a still-settling
            // config dir. Keep polling rather than trusting the file alone.
        }

        if let Some(child) = window_child.as_mut() {
            let wait_result = child.try_wait();
            match wait_result {
                Ok(Some(status)) if started.elapsed() < MIN_WINDOW_LIFETIME => {
                    window_child = None;
                    if !status.success() {
                        // Too fast *and* failed: the emulator rejected our
                        // flags, so no window ever opened. Don't wait 10 min.
                        return Err(LoginError::Io(io::Error::other(
                            "terminal emulator exited immediately without opening a window",
                        )));
                    }
                    // Exited cleanly and instantly: a hand-off to a window we
                    // can't observe. Keep polling for the credential.
                    log::info!("login terminal handed off; falling back to credential polling");
                }
                Ok(Some(_status)) => {
                    // Window closed. Give the filesystem one last instant in
                    // case completion raced the window closing itself, then
                    // make the final call (Cancelled vs BadCredential vs Ok).
                    std::thread::sleep(Duration::from_millis(300));
                    return classify_window_closed(claude_path, config_dir);
                }
                Ok(None) => {}
                Err(_) => {
                    // Can no longer observe this child (platform-specific
                    // wait failure) — fall back to file-polling + timeout
                    // only, same as the detached-launcher platforms.
                    window_child = None;
                }
            }
        }

        if Instant::now() >= deadline {
            if let Some(mut child) = window_child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(LoginError::TimedOut);
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run an isolated interactive login and return the captured credential.
///
/// Opens a visible terminal running `claude auth login` with
/// `CLAUDE_CONFIG_DIR` pointed at a fresh temp directory (see the module
/// doc), waits for a validated credential (or cancellation, or timeout), and
/// always removes the temp directory before returning — the [`TempDir`]
/// lives in this function's own stack frame for its entire body, so it drops
/// (and deletes its contents) on every `return` path, `?`-propagated error,
/// and ordinary fall-through alike.
///
/// Never touches this user's real `~/.claude` / `~/.claude.json` — every
/// filesystem path this function or its helpers resolve is under the temp
/// directory it creates for itself.
/// Prefix for the isolated login directories, so [`sweep_stale_login_dirs`]
/// can identify ours and only ours.
pub const TEMP_DIR_PREFIX: &str = "claude-switcher-login-";

/// Delete leftover isolated-login directories from previous runs.
///
/// [`interactive_login`] cleans up via [`TempDir`]'s `Drop`, which covers every
/// normal path including errors and panics — but this crate sets
/// `panic = "abort"` in its release profile, and an abort runs no destructors
/// at all. A directory that briefly held a real credential would then survive
/// on disk indefinitely.
///
/// So this runs at startup as a backstop. It is deliberately conservative:
/// only directories carrying [`TEMP_DIR_PREFIX`] are considered, and only those
/// older than `min_age`, so a login in progress in another instance is never
/// deleted out from under it. Returns how many it removed.
pub fn sweep_stale_login_dirs(min_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TEMP_DIR_PREFIX) {
            continue;
        }
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        // Age check: never race a login happening right now in another window.
        let old_enough = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= min_age);
        if !old_enough {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            log::info!("swept stale login dir {}", entry.path().display());
            removed += 1;
        }
    }
    removed
}

pub async fn interactive_login(
    claude_binary_setting: Option<PathBuf>,
) -> Result<LoginOutcome, LoginError> {
    let resolved =
        claude_cli::resolve(claude_binary_setting).map_err(LoginError::ClaudeNotInstalled)?;
    log::info!(
        "using claude binary from {}: {}",
        resolved.source.label(),
        resolved.path.display()
    );
    let claude_path = resolved.path;

    // Distinctive prefix on purpose: `TempDir::new()` produces a generic
    // `.tmpXXXXXX` name, indistinguishable from every other program's scratch
    // directory — which makes the startup sweep below impossible to do safely.
    // See `sweep_stale_login_dirs`.
    let temp_dir = tempfile::Builder::new().prefix(TEMP_DIR_PREFIX).tempdir()?;
    let temp_path = temp_dir.path().to_path_buf();
    log::info!(
        "interactive login: isolated config dir {} (real ~/.claude untouched)",
        temp_path.display()
    );

    let plan = build_launch_plan(&claude_path, &temp_path)?;

    let blocking_claude_path = claude_path.clone();
    let blocking_temp_path = temp_path.clone();
    let poll_result = tokio::task::spawn_blocking(move || {
        run_login_blocking(&blocking_claude_path, &blocking_temp_path, plan)
    })
    .await;

    let credentials = match poll_result {
        Ok(Ok(creds)) => creds,
        Ok(Err(e)) => {
            log::info!(
                "interactive login did not complete: {}",
                login_outcome_kind(&e)
            );
            return Err(e);
            // `temp_dir` drops here, deleting the temp config dir.
        }
        Err(_join_err) => {
            log::warn!("interactive login worker task did not finish cleanly");
            return Err(LoginError::Io(io::Error::other(
                "login worker task did not finish cleanly",
            )));
            // `temp_dir` drops here too.
        }
    };

    // Best-effort identity resolution — never fatal to a successful login.
    let access_token = crate::oauth::extract_access_token(&credentials);
    let identity = match access_token {
        Some(token) => crate::oauth::fetch_oauth_profile(&token).await,
        None => None,
    };

    log::info!("interactive login succeeded");

    Ok(LoginOutcome {
        credentials,
        uuid: identity
            .as_ref()
            .map(|i| i.uuid.clone())
            .filter(|uuid| !uuid.is_empty()),
        email: identity.as_ref().and_then(|i| i.email.clone()),
        organization_uuid: identity.as_ref().and_then(|i| i.organization_uuid.clone()),
    })
    // `temp_dir` drops here on the success path too.
}

/// A short, content-free label for a [`LoginError`], for the one log line
/// `interactive_login` emits on failure. Never includes file contents.
fn login_outcome_kind(e: &LoginError) -> &'static str {
    match e {
        LoginError::ClaudeNotInstalled(_) => "claude-not-installed",
        LoginError::NoTerminalAvailable => "no-terminal-available",
        LoginError::Cancelled => "cancelled",
        LoginError::TimedOut => "timed-out",
        LoginError::Io(_) => "io-error",
        LoginError::BadCredential(_) => "bad-credential",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Per this module's task constraints: no spawning a real terminal, no
// invoking a real `claude` binary. Everything below exercises pure string
// construction, the Linux-terminal selection logic (via an injected `exists`
// predicate), and file-based credential validation against files this test
// suite writes itself under a `tempfile::TempDir` — never any real store
// path.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -- windows command-line construction -----------------------------------

    /// A representative resolved Windows `claude` path, used across the
    /// Windows tests below.
    const WIN_CLAUDE: &str = r"C:\Users\me\.local\bin\claude.exe";

    #[test]
    fn windows_command_line_has_no_spaces_form() {
        let claude = Path::new(WIN_CLAUDE);
        let dir = Path::new(r"C:\Users\me\AppData\Local\Temp\claude-login-abc123");
        let line = windows_command_line(claude, dir);
        assert_eq!(
            line,
            "/c start \"Add Claude account\" cmd /c \"set \"CLAUDE_CONFIG_DIR=C:\\Users\\me\\AppData\\Local\\Temp\\claude-login-abc123\" && set \"PATH=C:\\Users\\me\\.local\\bin;%PATH%\" && claude auth login\""
        );
        // Uses /c (not /k) on both the outer `start` and the inner `cmd` so
        // the window closes itself once login finishes.
        assert!(line.starts_with("/c start"));
        assert!(line.contains("cmd /c \""));
        assert!(!line.contains("/k"));
    }

    #[test]
    fn windows_command_line_quotes_path_containing_spaces() {
        let claude = Path::new(WIN_CLAUDE);
        let dir = Path::new(r"C:\Users\Jane Doe\AppData\Local\Temp\claude-login-xyz");
        let line = windows_command_line(claude, dir);
        // The whole `VAR=value` assignment is quoted (not just the value),
        // so the quote characters never become part of the stored value.
        assert!(line.contains(
            "set \"CLAUDE_CONFIG_DIR=C:\\Users\\Jane Doe\\AppData\\Local\\Temp\\claude-login-xyz\""
        ));
    }

    #[test]
    fn windows_parent_dir_keeps_the_separator_on_a_drive_root() {
        // `C:` alone means "current directory on drive C" to cmd, not the root,
        // so trimming the separator here would point PATH somewhere else.
        assert_eq!(windows_parent_dir(Path::new(r"C:\claude.exe")), r"C:\");
        assert_eq!(
            windows_parent_dir(Path::new(r"C:\Users\me\bin\claude.exe")),
            r"C:\Users\me\bin"
        );
        // Forward slashes appear in Windows paths often enough to matter.
        assert_eq!(
            windows_parent_dir(Path::new("C:/tools/claude.exe")),
            "C:/tools"
        );
        // A bare filename has no directory to prepend; "." is the harmless
        // stand-in rather than an empty PATH entry.
        assert_eq!(windows_parent_dir(Path::new("claude.exe")), ".");
    }

    #[test]
    fn windows_command_line_prepends_the_resolved_directory_to_path() {
        // Windows still invokes a bare `claude` (see the doc comment on
        // `windows_command_line` for why an absolute path isn't inlined
        // instead), but it must prepend the resolved binary's *directory* to
        // `PATH` so that bare invocation actually finds our verified binary
        // first, not just whatever `claude` a user's shell PATH turns up.
        let claude = Path::new(WIN_CLAUDE);
        let dir = Path::new(r"C:\temp\abc");
        let line = windows_command_line(claude, dir);
        assert!(line.contains("set \"PATH=C:\\Users\\me\\.local\\bin;%PATH%\""));
    }

    #[test]
    fn windows_launch_plan_targets_cmd_exe_with_raw_args() {
        let claude = Path::new(WIN_CLAUDE);
        let dir = Path::new(r"C:\temp\abc");
        match windows_launch_plan(claude, dir) {
            LaunchPlan::WindowsRaw { program, raw_args } => {
                assert_eq!(program, "cmd.exe");
                assert_eq!(raw_args, windows_command_line(claude, dir));
            }
            other => panic!("expected WindowsRaw, got {other:?}"),
        }
    }

    // -- macOS command construction ------------------------------------------

    #[test]
    fn macos_launch_plan_uses_osascript_and_does_not_track_window_lifetime() {
        let claude = Path::new("/usr/local/bin/claude");
        let dir = Path::new("/tmp/claude-login-abc");
        match macos_launch_plan(claude, dir) {
            LaunchPlan::Argv {
                program,
                args,
                tracks_window_lifetime,
            } => {
                assert_eq!(program, "osascript");
                assert_eq!(args[0], "-e");
                assert!(args[1].contains("tell application \"Terminal\" to do script"));
                assert!(args[1].contains("CLAUDE_CONFIG_DIR="));
                assert!(args[1].contains("auth login"));
                // osascript hands off to Terminal.app and returns
                // immediately — its exit says nothing about the window.
                assert!(!tracks_window_lifetime);
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    #[test]
    fn macos_script_escapes_embedded_quotes_and_backslashes() {
        // A path with a backslash or quote is exotic on macOS but the
        // escaper must not corrupt the AppleScript string if one appears.
        let claude = Path::new("/usr/local/bin/claude");
        let dir = Path::new("/tmp/weird\"dir");
        match macos_launch_plan(claude, dir) {
            LaunchPlan::Argv { args, .. } => {
                // The raw `"` from the path must appear escaped as `\"` in
                // the AppleScript string literal, not as a bare quote that
                // would terminate the literal early.
                assert!(args[1].contains("\\\""));
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    #[test]
    fn macos_launch_plan_invokes_the_resolved_absolute_path_not_bare_claude() {
        // The launch plan must run the exact binary the app already resolved
        // and verified, not rely on the child shell's own PATH turning up
        // some other `claude` first.
        let claude = Path::new("/usr/local/bin/claude");
        let dir = Path::new("/tmp/claude-login-abc");
        match macos_launch_plan(claude, dir) {
            LaunchPlan::Argv { args, .. } => {
                assert!(args[1].contains("'/usr/local/bin/claude' auth login"));
                assert!(!args[1].contains(" claude auth login"));
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    // -- Linux terminal selection ---------------------------------------------

    /// A representative resolved Linux `claude` path, used across the Linux
    /// tests below.
    const LINUX_CLAUDE: &str = "/usr/local/bin/claude";

    /// Destructure a Linux plan into (args, tracks_window_lifetime), checking
    /// the program name and the always-identical `bash -c <cmd>` tail.
    fn linux_plan_parts(terminal: &str, claude: &Path, dir: &Path) -> (Vec<String>, bool) {
        match linux_launch_plan(terminal, claude, dir) {
            LaunchPlan::Argv {
                program,
                args,
                tracks_window_lifetime,
            } => {
                assert_eq!(program, terminal);
                let n = args.len();
                assert_eq!(args[n - 3], "bash");
                assert_eq!(args[n - 2], "-c");
                assert!(args[n - 1].contains("CLAUDE_CONFIG_DIR="));
                assert!(args[n - 1].contains("auth login"));
                (args, tracks_window_lifetime)
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    #[test]
    fn find_linux_terminal_honors_preference_order() {
        // gnome-terminal outranks xterm.
        let found = find_linux_terminal(|name| matches!(name, "gnome-terminal" | "xterm"));
        assert_eq!(found, Some("gnome-terminal"));
    }

    #[test]
    fn find_linux_terminal_prefers_known_terminals_over_x_terminal_emulator() {
        // x-terminal-emulator points at an unknown target, so it is the last
        // resort — never chosen while a terminal we have flags for exists.
        let found = find_linux_terminal(|name| matches!(name, "x-terminal-emulator" | "xterm"));
        assert_eq!(found, Some("xterm"));
    }

    #[test]
    fn find_linux_terminal_falls_back_to_x_terminal_emulator() {
        let found = find_linux_terminal(|name| name == "x-terminal-emulator");
        assert_eq!(found, Some("x-terminal-emulator"));
    }

    #[test]
    fn find_linux_terminal_none_when_nothing_present() {
        assert_eq!(find_linux_terminal(|_| false), None);
    }

    #[test]
    fn linux_launch_plan_gnome_terminal_waits_and_uses_double_dash() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, tracks) = linux_plan_parts("gnome-terminal", claude, dir);
        // `--wait` makes the process outlive the hand-off to
        // gnome-terminal-server, so its exit honestly means "window closed".
        assert_eq!(args[0], "--wait");
        assert_eq!(args[1], "--");
        assert!(tracks);
    }

    #[test]
    fn linux_launch_plan_konsole_uses_nofork_before_dash_e() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, tracks) = linux_plan_parts("konsole", claude, dir);
        // -e swallows everything after it, so --nofork must precede it.
        assert_eq!(args[0], "--nofork");
        assert_eq!(args[1], "-e");
        assert!(tracks);
    }

    #[test]
    fn linux_launch_plan_xfce4_uses_dash_x_not_dash_e() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, tracks) = linux_plan_parts("xfce4-terminal", claude, dir);
        // -e takes a single command string; -x takes the remainder as argv.
        assert_eq!(args[0], "--disable-server");
        assert_eq!(args[1], "-x");
        assert!(!args.contains(&"-e".to_string()));
        assert!(tracks);
    }

    #[test]
    fn linux_launch_plan_xterm_uses_dash_e_argv_form() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, tracks) = linux_plan_parts("xterm", claude, dir);
        // -e takes a program + its own argv, NOT one shell-string argument.
        assert_eq!(args[0], "-e");
        assert_eq!(args.len(), 4);
        assert!(tracks);
    }

    #[test]
    fn linux_launch_plan_x_terminal_emulator_is_conservative() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, tracks) = linux_plan_parts("x-terminal-emulator", claude, dir);
        // Only the Debian Policy `-e command [args]` form, and its target may
        // fork — so its exit must never be read as a cancellation.
        assert_eq!(args[0], "-e");
        assert_eq!(args.len(), 4);
        assert!(!tracks);
    }

    #[test]
    fn linux_launch_plan_unknown_terminal_falls_back_to_conservative_shape() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, tracks) = linux_plan_parts("some-future-terminal", claude, dir);
        assert_eq!(args[0], "-e");
        assert!(!tracks);
    }

    #[test]
    fn linux_terminal_table_flags_end_with_a_remainder_taking_flag() {
        // Every entry's last prefix option must be the one that consumes
        // `program args...`, or the tail would be parsed as options.
        for t in LINUX_TERMINALS {
            let last = t.prefix.last().copied().unwrap_or_default();
            assert!(
                matches!(last, "-e" | "-x" | "--"),
                "{} ends its flags with {last:?}",
                t.bin
            );
        }
    }

    #[test]
    fn linux_shell_command_quotes_path_containing_spaces_and_quotes() {
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/Jane's dir");
        let (args, _) = linux_plan_parts("xterm", claude, dir);
        // Single-quoted with the embedded `'` escaped via '\''.
        assert!(args[3].contains("'/tmp/Jane'\\''s dir'"));
    }

    #[test]
    fn linux_shell_command_quotes_claude_path_containing_spaces_and_quotes() {
        // The resolved `claude` binary path is embedded the same way the
        // config dir is (see the test above) — a space or apostrophe in an
        // install path (e.g. under "Jane's Applications") must not break out
        // of the single-quoted invocation.
        let claude = Path::new("/opt/Jane's Apps/claude");
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, _) = linux_plan_parts("xterm", claude, dir);
        assert!(args[3].contains("'/opt/Jane'\\''s Apps/claude'"));
    }

    #[test]
    fn linux_launch_plan_invokes_the_resolved_absolute_path_not_bare_claude() {
        // Same intent as the macOS equivalent: the shell string run inside
        // `bash -c` must invoke the exact resolved binary, not a bare
        // `claude` left to the child shell's own PATH.
        let claude = Path::new(LINUX_CLAUDE);
        let dir = Path::new("/tmp/claude-login-abc");
        let (args, _) = linux_plan_parts("xterm", claude, dir);
        assert!(args[3].contains("'/usr/local/bin/claude' auth login"));
        assert!(!args[3].contains(" claude auth login"));
    }

    // -- shell / AppleScript quoting helpers ----------------------------------

    #[test]
    fn shell_quote_single_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote_single("plain"), "'plain'");
        assert_eq!(shell_quote_single("a'b"), "'a'\\''b'");
    }

    #[test]
    fn applescript_escape_handles_quotes_and_backslashes() {
        assert_eq!(applescript_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(applescript_escape(r"a\b"), r"a\\b");
    }

    // -- credential validation -------------------------------------------------

    #[test]
    fn try_read_valid_credential_accepts_a_good_blob() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-real-token","refreshToken":"r1"}}"#,
        )
        .unwrap();

        let result = try_read_valid_credential(&path);
        assert!(result.is_some());
        assert!(result.unwrap().contains("sk-ant-oat-real-token"));
    }

    #[test]
    fn try_read_valid_credential_rejects_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        // Never created.
        assert_eq!(try_read_valid_credential(&path), None);
    }

    #[test]
    fn try_read_valid_credential_rejects_malformed_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        assert_eq!(try_read_valid_credential(&path), None);
    }

    #[test]
    fn try_read_valid_credential_rejects_empty_access_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":""}}"#).unwrap();
        assert_eq!(try_read_valid_credential(&path), None);
    }

    #[test]
    fn try_read_valid_credential_rejects_missing_oauth_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, r#"{"somethingElse": true}"#).unwrap();
        assert_eq!(try_read_valid_credential(&path), None);
    }

    #[test]
    fn try_read_valid_credential_rejects_partially_written_file() {
        // Simulates a write caught mid-flush: valid UTF-8, truncated JSON.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat"#).unwrap();
        assert_eq!(try_read_valid_credential(&path), None);
    }

    // -- window-closed classification (Cancelled vs BadCredential) ------------

    #[test]
    fn classify_window_closed_is_cancelled_when_nothing_ever_appeared() {
        let dir = TempDir::new().unwrap();
        // No .credentials.json written at all — the ordinary "closed the
        // window without logging in" case.
        let bogus_claude = Path::new("definitely-not-a-real-claude-binary-xyz");
        let err = classify_window_closed(bogus_claude, dir.path()).unwrap_err();
        assert!(matches!(err, LoginError::Cancelled));
    }

    #[test]
    fn classify_window_closed_is_cancelled_when_file_is_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"").unwrap();
        let bogus_claude = Path::new("definitely-not-a-real-claude-binary-xyz");
        let err = classify_window_closed(bogus_claude, dir.path()).unwrap_err();
        assert!(matches!(err, LoginError::Cancelled));
    }

    #[test]
    fn classify_window_closed_is_bad_credential_when_file_has_no_token() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), r#"{"oops": true}"#).unwrap();
        let bogus_claude = Path::new("definitely-not-a-real-claude-binary-xyz");
        let err = classify_window_closed(bogus_claude, dir.path()).unwrap_err();
        assert!(matches!(err, LoginError::BadCredential(_)));
    }

    #[test]
    fn classify_window_closed_is_bad_credential_when_status_check_fails() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-looks-real"}}"#,
        )
        .unwrap();
        // A token is present, but the "claude" binary here doesn't exist, so
        // `claude auth status` can't run and can't confirm it — this
        // exercises the failure path of `verify_login_status` without
        // spawning any real process (Command::spawn fails with NotFound).
        let bogus_claude = Path::new("definitely-not-a-real-claude-binary-xyz");
        let err = classify_window_closed(bogus_claude, dir.path()).unwrap_err();
        assert!(matches!(err, LoginError::BadCredential(_)));
    }

    // -- temp-dir cleanup -------------------------------------------------------

    #[test]
    fn temp_dir_is_removed_when_dropped_even_after_writes() {
        // Not a spawn of interactive_login (that needs a real `claude`
        // binary and terminal), but exercises the exact cleanup mechanism
        // interactive_login relies on: a TempDir that has had a credential
        // written into it is still fully removed on drop.
        let path_after;
        {
            let dir = TempDir::new().unwrap();
            let cred_path = dir.path().join(".credentials.json");
            std::fs::write(&cred_path, r#"{"claudeAiOauth":{"accessToken":"secret"}}"#).unwrap();
            assert!(cred_path.exists());
            path_after = dir.path().to_path_buf();
        } // `dir` (TempDir) drops here.
        assert!(!path_after.exists());
    }

    // -- LoginError logging surface ---------------------------------------------

    #[test]
    fn login_error_kind_labels_are_content_free() {
        // Sanity check that the log-line labels never echo file contents —
        // each must be a fixed string, not derived from any credential body.
        assert_eq!(
            login_outcome_kind(&LoginError::ClaudeNotInstalled(Default::default())),
            "claude-not-installed"
        );
        assert_eq!(
            login_outcome_kind(&LoginError::NoTerminalAvailable),
            "no-terminal-available"
        );
        assert_eq!(login_outcome_kind(&LoginError::Cancelled), "cancelled");
        assert_eq!(login_outcome_kind(&LoginError::TimedOut), "timed-out");
        assert_eq!(
            login_outcome_kind(&LoginError::BadCredential("missing accessToken")),
            "bad-credential"
        );
    }
}
