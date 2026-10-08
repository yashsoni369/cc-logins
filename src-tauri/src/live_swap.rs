//! Opt-in: sessions that are already running follow the selection.
//!
//! By default v0.4 only points *new* sessions at an account's folder. With
//! Settings -> "Switch running sessions too" on, picking an account also
//! swaps its login into the default `~/.claude`, the way v0.3 did, so a
//! running `claude` picks it up on its next request.
//!
//! There is still no vault. Each account's own profile folder stays its
//! home: swapping in copies the folder's login into `~/.claude`, and swapping
//! out hands the live login back to the outgoing account's folder. That
//! write-back matters because Claude Code rotates the refresh token while an
//! account is live, so the folder copy is stale until it comes home. The
//! swap itself is the journaled v0.3 transaction
//! ([`crate::switch_transaction::execute_locked`]), so a crash midway is
//! recovered at the next start. The app still never refreshes a token.
//!
//! In this mode `~/.claude` is the live slot, not any account's home, so the
//! default account first gets a folder of its own
//! ([`move_default_into`]).

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};

use crate::credentials::{CredentialError, CredentialStore};
use crate::switch_transaction::OutgoingDestination;
use crate::switcher::{GuiStoreHost, SwitchError};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether the live swap is on. Mirrors the setting, so code without access
/// to the settings store (`shim.json` sync, usage reads) can ask.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

/// Write a login into a profile folder's store: the Keychain item Claude Code
/// derives from the folder path on macOS (falling back to the file, as
/// Claude Code does), the `.credentials.json` file elsewhere.
pub(crate) fn write_profile_credentials(
    config_dir: &str,
    raw: &str,
) -> Result<(), CredentialError> {
    let file = crate::paths::credentials_path_in(std::path::Path::new(config_dir));
    // Never under test: the Keychain is machine-global, so a test would write
    // real items no temp directory can contain.
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let service = crate::profiles::keychain_service_for(Some(config_dir));
        match crate::credentials::write_claude_keychain_item(&service, raw) {
            // Keep an existing file in step so it never shadows the Keychain
            // with an older login.
            Ok(()) if !file.exists() => return Ok(()),
            Ok(()) => {}
            Err(error) => log::warn!("profile Keychain write failed, using the file: {error}"),
        }
    }
    crate::durable_fs::stage_sibling(&file, raw.as_bytes(), Some(0o600))
        .and_then(|stage| stage.commit())
        .map_err(|error| CredentialError::Write(format!("profile credentials: {error}")))
}

fn record<'a>(data: &'a Map<String, Value>, number: &str) -> Option<&'a Map<String, Value>> {
    data.get("accounts")?.get(number)?.as_object()
}

fn record_dir(record: &Map<String, Value>) -> Option<String> {
    record
        .get("configDir")
        .and_then(Value::as_str)
        .filter(|dir| !dir.is_empty())
        .map(str::to_string)
}

/// The default account (`configDir: null`), which must move to a folder of
/// its own before the live swap can be turned on.
pub fn default_without_folder(data: &Map<String, Value>) -> Option<u32> {
    data.get("accounts")?
        .as_object()?
        .iter()
        .find(|(_, record)| matches!(record.get("configDir"), Some(Value::Null)))
        .and_then(|(number, _)| number.parse().ok())
}

/// A live login whose tokens were wiped (signed out mid-flight) is not worth
/// writing over a folder's good copy.
fn tokens_wiped(live: &str) -> bool {
    crate::oauth::extract_oauth_data(live).is_some_and(|oauth| {
        let present = |key: &str| {
            oauth
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
        };
        !present("accessToken") && !present("refreshToken")
    })
}

/// Where the live login goes when another account is swapped in. Identity
/// comes from `~/.claude.json` (offline): the login goes back to that
/// account's folder, or to the recovery stash when it belongs to no folder,
/// so a folder is never overwritten with someone else's login.
fn outgoing_destination(
    data: &Map<String, Value>,
    current: Option<&str>,
    live: &str,
) -> OutgoingDestination {
    let Some(number) = current else {
        return OutgoingDestination::Unclaimed;
    };
    let Some(record) = record(data, number) else {
        return OutgoingDestination::Unclaimed;
    };
    let Some(config_dir) = record_dir(record) else {
        return OutgoingDestination::Unclaimed;
    };
    if tokens_wiped(live) {
        log::warn!("live login tokens are wiped; keeping them out of account {number}'s folder");
        return OutgoingDestination::Unclaimed;
    }
    OutgoingDestination::ProfileFolder {
        number: number.to_string(),
        email: record
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        config_dir,
    }
}

fn folder_oauth_account(config_dir: &str) -> Option<Value> {
    let path = crate::paths::global_config_path_in(std::path::Path::new(config_dir));
    let text = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&text).ok()?;
    config
        .get("oauthAccount")
        .filter(|value| value.is_object())
        .cloned()
}

/// Select `number` and swap its login into `~/.claude`, in one journaled
/// transaction. The target must be a ready profile account with a folder.
pub fn swap_to(number: u32) -> Result<(), SwitchError> {
    crate::switcher::refuse_pending_recovery()?;
    let num = number.to_string();
    let account = crate::switcher::read_accounts()?
        .into_iter()
        .find(|account| account.number == number)
        .ok_or_else(|| SwitchError::UnknownAccount(num.clone()))?;
    let config_dir = account
        .profile
        .as_ref()
        .and_then(|profile| profile.config_dir.clone())
        .ok_or_else(|| {
            SwitchError::InvalidInput(
                "this account has no folder of its own yet, so it can't be swapped in".into(),
            )
        })?;

    let swapped = {
        let _locks =
            crate::switch_transaction::acquire_live_state_locks(crate::locking::DEFAULT_TIMEOUT)?;
        let mut data =
            crate::switcher::read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;
        crate::profile_registry::apply_selection(&mut data, number)?;

        let current = crate::switcher::current_account_number(&data);
        if current.as_deref() == Some(num.as_str()) {
            // Already live: `~/.claude` holds its newest login. Record the
            // selection only.
            data.insert(
                "lastUpdated".into(),
                Value::String(chrono::Utc::now().to_rfc3339()),
            );
            crate::switcher::write_sequence_data(&data)?;
            data
        } else {
            let folder = crate::auth_status::read_folder_identity(
                &crate::paths::global_config_path_in(std::path::Path::new(&config_dir)),
            );
            if !folder.is_some_and(|identity| crate::switcher::folder_matches(&account, &identity))
            {
                return Err(SwitchError::InvalidInput(
                    "this account's folder is signed out or signed in as someone else; sign in again first"
                        .into(),
                ));
            }
            let target_credentials = crate::usage_reader::read_raw(Some(config_dir.as_str()))
                .map_err(SwitchError::InvalidInput)?
                .ok_or_else(|| SwitchError::NoStoredCredentials(num.clone()))?;
            let target_oauth = folder_oauth_account(&config_dir)
                .ok_or_else(|| SwitchError::InvalidBackupConfig(num.clone()))?;

            let mut store = CredentialStore::new(GuiStoreHost);
            let live = store.read_active_credentials().value.unwrap_or_default();
            let outgoing = outgoing_destination(&data, current.as_deref(), &live);
            let plan = crate::switch_transaction::SwitchPlan {
                target: crate::switch_journal::JournalTarget {
                    number: num.clone(),
                    email: account.email.clone(),
                    stable_key: account.stable_key(),
                    credential_generation: format!(
                        "sha256-full:{}",
                        crate::switch_journal::sha256(target_credentials.as_bytes())
                    ),
                },
                target_credentials,
                target_oauth,
                sequence: data.clone(),
                sequence_path: crate::switcher::accounts_file(),
                global_config_path: crate::paths::global_config_path(),
                outgoing,
            };
            crate::switch_transaction::execute_locked(
                &mut store,
                plan,
                &crate::switch_transaction::NoFaults,
            )?;
            data
        }
    };

    if let Err(error) = crate::profile_registry::sync_shim(&swapped) {
        log::warn!("shim.json not updated: {error}");
    }
    Ok(())
}

/// Turning the live swap off: hand the live login back to its account's
/// folder, which new sessions use again from now on. `~/.claude` keeps its
/// copy, so sessions already running are not cut off.
pub fn send_live_home() -> Result<(), SwitchError> {
    crate::switcher::refuse_pending_recovery()?;
    let _locks =
        crate::switch_transaction::acquire_live_state_locks(crate::locking::DEFAULT_TIMEOUT)?;
    let data = crate::switcher::read_sequence_data().unwrap_or_default();
    let mut store = CredentialStore::new(GuiStoreHost);
    let Some(live) = store.read_active_credentials().value else {
        return Ok(());
    };
    let current = crate::switcher::current_account_number(&data);
    if let OutgoingDestination::ProfileFolder { config_dir, .. } =
        outgoing_destination(&data, current.as_deref(), &live)
    {
        write_profile_credentials(&config_dir, &live)?;
    }
    Ok(())
}

/// Point a registry record at `dir`. Pure.
fn apply_relocation(data: &mut Map<String, Value>, number: u32, dir: &str) -> bool {
    let Some(record) = data
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .and_then(|accounts| accounts.get_mut(&number.to_string()))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    record.insert("configDir".into(), Value::String(dir.to_string()));
    true
}

/// Give the default account its own folder, `dir` (already created and
/// seeded): its login and `oauthAccount` are copied there and its record
/// points at it. `~/.claude` keeps the login, so nothing running notices.
/// `Ok(None)` when no account uses the default folder.
pub fn move_default_into(dir: &str) -> Result<Option<u32>, SwitchError> {
    crate::switcher::refuse_pending_recovery()?;
    let moved = {
        let _locks =
            crate::switch_transaction::acquire_live_state_locks(crate::locking::DEFAULT_TIMEOUT)?;
        let mut data =
            crate::switcher::read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;
        let Some(number) = default_without_folder(&data) else {
            return Ok(None);
        };
        if crate::switcher::current_account_number(&data) != Some(number.to_string()) {
            return Err(SwitchError::InvalidInput(
                "Claude Code's own folder is signed in as a different account. Sign back in to it with /login first."
                    .into(),
            ));
        }
        let mut store = CredentialStore::new(GuiStoreHost);
        let live = store
            .read_active_credentials()
            .value
            .filter(|live| !live.is_empty() && !tokens_wiped(live))
            .ok_or(SwitchError::NoLiveCredential)?;
        let oauth = std::fs::read_to_string(crate::paths::global_config_path())
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|config| config.get("oauthAccount").cloned())
            .ok_or_else(|| SwitchError::InvalidBackupConfig(number.to_string()))?;

        write_profile_credentials(dir, &live)?;
        let config_path = crate::paths::global_config_path_in(std::path::Path::new(dir));
        let mut config = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        config.insert("oauthAccount".into(), oauth);
        let bytes = serde_json::to_vec_pretty(&Value::Object(config))?;
        crate::durable_fs::stage_sibling(&config_path, &bytes, Some(0o600))
            .and_then(|stage| stage.commit())
            .map_err(std::io::Error::from)?;

        apply_relocation(&mut data, number, dir);
        data.insert(
            "lastUpdated".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
        crate::switcher::write_sequence_data(&data)?;
        (number, data)
    };
    if let Err(error) = crate::profile_registry::sync_shim(&moved.1) {
        log::warn!("shim.json not updated: {error}");
    }
    Ok(Some(moved.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard, StoreRootGuard};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    struct Env {
        _guards: Vec<EnvGuard>,
        _store: StoreRootGuard,
        _dirs: Vec<TempDir>,
        profiles: TempDir,
        vault: std::path::PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn setup() -> Env {
        let lock = env_lock();
        let home = TempDir::new().unwrap();
        let live = TempDir::new().unwrap();
        let vault = TempDir::new().unwrap();
        let profiles = TempDir::new().unwrap();
        let home_text = home.path().to_string_lossy().into_owned();
        let guards = vec![
            EnvGuard::set("HOME", &home_text),
            EnvGuard::set("USERPROFILE", &home_text),
            EnvGuard::set("CLAUDE_CONFIG_DIR", &live.path().to_string_lossy()),
            EnvGuard::unset("XDG_DATA_HOME"),
            EnvGuard::unset("WSL_DISTRO_NAME"),
        ];
        let store = StoreRootGuard::set(vault.path().to_path_buf());
        Env {
            _guards: guards,
            _store: store,
            vault: vault.path().to_path_buf(),
            _dirs: vec![home, live, vault],
            profiles,
            _lock: lock,
        }
    }

    fn login(token: &str) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"{token}-refresh"}}}}"#
        )
    }

    fn folder(env: &Env, name: &str, email: &str, token: &str) -> String {
        let dir = env.profiles.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".credentials.json"), login(token)).unwrap();
        fs::write(
            dir.join(".claude.json"),
            json!({"oauthAccount": {"emailAddress": email, "organizationUuid": ""}}).to_string(),
        )
        .unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn set_live(email: &str, token: &str) {
        fs::create_dir_all(crate::paths::claude_config_home()).unwrap();
        fs::write(crate::paths::credentials_path(), login(token)).unwrap();
        fs::write(
            crate::paths::global_config_path(),
            json!({"oauthAccount": {"emailAddress": email, "organizationUuid": ""}, "keep": 1})
                .to_string(),
        )
        .unwrap();
    }

    fn registry(env: &Env, accounts: Value, selected: u32) {
        let data = json!({
            "sequence": [1, 2],
            "selectedAccountNumber": selected,
            "accounts": accounts,
        });
        fs::write(env.vault.join("sequence.json"), data.to_string()).unwrap();
    }

    fn profile(email: &str, dir: Option<&str>) -> Value {
        json!({
            "email": email,
            "organizationUuid": "",
            "configDir": dir,
            "launcher": email.split('@').next().unwrap(),
            "profileState": "ready",
        })
    }

    fn live_token() -> String {
        let raw = fs::read_to_string(crate::paths::credentials_path()).unwrap();
        crate::oauth::extract_access_token(&raw).unwrap()
    }

    fn folder_token(dir: &str) -> String {
        let raw = fs::read_to_string(std::path::Path::new(dir).join(".credentials.json")).unwrap();
        crate::oauth::extract_access_token(&raw).unwrap()
    }

    #[test]
    fn swap_installs_the_target_and_sends_the_live_login_home() {
        let env = setup();
        let a = folder(&env, "a", "a@example.com", "a-stale");
        let b = folder(&env, "b", "b@example.com", "b-token");
        set_live("a@example.com", "a-rotated");
        registry(
            &env,
            json!({"1": profile("a@example.com", Some(&a)), "2": profile("b@example.com", Some(&b))}),
            1,
        );

        swap_to(2).unwrap();

        assert_eq!(live_token(), "b-token");
        assert_eq!(folder_token(&a), "a-rotated", "the rotated login went home");
        let config: Value =
            serde_json::from_str(&fs::read_to_string(crate::paths::global_config_path()).unwrap())
                .unwrap();
        assert_eq!(config["oauthAccount"]["emailAddress"], "b@example.com");
        assert_eq!(config["keep"], 1);
        let data = crate::switcher::read_sequence_data().unwrap();
        assert_eq!(crate::profile_registry::selected_number(&data), Some(2));
        assert_eq!(data["activeAccountNumber"], 2);

        swap_to(1).unwrap();
        assert_eq!(live_token(), "a-rotated");
        assert_eq!(folder_token(&b), "b-token");
    }

    #[test]
    fn a_live_login_from_no_folder_never_overwrites_one() {
        let env = setup();
        let a = folder(&env, "a", "a@example.com", "a-token");
        let b = folder(&env, "b", "b@example.com", "b-token");
        set_live("stranger@example.com", "stranger");
        registry(
            &env,
            json!({"1": profile("a@example.com", Some(&a)), "2": profile("b@example.com", Some(&b))}),
            1,
        );

        swap_to(2).unwrap();

        assert_eq!(live_token(), "b-token");
        assert_eq!(folder_token(&a), "a-token");
    }

    #[test]
    fn swapping_to_the_live_account_only_selects_it() {
        let env = setup();
        let a = folder(&env, "a", "a@example.com", "a-stale");
        let b = folder(&env, "b", "b@example.com", "b-token");
        set_live("b@example.com", "b-rotated");
        registry(
            &env,
            json!({"1": profile("a@example.com", Some(&a)), "2": profile("b@example.com", Some(&b))}),
            1,
        );

        swap_to(2).unwrap();

        assert_eq!(live_token(), "b-rotated");
        assert_eq!(folder_token(&b), "b-token");
        let data = crate::switcher::read_sequence_data().unwrap();
        assert_eq!(crate::profile_registry::selected_number(&data), Some(2));
    }

    #[test]
    fn a_folder_signed_in_as_someone_else_is_refused() {
        let env = setup();
        let a = folder(&env, "a", "a@example.com", "a-token");
        let b = folder(&env, "b", "someone@example.com", "b-token");
        set_live("a@example.com", "a-token");
        registry(
            &env,
            json!({"1": profile("a@example.com", Some(&a)), "2": profile("b@example.com", Some(&b))}),
            1,
        );

        assert!(matches!(swap_to(2), Err(SwitchError::InvalidInput(_))));
        assert_eq!(live_token(), "a-token");
    }

    #[test]
    fn the_default_account_moves_into_its_own_folder() {
        let env = setup();
        let b = folder(&env, "b", "b@example.com", "b-token");
        let a = env.profiles.path().join("a").to_string_lossy().into_owned();
        fs::create_dir_all(&a).unwrap();
        fs::write(
            std::path::Path::new(&a).join(".claude.json"),
            json!({"theme": "dark"}).to_string(),
        )
        .unwrap();
        set_live("a@example.com", "a-live");
        registry(
            &env,
            json!({"1": profile("a@example.com", None), "2": profile("b@example.com", Some(&b))}),
            1,
        );

        assert_eq!(move_default_into(&a).unwrap(), Some(1));

        assert_eq!(folder_token(&a), "a-live");
        assert_eq!(live_token(), "a-live", "~/.claude is left alone");
        let config: Value = serde_json::from_str(
            &fs::read_to_string(std::path::Path::new(&a).join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(config["oauthAccount"]["emailAddress"], "a@example.com");
        assert_eq!(config["theme"], "dark");
        let data = crate::switcher::read_sequence_data().unwrap();
        assert_eq!(default_without_folder(&data), None);
        assert_eq!(data["accounts"]["1"]["configDir"], a.as_str());

        // Now both accounts can be swapped.
        swap_to(2).unwrap();
        assert_eq!(live_token(), "b-token");
        assert_eq!(folder_token(&a), "a-live");
    }

    #[test]
    fn the_default_account_does_not_move_while_someone_else_is_live() {
        let env = setup();
        let b = folder(&env, "b", "b@example.com", "b-token");
        let a = env.profiles.path().join("a").to_string_lossy().into_owned();
        fs::create_dir_all(&a).unwrap();
        set_live("b@example.com", "b-live");
        registry(
            &env,
            json!({"1": profile("a@example.com", None), "2": profile("b@example.com", Some(&b))}),
            1,
        );

        assert!(matches!(
            move_default_into(&a),
            Err(SwitchError::InvalidInput(_))
        ));
        let data = crate::switcher::read_sequence_data().unwrap();
        assert_eq!(default_without_folder(&data), Some(1));
    }

    #[test]
    fn wiped_tokens_are_detected() {
        assert!(tokens_wiped(
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":""}}"#
        ));
        assert!(!tokens_wiped(&login("x")));
    }
}
