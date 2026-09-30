//! Installing and removing the `claude` command.
//!
//! "Install" means three things, all reversible and all opt-in:
//! 1. copy the shim that ships next to the app into `~/.cc-logins/bin/` as
//!    `claude` (plus one `claude-<slug>` per account),
//! 2. put that directory first on the user's `PATH` — the per-user registry
//!    value on Windows, a marked block in the shell startup files elsewhere,
//! 3. keep the copies current: the app refreshes them at every start, so an
//!    app update also updates the command.
//!
//! Nothing here reads or writes a Claude credential. The shim only chooses
//! which folder Claude Code runs with.
//!
//! The PATH and rc-file rules are pure functions ([`path_list`], [`rc_block`],
//! [`first_claude_dir`]) so they are tested without touching the real
//! registry or the developer's dotfiles.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::shim_core::SHIM_STEM;

/// File stem of the shim binary the installer places next to the app.
pub const SHIM_BINARY_STEM: &str = "cc-logins-shim";

/// Marker for copies retired while still running (Windows cannot replace a
/// running executable, but it can rename one). Swept at the next start.
const RETIRED_MARKER: &str = ".cc-logins-old-";

/// `name` with this platform's executable extension.
pub fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// The shim that shipped next to this app's executable, if this build has
/// one. Development builds started with `cargo run` usually do not.
pub fn bundled_shim() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(exe_name(SHIM_BINARY_STEM));
    candidate.is_file().then_some(candidate)
}

/// Command names for the plain shim and each launcher slug.
pub fn command_names(launchers: &[String]) -> Vec<String> {
    std::iter::once(SHIM_STEM.to_string())
        .chain(launchers.iter().map(|slug| format!("{SHIM_STEM}-{slug}")))
        .collect()
}

/// What [`materialize`] changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Materialized {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

/// Make `bin_dir` hold exactly one up-to-date copy of `source` per command
/// name: `claude`, then `claude-<slug>` for each launcher.
///
/// Copies whose bytes already match are left alone, so this is cheap to run
/// at every start. Launchers for accounts that no longer exist are removed.
pub fn materialize(
    bin_dir: &Path,
    source: &Path,
    launchers: &[String],
) -> io::Result<Materialized> {
    fs::create_dir_all(bin_dir)?;
    let bytes = fs::read(source)?;
    let want = Sha256::digest(&bytes);
    let wanted: Vec<String> = command_names(launchers)
        .iter()
        .map(|name| exe_name(name))
        .collect();

    let mut outcome = Materialized::default();
    for name in &wanted {
        let dest = bin_dir.join(name);
        let current = fs::read(&dest).ok().map(|b| Sha256::digest(&b));
        if current.as_ref() == Some(&want) {
            continue;
        }
        install_copy(&bytes, &dest)?;
        outcome.written.push(dest);
    }

    for entry in fs::read_dir(bin_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.contains(RETIRED_MARKER) {
            // Still running if this fails; the next start tries again.
            let _ = fs::remove_file(&path);
        } else if is_command_file(&name) && !wanted.contains(&name) {
            remove_or_retire(&path)?;
            outcome.removed.push(path);
        }
    }
    Ok(outcome)
}

/// Remove every copy from `bin_dir`. Running copies are renamed out of the
/// way and removed at the next start.
pub fn clear_bin_dir(bin_dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(bin_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_command_file(&name) || name.contains(RETIRED_MARKER) {
            remove_or_retire(&entry.path())?;
        }
    }
    Ok(())
}

/// True for a file this module manages: `claude` or `claude-<slug>`, with the
/// platform's executable extension.
fn is_command_file(name: &str) -> bool {
    let stem = if cfg!(windows) {
        match name.strip_suffix(".exe") {
            Some(stem) => stem,
            None => return false,
        }
    } else {
        name
    };
    stem == SHIM_STEM
        || stem
            .strip_prefix(SHIM_STEM)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|slug| !slug.is_empty() && !slug.contains('.'))
}

fn install_copy(bytes: &[u8], dest: &Path) -> io::Result<()> {
    let staged = crate::durable_fs::stage_sibling(dest, bytes, Some(0o755))?;
    if cfg!(windows) && dest.exists() {
        retire(dest)?;
    }
    staged.commit()?;
    #[cfg(target_os = "macos")]
    strip_quarantine(dest);
    Ok(())
}

fn remove_or_retire(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) if cfg!(windows) => retire(path),
        Err(error) => Err(error),
    }
}

/// Rename a (possibly running) copy aside so its name is free.
fn retire(path: &Path) -> io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    fs::rename(
        path,
        path.with_file_name(format!("{name}{RETIRED_MARKER}{nanos}")),
    )
}

/// Files the app writes never carry quarantine unless it propagates from the
/// app itself; clear it anyway so Gatekeeper never blocks the command.
#[cfg(target_os = "macos")]
fn strip_quarantine(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: both arguments are valid NUL-terminated strings that outlive
    // the call; a missing attribute is an ignored error, not UB.
    unsafe {
        libc::removexattr(c_path.as_ptr(), c"com.apple.quarantine".as_ptr(), 0);
    }
}

// ---------------------------------------------------------------------------
// PATH lists (pure)
// ---------------------------------------------------------------------------

/// Editing a `PATH`-style list without disturbing the rest of it.
pub mod path_list {
    fn separator(windows: bool) -> char {
        if windows {
            ';'
        } else {
            ':'
        }
    }

    fn key(entry: &str, windows: bool) -> String {
        let trimmed = entry.trim().trim_end_matches(['/', '\\']);
        if windows {
            trimmed.replace('/', "\\").to_lowercase()
        } else {
            trimmed.to_string()
        }
    }

    /// Whether `list` already names `dir`.
    pub fn contains(list: &str, dir: &str, windows: bool) -> bool {
        let want = key(dir, windows);
        list.split(separator(windows))
            .any(|entry| key(entry, windows) == want)
    }

    /// `dir` first, then every other entry of `list` in its original form
    /// (an existing copy of `dir` moves to the front; empty entries drop).
    pub fn prepend(list: &str, dir: &str, windows: bool) -> String {
        let sep = separator(windows);
        let want = key(dir, windows);
        let rest = list
            .split(sep)
            .filter(|entry| !entry.trim().is_empty() && key(entry, windows) != want);
        std::iter::once(dir)
            .chain(rest)
            .collect::<Vec<_>>()
            .join(&sep.to_string())
    }

    /// `list` without `dir`.
    pub fn remove(list: &str, dir: &str, windows: bool) -> String {
        let sep = separator(windows);
        let want = key(dir, windows);
        list.split(sep)
            .filter(|entry| !entry.trim().is_empty() && key(entry, windows) != want)
            .collect::<Vec<_>>()
            .join(&sep.to_string())
    }
}

/// The first directory in `dirs` holding one of `candidates`, i.e. where a
/// new shell finds `claude`. `is_file` is injected so this stays pure.
pub fn first_claude_dir(
    dirs: &[PathBuf],
    candidates: &[String],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    dirs.iter()
        .flat_map(|dir| candidates.iter().map(move |name| dir.join(name)))
        .find(|path| is_file(path))
}

// ---------------------------------------------------------------------------
// Shell startup files (pure)
// ---------------------------------------------------------------------------

/// The marked block this app adds to shell startup files.
pub mod rc_block {
    pub const BEGIN: &str = "# >>> cc-logins >>>";
    pub const END: &str = "# <<< cc-logins <<<";
    const NOTE: &str =
        "# Added by CC Logins so `claude` uses the account you pick. Remove it from Settings.";

    /// `content` with any existing block replaced by a fresh one at the end.
    /// Appended last so it runs after anything else that edits `PATH`.
    pub fn apply(content: &str, body: &str) -> String {
        let mut out = remove(content);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{BEGIN}\n{NOTE}\n{body}\n{END}\n"));
        out
    }

    /// `content` without the block. A `BEGIN` with no `END` is left as it
    /// is: deleting to the end of someone's file is not a guess to make.
    pub fn remove(content: &str) -> String {
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let begin = lines.iter().position(|l| l.trim_end() == BEGIN);
        let end = lines.iter().position(|l| l.trim_end() == END);
        match (begin, end) {
            (Some(b), Some(e)) if e > b => {
                let mut out: String = lines[..b].concat();
                out.push_str(&lines[e + 1..].concat());
                out
            }
            _ => content.to_string(),
        }
    }

    pub fn contains(content: &str) -> bool {
        content.lines().any(|l| l.trim_end() == BEGIN)
    }

    fn escape_double_quoted(dir: &str) -> String {
        let mut out = String::with_capacity(dir.len());
        for ch in dir.chars() {
            if matches!(ch, '\\' | '"' | '$' | '`') {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }

    /// POSIX-shell body (bash, zsh, sh).
    pub fn sh_body(dir: &str) -> String {
        format!("export PATH=\"{}:$PATH\"", escape_double_quoted(dir))
    }

    /// fish body.
    pub fn fish_body(dir: &str) -> String {
        format!("set -gx PATH \"{}\" $PATH", escape_double_quoted(dir))
    }
}

/// Which syntax a startup file takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcSyntax {
    Sh,
    Fish,
}

/// One startup file this app may edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcTarget {
    pub path: PathBuf,
    pub syntax: RcSyntax,
    /// Create the file when missing (it is the user's own shell's file).
    pub create: bool,
}

/// Startup files to consider, for a user whose login shell is `shell`
/// (`$SHELL`). Files for other shells are edited only when they exist.
pub fn rc_targets(home: &Path, shell: Option<&str>, macos: bool) -> Vec<RcTarget> {
    let shell_name = shell
        .and_then(|s| Path::new(s).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is = |name: &str| shell_name == name;
    let sh = |rel: &str, create: bool| RcTarget {
        path: home.join(rel),
        syntax: RcSyntax::Sh,
        create,
    };

    let mut targets = vec![sh(".zshrc", is("zsh"))];
    if macos {
        targets.push(sh(".zprofile", false));
        targets.push(sh(".bash_profile", is("bash")));
    } else {
        targets.push(sh(".bashrc", is("bash")));
        targets.push(sh(".profile", false));
    }
    targets.push(RcTarget {
        path: home
            .join(".config")
            .join("fish")
            .join("conf.d")
            .join("cc-logins.fish"),
        syntax: RcSyntax::Fish,
        create: is("fish"),
    });
    targets
}

/// Add or refresh the block in each applicable file. Returns the files
/// changed. Symlinked dotfiles are edited in place at their target, never
/// replaced, so a dotfiles repository keeps working.
pub fn install_rc_blocks(targets: &[RcTarget], dir: &str) -> io::Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for target in targets {
        let exists = target.path.exists();
        if !exists && !target.create {
            continue;
        }
        let body = match target.syntax {
            RcSyntax::Sh => rc_block::sh_body(dir),
            RcSyntax::Fish => rc_block::fish_body(dir),
        };
        let real = if exists {
            fs::canonicalize(&target.path)?
        } else {
            target.path.clone()
        };
        let before = if exists {
            fs::read_to_string(&real)?
        } else {
            String::new()
        };
        let after = rc_block::apply(&before, &body);
        if after == before {
            continue;
        }
        if exists {
            let backup = backup_path(&real);
            if !backup.exists() {
                fs::copy(&real, &backup)?;
            }
        }
        crate::durable_fs::stage_sibling(&real, after.as_bytes(), Some(0o644))?.commit()?;
        changed.push(target.path.clone());
    }
    Ok(changed)
}

/// Remove the block from every file that has it; delete the fish file this
/// app owns outright. Returns the files changed.
pub fn uninstall_rc_blocks(targets: &[RcTarget]) -> io::Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for target in targets {
        if !target.path.exists() {
            continue;
        }
        let real = fs::canonicalize(&target.path)?;
        let before = fs::read_to_string(&real)?;
        if !rc_block::contains(&before) {
            continue;
        }
        let after = rc_block::remove(&before);
        if target.syntax == RcSyntax::Fish && after.trim().is_empty() {
            fs::remove_file(&real)?;
        } else {
            crate::durable_fs::stage_sibling(&real, after.as_bytes(), Some(0o644))?.commit()?;
        }
        changed.push(target.path.clone());
    }
    Ok(changed)
}

/// Whether any target already carries the block.
pub fn rc_blocks_present(targets: &[RcTarget]) -> bool {
    targets.iter().any(|t| {
        fs::read_to_string(&t.path)
            .map(|c| rc_block::contains(&c))
            .unwrap_or(false)
    })
}

fn backup_path(file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    file.with_file_name(format!("{name}.cc-logins-backup"))
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Whether a new terminal gets this app's `claude`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum CliHealth {
    /// Not on `PATH`. Plain `claude` runs the default account.
    NotInstalled,
    /// First on `PATH` for new terminals.
    Installed,
    /// On `PATH`, but another `claude` comes first.
    Shadowed { by: String },
}

// ---------------------------------------------------------------------------
// Platform glue
// ---------------------------------------------------------------------------

/// Put `bin_dir` first on the user's `PATH`.
pub fn install_path(bin_dir: &Path) -> io::Result<()> {
    let dir = bin_dir.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        let (current, kind) = win::user_path_raw()?;
        let updated = path_list::prepend(&current, &dir, true);
        if updated != current {
            win::set_user_path(&updated, kind)?;
            win::broadcast_environment_change();
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        install_rc_blocks(&unix_targets(), &dir).map(|_| ())
    }
}

/// Take `bin_dir` off the user's `PATH`.
pub fn uninstall_path(bin_dir: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let dir = bin_dir.to_string_lossy().into_owned();
        let (current, kind) = win::user_path_raw()?;
        let updated = path_list::remove(&current, &dir, true);
        if updated != current {
            win::set_user_path(&updated, kind)?;
            win::broadcast_environment_change();
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = bin_dir;
        uninstall_rc_blocks(&unix_targets()).map(|_| ())
    }
}

/// Where new terminals will find `claude`, judged against `bin_dir`.
pub fn health(bin_dir: &Path) -> CliHealth {
    #[cfg(windows)]
    {
        windows_health(bin_dir)
    }
    #[cfg(not(windows))]
    {
        unix_health(bin_dir)
    }
}

#[cfg(windows)]
fn windows_health(bin_dir: &Path) -> CliHealth {
    let dir = bin_dir.to_string_lossy().into_owned();
    let raw = win::user_path_raw().map(|(p, _)| p).unwrap_or_default();
    if !path_list::contains(&raw, &dir, true) {
        return CliHealth::NotInstalled;
    }
    // Windows builds a new process's PATH as the machine list followed by the
    // user list, so a `claude` anywhere in the machine list wins.
    let system = win::system_path_expanded().unwrap_or_default();
    let user = win::user_path_expanded().unwrap_or_default();
    let dirs: Vec<PathBuf> = system
        .split(';')
        .chain(user.split(';'))
        .filter(|e| !e.trim().is_empty())
        .map(PathBuf::from)
        .collect();
    let pathext = crate::sys_env::env_non_empty("PATHEXT")
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    let candidates: Vec<String> = pathext
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|ext| format!("{SHIM_STEM}{}", ext.to_ascii_lowercase()))
        .collect();
    judge(
        first_claude_dir(&dirs, &candidates, &|p| p.is_file()),
        bin_dir,
    )
}

/// Installed when the first `claude` found is ours.
fn judge(first: Option<PathBuf>, bin_dir: &Path) -> CliHealth {
    match first {
        Some(found)
            if found
                .parent()
                .is_some_and(|parent| crate::shim_core::same_dir(parent, bin_dir)) =>
        {
            CliHealth::Installed
        }
        Some(found) => CliHealth::Shadowed {
            by: found.display().to_string(),
        },
        // Nothing found at all: the registry names our directory but the
        // copy is missing. Reinstalling fixes it; say "not installed".
        None => CliHealth::NotInstalled,
    }
}

#[cfg(not(windows))]
fn unix_targets() -> Vec<RcTarget> {
    let shell = std::env::var("SHELL").ok();
    rc_targets(
        &crate::sys_env::home_dir(),
        shell.as_deref(),
        cfg!(target_os = "macos"),
    )
}

#[cfg(not(windows))]
fn unix_health(bin_dir: &Path) -> CliHealth {
    if !rc_blocks_present(&unix_targets()) {
        return CliHealth::NotInstalled;
    }
    match login_shell_claude() {
        Some(found) => judge(Some(found), bin_dir),
        // The shell could not be asked (slow rc file, no $SHELL). The block
        // is in place, which is the part this app controls.
        None => CliHealth::Installed,
    }
}

/// `command -v claude` in a fresh interactive login shell: what a new
/// terminal would run. Bounded to a few seconds so a slow rc file cannot hang
/// the settings screen.
#[cfg(not(windows))]
fn login_shell_claude() -> Option<PathBuf> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let shell = crate::sys_env::env_non_empty("SHELL")?;
    let mut child = Command::new(shell)
        .args(["-lic", "command -v claude"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with('/'))
        .map(PathBuf::from)
}

#[cfg(windows)]
mod win {
    //! The per-user `Path` registry value, edited the way the System
    //! Properties dialog does: read raw (unexpanded, keeping its type) and
    //! write back with the same type, then announce the change so Explorer
    //! and new terminals pick it up. Never `setx`, which truncates at 1024
    //! characters, and never PowerShell's SetEnvironmentVariable, which
    //! flattens `%VAR%` entries.

    use std::io;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_EXPAND_SZ,
        RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };

    const USER_ENV: &str = "Environment";
    const SYSTEM_ENV: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    const PATH_VALUE: &str = "Path";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A string value and its registry type, or `None` when absent.
    /// `expand` asks the registry to expand `%VAR%` references.
    fn read_string(
        root: HKEY,
        subkey: &str,
        name: &str,
        expand: bool,
    ) -> io::Result<Option<(String, u32)>> {
        let subkey = wide(subkey);
        let name = wide(name);
        // Expansion only applies without RRF_NOEXPAND, and the registry then
        // reports REG_EXPAND_SZ values as REG_SZ, so only that type is asked
        // for; asking for REG_EXPAND_SZ without RRF_NOEXPAND is an error.
        let flags = if expand {
            RRF_RT_REG_SZ
        } else {
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND
        };
        let mut kind: u32 = 0;
        let mut size: u32 = 0;
        // SAFETY: a size query: valid NUL-terminated names, null data buffer.
        let status = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                name.as_ptr(),
                flags,
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        loop {
            let mut buffer: Vec<u16> = vec![0; (size as usize).div_ceil(2) + 1];
            let mut bytes = (buffer.len() * 2) as u32;
            // SAFETY: `buffer` is writable for `bytes` bytes.
            let status = unsafe {
                RegGetValueW(
                    root,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    flags,
                    &mut kind,
                    buffer.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if status == ERROR_MORE_DATA {
                // The value grew between the two calls.
                size = bytes;
                continue;
            }
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let len = (bytes as usize / 2).min(buffer.len());
            let text = String::from_utf16_lossy(&buffer[..len]);
            return Ok(Some((text.trim_end_matches('\0').to_string(), kind)));
        }
    }

    /// The user's `Path`, unexpanded, and its type (`REG_EXPAND_SZ` if the
    /// value does not exist yet, which is what Windows itself creates).
    pub fn user_path_raw() -> io::Result<(String, u32)> {
        Ok(read_string(HKEY_CURRENT_USER, USER_ENV, PATH_VALUE, false)?
            .unwrap_or((String::new(), REG_EXPAND_SZ)))
    }

    pub fn user_path_expanded() -> io::Result<String> {
        Ok(read_string(HKEY_CURRENT_USER, USER_ENV, PATH_VALUE, true)?
            .map(|(value, _)| value)
            .unwrap_or_default())
    }

    pub fn system_path_expanded() -> io::Result<String> {
        Ok(
            read_string(HKEY_LOCAL_MACHINE, SYSTEM_ENV, PATH_VALUE, true)?
                .map(|(value, _)| value)
                .unwrap_or_default(),
        )
    }

    pub fn set_user_path(value: &str, kind: u32) -> io::Result<()> {
        let subkey = wide(USER_ENV);
        let name = wide(PATH_VALUE);
        let data = wide(value);
        // SAFETY: valid NUL-terminated names; `data` is readable for the
        // byte length given, including its terminating NUL as the API wants.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                kind,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    /// Tell Explorer (and so every terminal it starts from now on) that the
    /// environment changed. Hung windows are skipped rather than waited on.
    pub fn broadcast_environment_change() {
        let area = wide("Environment");
        let mut result: usize = 0;
        // SAFETY: `area` outlives the call; the result pointer is valid.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                area.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                2000,
                &mut result,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_are_claude_then_launchers() {
        assert_eq!(
            command_names(&["work".to_string(), "home".to_string()]),
            vec!["claude", "claude-work", "claude-home"]
        );
    }

    #[test]
    fn command_file_recognition() {
        let ext = if cfg!(windows) { ".exe" } else { "" };
        assert!(is_command_file(&format!("claude{ext}")));
        assert!(is_command_file(&format!("claude-work{ext}")));
        assert!(!is_command_file(&format!("claude-{ext}")));
        assert!(!is_command_file("claude.json"));
        assert!(!is_command_file("notes.txt"));
    }

    #[test]
    fn path_list_prepend_moves_existing_entry_to_front() {
        assert_eq!(
            path_list::prepend("/usr/bin:/x/bin/:/opt", "/x/bin", false),
            "/x/bin:/usr/bin:/opt"
        );
        assert_eq!(path_list::prepend("", "/x/bin", false), "/x/bin");
    }

    #[test]
    fn path_list_windows_is_case_and_slash_insensitive_and_keeps_vars() {
        let list = r"%USERPROFILE%\.local\bin;C:\USERS\U\.CC-LOGINS\BIN\;;C:\Tools";
        let got = path_list::prepend(list, r"C:\Users\u\.cc-logins\bin", true);
        assert_eq!(
            got,
            r"C:\Users\u\.cc-logins\bin;%USERPROFILE%\.local\bin;C:\Tools"
        );
        assert!(path_list::contains(&got, "c:/users/u/.cc-logins/bin", true));
        assert_eq!(
            path_list::remove(&got, r"C:\Users\u\.cc-logins\bin", true),
            r"%USERPROFILE%\.local\bin;C:\Tools"
        );
    }

    #[test]
    fn first_claude_dir_follows_order() {
        let dirs = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let names = vec!["claude".to_string()];
        let found = first_claude_dir(&dirs, &names, &|p| {
            p == Path::new("/b").join("claude") || p == Path::new("/c").join("claude")
        });
        assert_eq!(found, Some(Path::new("/b").join("claude")));
    }

    #[test]
    fn judge_distinguishes_ours_from_shadowing() {
        let bin = PathBuf::from("/home/u/.cc-logins/bin");
        assert_eq!(judge(Some(bin.join("claude")), &bin), CliHealth::Installed);
        assert_eq!(
            judge(Some(PathBuf::from("/usr/local/bin/claude")), &bin),
            CliHealth::Shadowed {
                by: PathBuf::from("/usr/local/bin/claude").display().to_string()
            }
        );
        assert_eq!(judge(None, &bin), CliHealth::NotInstalled);
    }

    #[test]
    fn rc_block_apply_is_idempotent_and_removable() {
        let original = "export FOO=1\nalias ll='ls -l'";
        let body = rc_block::sh_body("/home/u/.cc-logins/bin");
        let once = rc_block::apply(original, &body);
        let twice = rc_block::apply(&once, &body);
        assert_eq!(once, twice);
        assert!(once.starts_with("export FOO=1\nalias ll='ls -l'\n# >>> cc-logins >>>\n"));
        assert!(once.contains("export PATH=\"/home/u/.cc-logins/bin:$PATH\"\n"));
        assert!(rc_block::contains(&once));
        assert_eq!(rc_block::remove(&once), "export FOO=1\nalias ll='ls -l'\n");
    }

    #[test]
    fn rc_block_moves_to_the_end_on_reapply() {
        let body = rc_block::sh_body("/b");
        let with_block = rc_block::apply("a\n", &body);
        let later = format!("{with_block}export PATH=\"/other:$PATH\"\n");
        let reapplied = rc_block::apply(&later, &body);
        assert!(reapplied.ends_with("# <<< cc-logins <<<\n"));
        assert!(reapplied.starts_with("a\nexport PATH=\"/other:$PATH\"\n"));
    }

    #[test]
    fn rc_block_without_end_marker_is_left_alone() {
        let broken = "a\n# >>> cc-logins >>>\nb\n";
        assert_eq!(rc_block::remove(broken), broken);
    }

    #[test]
    fn rc_bodies_escape_shell_metacharacters() {
        assert_eq!(
            rc_block::sh_body("/home/a \"b\" $c"),
            "export PATH=\"/home/a \\\"b\\\" \\$c:$PATH\""
        );
        assert_eq!(rc_block::fish_body("/h/x"), "set -gx PATH \"/h/x\" $PATH");
    }

    #[test]
    fn rc_targets_follow_the_login_shell() {
        let home = PathBuf::from("/home/u");
        let linux_bash = rc_targets(&home, Some("/bin/bash"), false);
        let created: Vec<_> = linux_bash
            .iter()
            .filter(|t| t.create)
            .map(|t| t.path.clone())
            .collect();
        assert_eq!(created, vec![home.join(".bashrc")]);

        let mac_zsh = rc_targets(&home, Some("/bin/zsh"), true);
        assert!(mac_zsh
            .iter()
            .any(|t| t.path == home.join(".zshrc") && t.create));
        assert!(mac_zsh.iter().any(|t| t.path == home.join(".zprofile")));
        assert!(!mac_zsh.iter().any(|t| t.path == home.join(".bashrc")));
    }

    #[test]
    fn rc_install_and_uninstall_round_trip_on_disk() {
        let home = tempfile::TempDir::new().unwrap();
        let zshrc = home.path().join(".zshrc");
        fs::write(&zshrc, "export A=1\n").unwrap();
        let targets = rc_targets(home.path(), Some("/bin/bash"), false);

        let changed = install_rc_blocks(&targets, "/opt/cc/bin").unwrap();
        // .zshrc exists; .bashrc is the login shell's file and gets created;
        // .profile and fish do not exist and are not the login shell's.
        assert_eq!(changed, vec![zshrc.clone(), home.path().join(".bashrc")]);
        assert!(rc_blocks_present(&targets));
        assert!(home.path().join(".zshrc.cc-logins-backup").exists());
        assert!(!home.path().join(".bashrc.cc-logins-backup").exists());

        // A second install changes nothing.
        assert!(install_rc_blocks(&targets, "/opt/cc/bin")
            .unwrap()
            .is_empty());

        let removed = uninstall_rc_blocks(&targets).unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(fs::read_to_string(&zshrc).unwrap(), "export A=1\n");
        assert!(!rc_blocks_present(&targets));
    }

    #[cfg(unix)]
    #[test]
    fn rc_install_edits_a_symlinked_dotfile_in_place() {
        let home = tempfile::TempDir::new().unwrap();
        let repo = home.path().join("dotfiles");
        fs::create_dir_all(&repo).unwrap();
        let real = repo.join("zshrc");
        fs::write(&real, "export A=1\n").unwrap();
        let link = home.path().join(".zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let targets = rc_targets(home.path(), Some("/bin/zsh"), false);
        install_rc_blocks(&targets, "/opt/cc/bin").unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(rc_block::contains(&fs::read_to_string(&real).unwrap()));
    }

    #[test]
    fn materialize_writes_updates_and_prunes() {
        let root = tempfile::TempDir::new().unwrap();
        let source = root.path().join("shim-src");
        fs::write(&source, b"v1").unwrap();
        let bin = root.path().join("bin");

        let first = materialize(&bin, &source, &["work".to_string()]).unwrap();
        assert_eq!(first.written.len(), 2);
        assert_eq!(fs::read(bin.join(exe_name("claude-work"))).unwrap(), b"v1");

        // Unchanged source: nothing rewritten.
        let again = materialize(&bin, &source, &["work".to_string()]).unwrap();
        assert_eq!(again, Materialized::default());

        // New source, launcher renamed: both copies refreshed, old launcher gone.
        fs::write(&source, b"v2").unwrap();
        let third = materialize(&bin, &source, &["home".to_string()]).unwrap();
        assert_eq!(third.written.len(), 2);
        assert_eq!(third.removed, vec![bin.join(exe_name("claude-work"))]);
        assert_eq!(fs::read(bin.join(exe_name("claude"))).unwrap(), b"v2");
        assert!(!bin.join(exe_name("claude-work")).exists());

        clear_bin_dir(&bin).unwrap();
        assert!(!bin.join(exe_name("claude")).exists());
    }
}
