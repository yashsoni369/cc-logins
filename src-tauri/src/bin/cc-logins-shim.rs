//! The `claude` shim.
//!
//! CC Logins copies this binary into `~/.cc-logins/bin/` as `claude` (and as
//! `claude-<slug>` for each account). Put first on `PATH`, it starts the real
//! Claude Code with `CLAUDE_CONFIG_DIR` pointing at the account chosen in the
//! tray, so every new session uses that account. Sessions already running
//! keep theirs.
//!
//! It never reads, copies or stores a credential: Claude Code signs in to and
//! reads from each folder itself. All the shim decides is which folder, using
//! the pure rules in `shim_core.rs`, which the app compiles too.
//!
//! This binary deliberately does not link the app library, which would pull
//! Tauri and the webview into something that runs on every `claude` call. It
//! shares code by compiling the same source files through `#[path]`.

#[allow(dead_code)]
#[path = "../sys_env.rs"]
mod sys_env;

#[allow(dead_code)]
#[path = "../claude_resolve.rs"]
mod claude_resolve;

#[allow(dead_code)]
#[path = "../shim_core.rs"]
mod shim_core;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use claude_resolve::{is_executable_file, resolve_in, SearchContext};
use shim_core::{
    looks_like_wrapped_claude, parse_depth, plan, EnvAction, Invocation, LaunchPlan, PlanInput,
    ShimConfig, CONFIG_DIR_ENV, DEPTH_ENV, PROFILE_ENV, SHIM_BIN_DIR, SHIM_CONFIG_FILE,
};

/// Prints what the shim would do and exits, without starting Claude Code.
/// The app's health check runs it through the user's login shell to confirm
/// which `claude` a new terminal actually gets.
const WHICH_FLAG: &str = "--cc-logins-which";

/// Exit codes for the shim's own failures, chosen to match what a shell
/// reports for the same situations.
const EXIT_PLAN: i32 = 2;
const EXIT_EXEC: i32 = 126;
const EXIT_NOT_FOUND: i32 = 127;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<OsString> = env::args_os().collect();
    let exe = env::current_exe().ok();
    let invocation = Invocation::from_stem(&invocation_stem(exe.as_deref(), args.first()));

    let home = sys_env::cc_logins_home_dir();
    let config = std::fs::read(home.join(SHIM_CONFIG_FILE))
        .ok()
        .and_then(|bytes| ShimConfig::parse(&bytes));

    let inherited = env::var_os(CONFIG_DIR_ENV).map(|v| v.to_string_lossy().into_owned());
    let profile_env = sys_env::env_non_empty(PROFILE_ENV);
    let depth = parse_depth(env::var(DEPTH_ENV).ok().as_deref());
    let default_dir = sys_env::default_claude_config_dir();

    let decided = plan(&PlanInput {
        invocation: &invocation,
        inherited_config_dir: inherited.as_deref(),
        profile_env: profile_env.as_deref(),
        depth,
        config: config.as_ref(),
        default_config_dir: &default_dir,
    });
    let decided = match decided {
        Ok(p) => p,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_PLAN;
        }
    };

    let mut rest: Vec<OsString> = args.iter().skip(1).cloned().collect();
    let which = rest.first().is_some_and(|a| a == WHICH_FLAG);

    // VS Code's `claudeCode.claudeProcessWrapper` passes the real binary as
    // the first argument. Use it as given, unless it is this shim again.
    let wrapped = rest
        .first()
        .map(PathBuf::from)
        .filter(|p| looks_like_wrapped_claude(p) && is_executable_file(p))
        .filter(|p| !is_self(p, exe.as_deref()));
    let real = match wrapped {
        Some(path) => {
            rest.remove(0);
            path
        }
        None => match resolve_real_claude(&home, exe.as_deref(), config.as_ref()) {
            Ok(path) => path,
            Err(message) => {
                eprintln!("cc-logins: {message}");
                return EXIT_NOT_FOUND;
            }
        },
    };

    if which {
        print_which(exe.as_deref(), &decided, &real);
        return 0;
    }

    let mut command = Command::new(&real);
    command.args(&rest);
    match &decided.config_dir {
        EnvAction::Keep => {}
        EnvAction::Set(dir) => {
            command.env(CONFIG_DIR_ENV, dir);
        }
        EnvAction::Remove => {
            command.env_remove(CONFIG_DIR_ENV);
        }
    }
    command.env(DEPTH_ENV, (depth + 1).to_string());
    launch(command, &real)
}

/// The name this shim was started as: its own file stem, falling back to
/// `argv[0]`. The copies are real files, not symlinks, so the executable's
/// own name is reliable on every platform.
fn invocation_stem(exe: Option<&Path>, argv0: Option<&OsString>) -> String {
    exe.and_then(Path::file_stem)
        .or_else(|| argv0.and_then(|a| Path::new(a).file_stem()))
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Locate the real Claude Code: the app's configured path, then `PATH`, then
/// the documented install locations, never this shim or its directory.
fn resolve_real_claude(
    home: &Path,
    exe: Option<&Path>,
    config: Option<&ShimConfig>,
) -> Result<PathBuf, String> {
    let mut ctx = SearchContext::from_env();
    ctx.settings_override = config.and_then(|c| c.claude_binary.clone());
    ctx.exclude_dirs.push(home.join(SHIM_BIN_DIR));
    if let Some(exe) = exe {
        if let Some(dir) = exe.parent() {
            ctx.exclude_dirs.push(dir.to_path_buf());
        }
        ctx.exclude_files.push(exe.to_path_buf());
        if let Ok(canonical) = std::fs::canonicalize(exe) {
            ctx.exclude_files.push(canonical);
        }
    }
    resolve_in(&ctx, &is_executable_file)
        .map(|resolved| resolved.path)
        .map_err(|not_found| not_found.to_string())
}

fn is_self(candidate: &Path, exe: Option<&Path>) -> bool {
    let Some(exe) = exe else { return false };
    match (std::fs::canonicalize(candidate), std::fs::canonicalize(exe)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn print_which(exe: Option<&Path>, decided: &LaunchPlan, real: &Path) {
    let shim = exe.map(|e| e.display().to_string()).unwrap_or_default();
    let account = decided
        .account
        .map(|n| n.to_string())
        .unwrap_or_else(|| "default".to_string());
    let config_dir = match &decided.config_dir {
        EnvAction::Keep => "inherited".to_string(),
        EnvAction::Set(dir) => dir.clone(),
        EnvAction::Remove => "unset".to_string(),
    };
    println!("cc-logins-shim {}", env!("CARGO_PKG_VERSION"));
    println!("shim: {shim}");
    println!("account: {account}");
    println!("{CONFIG_DIR_ENV}: {config_dir}");
    println!("claude: {}", real.display());
}

/// Replace this process with Claude Code, so signals, the terminal and the
/// exit status all belong to Claude Code directly.
#[cfg(unix)]
fn launch(mut command: Command, real: &Path) -> i32 {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    eprintln!("cc-logins: could not start {}: {error}", real.display());
    EXIT_EXEC
}

/// Windows has no `exec`: run Claude Code as a child sharing this console,
/// and pass its exit code back unchanged.
#[cfg(windows)]
fn launch(mut command: Command, real: &Path) -> i32 {
    // Ctrl+C reaches every process on the console. Claude Code handles it
    // itself (it interrupts the current turn), so the shim must not die from
    // it and orphan the session. A handler that reports "handled" does that
    // without setting the inheritable ignore-Ctrl+C flag a NULL handler would,
    // which Claude Code would then inherit.
    unsafe extern "system" fn ignore_ctrl(_ctrl_type: u32) -> windows_sys::core::BOOL {
        1
    }
    // SAFETY: registers a handler that touches no state and returns
    // immediately; valid for the life of the process.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore_ctrl), 1);
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("cc-logins: could not start {}: {error}", real.display());
            EXIT_EXEC
        }
    }
}
