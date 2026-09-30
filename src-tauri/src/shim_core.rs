//! What the `claude` shim decides, without doing any of it.
//!
//! The app writes `~/.cc-logins/shim.json`; the `cc-logins-shim` binary reads
//! it and launches the real Claude Code with the chosen account's
//! `CLAUDE_CONFIG_DIR`. Both sides compile this one file — the shim through
//! `#[path]`, because linking the app library would drag Tauri and the webview
//! into a binary that runs on every `claude` invocation — so the file format
//! and the decision rules cannot drift apart.
//!
//! Rules for this file: `std`, `serde` and `serde_json` only, and no `crate::`
//! references beyond the other shared modules (`sys_env`, `claude_resolve`).
//! Everything here is pure; the binary does the I/O.
//!
//! The shim never reads, copies or stores a credential. It only chooses which
//! folder Claude Code itself signs in to and reads from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current `shim.json` format. A file with a higher version was written by a
/// newer app than this shim; it is ignored (default account) rather than
/// half-understood.
pub const SHIM_CONFIG_VERSION: u32 = 1;

/// File name inside the cc-logins home directory.
pub const SHIM_CONFIG_FILE: &str = "shim.json";

/// Directory inside the cc-logins home directory that holds the shim copies.
pub const SHIM_BIN_DIR: &str = "bin";

/// Picks a launcher by slug for one command, e.g.
/// `CC_LOGINS_PROFILE=work claude`. Only consulted by the plain `claude` shim.
pub const PROFILE_ENV: &str = "CC_LOGINS_PROFILE";

/// Nesting counter the shim increments for its child. A session that runs
/// `claude -p` from a tool legitimately nests a level or two; only a runaway
/// chain (the shim resolving to another copy of itself) gets this deep.
pub const DEPTH_ENV: &str = "CC_LOGINS_SHIM_DEPTH";

/// Nesting level at which the shim refuses to launch anything.
pub const MAX_DEPTH: u32 = 8;

/// Claude Code's own variable. Everything the shim does comes down to this.
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Base name of the plain shim, and prefix of the per-account launchers
/// (`claude-work`, `claude-personal`, …).
pub const SHIM_STEM: &str = "claude";

/// One account as the shim sees it: which folder, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShimProfile {
    /// Registry slot, for diagnostics only.
    pub account: u32,
    /// The exact `CLAUDE_CONFIG_DIR` string, or `None` for the default
    /// profile (`~/.claude`, variable unset). Exact matters: on macOS Claude
    /// Code names its Keychain item after a hash of this literal string.
    #[serde(default)]
    pub config_dir: Option<String>,
}

/// The whole of `shim.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShimConfig {
    #[serde(default)]
    pub v: u32,
    /// The account plain `claude` starts new sessions with. `None` means the
    /// default profile.
    #[serde(default)]
    pub selected: Option<ShimProfile>,
    /// Launcher slug → account, for `claude-<slug>`.
    #[serde(default)]
    pub launchers: BTreeMap<String, ShimProfile>,
    /// The app's `claudeBinaryPath` setting, mirrored so the shim finds the
    /// same Claude Code the app does.
    #[serde(default)]
    pub claude_binary: Option<PathBuf>,
}

impl ShimConfig {
    /// Parse `shim.json`. `None` for anything unreadable or from a newer
    /// format; the caller then behaves as if no account were selected.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let config: Self = serde_json::from_slice(bytes).ok()?;
        (config.v <= SHIM_CONFIG_VERSION).then_some(config)
    }
}

/// How the shim was started, from its own file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// `claude`: the account selected in the tray.
    Selected,
    /// `claude-<slug>`: always that account.
    Launcher(String),
}

impl Invocation {
    /// Classify an executable stem (`claude`, `claude-work`). Case-folded,
    /// because Windows file names are case-insensitive. Any other stem is
    /// treated as the plain shim: a user who renamed the file still gets the
    /// selected account rather than an error.
    pub fn from_stem(stem: &str) -> Self {
        let stem = stem.to_ascii_lowercase();
        match stem
            .strip_prefix(SHIM_STEM)
            .and_then(|r| r.strip_prefix('-'))
        {
            Some(slug) if !slug.is_empty() => Invocation::Launcher(slug.to_string()),
            _ => Invocation::Selected,
        }
    }
}

/// What to do with `CLAUDE_CONFIG_DIR` in the child's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvAction {
    /// Leave it exactly as inherited.
    Keep,
    /// Set it to this exact string.
    Set(String),
    /// Remove it, so Claude Code uses `~/.claude`.
    Remove,
}

/// The shim's decision for one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub config_dir: EnvAction,
    /// The account chosen, when one was; for the `--cc-logins-which` probe
    /// and error messages.
    pub account: Option<u32>,
}

/// Why the shim will not launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// `claude-<slug>` or `CC_LOGINS_PROFILE=<slug>` named no known account.
    UnknownLauncher(String),
    /// [`DEPTH_ENV`] reached [`MAX_DEPTH`].
    Loop(u32),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::UnknownLauncher(slug) => write!(
                f,
                "cc-logins: no account is set up for \"{slug}\". Open CC Logins to see \
                 the launcher names, or run plain `claude`."
            ),
            PlanError::Loop(depth) => write!(
                f,
                "cc-logins: stopped after {depth} nested launches. The `claude` command \
                 seems to point back at this shim; set CC_LOGINS_CLAUDE_BIN to the real \
                 Claude Code binary."
            ),
        }
    }
}

/// Everything [`plan`] reads, injected so it stays pure.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    pub invocation: &'a Invocation,
    /// `CLAUDE_CONFIG_DIR` as inherited, if set (possibly empty).
    pub inherited_config_dir: Option<&'a str>,
    /// [`PROFILE_ENV`] as inherited, if set and non-empty.
    pub profile_env: Option<&'a str>,
    /// [`DEPTH_ENV`] as inherited, parsed; 0 when absent or unparsable.
    pub depth: u32,
    /// `shim.json`, when present and readable.
    pub config: Option<&'a ShimConfig>,
    /// `~/.claude`, so a profile that names it is launched with the variable
    /// unset rather than set to the default path.
    pub default_config_dir: &'a Path,
}

/// Decide how to launch.
///
/// Precedence for plain `claude`:
/// 1. An inherited non-empty `CLAUDE_CONFIG_DIR` is kept. That is the user's
///    own alias, or a nested call from inside a session that must stay on the
///    session's account.
/// 2. [`PROFILE_ENV`].
/// 3. `shim.json`'s selected account.
/// 4. The default profile.
///
/// A launcher (`claude-<slug>`) always applies its own account, overriding
/// anything inherited: running `claude-work` from inside a personal session
/// must still mean work.
pub fn plan(input: &PlanInput) -> Result<LaunchPlan, PlanError> {
    if input.depth >= MAX_DEPTH {
        return Err(PlanError::Loop(input.depth));
    }

    let launcher = |slug: &str| -> Result<LaunchPlan, PlanError> {
        input
            .config
            .and_then(|c| c.launchers.get(&slug.to_ascii_lowercase()))
            .map(|p| profile_plan(p, input.default_config_dir))
            .ok_or_else(|| PlanError::UnknownLauncher(slug.to_string()))
    };

    match input.invocation {
        Invocation::Launcher(slug) => launcher(slug.as_str()),
        Invocation::Selected => {
            if input.inherited_config_dir.is_some_and(|d| !d.is_empty()) {
                return Ok(LaunchPlan {
                    config_dir: EnvAction::Keep,
                    account: None,
                });
            }
            if let Some(slug) = input.profile_env {
                return launcher(slug);
            }
            match input.config.and_then(|c| c.selected.as_ref()) {
                Some(profile) => Ok(profile_plan(profile, input.default_config_dir)),
                None => Ok(LaunchPlan {
                    config_dir: EnvAction::Remove,
                    account: None,
                }),
            }
        }
    }
}

fn profile_plan(profile: &ShimProfile, default_config_dir: &Path) -> LaunchPlan {
    let config_dir = match &profile.config_dir {
        Some(dir) if !same_dir(Path::new(dir), default_config_dir) => EnvAction::Set(dir.clone()),
        _ => EnvAction::Remove,
    };
    LaunchPlan {
        config_dir,
        account: Some(profile.account),
    }
}

/// Lexical directory equality: trailing separators ignored, and on Windows
/// `/` versus `\` and letter case too. No filesystem access.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    dir_key(a) == dir_key(b)
}

fn dir_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.replace('/', "\\").to_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// Parse [`DEPTH_ENV`]; anything unparsable counts as the top level.
pub fn parse_depth(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// VS Code's `claudeCode.claudeProcessWrapper` runs the wrapper with the real
/// Claude Code path as its first argument. True when `arg` looks like that:
/// an absolute path whose file stem is `claude`. The caller still checks the
/// file exists before trusting it.
pub fn looks_like_wrapped_claude(arg: &Path) -> bool {
    arg.is_absolute()
        && arg
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(SHIM_STEM))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        }
    }

    fn profile(account: u32, dir: Option<&str>) -> ShimProfile {
        ShimProfile {
            account,
            config_dir: dir.map(str::to_string),
        }
    }

    fn config() -> ShimConfig {
        let mut launchers = BTreeMap::new();
        launchers.insert("work".to_string(), profile(2, Some("/p/work")));
        launchers.insert("main".to_string(), profile(1, None));
        ShimConfig {
            v: SHIM_CONFIG_VERSION,
            selected: Some(profile(2, Some("/p/work"))),
            launchers,
            claude_binary: None,
        }
    }

    fn input<'a>(
        invocation: &'a Invocation,
        config: Option<&'a ShimConfig>,
        default_dir: &'a Path,
    ) -> PlanInput<'a> {
        PlanInput {
            invocation,
            inherited_config_dir: None,
            profile_env: None,
            depth: 0,
            config,
            default_config_dir: default_dir,
        }
    }

    #[test]
    fn invocation_from_stem() {
        assert_eq!(Invocation::from_stem("claude"), Invocation::Selected);
        assert_eq!(Invocation::from_stem("CLAUDE"), Invocation::Selected);
        assert_eq!(
            Invocation::from_stem("claude-Work"),
            Invocation::Launcher("work".to_string())
        );
        assert_eq!(Invocation::from_stem("claude-"), Invocation::Selected);
        assert_eq!(Invocation::from_stem("renamed"), Invocation::Selected);
    }

    #[test]
    fn plain_claude_uses_the_selected_account() {
        let default_dir = home().join(".claude");
        let cfg = config();
        let got = plan(&input(&Invocation::Selected, Some(&cfg), &default_dir)).unwrap();
        assert_eq!(got.config_dir, EnvAction::Set("/p/work".to_string()));
        assert_eq!(got.account, Some(2));
    }

    #[test]
    fn inherited_config_dir_passes_through_for_plain_claude() {
        let default_dir = home().join(".claude");
        let cfg = config();
        let mut i = input(&Invocation::Selected, Some(&cfg), &default_dir);
        i.inherited_config_dir = Some("/my/alias");
        let got = plan(&i).unwrap();
        assert_eq!(got.config_dir, EnvAction::Keep);
    }

    #[test]
    fn empty_inherited_config_dir_counts_as_unset() {
        let default_dir = home().join(".claude");
        let cfg = config();
        let mut i = input(&Invocation::Selected, Some(&cfg), &default_dir);
        i.inherited_config_dir = Some("");
        let got = plan(&i).unwrap();
        assert_eq!(got.config_dir, EnvAction::Set("/p/work".to_string()));
    }

    #[test]
    fn launcher_overrides_an_inherited_config_dir() {
        let default_dir = home().join(".claude");
        let cfg = config();
        let inv = Invocation::Launcher("main".to_string());
        let mut i = input(&inv, Some(&cfg), &default_dir);
        i.inherited_config_dir = Some("/p/work");
        let got = plan(&i).unwrap();
        assert_eq!(got.config_dir, EnvAction::Remove);
        assert_eq!(got.account, Some(1));
    }

    #[test]
    fn profile_env_picks_a_launcher() {
        let default_dir = home().join(".claude");
        let cfg = config();
        let mut i = input(&Invocation::Selected, Some(&cfg), &default_dir);
        i.profile_env = Some("MAIN");
        assert_eq!(plan(&i).unwrap().account, Some(1));
    }

    #[test]
    fn unknown_launcher_is_an_error_not_the_default_account() {
        let default_dir = home().join(".claude");
        let cfg = config();
        let inv = Invocation::Launcher("nope".to_string());
        assert_eq!(
            plan(&input(&inv, Some(&cfg), &default_dir)),
            Err(PlanError::UnknownLauncher("nope".to_string()))
        );
        let mut i = input(&Invocation::Selected, Some(&cfg), &default_dir);
        i.profile_env = Some("nope");
        assert!(matches!(plan(&i), Err(PlanError::UnknownLauncher(_))));
    }

    #[test]
    fn missing_config_means_default_profile() {
        let default_dir = home().join(".claude");
        let got = plan(&input(&Invocation::Selected, None, &default_dir)).unwrap();
        assert_eq!(got.config_dir, EnvAction::Remove);
        assert_eq!(got.account, None);
    }

    #[test]
    fn a_profile_naming_the_default_dir_unsets_the_variable() {
        let default_dir = home().join(".claude");
        let default_str = default_dir.to_string_lossy().into_owned();
        let with_slash = format!("{default_str}{}", std::path::MAIN_SEPARATOR);
        let cfg = ShimConfig {
            v: 1,
            selected: Some(profile(1, Some(&with_slash))),
            ..ShimConfig::default()
        };
        let got = plan(&input(&Invocation::Selected, Some(&cfg), &default_dir)).unwrap();
        assert_eq!(got.config_dir, EnvAction::Remove);
    }

    #[test]
    fn depth_guard_stops_a_loop() {
        let default_dir = home().join(".claude");
        let mut i = input(&Invocation::Selected, None, &default_dir);
        i.depth = MAX_DEPTH;
        assert_eq!(plan(&i), Err(PlanError::Loop(MAX_DEPTH)));
        i.depth = MAX_DEPTH - 1;
        assert!(plan(&i).is_ok());
    }

    #[test]
    fn parse_rejects_garbage_and_newer_versions() {
        assert_eq!(ShimConfig::parse(b"not json"), None);
        assert_eq!(ShimConfig::parse(br#"{"v": 99}"#), None);
        let parsed = ShimConfig::parse(br#"{"v":1,"selected":{"account":3,"configDir":"/x"}}"#)
            .expect("valid config");
        assert_eq!(parsed.selected, Some(profile(3, Some("/x"))));
        assert!(parsed.launchers.is_empty());
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = config();
        let bytes = serde_json::to_vec(&cfg).unwrap();
        assert_eq!(ShimConfig::parse(&bytes), Some(cfg));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"configDir\""), "camelCase on disk: {text}");
        assert!(
            text.contains("\"claudeBinary\""),
            "camelCase on disk: {text}"
        );
    }

    #[test]
    fn parse_depth_defaults_to_zero() {
        assert_eq!(parse_depth(None), 0);
        assert_eq!(parse_depth(Some("x")), 0);
        assert_eq!(parse_depth(Some(" 3 ")), 3);
    }

    #[test]
    fn wrapped_claude_detection() {
        let abs = home().join(".local").join("bin").join("claude");
        assert!(looks_like_wrapped_claude(&abs));
        assert!(looks_like_wrapped_claude(&abs.with_extension("exe")));
        assert!(!looks_like_wrapped_claude(Path::new("claude")));
        assert!(!looks_like_wrapped_claude(&home().join("--resume")));
    }

    #[test]
    fn same_dir_ignores_trailing_separators() {
        let a = home().join(".claude");
        let b = PathBuf::from(format!("{}/", a.display()));
        assert!(same_dir(&a, &b));
        assert!(!same_dir(&a, &home().join(".claude-work")));
    }
}
