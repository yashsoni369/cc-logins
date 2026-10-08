//! Profile accounts in the registry (`sequence.json`, schema 2 fields).
//!
//! A profile account's record carries, next to the v0.3 identity fields:
//! - `configDir`: the exact `CLAUDE_CONFIG_DIR` string, or `null` for the
//!   default `~/.claude`. The key's *presence* is what marks a profile
//!   account; a v0.3 record has no such key.
//! - `launcher`: the slug of its `claude-<slug>` command.
//! - `profileState`: see [`ProfileState`].
//!
//! The registry also records `selectedAccountNumber`: the account plain
//! `claude` starts new sessions with. Every change here is mirrored into
//! `shim.json`, which is all the shim reads.
//!
//! Records are edited as JSON maps, so fields this build does not know
//! survive a round trip (and a downgrade).

use serde_json::{Map, Value};

use crate::model::{AccountProfile, ProfileState};
use crate::shim_core::{ShimConfig, ShimProfile};
use crate::switcher::{self, SwitchError};

const CONFIG_DIR: &str = "configDir";
const LAUNCHER: &str = "launcher";
const PROFILE_STATE: &str = "profileState";
const SELECTED: &str = "selectedAccountNumber";

/// The profile half of a registry record, or `None` for a v0.3 record.
pub fn profile_of(record: &Map<String, Value>) -> Option<AccountProfile> {
    let config_dir = match record.get(CONFIG_DIR)? {
        Value::String(dir) if !dir.is_empty() => Some(dir.clone()),
        Value::Null => None,
        _ => return None,
    };
    Some(AccountProfile {
        is_default: config_dir.is_none(),
        config_dir,
        launcher: record
            .get(LAUNCHER)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        state: ProfileState::parse(record.get(PROFILE_STATE).and_then(Value::as_str)),
    })
}

/// The account selected for new sessions, if one is.
pub fn selected_number(data: &Map<String, Value>) -> Option<u32> {
    let raw = data.get(SELECTED)?;
    raw.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok()))
}

fn accounts(data: &Map<String, Value>) -> impl Iterator<Item = (u32, &Map<String, Value>)> {
    data.get("accounts")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|accounts| accounts.iter())
        .filter_map(|(num, value)| Some((num.parse().ok()?, value.as_object()?)))
}

/// `shim.json`'s account half, derived from the registry: the selected
/// account (when it is a ready profile) and one launcher per ready profile.
/// With the live swap on, plain `claude` is left on `~/.claude`, which then
/// holds the selected account's login.
pub fn shim_accounts(
    data: &Map<String, Value>,
) -> (Option<ShimProfile>, Vec<(String, ShimProfile)>) {
    let selected = selected_number(data).filter(|_| !crate::live_swap::enabled());
    let mut chosen = None;
    let mut launchers = Vec::new();
    for (number, record) in accounts(data) {
        let Some(profile) = profile_of(record) else {
            continue;
        };
        if profile.state != ProfileState::Ready {
            continue;
        }
        let shim = ShimProfile {
            account: number,
            config_dir: profile.config_dir.clone(),
        };
        if selected == Some(number) {
            chosen = Some(shim.clone());
        }
        if let Some(slug) = profile.launcher {
            launchers.push((slug, shim));
        }
    }
    launchers.sort_by(|a, b| a.0.cmp(&b.0));
    (chosen, launchers)
}

/// Launcher slugs with their account numbers, for the settings list and for
/// materialising `claude-<slug>` copies.
pub fn launchers(data: &Map<String, Value>) -> Vec<(String, u32)> {
    shim_accounts(data)
        .1
        .into_iter()
        .map(|(slug, profile)| (slug, profile.account))
        .collect()
}

/// Mirror the registry into `shim.json`, keeping its other fields.
pub fn sync_shim(data: &Map<String, Value>) -> std::io::Result<ShimConfig> {
    let (selected, launchers) = shim_accounts(data);
    crate::profiles::update_shim_config(|config| {
        config.selected = selected;
        config.launchers = launchers.into_iter().collect();
    })
}

/// A signed-in profile to add to the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewProfile {
    pub email: String,
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
    /// `None` registers the default `~/.claude` login.
    pub config_dir: Option<String>,
    pub alias: Option<String>,
}

/// Why [`apply_registration`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// This account is already a profile account in another folder.
    AlreadyRegistered(u32),
    /// Another account already uses this folder.
    FolderInUse(u32),
}

fn field<'a>(record: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    record
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Whether `record` is the same Claude account as `new`: account UUID when
/// both have one, else organization plus email, else email alone when
/// neither side belongs to an organization.
pub fn same_account(record: &Map<String, Value>, new: &NewProfile) -> bool {
    if let (Some(a), Some(b)) = (field(record, "uuid"), new.account_uuid.as_deref()) {
        return a == b;
    }
    let email_matches =
        field(record, "email").is_some_and(|e| e.eq_ignore_ascii_case(new.email.trim()));
    let record_org = field(record, "organizationUuid");
    let new_org = new.organization_uuid.as_deref().filter(|s| !s.is_empty());
    email_matches && record_org == new_org
}

fn same_folder(record: &Map<String, Value>, config_dir: Option<&str>) -> bool {
    match (record.get(CONFIG_DIR), config_dir) {
        (Some(Value::Null), None) => true,
        (Some(Value::String(a)), Some(b)) => {
            crate::shim_core::same_dir(std::path::Path::new(a), std::path::Path::new(b))
        }
        _ => false,
    }
}

/// Add `new` to the registry, or attach it to the v0.3 record for the same
/// account (which is how an account moves to token-free mode). Returns the
/// slot. Pure: the caller holds the vault lock and writes the result.
pub fn apply_registration(
    data: &mut Map<String, Value>,
    new: &NewProfile,
) -> Result<u32, RegisterError> {
    switcher::ensure_accounts_object(data);

    let mut existing = None;
    let mut taken_slugs = Vec::new();
    for (number, record) in accounts(data) {
        let is_same = same_account(record, new);
        if !is_same
            && record.contains_key(CONFIG_DIR)
            && same_folder(record, new.config_dir.as_deref())
        {
            return Err(RegisterError::FolderInUse(number));
        }
        if is_same {
            existing = Some(number);
        } else if let Some(slug) = field(record, LAUNCHER) {
            taken_slugs.push(slug.to_string());
        }
    }

    let mut upgrading_v03 = false;
    let slot = match existing {
        Some(number) => {
            let record = data["accounts"][number.to_string()]
                .as_object()
                .expect("slot found above");
            if record.contains_key(CONFIG_DIR) && !same_folder(record, new.config_dir.as_deref()) {
                return Err(RegisterError::AlreadyRegistered(number));
            }
            upgrading_v03 = !record.contains_key(CONFIG_DIR);
            number
        }
        None => {
            let number = switcher::next_free_slot(data);
            switcher::add_to_sequence(data, number);
            number
        }
    };

    let accounts = data
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("ensured above");
    let record = accounts
        .entry(slot.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let record = record.as_object_mut().expect("records are objects");

    let text = |value: &str| Value::String(value.to_string());
    record.insert("email".into(), text(new.email.trim()));
    if let Some(uuid) = &new.account_uuid {
        record.insert("uuid".into(), text(uuid));
    }
    match &new.organization_uuid {
        Some(org) => record.insert("organizationUuid".into(), text(org)),
        None => record.insert("organizationUuid".into(), text("")),
    };
    if let Some(name) = &new.organization_name {
        record.insert("organizationName".into(), text(name));
    }
    if let Some(alias) = new
        .alias
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        record.insert("alias".into(), text(alias));
    }
    record.insert(
        CONFIG_DIR.into(),
        new.config_dir.as_deref().map(text).unwrap_or(Value::Null),
    );
    if field(record, LAUNCHER).is_none() {
        let base = field(record, "alias")
            .unwrap_or(new.email.as_str())
            .to_string();
        let slug = crate::profiles::launcher_slug(&base, &taken_slugs);
        record.insert(LAUNCHER.into(), Value::String(slug));
    }
    record.insert(PROFILE_STATE.into(), text(ProfileState::Ready.as_str()));
    // A v0.3 record's stored login is now redundant. Flag it in this same
    // write, so a crash before the deletion still leaves a record of it.
    if upgrading_v03 {
        record.insert(crate::migration::LEGACY_VAULT.into(), Value::Bool(true));
    }
    record.insert(
        "lastVerifiedAt".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    // The first account ever registered is also the one new sessions use.
    if selected_number(data).is_none() {
        data.insert(SELECTED.into(), Value::from(slot));
    }
    Ok(slot)
}

/// Point new sessions at `number`. Pure; see [`apply_registration`].
pub fn apply_selection(data: &mut Map<String, Value>, number: u32) -> Result<(), SwitchError> {
    let record = data
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|a| a.get(&number.to_string()))
        .and_then(Value::as_object)
        .ok_or_else(|| SwitchError::UnknownAccount(number.to_string()))?;
    match profile_of(record) {
        Some(profile) if profile.state == ProfileState::Ready => {}
        Some(_) => {
            return Err(SwitchError::InvalidInput(
                "this account needs you to sign in again before new sessions can use it".into(),
            ))
        }
        None => {
            return Err(SwitchError::InvalidInput(
                "sign in to this account once to move it to its own folder first".into(),
            ))
        }
    }
    data.insert(SELECTED.into(), Value::from(number));
    Ok(())
}

/// Record a profile's state (after an identity check, say). Pure.
pub fn apply_state(data: &mut Map<String, Value>, number: u32, state: ProfileState) -> bool {
    let Some(record) = data
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .and_then(|a| a.get_mut(&number.to_string()))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    if !record.contains_key(CONFIG_DIR) {
        return false;
    }
    let changed = profile_of(record).map(|p| p.state) != Some(state);
    record.insert(PROFILE_STATE.into(), Value::String(state.as_str().into()));
    changed
}

/// Load, change and save the registry under the vault lock, then mirror it
/// into `shim.json`.
pub(crate) fn edit<T>(
    change: impl FnOnce(&mut Map<String, Value>) -> Result<T, SwitchError>,
) -> Result<T, SwitchError> {
    let _lock = crate::locking::acquire_or_err(
        switcher::vault_lock_path(),
        crate::locking::DEFAULT_TIMEOUT,
    )?;
    switcher::refuse_pending_recovery()?;
    let mut data = switcher::read_sequence_data().unwrap_or_default();
    let out = change(&mut data)?;
    data.insert(
        "lastUpdated".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    switcher::write_sequence_data(&data)?;
    if let Err(error) = sync_shim(&data) {
        log::warn!("shim.json not updated: {error}");
    }
    Ok(out)
}

/// Register a signed-in profile. See [`apply_registration`].
pub fn register(new: &NewProfile) -> Result<u32, SwitchError> {
    edit(|data| {
        apply_registration(data, new).map_err(|error| match error {
            RegisterError::AlreadyRegistered(n) => SwitchError::AlreadyRegistered(n.to_string()),
            RegisterError::FolderInUse(n) => {
                SwitchError::InvalidInput(format!("that folder already belongs to account {n}"))
            }
        })
    })
}

/// Select `number` for new sessions.
pub fn select(number: u32) -> Result<(), SwitchError> {
    edit(|data| apply_selection(data, number))
}

/// Set a profile's state; `Ok(true)` when it changed.
pub fn set_state(number: u32, state: ProfileState) -> Result<bool, SwitchError> {
    edit(|data| Ok(apply_state(data, number, state)))
}

/// Re-mirror the current registry into `shim.json` (startup, settings
/// change). Reads without the lock: `shim.json` is derived and idempotent.
pub fn resync_shim() -> std::io::Result<ShimConfig> {
    sync_shim(&switcher::read_sequence_data().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn new_profile(email: &str, dir: Option<&str>) -> NewProfile {
        NewProfile {
            email: email.to_string(),
            config_dir: dir.map(str::to_string),
            ..NewProfile::default()
        }
    }

    #[test]
    fn profile_of_distinguishes_v03_default_and_folder_records() {
        assert_eq!(profile_of(&registry(json!({"email": "a@x"}))), None);

        let default = profile_of(&registry(json!({"configDir": null}))).unwrap();
        assert!(default.is_default);
        assert_eq!(default.config_dir, None);
        assert_eq!(default.state, ProfileState::Ready);

        let folder = profile_of(&registry(json!({
            "configDir": "/p/2", "launcher": "work", "profileState": "loginRequired"
        })))
        .unwrap();
        assert_eq!(folder.config_dir.as_deref(), Some("/p/2"));
        assert_eq!(folder.launcher.as_deref(), Some("work"));
        assert_eq!(folder.state, ProfileState::LoginRequired);
    }

    #[test]
    fn registering_into_an_empty_registry_selects_it() {
        let mut data = Map::new();
        let slot = apply_registration(&mut data, &new_profile("sam@x.com", Some("/p/1"))).unwrap();
        assert_eq!(slot, 1);
        assert_eq!(selected_number(&data), Some(1));
        let record = data["accounts"]["1"].as_object().unwrap();
        assert_eq!(record["configDir"], "/p/1");
        assert!(record.get("legacyVault").is_none());
        assert_eq!(record["launcher"], "sam");
        assert_eq!(record["profileState"], "ready");
        assert_eq!(data["sequence"], json!([1]));
    }

    #[test]
    fn a_second_account_gets_its_own_slot_and_a_unique_launcher() {
        let mut data = Map::new();
        apply_registration(&mut data, &new_profile("sam@x.com", Some("/p/1"))).unwrap();
        let mut other = new_profile("sam@y.com", Some("/p/2"));
        other.organization_uuid = Some("org-y".into());
        let slot = apply_registration(&mut data, &other).unwrap();
        assert_eq!(slot, 2);
        assert_eq!(data["accounts"]["2"]["launcher"], "sam-2");
        // Selection stays with the first account.
        assert_eq!(selected_number(&data), Some(1));
    }

    #[test]
    fn a_v03_record_for_the_same_account_is_upgraded_in_place() {
        let mut data = registry(json!({
            "accounts": {"3": {"email": "sam@x.com", "organizationUuid": "", "alias": "Main"}},
            "sequence": [3]
        }));
        let slot = apply_registration(&mut data, &new_profile("Sam@X.com", None)).unwrap();
        assert_eq!(slot, 3);
        let record = data["accounts"]["3"].as_object().unwrap();
        assert_eq!(record["configDir"], Value::Null);
        assert_eq!(record["alias"], "Main");
        assert_eq!(record["launcher"], "main");
        assert_eq!(record["legacyVault"], true);
        assert_eq!(data["sequence"], json!([3]));
    }

    #[test]
    fn the_same_account_in_a_different_folder_is_refused() {
        let mut data = Map::new();
        apply_registration(&mut data, &new_profile("sam@x.com", Some("/p/1"))).unwrap();
        assert_eq!(
            apply_registration(&mut data, &new_profile("sam@x.com", Some("/p/9"))),
            Err(RegisterError::AlreadyRegistered(1))
        );
        // Signing in again into its own folder is fine.
        assert_eq!(
            apply_registration(&mut data, &new_profile("sam@x.com", Some("/p/1/"))),
            Ok(1)
        );
    }

    #[test]
    fn another_account_cannot_take_a_used_folder() {
        let mut data = Map::new();
        apply_registration(&mut data, &new_profile("sam@x.com", Some("/p/1"))).unwrap();
        assert_eq!(
            apply_registration(&mut data, &new_profile("kim@x.com", Some("/p/1"))),
            Err(RegisterError::FolderInUse(1))
        );
    }

    #[test]
    fn account_uuid_beats_email_matching() {
        let record = registry(json!({"uuid": "u-1", "email": "old@x.com"}));
        let mut renamed = new_profile("new@x.com", None);
        renamed.account_uuid = Some("u-1".into());
        assert!(same_account(&record, &renamed));
        renamed.account_uuid = Some("u-2".into());
        assert!(!same_account(&record, &renamed));
    }

    #[test]
    fn selection_requires_a_ready_profile() {
        let mut data = registry(json!({
            "accounts": {
                "1": {"email": "a@x", "configDir": "/p/1", "profileState": "ready"},
                "2": {"email": "b@x", "configDir": "/p/2", "profileState": "loginRequired"},
                "3": {"email": "c@x"}
            }
        }));
        apply_selection(&mut data, 1).unwrap();
        assert_eq!(selected_number(&data), Some(1));
        assert!(apply_selection(&mut data, 2).is_err());
        assert!(apply_selection(&mut data, 3).is_err());
        assert!(apply_selection(&mut data, 9).is_err());
        assert_eq!(selected_number(&data), Some(1));
    }

    #[test]
    fn shim_accounts_lists_only_ready_profiles() {
        let data = registry(json!({
            "selectedAccountNumber": 2,
            "accounts": {
                "1": {"configDir": null, "launcher": "main"},
                "2": {"configDir": "/p/2", "launcher": "work"},
                "3": {"configDir": "/p/3", "launcher": "old", "profileState": "identityMismatch"},
                "4": {"email": "legacy@x"}
            }
        }));
        let (selected, launchers) = shim_accounts(&data);
        assert_eq!(
            selected,
            Some(ShimProfile {
                account: 2,
                config_dir: Some("/p/2".into())
            })
        );
        let slugs: Vec<_> = launchers.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(slugs, vec!["main", "work"]);
    }

    #[test]
    fn apply_state_only_touches_profile_records() {
        let mut data = registry(json!({
            "accounts": {"1": {"configDir": "/p/1"}, "2": {"email": "b@x"}}
        }));
        assert!(apply_state(&mut data, 1, ProfileState::IdentityMismatch));
        assert!(!apply_state(&mut data, 1, ProfileState::IdentityMismatch));
        assert!(!apply_state(&mut data, 2, ProfileState::Ready));
        assert_eq!(data["accounts"]["1"]["profileState"], "identityMismatch");
    }

    #[test]
    fn selected_number_accepts_numbers_and_numeric_strings() {
        assert_eq!(
            selected_number(&registry(json!({"selectedAccountNumber": 4}))),
            Some(4)
        );
        assert_eq!(
            selected_number(&registry(json!({"selectedAccountNumber": "5"}))),
            Some(5)
        );
        assert_eq!(selected_number(&Map::new()), None);
    }
}
