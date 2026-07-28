use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use cc_logins_lib::claude_locks::{DirectoryLock, CONFIG_STALENESS, CREDENTIAL_STALENESS};

fn python_command() -> Command {
    #[cfg(windows)]
    {
        // `python` is a pyenv .bat shim on this host. CreateProcess does not
        // resolve batch files, while cmd does exactly what the user's shell does.
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/c", "python"]);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("python3")
    }
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upstream_lock_holder.py")
}

fn wait_ready(child: &mut Child, ready: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("holder exited before readiness: {status}");
        }
        assert!(Instant::now() < deadline, "holder did not signal readiness");
        std::thread::yield_now();
    }
}

#[test]
fn rust_lock_holder_helper() {
    let Some(path) = std::env::var_os("CC_LOGINS_LOCK_HELPER_PATH") else {
        return;
    };
    let release = PathBuf::from(std::env::var_os("CC_LOGINS_LOCK_HELPER_RELEASE").unwrap());
    let ready = PathBuf::from(std::env::var_os("CC_LOGINS_LOCK_HELPER_READY").unwrap());
    let lock = DirectoryLock::acquire(
        PathBuf::from(path),
        Duration::from_secs(2),
        CREDENTIAL_STALENESS,
    )
    .unwrap();
    fs::write(ready, b"ready").unwrap();
    while !release.exists() {
        std::thread::yield_now();
    }
    drop(lock);
}

#[test]
fn rust_holder_excludes_a_second_rust_process() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("credential.lock");
    let release = root.path().join("release");
    let ready = root.path().join("ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rust_lock_holder_helper", "--nocapture"])
        .env("CC_LOGINS_LOCK_HELPER_PATH", &path)
        .env("CC_LOGINS_LOCK_HELPER_RELEASE", &release)
        .env("CC_LOGINS_LOCK_HELPER_READY", &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_ready(&mut child, &ready);

    assert!(
        DirectoryLock::acquire(&path, Duration::from_millis(80), CREDENTIAL_STALENESS).is_err()
    );
    fs::write(&release, b"release").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!path.exists());
}

#[test]
fn python_holder_excludes_rust_for_credential_and_config_locks() {
    for (name, staleness) in [
        ("credential.lock", CREDENTIAL_STALENESS),
        ("config.lock", CONFIG_STALENESS),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        let release = root.path().join("release");
        let ready = root.path().join("ready");
        let mut command = python_command();
        command
            .arg(fixture())
            .arg("hold")
            .arg(&path)
            .arg(&release)
            .arg(&ready)
            .arg(staleness.as_secs_f64().to_string())
            .stdout(Stdio::null());
        let mut child = command.spawn().unwrap();
        wait_ready(&mut child, &ready);

        assert!(DirectoryLock::acquire(&path, Duration::from_millis(80), staleness).is_err());
        fs::write(&release, b"release").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(!path.exists());
    }
}

#[test]
fn rust_holder_excludes_pinned_python_contender() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("credential.lock");
    let _lock =
        DirectoryLock::acquire(&path, Duration::from_secs(1), CREDENTIAL_STALENESS).unwrap();
    let output = python_command()
        .arg(fixture())
        .arg("contend")
        .arg(&path)
        .arg("0.08")
        .arg(CREDENTIAL_STALENESS.as_secs_f64().to_string())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "TIMEOUT");
}

#[test]
fn pinned_python_stale_lock_is_taken_over_by_rust() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.lock");
    fs::create_dir(&path).unwrap();
    set_old_mtime(&path, SystemTime::now() - Duration::from_secs(11));

    let lock = DirectoryLock::acquire(&path, Duration::from_secs(1), CONFIG_STALENESS).unwrap();
    assert!(path.is_dir());
    drop(lock);
    assert!(!path.exists());
}

#[cfg(not(windows))]
fn set_old_mtime(path: &Path, time: SystemTime) {
    fs::File::open(path).unwrap().set_modified(time).unwrap();
}

#[cfg(windows)]
fn set_old_mtime(path: &Path, time: SystemTime) {
    let script = "import os,sys; os.utime(sys.argv[1], (float(sys.argv[2]), float(sys.argv[2])))";
    let epoch = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!(python_command()
        .arg("-c")
        .arg(script)
        .arg(path)
        .arg(epoch.to_string())
        .status()
        .unwrap()
        .success());
}
