//! What profile folders share with the user's default `~/.claude`.
//!
//! Each account has its own folder, so without this every account would
//! start with no settings, no CLAUDE.md, no agents or commands. Sharing is an
//! allow-list:
//! - directories (`agents`, `commands`, `skills`, `output-styles`, `hooks`,
//!   and `ide` so editor integrations find each other) are linked — a
//!   symlink on macOS and Linux, a directory junction on Windows (no admin
//!   rights or Developer Mode needed);
//! - files (`settings.json`, `keybindings.json`, `CLAUDE.md`) are copied and
//!   re-synced from the default folder, because Claude Code rewrites some of
//!   them in place and a link would be replaced by the first write.
//!
//! Never shared: `plugins/` and `projects/` (Claude Code breaks when those
//! are symlinked), and anything holding an account: `.credentials.json`,
//! `.claude.json`, history.
//!
//! Every link and copy is recorded in the profile's manifest, so removing
//! them only ever touches what this module made. A link is never deleted
//! recursively — only the link itself.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Directories linked into every profile.
pub const SHARED_DIRS: &[&str] = &[
    "agents",
    "commands",
    "skills",
    "output-styles",
    "hooks",
    "ide",
];

/// Files copied into every profile and kept in step with the default folder.
pub const SHARED_FILES: &[&str] = &["settings.json", "keybindings.json", "CLAUDE.md"];

const MANIFEST: &str = ".cc-logins-links.json";

/// What this module made in one profile folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default)]
    pub v: u32,
    /// Link name → the directory it points at.
    #[serde(default)]
    pub links: BTreeMap<String, PathBuf>,
    /// File name → sha256 of the content last synced from the default folder.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

impl Manifest {
    pub fn load(profile: &Path) -> Self {
        fs::read(profile.join(MANIFEST))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, profile: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        crate::durable_fs::stage_sibling(&profile.join(MANIFEST), &bytes, Some(0o644))?.commit()?;
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> String {
    crate::hex::lower(&Sha256::digest(bytes))
}

/// Link the shared directories and copy the shared files into `profile`.
/// Anything already present in the profile under a shared name is left
/// alone. Safe to run again.
pub fn share_into(profile: &Path, default_dir: &Path) -> io::Result<Manifest> {
    let mut manifest = Manifest::load(profile);
    manifest.v = 1;
    for name in SHARED_DIRS {
        let link = profile.join(name);
        if fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        let target = default_dir.join(name);
        // A junction needs an existing target, and an empty directory in the
        // user's own folder is harmless.
        fs::create_dir_all(&target)?;
        create_dir_link(&target, &link)?;
        manifest.links.insert((*name).to_string(), target);
    }
    for name in SHARED_FILES {
        let source = default_dir.join(name);
        let dest = profile.join(name);
        if dest.exists() {
            continue;
        }
        let Ok(bytes) = fs::read(&source) else {
            continue;
        };
        crate::durable_fs::stage_sibling(&dest, &bytes, Some(0o644))?.commit()?;
        manifest.files.insert((*name).to_string(), digest(&bytes));
    }
    manifest.save(profile)?;
    Ok(manifest)
}

/// What to do with one shared file, from three hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resync {
    /// Nothing changed, or only the profile's copy did (kept).
    Keep,
    /// The default folder changed and the profile's copy did not.
    CopyToProfile,
    /// Both changed: the default's version wins and the profile's copy is
    /// saved beside it.
    Conflict,
    /// Both now hold the same content; just record it.
    Record,
}

/// Decide a resync from the default's hash, the profile's hash and the hash
/// recorded at the last sync.
pub fn plan_resync(source: &str, dest: &str, synced: &str) -> Resync {
    if source == dest {
        return if source == synced {
            Resync::Keep
        } else {
            Resync::Record
        };
    }
    match (source == synced, dest == synced) {
        (true, _) => Resync::Keep,
        (false, true) => Resync::CopyToProfile,
        (false, false) => Resync::Conflict,
    }
}

/// Bring the profile's shared files up to date with the default folder.
/// Returns the conflict files written, for a notification.
pub fn resync(profile: &Path, default_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut manifest = Manifest::load(profile);
    let mut conflicts = Vec::new();
    let mut changed = false;
    let names: Vec<String> = manifest.files.keys().cloned().collect();
    for name in names {
        let Ok(source) = fs::read(default_dir.join(&name)) else {
            continue;
        };
        let dest_path = profile.join(&name);
        let dest = fs::read(&dest_path).unwrap_or_default();
        let (source_hash, dest_hash) = (digest(&source), digest(&dest));
        let synced = manifest.files.get(&name).cloned().unwrap_or_default();
        match plan_resync(&source_hash, &dest_hash, &synced) {
            Resync::Keep => continue,
            Resync::Record => {}
            Resync::CopyToProfile => {
                crate::durable_fs::stage_sibling(&dest_path, &source, Some(0o644))?.commit()?;
            }
            Resync::Conflict => {
                let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                let saved = profile.join(format!("{name}.cc-logins-conflict-{stamp}"));
                fs::write(&saved, &dest)?;
                crate::durable_fs::stage_sibling(&dest_path, &source, Some(0o644))?.commit()?;
                conflicts.push(saved);
            }
        }
        manifest.files.insert(name, source_hash);
        changed = true;
    }
    if changed {
        manifest.save(profile)?;
    }
    Ok(conflicts)
}

/// Remove the links this module made in `profile`, and only those: a link
/// is removed only while it still points where the manifest says. Shared
/// directories' contents are never touched.
pub fn unshare(profile: &Path) -> io::Result<()> {
    let mut manifest = Manifest::load(profile);
    let names: Vec<String> = manifest.links.keys().cloned().collect();
    for name in names {
        let link = profile.join(&name);
        let recorded = manifest.links.get(&name).cloned().unwrap_or_default();
        if is_link_to(&link, &recorded) {
            remove_dir_link(&link)?;
        }
        manifest.links.remove(&name);
    }
    manifest.save(profile)
}

/// Whether `link` is a symlink or junction resolving to `target`.
pub fn is_link_to(link: &Path, target: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(link) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    match (fs::canonicalize(link), fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(unix)]
fn create_dir_link(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(unix)]
fn remove_dir_link(link: &Path) -> io::Result<()> {
    fs::remove_file(link)
}

/// A directory junction via `mklink /J`, which any user may create.
#[cfg(windows)]
fn create_dir_link(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mklink /J failed for {}",
            link.display()
        )))
    }
}

/// Removing a junction with `remove_dir` removes the junction only.
#[cfg(windows)]
fn remove_dir_link(link: &Path) -> io::Result<()> {
    fs::remove_dir(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::TempDir::new().unwrap();
        let default_dir = root.path().join("default");
        let profile = root.path().join("profile");
        fs::create_dir_all(default_dir.join("agents")).unwrap();
        fs::write(default_dir.join("agents").join("a.md"), "agent").unwrap();
        fs::write(default_dir.join("settings.json"), "{\"a\":1}").unwrap();
        fs::write(default_dir.join("CLAUDE.md"), "rules").unwrap();
        fs::create_dir_all(&profile).unwrap();
        (root, default_dir, profile)
    }

    #[test]
    fn plan_resync_covers_every_case() {
        assert_eq!(plan_resync("a", "a", "a"), Resync::Keep);
        assert_eq!(plan_resync("b", "b", "a"), Resync::Record);
        assert_eq!(plan_resync("a", "b", "a"), Resync::Keep);
        assert_eq!(plan_resync("b", "a", "a"), Resync::CopyToProfile);
        assert_eq!(plan_resync("b", "c", "a"), Resync::Conflict);
    }

    #[test]
    fn share_links_directories_and_copies_files() {
        let (_root, default_dir, profile) = setup();
        let manifest = share_into(&profile, &default_dir).unwrap();

        assert!(is_link_to(
            &profile.join("agents"),
            &default_dir.join("agents")
        ));
        assert_eq!(
            fs::read_to_string(profile.join("agents").join("a.md")).unwrap(),
            "agent"
        );
        assert_eq!(
            fs::read_to_string(profile.join("CLAUDE.md")).unwrap(),
            "rules"
        );
        assert!(manifest.links.contains_key("hooks"));
        assert!(manifest.files.contains_key("settings.json"));
        // Absent in the default folder: not copied, not recorded.
        assert!(!profile.join("keybindings.json").exists());
        assert!(!profile.join("plugins").exists());
        assert!(!profile.join("projects").exists());

        // Running again changes nothing.
        assert_eq!(share_into(&profile, &default_dir).unwrap(), manifest);
    }

    #[test]
    fn share_never_replaces_what_the_profile_already_has() {
        let (_root, default_dir, profile) = setup();
        fs::create_dir_all(profile.join("agents")).unwrap();
        fs::write(profile.join("CLAUDE.md"), "mine").unwrap();
        let manifest = share_into(&profile, &default_dir).unwrap();
        assert!(!manifest.links.contains_key("agents"));
        assert!(!manifest.files.contains_key("CLAUDE.md"));
        assert_eq!(
            fs::read_to_string(profile.join("CLAUDE.md")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn resync_copies_updates_and_saves_conflicts() {
        let (_root, default_dir, profile) = setup();
        share_into(&profile, &default_dir).unwrap();

        fs::write(default_dir.join("CLAUDE.md"), "rules v2").unwrap();
        assert!(resync(&profile, &default_dir).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(profile.join("CLAUDE.md")).unwrap(),
            "rules v2"
        );

        fs::write(default_dir.join("settings.json"), "{\"a\":2}").unwrap();
        fs::write(profile.join("settings.json"), "{\"a\":3}").unwrap();
        let conflicts = resync(&profile, &default_dir).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(fs::read_to_string(&conflicts[0]).unwrap(), "{\"a\":3}");
        assert_eq!(
            fs::read_to_string(profile.join("settings.json")).unwrap(),
            "{\"a\":2}"
        );
    }

    #[test]
    fn unshare_removes_links_but_never_their_contents() {
        let (_root, default_dir, profile) = setup();
        share_into(&profile, &default_dir).unwrap();
        unshare(&profile).unwrap();
        assert!(fs::symlink_metadata(profile.join("agents")).is_err());
        assert_eq!(
            fs::read_to_string(default_dir.join("agents").join("a.md")).unwrap(),
            "agent"
        );
        assert!(Manifest::load(&profile).links.is_empty());
    }

    #[test]
    fn unshare_leaves_a_link_that_now_points_elsewhere() {
        let (root, default_dir, profile) = setup();
        share_into(&profile, &default_dir).unwrap();
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        remove_dir_link(&profile.join("agents")).unwrap();
        create_dir_link(&elsewhere, &profile.join("agents")).unwrap();
        unshare(&profile).unwrap();
        assert!(is_link_to(&profile.join("agents"), &elsewhere));
    }
}
