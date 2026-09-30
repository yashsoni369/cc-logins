//! End-to-end tests for the `claude` shim: the real `cc-logins-shim` binary,
//! copied under the names the app gives it, launching a fake Claude Code that
//! reports the environment it was started with.
//!
//! Every test builds its own sandbox and passes all environment explicitly to
//! the child, so nothing here reads or changes the developer's real
//! `~/.claude`, `~/.cc-logins` or process environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SHIM: &str = env!("CARGO_BIN_EXE_cc-logins-shim");

/// Exit code the fake Claude Code returns, to prove the shim passes it back.
const FAKE_EXIT: i32 = 7;

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    cc_home: PathBuf,
    bin: PathBuf,
    real: PathBuf,
    work_dir: String,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::TempDir::new().unwrap();
        let home = root.path().join("home");
        let cc_home = root.path().join("cc-home");
        let bin = cc_home.join("bin");
        let real = root.path().join("real");
        for dir in [&home, &bin, &real] {
            fs::create_dir_all(dir).unwrap();
        }
        let work_dir = cc_home
            .join("profiles")
            .join("p2-abcdef")
            .to_string_lossy()
            .into_owned();
        let sandbox = Self {
            _root: root,
            home,
            cc_home,
            bin,
            real,
            work_dir,
        };
        sandbox.install_shim("claude");
        sandbox.install_shim("claude-work");
        write_fake_claude(&sandbox.real);
        sandbox
    }

    fn install_shim(&self, stem: &str) {
        fs::copy(SHIM, self.bin.join(exe_name(stem))).unwrap();
    }

    fn write_config(&self, selected: bool) {
        let work = serde_json::json!({ "account": 2, "configDir": self.work_dir });
        let config = serde_json::json!({
            "v": 1,
            "selected": if selected { work.clone() } else { serde_json::Value::Null },
            "launchers": { "work": work },
            "claudeBinary": null,
        });
        fs::write(
            self.cc_home.join("shim.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
    }

    fn run(&self, stem: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        // The shim's own directory comes first, exactly as after "Install the
        // claude command": it must skip itself and find the real one after it.
        let path = std::env::join_paths([&self.bin, &self.real]).unwrap();
        let mut command = Command::new(self.bin.join(exe_name(stem)));
        command
            .args(args)
            .env("PATH", path)
            .env("CC_LOGINS_HOME", &self.cc_home)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CC_LOGINS_CLAUDE_BIN")
            .env_remove("CC_LOGINS_PROFILE")
            .env_remove("CC_LOGINS_SHIM_DEPTH");
        for (key, value) in extra_env {
            command.env(key, value);
        }
        // Tests run in parallel: another test copying a shim while this one
        // forks can leave the new file briefly open for writing in a child,
        // and Linux refuses to exec it ("Text file busy"). Retry that only.
        for _ in 0..50 {
            match command.output() {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return result.unwrap(),
            }
        }
        panic!("shim stayed busy");
    }
}

#[cfg(unix)]
fn write_fake_claude(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("claude");
    fs::write(
        &script,
        "#!/bin/sh\n\
         echo \"CFG=${CLAUDE_CONFIG_DIR-unset}\"\n\
         echo \"DEPTH=${CC_LOGINS_SHIM_DEPTH-unset}\"\n\
         echo \"ARGS=$*\"\n\
         exit 7\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
fn write_fake_claude(dir: &Path) {
    fs::write(
        dir.join("claude.cmd"),
        "@echo off\r\n\
         if defined CLAUDE_CONFIG_DIR (echo CFG=%CLAUDE_CONFIG_DIR%) else (echo CFG=unset)\r\n\
         echo DEPTH=%CC_LOGINS_SHIM_DEPTH%\r\n\
         echo ARGS=%*\r\n\
         exit /b 7\r\n",
    )
    .unwrap();
}

/// The value of `KEY=value` in the fake's output.
fn reported(output: &Output, key: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{key}=");
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(str::to_string))
        .unwrap_or_else(|| {
            panic!(
                "no {key}= line.\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

#[test]
fn plain_claude_runs_the_selected_account_and_skips_itself() {
    let sandbox = Sandbox::new();
    sandbox.write_config(true);
    let output = sandbox.run("claude", &["-p", "hi"], &[]);
    assert_eq!(reported(&output, "CFG"), sandbox.work_dir);
    assert_eq!(reported(&output, "ARGS"), "-p hi");
    assert_eq!(reported(&output, "DEPTH"), "1");
    assert_eq!(output.status.code(), Some(FAKE_EXIT));
}

#[test]
fn launcher_runs_its_own_account_without_a_selection() {
    let sandbox = Sandbox::new();
    sandbox.write_config(false);
    let output = sandbox.run("claude-work", &[], &[]);
    assert_eq!(reported(&output, "CFG"), sandbox.work_dir);
    assert_eq!(output.status.code(), Some(FAKE_EXIT));
}

#[test]
fn no_config_means_the_default_account() {
    let sandbox = Sandbox::new();
    let output = sandbox.run("claude", &[], &[]);
    assert_eq!(reported(&output, "CFG"), "unset");
}

#[test]
fn an_inherited_config_dir_passes_through() {
    let sandbox = Sandbox::new();
    sandbox.write_config(true);
    let mine = sandbox.home.join("my-alias").to_string_lossy().into_owned();
    let output = sandbox.run("claude", &[], &[("CLAUDE_CONFIG_DIR", &mine)]);
    assert_eq!(reported(&output, "CFG"), mine);
}

#[test]
fn an_unknown_launcher_fails_instead_of_using_another_account() {
    let sandbox = Sandbox::new();
    sandbox.write_config(true);
    sandbox.install_shim("claude-nope");
    let output = sandbox.run("claude-nope", &[], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("nope"));
    assert!(output.stdout.is_empty(), "the fake must not have run");
}

#[test]
fn the_depth_guard_stops_a_runaway_chain() {
    let sandbox = Sandbox::new();
    let output = sandbox.run("claude", &[], &[("CC_LOGINS_SHIM_DEPTH", "8")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "the fake must not have run");
}

#[test]
fn the_which_probe_reports_without_launching() {
    let sandbox = Sandbox::new();
    sandbox.write_config(true);
    let output = sandbox.run("claude", &["--cc-logins-which"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("account: 2"), "{stdout}");
    assert!(
        stdout.contains(&format!("CLAUDE_CONFIG_DIR: {}", sandbox.work_dir)),
        "{stdout}"
    );
    assert!(!stdout.contains("CFG="), "the fake must not have run");
    let real_dir = sandbox.real.to_string_lossy().into_owned();
    assert!(stdout.contains(&real_dir), "{stdout}");
}
