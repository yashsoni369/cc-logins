//! Detection of Claude Code credential *environments* on Windows, including
//! WSL distros.
//!
//! Claude Code on native Windows and Claude Code inside a WSL distro keep
//! completely separate logins (separate `~/.claude` / `.credentials.json`
//! trees, on separate filesystems). This module enumerates every place a
//! login could live — the native Windows realm plus one per WSL distro — so
//! the rest of the app can present them as independent "environments" to
//! switch between.
//!
//! # The hard rule this module exists to enforce
//!
//! `wsl.exe -l -q` and `wsl.exe -l -q --running` only ever *list* distros —
//! verified empirically to never start one, so both are safe to call from a
//! background poll. But touching a `\\wsl$\<distro>\...` UNC path **silently
//! boots the distro**: `Test-Path "\\wsl$\Ubuntu\home"` returned `true` while
//! Ubuntu was `Stopped`, because the mere filesystem access started the VM.
//! The same is true of `wsl.exe -d <name> -e ...` — running a command *in* a
//! distro starts it if it isn't already running.
//!
//! The rule that follows, and that every function below is structured to
//! uphold: **never touch a stopped distro's filesystem or execute inside it
//! from a polling path.** [`list_distros`] and [`detect_environments`] only
//! ever call the two `-l -q` list forms; they read [`Distro::running`] (itself
//! sourced from `--running`) and skip straight to [`EnvStatus::Asleep`] for
//! anything not already running, without dereferencing a single path inside
//! it. The only function in this module allowed to start a distro is
//! [`wake_and_read`], which is documented as an explicit, user-initiated
//! action and is never called by [`detect_environments`] or any other polling
//! path here.
//!
//! # UTF-16LE gotcha
//!
//! `wsl.exe` writes its list output as UTF-16LE (it's a native Win32 console
//! tool). Decoding that as UTF-8 (e.g. `String::from_utf8_lossy`) does not
//! error — it silently produces a string with every character separated by a
//! NUL byte, which then fails to match anything. [`decode_utf16le`] decodes
//! the byte pairs explicitly and strips a leading BOM; see its tests for a
//! worked example of the corruption this avoids.
//!
//! # UNC path vs. `wsl.exe -d <name> -e sh -c "..."`
//!
//! For reading credentials out of an already-*running* distro, this module
//! always shells out (`wsl.exe -d <name> -e sh -c "..."`) rather than reading
//! `\\wsl$\<name>\...` directly, for three reasons:
//!
//! 1. **Correctness**: the distro's `$HOME` is only reliably known *inside*
//!    the distro (it depends on the distro's default user, which this
//!    Windows process cannot observe). Running a shell inside the distro
//!    lets Linux resolve its own `$HOME` and `$CLAUDE_CONFIG_DIR`, instead of
//!    us guessing a `\\wsl$\<name>\home\<user>` path from the Windows side.
//! 2. **Speed**: the `\\wsl$` share is a 9P network redirector; a `test -f`
//!    inside the distro is a local syscall and avoids that overhead.
//! 2. **No incidental UNC access**: since [`list_distros`] already tells us
//!    whether a distro is running without touching its filesystem, using
//!    `-d`/`-e` for the follow-up read means this module never constructs a
//!    `\\wsl$` path anywhere, which makes the "never silently boot a distro"
//!    invariant trivially true by construction rather than something we have
//!    to separately audit UNC call sites for.
//!
//! # Linux-side credential path
//!
//! The path resolved inside the distro mirrors [`crate::paths::claude_config_home`]
//! and [`crate::paths::credentials_path`] exactly: `$CLAUDE_CONFIG_DIR` if set
//! and non-empty, else `$HOME/.claude`, then `.credentials.json` under that.
//! It's re-expressed as a POSIX shell fragment (`${CLAUDE_CONFIG_DIR:-$HOME/.claude}`,
//! whose `:-` has the same "unset or empty" trigger as `paths.rs`'s
//! `env_non_empty`) rather than called as a Rust function, because it has to
//! evaluate against the *distro's* environment, which only a process running
//! inside that distro can see — but it is the same two-step rule, not a
//! re-derivation of it. If `paths.rs`'s resolution rule ever changes, this
//! script needs to change with it.
//!
//! # Cross-platform no-op
//!
//! Every public function is defined on all platforms so callers never need
//! `#[cfg]` branches. Off Windows, [`is_wsl_available`] is `false`,
//! [`list_distros`] is always empty, and [`detect_environments`] returns just
//! the native environment for the host OS (credential detection there still
//! goes through [`crate::paths::credentials_path`], which is itself already
//! cross-platform).

use crate::model::{EnvKind, EnvStatus, Environment};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from shelling out to `wsl.exe`.
#[derive(Debug, thiserror::Error)]
pub enum WslError {
    /// The `wsl.exe` process itself could not be started (e.g. not found, or
    /// blocked by policy). Note this is distinct from the *command*
    /// reporting failure (a non-zero exit), which list operations treat as
    /// "empty" rather than an error — see [`list_distros`].
    #[error("failed to launch {program}: {source}")]
    Spawn {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// [`wake_and_read`] (or any WSL-specific operation) was called on a
    /// platform other than Windows, where WSL cannot exist.
    #[error("WSL is only available on Windows")]
    Unsupported,
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A WSL distro as reported by `wsl.exe -l -q[--running]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    pub name: String,
    /// Whether the distro appeared in the `--running` listing. Sourced
    /// *only* from that listing — never inferred by touching the distro.
    pub running: bool,
    /// Infrastructure distros (`docker-desktop`, `docker-desktop-data`,
    /// Podman's machine, ...) that host tooling rather than a user shell.
    /// Claude Code cannot plausibly be logged into these, so the UI should
    /// label them "ignored" rather than offer them as switchable
    /// environments.
    pub is_system: bool,
}

// `EnvStatus`, `EnvKind`, and `Environment` used to be defined locally here,
// duplicating the shapes already owned by [`crate::model`] (the single
// source of truth for what crosses the Tauri IPC boundary). This module now
// builds `crate::model::Environment` directly instead of a parallel type:
//
// - local `EnvStatus::{Live, Asleep, Ignored}` was already identical to
//   [`crate::model::EnvStatus`] and is now literally that type.
// - local `EnvironmentKind::NativeWindows` / `EnvironmentKind::Wsl(name)`
//   collapse onto [`crate::model::EnvKind::Native`] /
//   [`crate::model::EnvKind::Wsl`]; the distro name that used to live
//   *inside* the `Wsl` variant is now carried in `Environment::id` /
//   `Environment::label` instead (see [`build_environments`]).
// - local `Environment { kind, status, has_credentials }` is now
//   [`crate::model::Environment`], whose `id`/`label`/`path`/`accounts`
//   fields this module fills in; `accounts` is always empty for now, since
//   this module only *detects* environments — reading the accounts living
//   inside one is a separate, not-yet-implemented step. `has_credentials`
//   *is* still populated here, though: [`build_environments`] already runs a
//   credential probe for every running, non-system distro (to decide whether
//   there's anything worth reading later), so its `Ok` result is captured
//   into `Environment::has_credentials` instead of being discarded. `None`
//   means "not determined" (asleep/ignored distros, or a probe that
//   errored) — never conflated with `Some(false)`, "checked, nothing there".

// ---------------------------------------------------------------------------
// Pure helpers (platform-independent, always compiled and tested)
// ---------------------------------------------------------------------------
//
// Compiled everywhere so they stay unit-tested everywhere, but only *called* on
// Windows — hence the per-item `allow(dead_code)` for non-Windows targets.

/// Known infrastructure distros that back other tooling rather than hosting
/// a user's shell/login. Matched case-insensitively since WSL distro names
/// are case-preserving but not case-sensitive for most user-facing purposes.
#[cfg_attr(not(windows), allow(dead_code))]
const SYSTEM_DISTRO_NAMES: &[&str] = &[
    "docker-desktop",
    "docker-desktop-data",
    "podman-machine-default",
];

/// `true` if `name` is a known infrastructure distro (see
/// [`Distro::is_system`]) rather than a real user login surface.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_system_distro(name: &str) -> bool {
    SYSTEM_DISTRO_NAMES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
}

/// Decode `wsl.exe`'s list output, which is emitted as UTF-16LE (optionally
/// BOM-prefixed), *not* UTF-8. Decoding it as UTF-8 does not error — it
/// silently yields a string with a NUL byte after every character — so this
/// must be a real UTF-16LE decode, not `String::from_utf8_lossy`.
///
/// A trailing odd byte (a truncated final code unit) is dropped rather than
/// erroring, since this only ever feeds a best-effort list operation.
/// Unpaired/invalid surrogates decode to U+FFFD, matching
/// `String::from_utf8_lossy`'s "never fail, substitute" behavior for the
/// UTF-8 case.
#[cfg_attr(not(windows), allow(dead_code))]
fn decode_utf16le(bytes: &[u8]) -> String {
    let bytes = match bytes {
        // U+FEFF BOM, little-endian byte order: FF FE.
        [0xFF, 0xFE, rest @ ..] => rest,
        _ => bytes,
    };
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Split decoded `wsl.exe -l -q` output into distro names: one per line,
/// trimmed, blank lines (including the trailing blank line `wsl.exe` emits
/// after the last entry) dropped.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_distro_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Display-only stand-in for a WSL distro's actual `.credentials.json`
/// location. The *real* path can only be resolved correctly from inside the
/// distro (its `$HOME` isn't reliably observable from this Windows process —
/// see the module docs), so this module never attempts to compute one; this
/// constant just documents, for the UI, the same two-step rule
/// [`crate::paths::credentials_path`] uses on the native side, re-expressed
/// as the POSIX form actually evaluated by [`windows_impl::read_credentials_via_exec`].
const WSL_CREDENTIALS_PATTERN: &str = "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json";

/// Resolve the native Windows (or, off Windows, host-OS) environment.
/// The `path` reuses [`crate::paths::credentials_path`] verbatim — this is
/// the one part of environment detection that runs in *this* process's own
/// environment, so, unlike the WSL case, there is no reason to re-derive it.
fn native_environment() -> Environment {
    Environment {
        id: "native".to_string(),
        // Labelled by host OS. This said "Windows" on every platform.
        label: match crate::paths::Platform::detect() {
            crate::paths::Platform::Windows => "Windows",
            crate::paths::Platform::Macos => "macOS",
            crate::paths::Platform::Linux | crate::paths::Platform::Wsl => "Linux",
            _ => "Native",
        }
        .to_string(),
        path: crate::paths::credentials_path().display().to_string(),
        kind: EnvKind::Native,
        status: EnvStatus::Live,
        accounts: Vec::new(),
        last_seen_seconds: None,
        // Not probed here — this module's credential probe only ever runs
        // for WSL distros (see `build_environments`). "Not determined" is
        // the honest answer for the native realm at this layer.
        has_credentials: None,
    }
}

/// Build the full environment list from an already-fetched distro list and a
/// credential probe function, without either one performing any I/O of its
/// own. Split out from [`detect_environments`] purely so tests can inject
/// both — in particular, a probe that panics if ever invoked for a distro
/// that isn't `running`, which is how the "no filesystem access for a
/// stopped distro" guarantee is proven in `mod tests` below, rather than
/// merely asserted in a comment.
fn build_environments(
    distros: &[Distro],
    probe: &dyn Fn(&str) -> Result<bool, WslError>,
) -> Vec<Environment> {
    let mut environments = Vec::with_capacity(distros.len() + 1);
    environments.push(native_environment());

    for distro in distros {
        // `has_credentials` stays `None` — "not determined" — unless the
        // probe branch below actually runs and succeeds. It is never set
        // for an asleep or ignored distro, since those branches never call
        // `probe` at all.
        let mut has_credentials = None;
        let status = if distro.is_system {
            EnvStatus::Ignored
        } else if !distro.running {
            // Hard rule: a stopped distro's filesystem is never touched
            // from here. `probe` is simply not called.
            EnvStatus::Asleep
        } else {
            // Only reachable for a running, non-system distro — see the
            // module invariant. This is the same call site that proves the
            // no-auto-start invariant (via the panicking probe in `mod
            // tests` below), now also the one that populates
            // `has_credentials` on the `Environment` this loop iteration
            // produces.
            match probe(&distro.name) {
                Ok(found) => has_credentials = Some(found),
                Err(e) => {
                    log::debug!(
                        "credential probe failed for WSL distro {}: {e}",
                        distro.name
                    );
                }
            }
            EnvStatus::Live
        };
        environments.push(Environment {
            id: format!("wsl:{}", distro.name),
            label: format!("WSL · {}", distro.name),
            path: WSL_CREDENTIALS_PATTERN.to_string(),
            kind: EnvKind::Wsl,
            status,
            accounts: Vec::new(),
            last_seen_seconds: None,
            has_credentials,
        });
    }

    environments
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{decode_utf16le, is_system_distro, parse_distro_list, Distro, WslError};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Output};

    /// Prevents a console window from flashing on screen — this module is
    /// called from a background poll as well as user actions, and a poll
    /// popping a terminal every few seconds would be a bug users would
    /// immediately notice.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn spawn(args: &[&str]) -> Result<Output, WslError> {
        Command::new("wsl.exe")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|source| WslError::Spawn {
                program: "wsl.exe",
                source,
            })
    }

    /// `true` if `wsl.exe` could be launched at all. Uses `--status`, which
    /// (like the `-l -q` forms) only reports state and starts nothing.
    pub(super) fn is_wsl_available() -> bool {
        match spawn(&["--status"]) {
            Ok(_) => true,
            Err(WslError::Spawn { source, .. }) => source.kind() != std::io::ErrorKind::NotFound,
            Err(_) => false,
        }
    }

    /// One of the two safe list forms: `-l -q` (all distros) or
    /// `-l -q --running` (only running ones). Neither starts anything —
    /// verified empirically, see the module docs.
    fn list_names(args: &[&str]) -> Result<Vec<String>, WslError> {
        let output = spawn(args)?;
        if !output.status.success() {
            // wsl.exe exits non-zero for valid empty states too (e.g. "no
            // installed distributions", or nothing currently running) —
            // that's a legitimate empty list, not a failure of this
            // function, so it is never surfaced as `Err`.
            return Ok(Vec::new());
        }
        Ok(parse_distro_list(&decode_utf16le(&output.stdout)))
    }

    pub(super) fn list_distros() -> Result<Vec<Distro>, WslError> {
        let running = list_names(&["-l", "-q", "--running"])?;
        let all = list_names(&["-l", "-q"])?;
        Ok(all
            .into_iter()
            .map(|name| {
                let running = running.iter().any(|r| r.eq_ignore_ascii_case(&name));
                let is_system = is_system_distro(&name);
                Distro {
                    name,
                    running,
                    is_system,
                }
            })
            .collect())
    }

    /// Read whether Claude Code credentials exist inside `name`. **Only
    /// safe to call for a distro already known to be `running`** — this
    /// executes inside the distro via `wsl.exe -d <name> -e ...`, which
    /// starts it if it is stopped. Callers on a polling path must check
    /// [`Distro::running`] first; [`super::wake_and_read`] is the only
    /// caller allowed to invoke this unconditionally, precisely because it
    /// is the explicit, user-initiated "wake it up" action.
    ///
    /// Shells out rather than reading `\\wsl$\<name>\...` — see the "UNC
    /// path vs. exec" section of the module docs for why.
    pub(super) fn read_credentials_via_exec(name: &str) -> Result<bool, WslError> {
        // Mirrors `paths::claude_config_home` + `paths::credentials_path`:
        // $CLAUDE_CONFIG_DIR if set and non-empty, else $HOME/.claude, then
        // .credentials.json under that. `${VAR:-default}` triggers on
        // unset *or empty*, matching `paths.rs`'s `env_non_empty` check.
        const SCRIPT: &str =
            r#"cfg="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"; test -f "$cfg/.credentials.json""#;
        let output = spawn(&["-d", name, "-e", "sh", "-c", SCRIPT])?;
        Ok(output.status.success())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Whether `wsl.exe` is present on this machine at all. Never starts a
/// distro (it only probes for the launcher itself).
#[cfg(target_os = "windows")]
pub fn is_wsl_available() -> bool {
    windows_impl::is_wsl_available()
}

#[cfg(not(target_os = "windows"))]
pub fn is_wsl_available() -> bool {
    false
}

/// List every WSL distro, each with its current `running` state. Backed
/// solely by `wsl.exe -l -q` and `wsl.exe -l -q --running`, both of which are
/// list-only and start nothing — safe to call on a timer.
#[cfg(target_os = "windows")]
pub fn list_distros() -> Result<Vec<Distro>, WslError> {
    windows_impl::list_distros()
}

#[cfg(not(target_os = "windows"))]
pub fn list_distros() -> Result<Vec<Distro>, WslError> {
    Ok(Vec::new())
}

#[cfg(target_os = "windows")]
fn credential_probe(name: &str) -> Result<bool, WslError> {
    windows_impl::read_credentials_via_exec(name)
}

#[cfg(not(target_os = "windows"))]
fn credential_probe(_name: &str) -> Result<bool, WslError> {
    Ok(false)
}

/// Detect every credential environment: the native environment plus one per
/// non-system WSL distro.
///
/// **Never starts a distro.** A stopped distro is reported as
/// [`EnvStatus::Asleep`] with an empty `accounts` list, without touching its
/// filesystem — see [`build_environments`], which is the testable core of
/// this function and is what actually enforces that. If [`list_distros`]
/// itself fails (e.g. `wsl.exe` missing), this degrades to just the native
/// environment rather than propagating an error, since the native
/// environment is always determinable regardless of WSL's state.
pub fn detect_environments() -> Vec<Environment> {
    let distros = list_distros().unwrap_or_default();
    build_environments(&distros, &credential_probe)
}

/// Explicitly wake `name` (starting it if it is stopped) and read whether
/// Claude Code credentials exist inside it.
///
/// **User-initiated only.** This is the one function in this module allowed
/// to start a Linux VM — do not call it from a background poll or from
/// [`detect_environments`]. Call it only in direct response to a user action
/// (e.g. clicking a sleeping environment to explicitly wake it).
#[cfg(target_os = "windows")]
pub fn wake_and_read(name: &str) -> Result<bool, WslError> {
    windows_impl::read_credentials_via_exec(name)
}

#[cfg(not(target_os = "windows"))]
pub fn wake_and_read(_name: &str) -> Result<bool, WslError> {
    Err(WslError::Unsupported)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};
    use std::cell::Cell;
    use tempfile::TempDir;

    fn utf16le_bytes(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    // `build_environments` unconditionally calls `native_environment()`,
    // which resolves `crate::paths::credentials_path()` — a real
    // filesystem-path resolution guarded by `test_support::guard_real_store`
    // under `cfg(test)`. Every test below that calls `build_environments`
    // must therefore isolate HOME/USERPROFILE at a temp dir and hold the
    // crate-wide `env_lock()`, exactly like `paths.rs`/`credentials.rs`/
    // `switcher.rs` — a per-module lock would not help, since env vars are
    // process-global.
    //
    // `_lock` is declared FIRST: local variables drop in *reverse*
    // declaration order, so declaring the lock first means it's released
    // *last* — after `_home_guard`/`_cfg_guard` have already restored the
    // real environment, so no other thread can start mutating HOME/
    // CLAUDE_CONFIG_DIR while this scope's guards are still tearing down.

    #[cfg(windows)]
    fn set_home(dir: &std::path::Path) -> EnvGuard {
        EnvGuard::set("USERPROFILE", dir.to_str().expect("utf8 temp path"))
    }

    #[cfg(not(windows))]
    fn set_home(dir: &std::path::Path) -> EnvGuard {
        EnvGuard::set("HOME", dir.to_str().expect("utf8 temp path"))
    }

    // -- decode_utf16le ----------------------------------------------------

    #[test]
    fn decode_utf16le_handles_bom_and_trailing_blank_line() {
        // Realistic `wsl.exe -l -q` output: BOM, two distro names, and the
        // trailing blank line wsl.exe emits after the last entry.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(utf16le_bytes("Ubuntu\r\ndocker-desktop\r\n"));

        let text = decode_utf16le(&bytes);
        assert_eq!(text, "Ubuntu\r\ndocker-desktop\r\n");

        let names = parse_distro_list(&text);
        assert_eq!(
            names,
            vec!["Ubuntu".to_string(), "docker-desktop".to_string()]
        );
    }

    #[test]
    fn decode_utf16le_without_bom_still_decodes_correctly() {
        let bytes = utf16le_bytes("Debian\r\n");
        assert_eq!(decode_utf16le(&bytes), "Debian\r\n");
    }

    #[test]
    fn naive_utf8_decode_of_utf16le_bytes_would_be_corrupt() {
        // Documents *why* decode_utf16le exists: proves the naive approach
        // this module deliberately avoids (String::from_utf8_lossy on raw
        // wsl.exe bytes) really does yield NUL-separated garbage, so the
        // gotcha in the module docs isn't just asserted, it's demonstrated.
        let bytes = utf16le_bytes("Ubuntu");
        let naive = String::from_utf8_lossy(&bytes);
        assert!(
            naive.contains('\u{0}'),
            "expected NUL-corrupted output, got {naive:?}"
        );
        assert_ne!(naive, "Ubuntu");

        // The real decoder produces the clean string.
        assert_eq!(decode_utf16le(&bytes), "Ubuntu");
    }

    // -- parse_distro_list ---------------------------------------------------

    #[test]
    fn parse_distro_list_drops_blank_lines_and_trims() {
        let names = parse_distro_list("Ubuntu\r\n  Debian  \r\n\r\n");
        assert_eq!(names, vec!["Ubuntu".to_string(), "Debian".to_string()]);
    }

    #[test]
    fn parse_distro_list_of_empty_output_is_empty() {
        assert!(parse_distro_list("").is_empty());
        assert!(parse_distro_list("\r\n").is_empty());
    }

    // -- is_system_distro -----------------------------------------------------

    #[test]
    fn classifies_known_system_distros_case_insensitively() {
        assert!(is_system_distro("docker-desktop"));
        assert!(is_system_distro("Docker-Desktop-Data"));
        assert!(is_system_distro("PODMAN-MACHINE-DEFAULT"));
        assert!(!is_system_distro("Ubuntu"));
        assert!(!is_system_distro("Ubuntu-22.04"));
        assert!(!is_system_distro("docker-desktop-but-not-really"));
    }

    // -- build_environments: the no-auto-start guarantee ----------------------

    #[test]
    fn build_environments_never_probes_stopped_or_system_distros() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        let distros = vec![
            Distro {
                name: "Ubuntu".into(),
                running: true,
                is_system: false,
            },
            Distro {
                name: "Debian".into(),
                running: false,
                is_system: false,
            },
            Distro {
                name: "docker-desktop".into(),
                // Even though this is (hypothetically) running, it's a
                // system distro and must still never be probed.
                running: true,
                is_system: true,
            },
        ];

        let probed_names: Cell<Vec<String>> = Cell::new(Vec::new());
        let probe = |name: &str| -> Result<bool, WslError> {
            // The core assertion: a probe call for anything but the one
            // running, non-system distro is a bug in `build_environments`,
            // and here it fails the test immediately rather than being
            // merely logged, so this doubles as the proof that
            // `detect_environments` performs no filesystem/exec access for
            // a stopped distro.
            assert_eq!(
                name, "Ubuntu",
                "probed a distro that must not have been touched"
            );
            let mut names = probed_names.take();
            names.push(name.to_string());
            probed_names.set(names);
            Ok(true)
        };

        let environments = build_environments(&distros, &probe);

        assert_eq!(probed_names.take(), vec!["Ubuntu".to_string()]);
        // Native + 3 distro entries.
        assert_eq!(environments.len(), 4);

        // The distro name no longer lives inside `kind` (there is no
        // per-variant payload on `EnvKind::Wsl`) — it was carried over to
        // `id`/`label` instead, so lookups key off `id` now.
        let ubuntu = environments.iter().find(|e| e.id == "wsl:Ubuntu").unwrap();
        assert_eq!(ubuntu.kind, EnvKind::Wsl);
        assert_eq!(ubuntu.status, EnvStatus::Live);
        assert!(ubuntu.accounts.is_empty());
        // The only distro actually probed: its `Ok(true)` result must land
        // on `has_credentials`, not be discarded.
        assert_eq!(ubuntu.has_credentials, Some(true));

        let debian = environments.iter().find(|e| e.id == "wsl:Debian").unwrap();
        assert_eq!(debian.kind, EnvKind::Wsl);
        assert_eq!(debian.status, EnvStatus::Asleep);
        assert!(debian.accounts.is_empty());
        // Never probed (asleep) — "not determined", never `Some(false)`.
        assert_eq!(debian.has_credentials, None);

        let docker = environments
            .iter()
            .find(|e| e.id == "wsl:docker-desktop")
            .unwrap();
        assert_eq!(docker.kind, EnvKind::Wsl);
        assert_eq!(docker.status, EnvStatus::Ignored);
        assert!(docker.accounts.is_empty());
        // Never probed (system distro) — "not determined".
        assert_eq!(docker.has_credentials, None);
    }

    #[test]
    fn build_environments_reports_some_false_distinctly_from_not_determined() {
        // `Some(false)` ("checked, nothing there") must never collapse into
        // `None` ("not determined") — the UI needs to tell "no install"
        // apart from "haven't looked yet".
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        let distros = vec![Distro {
            name: "Ubuntu".into(),
            running: true,
            is_system: false,
        }];
        let environments = build_environments(&distros, &|_| Ok(false));
        let ubuntu = environments.iter().find(|e| e.id == "wsl:Ubuntu").unwrap();
        assert_eq!(ubuntu.status, EnvStatus::Live);
        assert_eq!(ubuntu.has_credentials, Some(false));
    }

    #[test]
    fn build_environments_always_includes_native_first() {
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        let environments = build_environments(&[], &|_| Ok(false));
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].kind, EnvKind::Native);
        assert_eq!(environments[0].id, "native");
        assert_eq!(environments[0].status, EnvStatus::Live);
        assert!(environments[0].accounts.is_empty());
        // Native is never probed by this module — "not determined".
        assert_eq!(environments[0].has_credentials, None);
    }

    #[test]
    fn build_environments_stays_live_when_probe_errors() {
        // A probe failure (e.g. the distro raced from running to stopped
        // between the list call and the probe) must not be conflated with
        // "stopped" or "ignored" — it's a genuinely unknown result, and the
        // safest reading is to still surface the distro as live rather than
        // hide it.
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        let distros = vec![Distro {
            name: "Ubuntu".into(),
            running: true,
            is_system: false,
        }];
        let probe = |_: &str| -> Result<bool, WslError> {
            Err(WslError::Spawn {
                program: "wsl.exe",
                source: std::io::Error::from(std::io::ErrorKind::Other),
            })
        };
        let environments = build_environments(&distros, &probe);
        let ubuntu = &environments[1];
        assert_eq!(ubuntu.status, EnvStatus::Live);
        assert!(ubuntu.accounts.is_empty());
        // A probe error must not be conflated with `Some(false)` either —
        // still "not determined", same as never having probed at all.
        assert_eq!(ubuntu.has_credentials, None);
    }

    // -- non-Windows no-op surface --------------------------------------------

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn non_windows_public_api_is_a_native_only_no_op() {
        // detect_environments() resolves the native config home, so HOME has
        // to be isolated or `guard_real_store` refuses the run.
        let _lock = env_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = set_home(home.path());
        let _cfg_guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        assert!(!is_wsl_available());
        assert_eq!(list_distros().unwrap(), Vec::new());
        let environments = detect_environments();
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].kind, EnvKind::Native);
        assert_eq!(environments[0].id, "native");
        assert!(wake_and_read("anything").is_err());
    }
}
