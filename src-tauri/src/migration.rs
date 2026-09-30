//! Moving v0.3 accounts to token-free profiles.
//!
//! A v0.3 account lives in the vault as a stored copy of its login. v0.4
//! never uses that copy. Instead each account signs in once through Claude
//! Code into its own folder, and only once that sign-in is verified as the
//! same account is the stored copy deleted:
//!
//! 1. The account that is signed in to the default `~/.claude` already has a
//!    folder: it becomes the default profile as soon as `claude auth status`
//!    confirms it. No sign-in needed.
//! 2. Every other account shows "Sign in to move". Signing in creates its
//!    folder; the identity must match the slot before anything is deleted.
//!
//! A deletion that fails leaves `legacyVault: true` on the record and is
//! retried at the next start. Nothing is ever deleted for an account whose
//! new login has not been verified.

use serde_json::{Map, Value};

use crate::auth_status::FolderIdentity;
use crate::profile_registry::{self, NewProfile};

/// Registry flag: this profile's v0.3 vault copy still needs deleting.
pub const LEGACY_VAULT: &str = "legacyVault";

fn records(data: &Map<String, Value>) -> impl Iterator<Item = (u32, &Map<String, Value>)> {
    data.get("accounts")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|accounts| accounts.iter())
        .filter_map(|(num, value)| Some((num.parse().ok()?, value.as_object()?)))
}

/// The profile a folder identity describes, ready to register.
pub fn new_profile(identity: &FolderIdentity, config_dir: Option<String>) -> NewProfile {
    NewProfile {
        email: identity.email.clone(),
        account_uuid: identity.account_uuid.clone(),
        organization_uuid: identity.organization_uuid.clone(),
        organization_name: identity.organization_name.clone(),
        config_dir,
        alias: None,
    }
}

/// The v0.3 record for the account signed in to the default folder, when
/// there is one and no account has claimed the default folder yet.
pub fn default_adoption_candidate(
    data: &Map<String, Value>,
    identity: &FolderIdentity,
) -> Option<u32> {
    let default_claimed =
        records(data).any(|(_, r)| matches!(r.get("configDir"), Some(Value::Null)));
    if default_claimed {
        return None;
    }
    let wanted = new_profile(identity, None);
    records(data)
        .find(|(_, record)| {
            !record.contains_key("configDir") && profile_registry::same_account(record, &wanted)
        })
        .map(|(number, _)| number)
}

/// Accounts still held the v0.3 way.
pub fn pending(data: &Map<String, Value>) -> Vec<u32> {
    records(data)
        .filter(|(_, record)| !record.contains_key("configDir"))
        .map(|(number, _)| number)
        .collect()
}

/// Moved accounts whose vault copy still needs deleting, with their email
/// (the vault keys copies by slot and email).
pub fn vault_cleanup_due(data: &Map<String, Value>) -> Vec<(u32, String)> {
    records(data)
        .filter(|(_, record)| {
            record.contains_key("configDir") && record.get(LEGACY_VAULT) == Some(&Value::Bool(true))
        })
        .map(|(number, record)| {
            let email = record
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (number, email.to_string())
        })
        .collect()
}

/// Mark or clear [`LEGACY_VAULT`] on a record. Pure.
pub fn set_legacy_vault(data: &mut Map<String, Value>, number: u32, pending: bool) {
    let Some(record) = data
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .and_then(|a| a.get_mut(&number.to_string()))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if pending {
        record.insert(LEGACY_VAULT.into(), Value::Bool(true));
    } else {
        record.remove(LEGACY_VAULT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn identity(email: &str) -> FolderIdentity {
        FolderIdentity {
            email: email.to_string(),
            ..FolderIdentity::default()
        }
    }

    #[test]
    fn the_default_login_adopts_its_v03_record() {
        let data = registry(json!({"accounts": {
            "1": {"email": "a@x.com", "organizationUuid": ""},
            "2": {"email": "b@x.com", "organizationUuid": ""}
        }}));
        assert_eq!(
            default_adoption_candidate(&data, &identity("B@x.com")),
            Some(2)
        );
        assert_eq!(
            default_adoption_candidate(&data, &identity("c@x.com")),
            None
        );
    }

    #[test]
    fn no_adoption_once_the_default_folder_is_claimed() {
        let data = registry(json!({"accounts": {
            "1": {"email": "a@x.com", "configDir": null},
            "2": {"email": "b@x.com"}
        }}));
        assert_eq!(
            default_adoption_candidate(&data, &identity("b@x.com")),
            None
        );
    }

    #[test]
    fn pending_lists_v03_records_only() {
        let data = registry(json!({"accounts": {
            "1": {"email": "a@x.com", "configDir": null},
            "2": {"email": "b@x.com"},
            "3": {"email": "c@x.com", "configDir": "/p/3"}
        }}));
        assert_eq!(pending(&data), vec![2]);
    }

    #[test]
    fn vault_cleanup_is_only_for_moved_accounts_flagged_pending() {
        let mut data = registry(json!({"accounts": {
            "1": {"email": "a@x.com", "configDir": null, "legacyVault": true},
            "2": {"email": "b@x.com", "legacyVault": true},
            "3": {"email": "c@x.com", "configDir": "/p/3"}
        }}));
        assert_eq!(vault_cleanup_due(&data), vec![(1, "a@x.com".to_string())]);
        set_legacy_vault(&mut data, 1, false);
        assert!(vault_cleanup_due(&data).is_empty());
        set_legacy_vault(&mut data, 3, true);
        assert_eq!(vault_cleanup_due(&data), vec![(3, "c@x.com".to_string())]);
    }
}
