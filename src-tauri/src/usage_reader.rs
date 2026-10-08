//! Reading a profile's access token where Claude Code keeps it, for one
//! usage request.
//!
//! This is the whole of the app's contact with a Claude token in v0.4: it
//! reads the current access token from the profile's own store (the
//! `.credentials.json` file on Windows and Linux, Claude Code's Keychain item
//! on macOS), uses it for a single usage request, and drops it. It never
//! writes, copies, caches or refreshes one. An expired token is left for
//! Claude Code to refresh on its next run; until then the account is shown
//! with its last reading.

use std::path::{Path, PathBuf};

/// What the profile's store holds right now.
#[derive(Debug, PartialEq, Eq)]
pub enum Access {
    /// A current access token. Use it once, then drop it.
    Token(String),
    /// A login whose access token has expired. Claude Code refreshes it the
    /// next time a session starts on this account; the app must not.
    Expired,
    /// No login in the store.
    Missing,
    /// The store could not be read (a Keychain prompt dismissed, a file
    /// permission). The message never contains credential bytes.
    Unreadable(String),
}

/// Read a profile's access token in place. `None` is the default profile.
pub fn read_access(config_dir: Option<&str>) -> Access {
    let now_ms = chrono::Utc::now().timestamp_millis() as f64;
    match read_raw(config_dir) {
        Ok(Some(raw)) => classify(&raw, now_ms),
        Ok(None) => Access::Missing,
        Err(message) => Access::Unreadable(message),
    }
}

/// Pure half of [`read_access`]: what a stored credential blob amounts to at
/// `now_ms`. Uses the same 5-minute early-expiry margin as the rest of the
/// app, so a token about to lapse is never sent.
pub fn classify(raw: &str, now_ms: f64) -> Access {
    let Some(oauth) = crate::oauth::extract_oauth_data(raw) else {
        return Access::Missing;
    };
    let token = oauth
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if token.is_empty() {
        return Access::Missing;
    }
    let expires_at = oauth.get("expiresAt").and_then(serde_json::Value::as_f64);
    if crate::oauth::is_oauth_token_expired_at(expires_at, now_ms) {
        return Access::Expired;
    }
    Access::Token(token.to_string())
}

/// The credentials file Claude Code uses for a profile.
pub fn credentials_file(config_dir: Option<&str>) -> PathBuf {
    match config_dir {
        Some(dir) => crate::paths::credentials_path_in(Path::new(dir)),
        None => crate::paths::credentials_path_in(&crate::sys_env::default_claude_config_dir()),
    }
}

pub(crate) fn read_raw(config_dir: Option<&str>) -> Result<Option<String>, String> {
    #[cfg(target_os = "macos")]
    {
        // Claude Code keeps the login in the Keychain on macOS, under a
        // service named after the exact folder path; the file is only its
        // fallback when the Keychain is unavailable.
        let service = crate::profiles::keychain_service_for(config_dir);
        match crate::credentials::read_claude_keychain_item(&service) {
            Ok(Some(raw)) => return Ok(Some(raw)),
            Ok(None) => {}
            Err(message) => {
                if !credentials_file(config_dir).exists() {
                    return Err(message);
                }
            }
        }
    }
    match std::fs::read_to_string(credentials_file(config_dir)) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "could not read the profile's credentials file: {}",
            error.kind()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_800_000_000_000.0;

    fn blob(token: &str, expires_at: f64) -> String {
        serde_json::json!({
            "claudeAiOauth": { "accessToken": token, "refreshToken": "r", "expiresAt": expires_at }
        })
        .to_string()
    }

    #[test]
    fn a_current_token_is_used() {
        let raw = blob("tok", NOW + 3_600_000.0);
        assert_eq!(classify(&raw, NOW), Access::Token("tok".to_string()));
    }

    #[test]
    fn an_expired_or_nearly_expired_token_is_never_sent() {
        assert_eq!(classify(&blob("tok", NOW - 1.0), NOW), Access::Expired);
        // Inside the 5-minute margin counts as expired.
        assert_eq!(classify(&blob("tok", NOW + 60_000.0), NOW), Access::Expired);
    }

    #[test]
    fn a_blob_without_a_token_is_missing() {
        assert_eq!(classify("{}", NOW), Access::Missing);
        assert_eq!(classify(&blob("", NOW + 3_600_000.0), NOW), Access::Missing);
        assert_eq!(classify("not json", NOW), Access::Missing);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn reads_the_profile_file_in_place_without_changing_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_str = dir.path().to_str().unwrap();
        assert_eq!(read_access(Some(dir_str)), Access::Missing);

        let expires = chrono::Utc::now().timestamp_millis() as f64 + 3_600_000.0;
        let raw = blob("tok", expires);
        let file = credentials_file(Some(dir_str));
        std::fs::write(&file, &raw).unwrap();
        assert_eq!(read_access(Some(dir_str)), Access::Token("tok".to_string()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), raw);
    }
}
