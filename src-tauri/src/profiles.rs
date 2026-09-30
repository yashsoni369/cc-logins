//! Per-account Claude Code profiles (v0.4 token-free mode).
//!
//! Each account gets its own `CLAUDE_CONFIG_DIR` folder under
//! `~/.cc-logins/profiles/`, and signs in there once through Claude Code's own
//! `claude auth login`. The app never copies, stores or refreshes the login
//! inside. It only decides which folder a new `claude` session starts in, by
//! writing `shim.json` for the shim to read.
//!
//! This module holds the pure rules (folder naming, path normalisation,
//! launcher slugs, the macOS Keychain service name Claude Code derives from a
//! folder) plus the one writer for `shim.json`.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::shim_core::ShimConfig;

/// Claude Code's macOS Keychain service for the default profile.
pub const KEYCHAIN_SERVICE_BASE: &str = "Claude Code-credentials";

/// Longest launcher slug, so `claude-<slug>` stays a comfortable command.
const MAX_SLUG_LEN: usize = 24;

/// Folder name for a new profile: `p<slot>-<suffix>`.
///
/// The name never changes after creation. On macOS Claude Code names its
/// Keychain item after a hash of the exact folder path, so renaming the folder
/// would silently orphan the login. Renaming an account changes its launcher
/// slug instead.
pub fn profile_dir_name(slot: u32, suffix: &str) -> String {
    format!("p{slot}-{suffix}")
}

/// Six lowercase hex characters that make a profile folder name unique even
/// when a slot number is reused after an account is removed. Not a secret;
/// only needs to differ between calls.
pub fn random_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    crate::hex::lower(&hasher.finalize())[..6].to_string()
}

/// Why a candidate profile path was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfilePathError {
    #[error("profile path must be absolute: {0}")]
    NotAbsolute(String),
    #[error("profile path must be inside the home directory: {0}")]
    OutsideHome(String),
    #[error("profile path has a `~`, `.` or `..` segment: {0}")]
    RelativeSegment(String),
    #[error("profile path must not use the \\\\?\\ prefix: {0}")]
    Verbatim(String),
    #[error("profile folder names must be plain ASCII: {0}")]
    NonAscii(String),
    #[error("profile path is not valid Unicode: {0}")]
    NotUnicode(String),
    #[error("the default ~/.claude folder is not a profile folder")]
    IsDefault,
}

/// The exact string stored for a profile folder, used everywhere afterwards:
/// `shim.json`, launchers, sign-in, and the Keychain service name.
///
/// Pure: never touches the filesystem, and in particular never canonicalises
/// (that would produce a `\\?\` path on Windows and resolve symlinks the user
/// may rely on). Rules:
/// - absolute, and inside `home`;
/// - no `~`, `.` or `..` segment, no verbatim prefix;
/// - the segments below `home` are non-empty ASCII;
/// - no trailing separator;
/// - never `home/.claude` itself.
pub fn normalize_profile_path(home: &Path, candidate: &Path) -> Result<String, ProfilePathError> {
    let shown = || candidate.to_string_lossy().into_owned();
    if !candidate.is_absolute() {
        return Err(ProfilePathError::NotAbsolute(shown()));
    }
    for component in candidate.components() {
        match component {
            Component::CurDir | Component::ParentDir => {
                return Err(ProfilePathError::RelativeSegment(shown()));
            }
            Component::Normal(part) if part == "~" => {
                return Err(ProfilePathError::RelativeSegment(shown()));
            }
            Component::Prefix(prefix) if prefix.kind().is_verbatim() => {
                return Err(ProfilePathError::Verbatim(shown()));
            }
            _ => {}
        }
    }
    let below = candidate
        .strip_prefix(home)
        .map_err(|_| ProfilePathError::OutsideHome(shown()))?;
    let mut rebuilt = home.to_path_buf();
    let mut segments = 0;
    for component in below.components() {
        let Component::Normal(part) = component else {
            return Err(ProfilePathError::RelativeSegment(shown()));
        };
        let part = part
            .to_str()
            .ok_or_else(|| ProfilePathError::NotUnicode(shown()))?;
        if part.is_empty() || !part.is_ascii() {
            return Err(ProfilePathError::NonAscii(shown()));
        }
        rebuilt.push(part);
        segments += 1;
    }
    if segments == 0 {
        return Err(ProfilePathError::OutsideHome(shown()));
    }
    if crate::shim_core::same_dir(&rebuilt, &home.join(".claude")) {
        return Err(ProfilePathError::IsDefault);
    }
    let text = rebuilt
        .to_str()
        .ok_or_else(|| ProfilePathError::NotUnicode(shown()))?;
    Ok(text.trim_end_matches(['/', '\\']).to_string())
}

/// The macOS Keychain service Claude Code uses for a profile.
///
/// `None` (the default profile, variable unset) is the plain service. Any
/// other folder gets `-<first 8 hex of sha256(exact path string)>`, which is
/// why [`normalize_profile_path`] fixes the string once and it is never
/// rebuilt.
pub fn keychain_service_for(config_dir: Option<&str>) -> String {
    match config_dir {
        None => KEYCHAIN_SERVICE_BASE.to_string(),
        Some(dir) => {
            let digest = Sha256::digest(dir.as_bytes());
            format!(
                "{KEYCHAIN_SERVICE_BASE}-{}",
                &crate::hex::lower(&digest)[..8]
            )
        }
    }
}

/// A launcher slug for `claude-<slug>`, derived from an alias or email and
/// made unique against `taken`.
///
/// Lowercase ASCII letters and digits, with runs of anything else collapsed
/// to one `-`. An email contributes only its local part. Empty input becomes
/// `account`; a clash gets `-2`, `-3`, ….
pub fn launcher_slug(base: &str, taken: &[String]) -> String {
    let source = base.split('@').next().unwrap_or(base);
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    slug.truncate(MAX_SLUG_LEN);
    let slug = slug.trim_end_matches('-').to_string();
    let slug = if slug.is_empty() {
        "account".to_string()
    } else {
        slug
    };
    if !taken.iter().any(|t| t == &slug) {
        return slug;
    }
    (2u32..)
        .map(|n| format!("{slug}-{n}"))
        .find(|candidate| !taken.iter().any(|t| t == candidate))
        .expect("an unbounded counter always finds a free slug")
}

/// Where the shim reads its configuration.
pub fn shim_config_path() -> PathBuf {
    crate::paths::cc_logins_home().join(crate::shim_core::SHIM_CONFIG_FILE)
}

/// Read `shim.json`, or `None` when absent or unreadable.
pub fn read_shim_config(path: &Path) -> Option<ShimConfig> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| ShimConfig::parse(&bytes))
}

/// Replace `shim.json` atomically, so a `claude` starting mid-write sees
/// either the old selection or the new one, never a torn file.
pub fn write_shim_config(path: &Path, config: &ShimConfig) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(config).map_err(std::io::Error::other)?;
    crate::durable_fs::stage_sibling(path, &bytes, Some(0o644))?.commit()?;
    Ok(())
}

/// Read-modify-write `shim.json`, stamping the current format version.
pub fn update_shim_config(update: impl FnOnce(&mut ShimConfig)) -> std::io::Result<ShimConfig> {
    let path = shim_config_path();
    let mut config = read_shim_config(&path).unwrap_or_default();
    config.v = crate::shim_core::SHIM_CONFIG_VERSION;
    update(&mut config);
    write_shim_config(&path, &config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim_core::{ShimProfile, SHIM_CONFIG_VERSION};

    fn home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\alice")
        } else {
            PathBuf::from("/Users/alice")
        }
    }

    #[test]
    fn dir_name_is_slot_and_suffix() {
        assert_eq!(profile_dir_name(3, "a1b2c3"), "p3-a1b2c3");
    }

    #[test]
    fn random_suffix_is_six_hex_and_varies() {
        let a = random_suffix();
        let b = random_suffix();
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn normalize_accepts_a_plain_profile_path() {
        let home = home();
        let candidate = home.join(".cc-logins").join("profiles").join("p2-abcdef");
        let got = normalize_profile_path(&home, &candidate).unwrap();
        assert_eq!(got, candidate.to_str().unwrap());
    }

    #[test]
    fn normalize_drops_a_trailing_separator() {
        let home = home();
        let candidate = home.join(".cc-logins").join("p2");
        let with_slash = PathBuf::from(format!(
            "{}{}",
            candidate.display(),
            std::path::MAIN_SEPARATOR
        ));
        let got = normalize_profile_path(&home, &with_slash).unwrap();
        assert_eq!(got, candidate.to_str().unwrap());
    }

    #[test]
    fn normalize_rejects_relative_and_dot_segments() {
        let home = home();
        assert!(matches!(
            normalize_profile_path(&home, Path::new("relative/p1")),
            Err(ProfilePathError::NotAbsolute(_))
        ));
        assert!(matches!(
            normalize_profile_path(&home, &home.join("..").join("bob")),
            Err(ProfilePathError::RelativeSegment(_))
        ));
        assert!(matches!(
            normalize_profile_path(&home, &home.join("~").join("p1")),
            Err(ProfilePathError::RelativeSegment(_))
        ));
    }

    #[test]
    fn normalize_rejects_outside_home_and_home_itself() {
        let home = home();
        let elsewhere = if cfg!(windows) {
            PathBuf::from(r"D:\profiles\p1")
        } else {
            PathBuf::from("/opt/profiles/p1")
        };
        assert!(matches!(
            normalize_profile_path(&home, &elsewhere),
            Err(ProfilePathError::OutsideHome(_))
        ));
        assert!(matches!(
            normalize_profile_path(&home, &home),
            Err(ProfilePathError::OutsideHome(_))
        ));
    }

    #[test]
    fn normalize_rejects_the_default_claude_dir() {
        let home = home();
        assert_eq!(
            normalize_profile_path(&home, &home.join(".claude")),
            Err(ProfilePathError::IsDefault)
        );
    }

    #[test]
    fn normalize_rejects_non_ascii_segments() {
        let home = home();
        assert!(matches!(
            normalize_profile_path(&home, &home.join("prófile")),
            Err(ProfilePathError::NonAscii(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn normalize_rejects_verbatim_paths() {
        let home = home();
        assert!(matches!(
            normalize_profile_path(&home, Path::new(r"\\?\C:\Users\alice\p1")),
            Err(ProfilePathError::Verbatim(_))
        ));
    }

    #[test]
    fn keychain_service_default_and_hashed() {
        assert_eq!(keychain_service_for(None), "Claude Code-credentials");
        // sha256("/Users/alice/.cc-logins/profiles/p2-abcdef") begins 2d1c6e3e.
        assert_eq!(
            keychain_service_for(Some("/Users/alice/.cc-logins/profiles/p2-abcdef")),
            format!("Claude Code-credentials-{KEYCHAIN_FIXTURE}")
        );
        // A trailing slash is a different string, so a different item. This
        // is exactly why the path is normalised once and stored.
        assert_ne!(
            keychain_service_for(Some("/Users/alice/.cc-logins/profiles/p2-abcdef/")),
            keychain_service_for(Some("/Users/alice/.cc-logins/profiles/p2-abcdef"))
        );
    }

    const KEYCHAIN_FIXTURE: &str = "2d1c6e3e";

    #[test]
    fn slug_from_alias_and_email() {
        assert_eq!(launcher_slug("Work Max", &[]), "work-max");
        assert_eq!(launcher_slug("sam.lee@example.com", &[]), "sam-lee");
        assert_eq!(launcher_slug("  --  ", &[]), "account");
        assert_eq!(launcher_slug("Ünïcode", &[]), "n-code");
    }

    #[test]
    fn slug_is_bounded_and_unique() {
        let long = "a".repeat(60);
        assert_eq!(launcher_slug(&long, &[]).len(), MAX_SLUG_LEN);
        let taken = vec!["work".to_string(), "work-2".to_string()];
        assert_eq!(launcher_slug("work", &taken), "work-3");
    }

    #[test]
    fn shim_config_write_then_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("shim.json");
        assert_eq!(read_shim_config(&path), None);
        let config = ShimConfig {
            v: SHIM_CONFIG_VERSION,
            selected: Some(ShimProfile {
                account: 2,
                config_dir: Some("/p/two".to_string()),
            }),
            ..ShimConfig::default()
        };
        write_shim_config(&path, &config).unwrap();
        assert_eq!(read_shim_config(&path), Some(config));
    }
}
