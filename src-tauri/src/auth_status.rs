//! Asking Claude Code itself whether a profile is signed in, and as whom.
//!
//! Two read-only sources, neither of which touches a token:
//! - `claude auth status` (JSON by default) with `CLAUDE_CONFIG_DIR` set to
//!   the profile. Authoritative, but it starts a process, so it is used after
//!   a sign-in, on a failed usage read, and at startup — never on every tick.
//! - the profile's `.claude.json` `oauthAccount` block, which Claude Code
//!   writes when it signs in. Cheap enough to read on every tick, and what
//!   tells the app a sign-in finished or a folder now holds another account.
//!
//! Captured shape (Claude Code 2.1.285), logged in:
//! `{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
//!   "email":"…","orgId":"…","orgName":"…","subscriptionType":"max",
//!   "configDirectory":"…",…}` with exit status 0. Logged out prints the same
//! object with `"loggedIn":false,"authMethod":"none"` and exits 1, so the JSON
//! is read regardless of the exit status.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

/// How long `claude auth status` may take before it is abandoned.
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// Variables that would make Claude Code report a login other than the
/// profile's own (an API key or token in the app's environment wins over the
/// folder), so the check never passes them on.
const OVERRIDING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CC_LOGINS_PROFILE",
    "CC_LOGINS_SHIM_DEPTH",
];

/// `claude auth status`, as far as this app reads it. Unknown fields are
/// ignored so a newer Claude Code cannot break the parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub config_directory: Option<String>,
}

impl AuthStatus {
    /// Signed in with a Claude account (not an API key or a cloud provider),
    /// which is the only kind of login this app tracks.
    pub fn is_claude_account(&self) -> bool {
        self.logged_in && self.auth_method.as_deref() != Some("none") && self.email.is_some()
    }
}

/// Why the status could not be read. Distinct from "not signed in", which is
/// an ordinary [`AuthStatus`] with `logged_in: false`.
#[derive(Debug, thiserror::Error)]
pub enum AuthStatusError {
    #[error("could not run claude auth status: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("claude auth status did not answer within {0:?}")]
    TimedOut(Duration),
    #[error("claude auth status printed something this app cannot read")]
    Unreadable,
}

/// Parse the command's output. Tolerates text before the JSON object (a
/// warning line, say) by starting at the first `{`.
pub fn parse(stdout: &[u8]) -> Option<AuthStatus> {
    let text = String::from_utf8_lossy(stdout);
    let start = text.find('{')?;
    serde_json::from_str(text[start..].trim_end()).ok()
}

/// Run `claude auth status` for a profile: `Some(dir)` sets
/// `CLAUDE_CONFIG_DIR`, `None` checks the default `~/.claude` with the
/// variable removed.
pub fn check(claude: &Path, config_dir: Option<&str>) -> Result<AuthStatus, AuthStatusError> {
    let mut command = Command::new(claude);
    command
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in OVERRIDING_ENV {
        command.env_remove(name);
    }
    match config_dir {
        Some(dir) => command.env(crate::shim_core::CONFIG_DIR_ENV, dir),
        None => command.env_remove(crate::shim_core::CONFIG_DIR_ENV),
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn()?;
    let deadline = Instant::now() + CHECK_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AuthStatusError::TimedOut(CHECK_TIMEOUT));
            }
        }
    }
    let output = child.wait_with_output()?;
    parse(&output.stdout).ok_or(AuthStatusError::Unreadable)
}

/// Who a profile folder says it is signed in as, from `.claude.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderIdentity {
    pub email: String,
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
}

/// The `oauthAccount` block of a Claude Code global config file, or `None`
/// when the file is missing, unreadable or signed out.
pub fn read_folder_identity(global_config: &Path) -> Option<FolderIdentity> {
    let text = std::fs::read_to_string(global_config).ok()?;
    identity_from_config_text(&text)
}

/// Pure half of [`read_folder_identity`].
pub fn identity_from_config_text(text: &str) -> Option<FolderIdentity> {
    let config: Value = serde_json::from_str(text).ok()?;
    let account = config.get("oauthAccount")?.as_object()?;
    let field = |name: &str| {
        account
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    Some(FolderIdentity {
        email: field("emailAddress")?,
        account_uuid: field("accountUuid"),
        organization_uuid: field("organizationUuid"),
        organization_name: field("organizationName"),
    })
}

/// The global config file for a profile: its own folder's, or for the
/// default profile the one Claude Code uses without `CLAUDE_CONFIG_DIR`.
pub fn global_config_for(config_dir: Option<&str>) -> std::path::PathBuf {
    match config_dir {
        Some(dir) => crate::paths::global_config_path_in(Path::new(dir)),
        None => crate::paths::default_global_config_path(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGGED_IN: &str = r#"{
  "loggedIn": true,
  "authMethod": "claude.ai",
  "apiProvider": "firstParty",
  "analyticsDisabled": false,
  "projectsDirectory": "/home/u/.cc-logins/profiles/p2-abcdef/projects",
  "configDirectory": "/home/u/.cc-logins/profiles/p2-abcdef",
  "email": "sam@example.com",
  "orgId": "org-1",
  "orgName": "Sam's Org",
  "subscriptionType": "max"
}"#;

    const LOGGED_OUT: &str = r#"{
  "loggedIn": false,
  "authMethod": "none",
  "apiProvider": "firstParty",
  "analyticsDisabled": false,
  "projectsDirectory": "/tmp/x/projects",
  "configDirectory": "/tmp/x"
}"#;

    #[test]
    fn parses_a_signed_in_profile() {
        let status = parse(LOGGED_IN.as_bytes()).unwrap();
        assert!(status.logged_in);
        assert!(status.is_claude_account());
        assert_eq!(status.email.as_deref(), Some("sam@example.com"));
        assert_eq!(status.org_id.as_deref(), Some("org-1"));
        assert_eq!(status.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn parses_a_signed_out_profile() {
        let status = parse(LOGGED_OUT.as_bytes()).unwrap();
        assert!(!status.logged_in);
        assert!(!status.is_claude_account());
        assert_eq!(status.email, None);
    }

    #[test]
    fn tolerates_a_leading_warning_and_rejects_garbage() {
        let noisy = format!("Warning: something\n{LOGGED_OUT}\n");
        assert!(parse(noisy.as_bytes()).is_some());
        assert_eq!(parse(b"command not found"), None);
    }

    #[test]
    fn folder_identity_reads_oauth_account() {
        let text = r#"{"numStartups":3,"oauthAccount":{"accountUuid":"u-1",
            "emailAddress":"sam@example.com","organizationUuid":"org-1",
            "organizationName":"Sam's Org"}}"#;
        let identity = identity_from_config_text(text).unwrap();
        assert_eq!(identity.email, "sam@example.com");
        assert_eq!(identity.account_uuid.as_deref(), Some("u-1"));
        assert_eq!(identity.organization_uuid.as_deref(), Some("org-1"));
    }

    #[test]
    fn folder_identity_is_none_when_signed_out_or_blank() {
        assert_eq!(identity_from_config_text(r#"{"numStartups":1}"#), None);
        assert_eq!(
            identity_from_config_text(r#"{"oauthAccount":{"emailAddress":" "}}"#),
            None
        );
        assert_eq!(identity_from_config_text("not json"), None);
    }

    #[test]
    fn folder_identity_from_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::paths::global_config_path_in(dir.path());
        assert_eq!(read_folder_identity(&config), None);
        std::fs::write(&config, r#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#).unwrap();
        assert_eq!(read_folder_identity(&config).unwrap().email, "a@b.c");
    }
}
