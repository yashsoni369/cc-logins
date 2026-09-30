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

use std::path::PathBuf;

pub use crate::claude_resolve::*;

/// Locate the Claude Code CLI on this machine.
///
/// `settings_override` is the persisted `claudeBinaryPath` setting, injected
/// by the caller — this module never reads the settings store itself. There
/// is deliberately no zero-argument variant: a shorter name would let a
/// future call site reach for it and silently ignore the setting.
pub fn resolve(settings_override: Option<PathBuf>) -> Result<Resolved, NotFound> {
    let mut ctx = SearchContext::from_env();
    ctx.settings_override = settings_override;
    // Never resolve our own `claude` shim: the app needs the real binary, and
    // the shim's directory is first on PATH once the user installs it.
    ctx.exclude_dirs
        .push(crate::sys_env::cc_logins_home_dir().join(crate::shim_core::SHIM_BIN_DIR));
    resolve_in(&ctx, &is_executable_file)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys_env::Platform;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

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
    fn excluded_path_dir_is_skipped_for_the_next_real_claude() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(std::env::join_paths(["/shim/bin", "/real/bin"]).unwrap()),
            exclude_dirs: vec![PathBuf::from("/shim/bin/")],
            ..SearchContext::default()
        };
        let is_exec =
            |p: &Path| p == Path::new("/shim/bin/claude") || p == Path::new("/real/bin/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: PathBuf::from("/real/bin/claude"),
                source: Source::Path,
            })
        );
    }

    #[test]
    fn excluded_file_is_skipped_even_outside_excluded_dirs() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            path_var: Some(OsString::from("/copy")),
            // Joined, not a literal, so the separator matches what the search
            // itself produces on the host running the test.
            exclude_files: vec![PathBuf::from("/copy").join("claude")],
            ..SearchContext::default()
        };
        let is_exec = |p: &Path| {
            p == Path::new("/copy/claude") || p == Path::new("/home/u/.local/bin/claude")
        };

        assert_eq!(
            resolve_in(&ctx, &is_exec),
            Ok(Resolved {
                path: PathBuf::from("/home/u/.local/bin/claude"),
                source: Source::WellKnown,
            })
        );
    }

    #[test]
    fn exclusions_never_apply_to_an_explicit_override() {
        let ctx = SearchContext {
            platform: Platform::Linux,
            home: PathBuf::from("/home/u"),
            env_override: Some(PathBuf::from("/shim/bin/claude")),
            exclude_dirs: vec![PathBuf::from("/shim/bin")],
            ..SearchContext::default()
        };
        let is_exec = |p: &Path| p == Path::new("/shim/bin/claude");

        assert_eq!(
            resolve_in(&ctx, &is_exec).map(|r| r.source),
            Ok(Source::EnvOverride)
        );
    }

    #[test]
    fn windows_exclusion_ignores_case_and_slash_direction() {
        assert_eq!(
            path_key(Path::new("c:/users/u/.cc-logins/bin"), Platform::Windows),
            path_key(
                Path::new("C:\\Users\\u\\.cc-logins\\bin\\"),
                Platform::Windows
            )
        );
        assert_ne!(
            path_key(Path::new("/a/B"), Platform::Linux),
            path_key(Path::new("/a/b"), Platform::Linux)
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
