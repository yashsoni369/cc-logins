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

use crate::paths::Platform;

/// How often the poll loop checks for the credential file / terminal exit.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A browser OAuth round trip can be slow (SSO, MFA, a user who steps away).
/// Ten minutes matches the ceiling documented for the design this module
/// implements.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Linux terminal emulators to try, in preference order. The first one found
/// on `PATH` is used; if none is, the caller gets [`LoginError::NoTerminalAvailable`]
/// so the UI can fall back to the paste-a-token path instead of failing
/// obscurely.
const LINUX_TERMINALS: &[&str] = &[
    "x-terminal-emulator",
    "gnome-terminal",
    "konsole",
    "xfce4-terminal",
    "xterm",
];

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
    /// No `claude` binary was found on `PATH`. Show "Claude Code isn't
    /// installed" rather than a generic failure.
    #[error("the Claude Code CLI (`claude`) was not found on PATH")]
    ClaudeNotInstalled,

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
// claude_binary — PATH lookup
// ---------------------------------------------------------------------------

/// Locate the `claude` binary on `PATH`. `None` if it isn't installed, so the
/// caller can show "Claude Code isn't installed" instead of a generic
/// terminal/process failure.
pub fn claude_binary() -> Option<PathBuf> {
    find_on_path("claude")
}

/// Locate an executable named `name` on `PATH`, honoring `PATHEXT` on Windows
/// (so `claude` resolves to an npm-shimmed `claude.cmd` or `claude.exe`, not
/// just a literal extension-less `claude` file, which rarely exists on
/// Windows).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let candidates = exe_candidates(name);
    for dir in std::env::split_paths(&path_var) {
        for candidate in &candidates {
            let full = dir.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

#[cfg(windows)]
fn exe_candidates(name: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut out = vec![name.to_string()];
    for ext in pathext.split(';') {
        if ext.is_empty() {
            continue;
        }
        out.push(format!("{name}{}", ext.to_ascii_lowercase()));
    }
    out
}

#[cfg(not(windows))]
fn exe_candidates(name: &str) -> Vec<String> {
    vec![name.to_string()]
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
        /// `true` when the spawned child *is* the terminal window (Linux: we
        /// spawn the terminal emulator directly), so the child process
        /// exiting means the window closed and can be used to detect
        /// cancellation. `false` when the spawned process only hands off to
        /// a window it doesn't own the lifetime of (macOS: `osascript`
        /// returns as soon as it has told Terminal.app to run the script,
        /// not when that window closes).
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
fn windows_command_line(config_dir: &Path) -> String {
    format!(
        "/c start \"Add Claude account\" cmd /c \"set \"CLAUDE_CONFIG_DIR={}\" && claude auth login\"",
        config_dir.display()
    )
}

fn windows_launch_plan(config_dir: &Path) -> LaunchPlan {
    LaunchPlan::WindowsRaw {
        program: "cmd.exe".to_string(),
        raw_args: windows_command_line(config_dir),
    }
}

/// The `osascript -e '<script>'` invocation for macOS, as an argv pair.
fn macos_launch_plan(config_dir: &Path) -> LaunchPlan {
    let shell_cmd = format!(
        "export CLAUDE_CONFIG_DIR={}; claude auth login",
        shell_quote_single(&config_dir.display().to_string())
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
/// exist on `PATH` by the caller). `gnome-terminal` uses the modern `--`
/// separator; every other terminal in [`LINUX_TERMINALS`] accepts the older
/// `-e <program> <args...>` form (an argv, not a shell string — passing the
/// whole shell command as one `-e` argument is a common bug: several of
/// these terminals then try to exec a literal file named that whole string).
fn linux_launch_plan(terminal: &str, config_dir: &Path) -> LaunchPlan {
    let shell_cmd = format!(
        "export CLAUDE_CONFIG_DIR={}; claude auth login",
        shell_quote_single(&config_dir.display().to_string())
    );
    let args = if terminal == "gnome-terminal" {
        vec!["--".to_string(), "bash".to_string(), "-c".to_string(), shell_cmd]
    } else {
        vec!["-e".to_string(), "bash".to_string(), "-c".to_string(), shell_cmd]
    };
    LaunchPlan::Argv {
        program: terminal.to_string(),
        args,
        tracks_window_lifetime: true,
    }
}

/// Find the first [`LINUX_TERMINALS`] entry for which `exists` returns
/// `true`. `exists` is injected so this selection logic is unit-testable
/// without depending on what is actually installed on the machine running
/// the tests.
fn find_linux_terminal(exists: impl Fn(&str) -> bool) -> Option<&'static str> {
    LINUX_TERMINALS.iter().copied().find(|t| exists(t))
}

/// Build the launch plan for the current platform. `Err(NoTerminalAvailable)`
/// on Linux/WSL when none of [`LINUX_TERMINALS`] is on `PATH`, and (out of
/// caution, though unreached on this crate's supported targets) on any
/// platform this crate doesn't otherwise recognize.
fn build_launch_plan(config_dir: &Path) -> Result<LaunchPlan, LoginError> {
    match Platform::detect() {
        Platform::Windows => Ok(windows_launch_plan(config_dir)),
        Platform::Macos => Ok(macos_launch_plan(config_dir)),
        Platform::Linux | Platform::Wsl => {
            let terminal = find_linux_terminal(|name| find_on_path(name).is_some())
                .ok_or(LoginError::NoTerminalAvailable)?;
            Ok(linux_launch_plan(terminal, config_dir))
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
    std::process::Command::new(claude_path)
        .args(["auth", "status"])
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .output()
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
    let deadline = Instant::now() + LOGIN_TIMEOUT;

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
/// Never touches this user's real `~/.claude` / `~/.claude.json` /
/// `~/.claude-swap-backup` — every filesystem path this function or its
/// helpers resolve is under the temp directory it creates for itself.
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

pub async fn interactive_login() -> Result<LoginOutcome, LoginError> {
    let claude_path = claude_binary().ok_or(LoginError::ClaudeNotInstalled)?;

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

    let plan = build_launch_plan(&temp_path)?;

    let blocking_claude_path = claude_path.clone();
    let blocking_temp_path = temp_path.clone();
    let poll_result = tokio::task::spawn_blocking(move || {
        run_login_blocking(&blocking_claude_path, &blocking_temp_path, plan)
    })
    .await;

    let credentials = match poll_result {
        Ok(Ok(creds)) => creds,
        Ok(Err(e)) => {
            log::info!("interactive login did not complete: {}", login_outcome_kind(&e));
            return Err(e);
            // `temp_dir` drops here, deleting the temp config dir.
        }
        Err(_join_err) => {
            log::warn!("interactive login worker task did not finish cleanly");
            return Err(LoginError::Io(io::Error::other("login worker task did not finish cleanly")));
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
        email: identity.as_ref().and_then(|i| i.email.clone()),
        organization_uuid: identity.as_ref().and_then(|i| i.organization_uuid.clone()),
    })
    // `temp_dir` drops here on the success path too.
}

/// A short, content-free label for a [`LoginError`], for the one log line
/// `interactive_login` emits on failure. Never includes file contents.
fn login_outcome_kind(e: &LoginError) -> &'static str {
    match e {
        LoginError::ClaudeNotInstalled => "claude-not-installed",
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

    #[test]
    fn windows_command_line_has_no_spaces_form() {
        let dir = Path::new(r"C:\Users\me\AppData\Local\Temp\claude-login-abc123");
        let line = windows_command_line(dir);
        assert_eq!(
            line,
            "/c start \"Add Claude account\" cmd /c \"set \"CLAUDE_CONFIG_DIR=C:\\Users\\me\\AppData\\Local\\Temp\\claude-login-abc123\" && claude auth login\""
        );
        // Uses /c (not /k) on both the outer `start` and the inner `cmd` so
        // the window closes itself once login finishes.
        assert!(line.starts_with("/c start"));
        assert!(line.contains("cmd /c \""));
        assert!(!line.contains("/k"));
    }

    #[test]
    fn windows_command_line_quotes_path_containing_spaces() {
        let dir = Path::new(r"C:\Users\Jane Doe\AppData\Local\Temp\claude-login-xyz");
        let line = windows_command_line(dir);
        // The whole `VAR=value` assignment is quoted (not just the value),
        // so the quote characters never become part of the stored value.
        assert!(line.contains("set \"CLAUDE_CONFIG_DIR=C:\\Users\\Jane Doe\\AppData\\Local\\Temp\\claude-login-xyz\""));
    }

    #[test]
    fn windows_launch_plan_targets_cmd_exe_with_raw_args() {
        let dir = Path::new(r"C:\temp\abc");
        match windows_launch_plan(dir) {
            LaunchPlan::WindowsRaw { program, raw_args } => {
                assert_eq!(program, "cmd.exe");
                assert_eq!(raw_args, windows_command_line(dir));
            }
            other => panic!("expected WindowsRaw, got {other:?}"),
        }
    }

    // -- macOS command construction ------------------------------------------

    #[test]
    fn macos_launch_plan_uses_osascript_and_does_not_track_window_lifetime() {
        let dir = Path::new("/tmp/claude-login-abc");
        match macos_launch_plan(dir) {
            LaunchPlan::Argv {
                program,
                args,
                tracks_window_lifetime,
            } => {
                assert_eq!(program, "osascript");
                assert_eq!(args[0], "-e");
                assert!(args[1].contains("tell application \"Terminal\" to do script"));
                assert!(args[1].contains("CLAUDE_CONFIG_DIR="));
                assert!(args[1].contains("claude auth login"));
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
        let dir = Path::new("/tmp/weird\"dir");
        match macos_launch_plan(dir) {
            LaunchPlan::Argv { args, .. } => {
                // The raw `"` from the path must appear escaped as `\"` in
                // the AppleScript string literal, not as a bare quote that
                // would terminate the literal early.
                assert!(args[1].contains("\\\""));
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    // -- Linux terminal selection ---------------------------------------------

    #[test]
    fn find_linux_terminal_honors_preference_order() {
        // Pretend both gnome-terminal and xterm exist: x-terminal-emulator
        // is not present, so gnome-terminal (next in LINUX_TERMINALS) wins.
        let found = find_linux_terminal(|name| matches!(name, "gnome-terminal" | "xterm"));
        assert_eq!(found, Some("gnome-terminal"));
    }

    #[test]
    fn find_linux_terminal_falls_back_to_last_resort_xterm() {
        let found = find_linux_terminal(|name| name == "xterm");
        assert_eq!(found, Some("xterm"));
    }

    #[test]
    fn find_linux_terminal_none_when_nothing_present() {
        assert_eq!(find_linux_terminal(|_| false), None);
    }

    #[test]
    fn linux_launch_plan_gnome_terminal_uses_double_dash_separator() {
        let dir = Path::new("/tmp/claude-login-abc");
        match linux_launch_plan("gnome-terminal", dir) {
            LaunchPlan::Argv {
                program,
                args,
                tracks_window_lifetime,
            } => {
                assert_eq!(program, "gnome-terminal");
                assert_eq!(args[0], "--");
                assert_eq!(args[1], "bash");
                assert_eq!(args[2], "-c");
                assert!(args[3].contains("CLAUDE_CONFIG_DIR="));
                assert!(args[3].contains("claude auth login"));
                // We spawn the terminal emulator directly here: its exit
                // really does mean the window closed.
                assert!(tracks_window_lifetime);
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    #[test]
    fn linux_launch_plan_xterm_uses_dash_e_argv_form() {
        let dir = Path::new("/tmp/claude-login-abc");
        match linux_launch_plan("xterm", dir) {
            LaunchPlan::Argv { program, args, .. } => {
                assert_eq!(program, "xterm");
                // -e takes a program + its own argv, NOT one shell-string
                // argument — passing `"bash -c '...'"` as a single -e
                // argument is the classic bug this shape avoids.
                assert_eq!(args, vec!["-e", "bash", "-c", args[3].as_str()]);
            }
            other => panic!("expected Argv, got {other:?}"),
        }
    }

    #[test]
    fn linux_shell_command_quotes_path_containing_spaces_and_quotes() {
        let dir = Path::new("/tmp/Jane's dir");
        match linux_launch_plan("xterm", dir) {
            LaunchPlan::Argv { args, .. } => {
                let shell_cmd = &args[3];
                // Single-quoted with the embedded `'` escaped via '\''.
                assert!(shell_cmd.contains("'/tmp/Jane'\\''s dir'"));
            }
            other => panic!("expected Argv, got {other:?}"),
        }
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

    // -- claude_binary / find_on_path ------------------------------------------

    #[test]
    fn find_on_path_locates_a_planted_executable() {
        let _lock = crate::test_support::env_lock();
        let dir = TempDir::new().unwrap();
        #[cfg(windows)]
        let file_name = "claude.cmd";
        #[cfg(not(windows))]
        let file_name = "claude";
        std::fs::write(dir.path().join(file_name), b"@echo off\n").unwrap();

        let _path_guard = crate::test_support::EnvGuard::set(
            "PATH",
            dir.path().to_str().expect("utf8 temp path"),
        );

        let found = claude_binary();
        assert_eq!(found, Some(dir.path().join(file_name)));
    }

    #[test]
    fn find_on_path_none_when_absent() {
        let _lock = crate::test_support::env_lock();
        let dir = TempDir::new().unwrap();
        // Empty directory: nothing named `claude*` in it.
        let _path_guard = crate::test_support::EnvGuard::set(
            "PATH",
            dir.path().to_str().expect("utf8 temp path"),
        );

        assert_eq!(claude_binary(), None);
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
        assert_eq!(login_outcome_kind(&LoginError::ClaudeNotInstalled), "claude-not-installed");
        assert_eq!(login_outcome_kind(&LoginError::NoTerminalAvailable), "no-terminal-available");
        assert_eq!(login_outcome_kind(&LoginError::Cancelled), "cancelled");
        assert_eq!(login_outcome_kind(&LoginError::TimedOut), "timed-out");
        assert_eq!(
            login_outcome_kind(&LoginError::BadCredential("missing accessToken")),
            "bad-credential"
        );
    }
}
