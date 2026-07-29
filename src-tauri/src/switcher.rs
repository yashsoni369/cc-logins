//! The account-switch core: backup-then-install account activation, and the
//! account-listing/read path used to build a [`Snapshot`].
//!
//! Ported from claude-swap (MIT) — <https://github.com/realiti4/claude-swap>,
//! `claude_swap/switcher.py` (`ClaudeAccountSwitcher`). This is a narrow slice
//! of a much larger module. The GUI ports the switch invariants it depends on:
//! coordinated Claude Code locks, generation validation, outgoing credential
//! provenance, durable journaling/rollback/recovery, and usage attribution.
//! CLI-only surfaces such as aliases, sessions, and interactive import/export
//! remain outside this module.
//!
//! # Account management additions
//!
//! [`add_current_account`], [`add_token`], and [`set_account_enabled`] port
//! the relevant slices of `add_account` / `add_account_from_token` /
//! `set_account_disabled` from the same upstream module — registering the
//! live login or a raw token as a new slot, and toggling the `disabled` flag.
//! Unlike upstream's `_get_next_account_number` (`max(existing) + 1`), slot
//! allocation here reuses the lowest free slot number, so a freed slot (e.g.
//! after upstream removal via the CLI) is recycled rather than left as a
//! permanent gap.
//!
//! [`add_oauth_credential`] is not a port of anything upstream either — it is
//! this app's own bridge from [`crate::login::interactive_login`]'s captured
//! credential blob to a registered slot, since [`add_token`] explicitly
//! rejects anything that looks like a JSON blob rather than a raw token. It
//! follows [`add_current_account`]'s structure closely (same identity-based
//! duplicate detection, same injectable-resolver pattern, same lock
//! discipline) but never touches the live credential/config and never sets
//! `activeAccountNumber` — see its own doc comment for why.
//!
//! # Reused, not reimplemented
//!
//! - [`crate::model`] — [`Account`], [`Usage`], [`UsageWindow`], [`Environment`],
//!   [`Snapshot`] are used as-is; this module defines no shapes of its own.
//! - [`crate::credentials`] — [`CredentialStore`] does every read/write of the
//!   active credential and the per-account backup stores (Keychain-vs-file
//!   routing, atomic writes, `.prev` retention). [`shared_credential_fields`] /
//!   [`merge_shared_credential_fields`] compose the target's stored login with
//!   the machine's live shared OAuth fields before activation, mirroring
//!   Python's `_prepare_credentials_for_activation`.
//! - [`crate::locking`] — [`crate::locking::acquire_or_err`] guards every
//!   mutation. Two *different* locks are in play here, not one shared between
//!   this app and the external store — see the "Locking" section further down
//!   this file (in `crate::switch_transaction`) for the full split.
//! - [`crate::paths`] — every on-disk location comes from here, never
//!   hand-rolled: [`crate::paths::backup_root`] for OUR vault, and
//!   `global_config_path`/`credentials_path`/`claude_config_home`
//!   for Claude Code's official files.
//! - [`crate::oauth`] — usage fetch, token refresh, and (new) profile lookup.
//!   Inactive refreshes use the generation coordinator; active refreshes use
//!   the narrower Claude-compatible lock path below so Claude Code and this
//!   app cannot consume the same grant concurrently. [`add_current_account`]
//!   and [`add_token`] call `oauth::fetch_oauth_profile` — advisory, `None`
//!   on any failure — to resolve account identity for duplicate detection;
//!   see the "Duplicate detection by account identity" section above
//!   [`find_registered_slot_by_identity`] for why byte-level fingerprinting
//!   alone (the pre-fix behavior) is not enough.
//!
//! # Correctness rules carried over from upstream
//!
//! 1. **Lock the whole mutate.** Every mutating function in this module
//!    acquires a [`crate::locking::FileLock`] before touching any file and
//!    holds it for the entire operation. [`switch_to`] holds the complete
//!    live-state lock set — see the "Locking" section below.
//! 2. **Keep network work outside mutation locks, except active refresh.**
//!    Profile/usage calls and switch target freshening run without the full
//!    live-state lock set. Upstream's bounded exception is reproduced:
//!    an active refresh grant holds Claude's credential locks and the GUI
//!    vault lock so the refresh generation cannot be consumed twice; it never
//!    holds the config lock.
//!    [`add_current_account`],
//!    [`add_token`], and [`add_oauth_credential`] are the exception to "no
//!    mutating function makes a network call": each resolves account
//!    identity via `oauth::fetch_oauth_profile` for duplicate detection, but
//!    does so strictly BEFORE acquiring the vault lock, and treats a failed
//!    lookup as advisory (degrade, don't block) — see
//!    [`find_registered_slot_by_identity`].
//! 3. **Back up the outgoing credential before installing the new one.** See
//!    [`switch_to`]'s doc comment and the `backup_happens_before_target_validation…`
//!    test below.
//! 4. **Atomic writes.** All local writes in this module go through
//!    [`atomic_write`] (write-temp-then-rename), matching `credentials.rs`.
//! 5. **Serialize every refresh of one account.** Active and inactive paths
//!    share the per-account refresh lease. Active refresh then takes the
//!    Claude credential and GUI vault locks in that order and re-reads
//!    identity and credentials before consuming a grant.
//! 6. **`.claude.json` lives at the home dir, not inside `.claude/`.** Always
//!    resolved via [`crate::paths::global_config_path`], never hand-rolled.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::credentials::{
    self, CredentialError, CredentialStore, Platform as CredPlatform, StoreHost,
};
use crate::model::{
    Account, EnvKind, EnvStatus, Environment, Snapshot, Usage, UsageStatus, UsageWindow,
};
use crate::oauth;
use crate::oauth_quarantine::OAuthQuarantine;
use crate::oauth_refresh::{
    self, AccountIdentity, CompareAndStore, GenerationStore, RefreshCoordinator,
    RefreshLeaseProvider, StoredGeneration, ValidatedCredential,
};
use crate::paths;

// ---------------------------------------------------------------------------
// On-disk layout — mirrors `claude_swap.switcher.ClaudeAccountSwitcher.__init__`.
// ---------------------------------------------------------------------------

/// `<backup_root>/sequence.json` — the account registry (slot numbers, email/
/// org identity, aliases, disabled flags, `activeAccountNumber`).
fn accounts_file() -> PathBuf {
    paths::backup_root().join("sequence.json")
}

/// `<backup_root>/credentials` — per-account credential backups, owned by
/// [`CredentialStore`] via [`GuiStoreHost::credentials_dir`].
fn credentials_dir() -> PathBuf {
    paths::backup_root().join("credentials")
}

/// `<our backup_root>/.lock` — the lock guarding OUR vault. Only ever taken by
/// this app; no other process has a reason to touch this specific file. See
/// `crate::switch_transaction` for how this relates to the live-state locks
/// this module sometimes also takes.
fn vault_lock_path() -> PathBuf {
    paths::backup_root().join(".lock")
}

fn account_config_path(account_num: &str, email: &str) -> PathBuf {
    account_config_path_at(&paths::backup_root(), account_num, email)
}

/// Same layout as [`account_config_path`], parameterized on the store root so
/// a caller can address a backup under a root other than the live vault.
fn account_config_path_at(root: &Path, account_num: &str, email: &str) -> PathBuf {
    root.join("configs")
        .join(format!(".claude-config-{account_num}-{email}.json"))
}

/// [`StoreHost`] for this crate's [`CredentialStore`]: platform is detected
/// live (never cached across calls, matching the trait's contract), and
/// `credentials_dir` is OUR OWN `<backup_root>/credentials` — this app's
/// vault, and the only credential store this app ever reads or writes.
pub(crate) struct GuiStoreHost;

impl StoreHost for GuiStoreHost {
    /// Under `cfg(test)` this is pinned to `Linux` — the file-only backend —
    /// no matter what OS is running the suite.
    ///
    /// `TempDir` isolates `credentials_dir()`, but the macOS Keychain branch
    /// ignores that directory entirely and writes to machine-global items
    /// keyed by service name. On a developer Mac the suite would overwrite
    /// `Claude Code-credentials`/$USER — the live login — and `guard_real_store`
    /// cannot stop it, because that guard checks paths and the Keychain is not
    /// a path. `credentials.rs`'s own `TestHost` already pins the platform for
    /// this reason; this host had been left detecting.
    fn platform(&self) -> CredPlatform {
        #[cfg(test)]
        {
            CredPlatform::Linux
        }
        #[cfg(not(test))]
        {
            CredPlatform::detect()
        }
    }
    fn credentials_dir(&self) -> PathBuf {
        credentials_dir()
    }
    /// Our own Keychain namespace — see [`crate::credentials::GUI_SECURITY_SERVICE`].
    fn keychain_service(&self) -> &str {
        crate::credentials::GUI_SECURITY_SERVICE
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// Errors from the switch path.
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    #[error(transparent)]
    Locking(#[from] crate::locking::LockingError),

    #[error(transparent)]
    LiveStateLock(#[from] crate::switch_transaction::LiveStateLockError),

    #[error(transparent)]
    Transaction(#[from] crate::switch_transaction::TransactionError),

    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Refresh(#[from] oauth_refresh::RefreshCoordinatorError),

    #[error("account {0}'s credential changed while activation was being validated")]
    TargetGenerationChanged(String),

    #[error("no accounts are managed yet")]
    NoAccountsManaged,

    #[error("account {0} does not exist")]
    UnknownAccount(String),

    #[error("could not read the active credential")]
    CredentialRead,

    #[error(
        "active credential for account {0} is empty (Keychain unreadable?); refusing to overwrite its backup"
    )]
    EmptyActiveCredential(String),

    #[error("account {0} has no stored credentials; re-add it")]
    NoStoredCredentials(String),

    #[error("account {0} has no stored config backup; re-add it")]
    NoStoredConfig(String),

    #[error("stored config backup for account {0} has no oauthAccount block")]
    InvalidBackupConfig(String),

    #[error("could not preserve the outgoing live credential: {0}")]
    Stash(String),

    #[error(
        "no active Claude Code login was found to add — log in with Claude Code first, then try again"
    )]
    NoLiveCredential,

    #[error(
        "this login is already registered as account {0}; refusing to create a duplicate slot"
    )]
    AlreadyRegistered(String),

    #[error("invalid token: {0}")]
    InvalidToken(String),

    #[error("invalid credential: {0}")]
    InvalidCredential(String),

    #[error(
        "account {0} is the active account; switch to a different account before disabling it"
    )]
    CannotDisableActive(String),
}

// ---------------------------------------------------------------------------
// Atomic writes — mirrors `credentials.rs::atomic_write` (private there, so
// this is a small local copy rather than a cross-module reach-in).
// ---------------------------------------------------------------------------

fn atomic_write(target: &Path, contents: &[u8]) -> std::io::Result<()> {
    crate::durable_fs::stage_sibling(target, contents, Some(0o600))?
        .commit()
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// sequence.json access.
// ---------------------------------------------------------------------------

fn read_sequence_data() -> Option<Map<String, Value>> {
    read_sequence_data_at(&paths::backup_root())
}

/// Same as [`read_sequence_data`], parameterized on the store root so a
/// caller can read a registry under a root other than the live vault.
fn read_sequence_data_at(root: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(root.join("sequence.json")).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn write_sequence_data(data: &Map<String, Value>) -> Result<(), SwitchError> {
    let body = serde_json::to_string_pretty(&Value::Object(data.clone()))?;
    atomic_write(&accounts_file(), body.as_bytes())?;
    Ok(())
}

fn write_account_config(account_num: &str, email: &str, config_text: &str) -> std::io::Result<()> {
    atomic_write(
        &account_config_path(account_num, email),
        config_text.as_bytes(),
    )
}

fn read_account_config(account_num: &str, email: &str) -> Option<String> {
    read_account_config_at(&paths::backup_root(), account_num, email)
}

/// Same as [`read_account_config`], parameterized on the store root (see
/// [`account_config_path_at`]).
fn read_account_config_at(root: &Path, account_num: &str, email: &str) -> Option<String> {
    std::fs::read_to_string(account_config_path_at(root, account_num, email))
        .ok()
        .filter(|s| !s.is_empty())
}

/// Slot number of the live login, or `None` when there is none or it is
/// unmanaged. Mirrors `_get_current_account` + `_find_account_slot`:
/// identity is read from `~/.claude.json`'s `oauthAccount` block (never from
/// the stored `activeAccountNumber`, which is just cswap's own memory of
/// where it left things and can drift from what's actually live).
fn current_account_number(data: &Map<String, Value>) -> Option<String> {
    let text = std::fs::read_to_string(paths::global_config_path()).ok()?;
    let config: Value = serde_json::from_str(&text).ok()?;
    let oauth_account = config.get("oauthAccount")?.as_object()?;
    let email = oauth_account
        .get("emailAddress")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let organization_uuid = oauth_account
        .get("organizationUuid")
        .and_then(Value::as_str)
        .unwrap_or("");

    let accounts = data.get("accounts").and_then(Value::as_object)?;
    for (num, record) in accounts {
        let record_email = record.get("email").and_then(Value::as_str).unwrap_or("");
        let record_org = record
            .get("organizationUuid")
            .and_then(Value::as_str)
            .unwrap_or("");
        if record_email == email && record_org == organization_uuid {
            return Some(num.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Account enumeration (no network) — mirrors the account-row half of
// `_build_list_payload` / `account_row`, without the usage fetch.
// ---------------------------------------------------------------------------

/// Enumerate every managed account, marking the one whose credential is
/// currently live. Pure local I/O — never touches the network.
///
/// A missing or unreadable `sequence.json` is treated as "no accounts
/// managed yet" (an empty list), matching Python's `_read_json` tolerance
/// for a corrupt/absent registry rather than raising.
pub fn read_accounts() -> Result<Vec<Account>, SwitchError> {
    let data = read_sequence_data().unwrap_or_default();
    Ok(accounts_from_sequence(&data))
}

fn accounts_from_sequence(data: &Map<String, Value>) -> Vec<Account> {
    let accounts_map = match data.get("accounts").and_then(Value::as_object) {
        Some(m) => m,
        None => return Vec::new(),
    };

    // Prefer the recorded rotation order; fall back to numeric slot order for
    // a registry with accounts but no (or a malformed) `sequence` array.
    let order: Vec<String> = match data.get("sequence").and_then(Value::as_array) {
        Some(seq) => seq
            .iter()
            .filter_map(|v| {
                v.as_u64()
                    .map(|n| n.to_string())
                    .or_else(|| v.as_str().map(str::to_string))
            })
            .collect(),
        None => {
            let mut nums: Vec<String> = accounts_map.keys().cloned().collect();
            nums.sort_by_key(|s| s.parse::<u64>().unwrap_or(u64::MAX));
            nums
        }
    };

    let active_num = current_account_number(data);

    let mut out = Vec::with_capacity(order.len());
    for num_str in order {
        let Some(record) = accounts_map.get(&num_str).and_then(Value::as_object) else {
            continue;
        };
        let Ok(number) = num_str.parse::<u32>() else {
            continue;
        };
        let email = record
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let uuid = record
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let org_uuid_raw = record
            .get("organizationUuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let organization_uuid = if org_uuid_raw.is_empty() {
            None
        } else {
            Some(org_uuid_raw.clone())
        };
        let organization_name = record
            .get("organizationName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let alias = record
            .get("alias")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // Disabled ("held out of rotation") is a separate boolean field in
        // cswap's JSON; this model folds it into `UsageStatus::Disabled`,
        // which is exactly what that variant's doc comment describes and is
        // what `Account::is_switchable` already keys off.
        let disabled = record
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let active = active_num.as_deref() == Some(num_str.as_str());

        out.push(Account {
            number,
            email,
            uuid,
            alias,
            organization_name,
            organization_uuid,
            is_organization: Some(!org_uuid_raw.is_empty()),
            active,
            usage_status: if disabled {
                UsageStatus::Disabled
            } else {
                UsageStatus::Unknown
            },
            usage: None,
            usage_fetched_at: None,
            usage_age_seconds: None,
        });
    }
    out
}

struct GuiGenerationStore {
    timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProvenanceVerdict {
    Owned,
    Foreign,
    Unresolved,
}

fn active_provenance_cache() -> &'static Mutex<HashMap<String, ProvenanceVerdict>> {
    static VERDICTS: OnceLock<Mutex<HashMap<String, ProvenanceVerdict>>> = OnceLock::new();
    VERDICTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_provenance_cache_key(account: &Account, credentials: &str) -> Option<String> {
    let fingerprint = oauth::credential_fingerprint(credentials)?;
    Some(format!(
        "{}|{}|{}|{}|{}",
        account.number,
        account.email.trim().to_ascii_lowercase(),
        account.organization_uuid.as_deref().unwrap_or_default(),
        account.uuid.as_deref().unwrap_or_default(),
        fingerprint
    ))
}

fn cached_active_usage_provenance(
    account: &Account,
    credentials: &str,
) -> Option<ProvenanceVerdict> {
    let key = active_provenance_cache_key(account, credentials)?;
    active_provenance_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .copied()
}

fn cache_active_usage_provenance(account: &Account, credentials: &str, verdict: ProvenanceVerdict) {
    if verdict == ProvenanceVerdict::Unresolved {
        return;
    }
    let Some(key) = active_provenance_cache_key(account, credentials) else {
        return;
    };
    active_provenance_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, verdict);
}

/// Mirrors cswap's uuid-first, tri-state profile-oracle decision. A partial
/// profile can prove ownership through UUID, but a UUID-less slot needs the
/// complete `(email, organization)` pair before either affirmation or
/// condemnation is safe.
fn active_usage_provenance(account: &Account, resolved: &oauth::TokenAccount) -> ProvenanceVerdict {
    let own_uuid = account.uuid.as_deref().unwrap_or_default().trim();
    let own_org = account.organization_uuid.as_deref().unwrap_or_default();
    let resolved_org = resolved.organization_uuid.as_deref();

    if !own_uuid.is_empty() {
        let compatible_org = match resolved_org {
            None => true,
            Some(org) => org.is_empty() || own_org.is_empty() || org == own_org,
        };
        return if resolved.uuid == own_uuid && compatible_org {
            ProvenanceVerdict::Owned
        } else {
            ProvenanceVerdict::Foreign
        };
    }

    match (resolved.email.as_deref(), resolved_org) {
        (Some(email), Some(org))
            if email.trim().eq_ignore_ascii_case(account.email.trim()) && org == own_org =>
        {
            ProvenanceVerdict::Owned
        }
        (Some(_), Some(_)) => ProvenanceVerdict::Foreign,
        _ => ProvenanceVerdict::Unresolved,
    }
}

async fn verify_active_usage_provenance(
    account: &Account,
    live: &str,
    backup: &str,
) -> ProvenanceVerdict {
    if !backup.is_empty()
        && (backup == live
            || oauth::credential_fingerprint(backup) == oauth::credential_fingerprint(live))
    {
        return ProvenanceVerdict::Owned;
    }
    let Some(token) = oauth::extract_access_token(live) else {
        return ProvenanceVerdict::Unresolved;
    };
    let Some(cache_key) = active_provenance_cache_key(account, live) else {
        return ProvenanceVerdict::Unresolved;
    };
    let verdicts = active_provenance_cache();
    if let Some(verdict) = verdicts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .copied()
    {
        return verdict;
    }

    let verdict = oauth::fetch_oauth_profile(&token)
        .await
        .map_or(ProvenanceVerdict::Unresolved, |resolved| {
            active_usage_provenance(account, &resolved)
        });
    // Exactly like cswap, only definitive verdicts are memoized. An endpoint
    // failure or partial schema must be retried on a later collection pass.
    if verdict != ProvenanceVerdict::Unresolved {
        verdicts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(cache_key, verdict);
    }
    verdict
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveUsageError {
    Missing,
    ForeignCredential,
    ReloginRequired,
    Unavailable,
}

fn complete_oauth(credentials: &str) -> bool {
    let Some(oauth) = oauth::extract_oauth_data(credentials) else {
        return false;
    };
    ["accessToken", "refreshToken"].into_iter().all(|field| {
        oauth
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    })
}

fn oauth_expired(credentials: &str) -> bool {
    match oauth::extract_oauth_data(credentials) {
        Some(data) => oauth::is_oauth_token_expired(data.get("expiresAt").and_then(Value::as_f64)),
        None => true,
    }
}

fn same_oauth_lineage(left: &str, right: &str) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && oauth::credential_fingerprint(left) == oauth::credential_fingerprint(right)
}

async fn fetch_normalized_usage(
    network: &dyn oauth::OAuthNetwork,
    credentials: &str,
) -> Result<oauth::UsageResult, oauth::UsageFetchError> {
    let access =
        oauth::extract_access_token(credentials).ok_or(oauth::UsageFetchError::BadResponse)?;
    let value = network.fetch_usage(&access).await?;
    oauth::normalize_usage_response(&value)
        .ok()
        .flatten()
        .ok_or(oauth::UsageFetchError::BadResponse)
}

async fn resync_active_backup_if_current(
    account: &Account,
    expected_live: &str,
    timeout: Duration,
) -> Result<(), ActiveUsageError> {
    let locks = tokio::task::spawn_blocking(move || {
        crate::switch_transaction::acquire_active_refresh_locks(timeout)
    })
    .await
    .map_err(|_| ActiveUsageError::Unavailable)?
    .map_err(|_| ActiveUsageError::Unavailable)?;
    let sequence = read_sequence_data().ok_or(ActiveUsageError::Unavailable)?;
    if current_account_number(&sequence).as_deref() != Some(account.number.to_string().as_str()) {
        return Err(ActiveUsageError::Unavailable);
    }
    let current = accounts_from_sequence(&sequence)
        .into_iter()
        .find(|current| {
            current.number == account.number
                && current.email == account.email
                && current.stable_key() == account.stable_key()
        })
        .ok_or(ActiveUsageError::Unavailable)?;
    {
        let mut store = CredentialStore::new(GuiStoreHost);
        if store.read_active_credentials().value.as_deref() != Some(expected_live) {
            return Err(ActiveUsageError::Unavailable);
        }
        store
            .write_account_credentials(&current.number.to_string(), &current.email, expected_live)
            .map_err(|_| ActiveUsageError::Unavailable)?;
    }
    drop(locks);
    Ok(())
}

/// Fetch active usage while preserving cswap/Claude refresh-token ownership.
/// The refresh POST is the sole bounded network exception inside the narrow
/// active-refresh lock set; the final usage request happens after release.
async fn fetch_active_usage_with_network(
    account: &Account,
    observed_live: &str,
    observed_backup: &str,
    network: &dyn oauth::OAuthNetwork,
    lock_timeout: Duration,
) -> Result<oauth::UsageResult, ActiveUsageError> {
    let observed_is_recoverable_wipe = observed_live.is_empty()
        || (oauth::extract_oauth_data(observed_live).is_some() && !complete_oauth(observed_live));
    if !complete_oauth(observed_live)
        && (!observed_is_recoverable_wipe || !complete_oauth(observed_backup))
    {
        return Err(ActiveUsageError::Missing);
    }
    let mut force_refresh = false;
    if complete_oauth(observed_live) {
        let observed_fingerprint = oauth::credential_fingerprint(observed_live)
            .unwrap_or_else(|| oauth_refresh::credential_generation(observed_live));
        if OAuthQuarantine::new(paths::backup_root())
            .is_rejected(&account.stable_key(), &observed_fingerprint)
        {
            return Err(ActiveUsageError::ReloginRequired);
        }
    }
    if complete_oauth(observed_live) && !oauth_expired(observed_live) {
        match fetch_normalized_usage(network, observed_live).await {
            Ok(usage) => {
                let verdict =
                    verify_active_usage_provenance(account, observed_live, observed_backup).await;
                if verdict == ProvenanceVerdict::Foreign {
                    return Err(ActiveUsageError::ForeignCredential);
                }
                if verdict == ProvenanceVerdict::Owned
                    && !same_oauth_lineage(observed_live, observed_backup)
                {
                    if let Err(error) =
                        resync_active_backup_if_current(account, observed_live, lock_timeout).await
                    {
                        log::warn!(
                            "active OAuth usage was attributable but its slot backup could not be resynced: {error:?}"
                        );
                    }
                }
                return Ok(usage);
            }
            Err(oauth::UsageFetchError::Http { status: 401, .. }) => {
                force_refresh = true;
            }
            Err(_) => return Err(ActiveUsageError::Unavailable),
        }
    }

    let cached = cached_active_usage_provenance(account, observed_live);
    if !same_oauth_lineage(observed_live, observed_backup)
        && !complete_oauth(observed_backup)
        && cached != Some(ProvenanceVerdict::Owned)
    {
        return Err(if cached == Some(ProvenanceVerdict::Foreign) {
            ActiveUsageError::ForeignCredential
        } else {
            ActiveUsageError::Unavailable
        });
    }

    // Both active and inactive refresh paths take this per-account lease
    // before any vault/Claude lock. A snapshot that classified this account
    // as inactive can therefore finish and persist its successor before the
    // active path re-reads live state, preventing duplicate grant use.
    let refresh_leases = oauth_refresh::FileRefreshLeases::new(paths::backup_root(), lock_timeout);
    let stable_key = account.stable_key();
    let refresh_lease = refresh_leases
        .acquire(&stable_key)
        .await
        .map_err(|_| ActiveUsageError::Unavailable)?;

    let timeout = lock_timeout;
    let locks = tokio::task::spawn_blocking(move || {
        crate::switch_transaction::acquire_active_refresh_locks(timeout)
    })
    .await
    .map_err(|_| ActiveUsageError::Unavailable)?
    .map_err(|_| ActiveUsageError::Unavailable)?;

    let sequence = read_sequence_data().ok_or(ActiveUsageError::Unavailable)?;
    if current_account_number(&sequence).as_deref() != Some(account.number.to_string().as_str()) {
        return Err(ActiveUsageError::Unavailable);
    }
    let current_account = accounts_from_sequence(&sequence)
        .into_iter()
        .find(|current| {
            current.number == account.number
                && current.email == account.email
                && current.stable_key() == account.stable_key()
        })
        .ok_or(ActiveUsageError::Unavailable)?;
    let (live, backup) = {
        let mut store = CredentialStore::new(GuiStoreHost);
        let active = store.read_active_credentials();
        if active.keychain_unavailable && active.value.as_deref().unwrap_or_default().is_empty() {
            return Err(ActiveUsageError::Unavailable);
        }
        let live = active.value.ok_or(ActiveUsageError::Unavailable)?;
        (
            live,
            store.read_account_credentials(
                &current_account.number.to_string(),
                &current_account.email,
            ),
        )
    };

    let live_changed = live != observed_live;
    if live_changed && complete_oauth(&live) && !oauth_expired(&live) {
        let licensed = same_oauth_lineage(&live, &backup)
            || cached_active_usage_provenance(&current_account, &live)
                == Some(ProvenanceVerdict::Owned);
        if !licensed {
            return Err(
                if cached_active_usage_provenance(&current_account, &live)
                    == Some(ProvenanceVerdict::Foreign)
                {
                    ActiveUsageError::ForeignCredential
                } else {
                    ActiveUsageError::Unavailable
                },
            );
        }
        CredentialStore::new(GuiStoreHost)
            .write_account_credentials(
                &current_account.number.to_string(),
                &current_account.email,
                &live,
            )
            .map_err(|_| ActiveUsageError::Unavailable)?;
        drop(locks);
        drop(refresh_lease);
        return fetch_normalized_usage(network, &live)
            .await
            .map_err(|_| ActiveUsageError::Unavailable);
    }

    if live_changed && !live.is_empty() {
        let live_verdict = cached_active_usage_provenance(&current_account, &live);
        let attributable = complete_oauth(&live)
            && (same_oauth_lineage(&live, &backup)
                || live_verdict == Some(ProvenanceVerdict::Owned));
        if !attributable {
            return Err(if live_verdict == Some(ProvenanceVerdict::Foreign) {
                ActiveUsageError::ForeignCredential
            } else {
                ActiveUsageError::Unavailable
            });
        }
    }

    let (working, restore_live) = if complete_oauth(&backup) && !oauth_expired(&backup) {
        let restore_live = live != backup;
        (backup, restore_live)
    } else if complete_oauth(&live)
        && (same_oauth_lineage(&live, &backup)
            || cached_active_usage_provenance(&current_account, &live)
                == Some(ProvenanceVerdict::Owned))
    {
        (live, false)
    } else if complete_oauth(&backup) {
        (backup, false)
    } else {
        return Err(ActiveUsageError::Unavailable);
    };

    let working_fingerprint = oauth::credential_fingerprint(&working)
        .unwrap_or_else(|| oauth_refresh::credential_generation(&working));
    if OAuthQuarantine::new(paths::backup_root())
        .is_rejected(&current_account.stable_key(), &working_fingerprint)
    {
        return Err(ActiveUsageError::ReloginRequired);
    }
    if restore_live {
        CredentialStore::new(GuiStoreHost)
            .write_refreshed_oauth_credentials(&working)
            .map_err(|_| ActiveUsageError::Unavailable)?;
    }

    if !oauth_expired(&working) && !force_refresh {
        drop(locks);
        drop(refresh_lease);
        return fetch_normalized_usage(network, &working)
            .await
            .map_err(|_| ActiveUsageError::Unavailable);
    }

    let refresh = network.refresh(&working).await;
    let Some(successor) = refresh.credentials else {
        if refresh.error == Some(oauth::RefreshError::InvalidGrant) {
            let fingerprint = oauth::credential_fingerprint(&working)
                .unwrap_or_else(|| oauth_refresh::credential_generation(&working));
            OAuthQuarantine::new(paths::backup_root())
                .reject(
                    &current_account.stable_key(),
                    &fingerprint,
                    chrono::Utc::now(),
                )
                .map_err(|_| ActiveUsageError::Unavailable)?;
            return Err(ActiveUsageError::ReloginRequired);
        }
        return Err(ActiveUsageError::Unavailable);
    };
    if !complete_oauth(&successor) {
        return Err(ActiveUsageError::Unavailable);
    }

    let (backup_write, live_write) = {
        let mut store = CredentialStore::new(GuiStoreHost);
        let backup_write = store.write_account_credentials(
            &current_account.number.to_string(),
            &current_account.email,
            &successor,
        );
        let live_write = store.write_refreshed_oauth_credentials(&successor);
        (backup_write, live_write)
    };
    match (backup_write, live_write) {
        (Err(backup_error), Ok(())) => log::warn!(
            "active OAuth successor reached live storage but backup persistence failed: {backup_error}"
        ),
        (_, Err(_)) => return Err(ActiveUsageError::Unavailable),
        (Ok(()), Ok(())) => {}
    }
    // This process consumed the predecessor grant, so the returned
    // successor is definitively this slot's lineage. Keep that evidence even
    // if the backup write failed; it licenses safe recovery at the next
    // expiry, matching cswap's in-memory ownership memo.
    cache_active_usage_provenance(&current_account, &successor, ProvenanceVerdict::Owned);
    let successor_fingerprint = oauth::credential_fingerprint(&successor)
        .unwrap_or_else(|| oauth_refresh::credential_generation(&successor));
    if let Err(error) = OAuthQuarantine::new(paths::backup_root())
        .clear_obsolete(&current_account.stable_key(), &successor_fingerprint)
    {
        log::warn!("could not clear obsolete OAuth quarantine after active refresh: {error}");
    }
    drop(locks);
    drop(refresh_lease);
    fetch_normalized_usage(network, &successor)
        .await
        .map_err(|_| ActiveUsageError::Unavailable)
}

async fn fetch_active_usage(
    account: &Account,
    observed_live: &str,
    observed_backup: &str,
) -> Result<oauth::UsageResult, ActiveUsageError> {
    let network = oauth::ReqwestOAuthNetwork::with_refresh_timeout(Duration::from_secs(6));
    fetch_active_usage_with_network(
        account,
        observed_live,
        observed_backup,
        &network,
        Duration::from_secs(10),
    )
    .await
}

fn production_refresh_coordinator(timeout: Duration) -> RefreshCoordinator {
    RefreshCoordinator::new(
        Arc::new(oauth::ReqwestOAuthNetwork::default()),
        Arc::new(GuiGenerationStore::new(timeout)),
        Arc::new(oauth_refresh::FileRefreshLeases::new(
            paths::backup_root(),
            timeout,
        )),
        Arc::new(oauth_refresh::SystemClock),
    )
}

impl GuiGenerationStore {
    fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    fn with_current<T>(
        &self,
        identity: &AccountIdentity,
        operation: impl FnOnce(&mut CredentialStore<GuiStoreHost>, &Account) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let _lock = crate::locking::acquire_or_err(vault_lock_path(), self.timeout)
            .map_err(|error| error.to_string())?;
        let account = read_accounts()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|account| {
                account.number.to_string() == identity.number
                    && account.email == identity.email
                    && account.stable_key() == identity.stable_key
            });
        let Some(account) = account else {
            return Ok(None);
        };
        let mut store = CredentialStore::new(GuiStoreHost);
        operation(&mut store, &account).map(Some)
    }
}

impl GenerationStore for GuiGenerationStore {
    fn read(&self, identity: &AccountIdentity) -> Result<Option<StoredGeneration>, String> {
        self.with_current(identity, |store, account| {
            let credentials =
                store.read_account_credentials(&account.number.to_string(), &account.email);
            Ok((!credentials.is_empty()).then(|| StoredGeneration::new(credentials)))
        })
        .map(Option::flatten)
    }

    fn compare_and_store(
        &self,
        identity: &AccountIdentity,
        expected_generation: &str,
        successor: &str,
    ) -> Result<CompareAndStore, String> {
        self.with_current(identity, |store, account| {
            let number = account.number.to_string();
            let current = store.read_account_credentials(&number, &account.email);
            if current.is_empty() {
                return Ok(CompareAndStore::Missing);
            }
            let current = StoredGeneration::new(current);
            let successor = StoredGeneration::new(successor.to_string());
            if current.generation == successor.generation {
                return Ok(CompareAndStore::AlreadyCurrent(current));
            }
            if current.generation != expected_generation {
                return Ok(CompareAndStore::Superseded(current));
            }
            store
                .write_account_credentials(&number, &account.email, &successor.credentials)
                .map_err(|error| error.to_string())?;
            OAuthQuarantine::new(paths::backup_root())
                .clear_obsolete(
                    &identity.stable_key,
                    oauth::credential_fingerprint(&successor.credentials)
                        .as_deref()
                        .unwrap_or(&successor.generation),
                )
                .map_err(|error| error.to_string())?;
            Ok(CompareAndStore::Persisted(successor))
        })
        .map(|result| result.unwrap_or(CompareAndStore::Missing))
    }

    fn is_rejected(&self, identity: &AccountIdentity, credentials: &str) -> Result<bool, String> {
        let fingerprint = oauth::credential_fingerprint(credentials)
            .unwrap_or_else(|| oauth_refresh::credential_generation(credentials));
        self.with_current(identity, |_, _| {
            Ok(OAuthQuarantine::new(paths::backup_root())
                .is_rejected(&identity.stable_key, &fingerprint))
        })
        .map(|value| value.unwrap_or(false))
    }

    fn reject_if_current(
        &self,
        identity: &AccountIdentity,
        expected_generation: &str,
        credentials: &str,
    ) -> Result<bool, String> {
        self.with_current(identity, |store, account| {
            let current =
                store.read_account_credentials(&account.number.to_string(), &account.email);
            if current.is_empty()
                || oauth_refresh::credential_generation(&current) != expected_generation
            {
                return Ok(false);
            }
            let fingerprint = oauth::credential_fingerprint(credentials)
                .unwrap_or_else(|| expected_generation.to_string());
            OAuthQuarantine::new(paths::backup_root())
                .reject(&identity.stable_key, &fingerprint, chrono::Utc::now())
                .map_err(|error| error.to_string())?;
            Ok(true)
        })
        .map(|value| value.unwrap_or(false))
    }
}

/// Release a dead-token verdict only when a successful credential-ingress
/// operation proved that the account now carries a different generation.
///
/// Current cswap performs the same recovery after add/re-login and also drops
/// autoswitch quarantines when the stored credential fingerprint changes. A
/// cleanup failure is deliberately non-fatal here: the credential and registry
/// write have already committed, and the generation mismatch means the stale
/// verdict no longer matches even if its file entry remains on disk.
fn clear_replaced_quarantine(email: &str, organization_uuid: Option<&str>, credentials: &str) {
    let identity = Account {
        email: email.to_string(),
        organization_uuid: organization_uuid.map(str::to_string),
        ..Account::default()
    }
    .stable_key();
    let fingerprint = oauth::credential_fingerprint(credentials)
        .unwrap_or_else(|| oauth_refresh::credential_generation(credentials));
    if let Err(error) =
        OAuthQuarantine::new(paths::backup_root()).clear_obsolete(&identity, &fingerprint)
    {
        log::warn!(
            "could not clear obsolete OAuth quarantine after credential replacement: {error}"
        );
    }
}

// ---------------------------------------------------------------------------
// Snapshot (accounts + freshly-fetched usage).
// ---------------------------------------------------------------------------

/// Accounts plus freshly-fetched usage, wrapped in the single `Native`
/// [`Environment`] this port produces (WSL/profile environments are out of
/// scope here; see `crate::wsl`).
///
/// No credential store is carried across an await. Profile and final usage
/// requests run unlocked; the active refresh grant uses the bounded lease and
/// credential-lock exception documented above. A per-account usage-fetch
/// failure degrades that account to [`UsageStatus::Stale`] rather than
/// failing the whole snapshot; a disabled account's status is left alone
/// either way. (This port has no persistent usage cache — `oauth.rs` and
/// `credentials.rs` are the only reused modules in scope — so "last-known"
/// degrades to "no reading, marked Stale" rather than serving a genuinely
/// cached prior measurement; see the port report.)
pub async fn read_snapshot() -> Result<Snapshot, SwitchError> {
    let accounts = read_accounts()?;
    let coordinator = production_refresh_coordinator(Duration::from_secs(10));

    // Phase 1 — read every credential up front, then DROP the store before any
    // network call happens.
    //
    // Two reasons, and they point the same way. It mirrors the rule inherited
    // from upstream that no store or lock may be held across I/O. And
    // `CredentialStore` is not `Send`, so holding it across an `.await` makes
    // this future non-`Send`, which Tauri rejects outright for async commands.
    let mut pending: Vec<(Account, Option<credentials::ActiveCredentials>, String)> =
        Vec::with_capacity(accounts.len());
    {
        let mut store = CredentialStore::new(GuiStoreHost);
        for account in accounts {
            let (creds, backup) = if account.active {
                (
                    Some(store.read_active_credentials()),
                    store.read_account_credentials(&account.number.to_string(), &account.email),
                )
            } else {
                (None, String::new())
            };
            pending.push((account, creds, backup));
        }
    }

    // Phase 2 — network work, with no credential store held.
    let mut measured = Vec::with_capacity(pending.len());
    for (mut account, creds, backup) in pending {
        let num = account.number.to_string();

        let result = if account.active {
            match creds {
                None => Some(Err(oauth_refresh::RefreshCoordinatorError::RefreshFailed(
                    None,
                ))),
                Some(active)
                    if active.value.is_none()
                        || (active.keychain_unavailable
                            && active.value.as_deref().unwrap_or_default().is_empty()) =>
                {
                    Some(Err(oauth_refresh::RefreshCoordinatorError::RefreshFailed(
                        None,
                    )))
                }
                Some(active) => {
                    let creds = active.value.unwrap_or_default();
                    match fetch_active_usage(&account, &creds, &backup).await {
                        Err(ActiveUsageError::ForeignCredential) => {
                            account.usage_status = UsageStatus::ForeignCredential;
                            log::warn!(
                            "active credential resolves to a different account than slot {num}; usage attribution suppressed"
                        );
                            None
                        }
                        Ok(usage) => Some(Ok(usage)),
                        Err(ActiveUsageError::Missing) => None,
                        Err(ActiveUsageError::ReloginRequired) => {
                            Some(Err(oauth_refresh::RefreshCoordinatorError::ReloginRequired))
                        }
                        Err(ActiveUsageError::Unavailable) => Some(Err(
                            oauth_refresh::RefreshCoordinatorError::RefreshFailed(None),
                        )),
                    }
                }
            }
        } else {
            let identity = AccountIdentity {
                number: num,
                email: account.email.clone(),
                stable_key: account.stable_key(),
            };
            Some(coordinator.fetch_inactive_usage(&identity).await)
        };

        match result {
            Some(Ok(result)) => {
                account.usage = Some(to_model_usage(&result));
                account.usage_fetched_at = Some(chrono::Utc::now().to_rfc3339());
                account.usage_age_seconds = Some(0.0);
                if account.usage_status != UsageStatus::Disabled {
                    account.usage_status = UsageStatus::Ok;
                }
            }
            Some(Err(oauth_refresh::RefreshCoordinatorError::ReloginRequired)) => {
                if account.usage_status != UsageStatus::Disabled {
                    account.usage_status = UsageStatus::ReloginRequired;
                }
            }
            Some(Err(oauth_refresh::RefreshCoordinatorError::Missing)) | None => {}
            Some(Err(_)) => {
                if account.usage_status != UsageStatus::Disabled {
                    account.usage_status = UsageStatus::Unavailable;
                }
            }
        }
        measured.push(account);
    }

    let environment = Environment {
        id: "native".to_string(),
        label: "Native".to_string(),
        path: paths::claude_config_home().display().to_string(),
        kind: EnvKind::Native,
        status: EnvStatus::Live,
        accounts: measured,
        last_seen_seconds: None,
        // Not probed independently of `accounts` here — this path already
        // reads real credentials to populate `accounts`, so there is no
        // separate "not determined" state to report the way there is for a
        // WSL distro (see `wsl.rs::build_environments`).
        has_credentials: None,
    };

    Ok(Snapshot::new(vec![environment]))
}

fn to_usage_window(w: &oauth::Window) -> UsageWindow {
    UsageWindow {
        pct: w.pct,
        resets_at: w.resets_at.clone(),
        countdown: w.countdown.clone(),
        clock: w.clock.clone(),
        ..Default::default()
    }
}

fn to_scoped_window(w: &oauth::ScopedWindow) -> UsageWindow {
    UsageWindow {
        pct: w.pct,
        resets_at: w.resets_at.clone(),
        countdown: w.countdown.clone(),
        clock: w.clock.clone(),
        name: Some(w.name.clone()),
        ..Default::default()
    }
}

fn to_model_usage(u: &oauth::UsageResult) -> Usage {
    Usage {
        five_hour: u.five_hour.as_ref().map(to_usage_window),
        seven_day: u.seven_day.as_ref().map(to_usage_window),
        scoped: if u.scoped.is_empty() {
            None
        } else {
            Some(u.scoped.iter().map(to_scoped_window).collect())
        },
    }
}

// ---------------------------------------------------------------------------
// Locking: our vault vs. Claude Code's official files.
// ---------------------------------------------------------------------------
//
// Two different resources ever need protecting in this module, and they are
// not the same resource wearing two names:
//
// - OUR VAULT (`<our backup_root>/.lock`, [`vault_lock_path`]) guards
//   `sequence.json` and the per-account credential/config backups — files
//   only this app ever writes. Nothing else on the machine has a reason to
//   touch this file, so locking it only ever contends with another instance
//   of this app.
// - CLAUDE CODE'S OFFICIAL FILES (`.credentials.json`, `.claude.json`) are
//   read and written by Claude Code itself. Mutual exclusion against it
//   therefore cannot use a lock file of our choosing — it requires locking the
//   directories Claude Code itself honours (see `crate::claude_locks`).
//
// So any function that writes the official files — today, only [`switch_to`]
// — holds the complete lock set from `crate::switch_transaction`: Claude's
// primary + legacy credential locks, Claude's config lock, then our vault
// lock. A function that only ever touches our own vault
// (`add_current_account`, `add_token`, `set_account_enabled`) has no reason to
// take Claude's locks at all — there is nothing there for another process to
// race it on — so those take [`vault_lock_path`] alone.

// ---------------------------------------------------------------------------
// Switch.
// ---------------------------------------------------------------------------

/// Switch the live login to `target`'s stored credential.
///
/// Holds the complete Claude/vault lock set for the whole mutation —
/// see `crate::switch_transaction` for the canonical order.
/// OAuth freshening is the only network phase and completes before those
/// mutation locks are acquired. Order of operations:
///
/// 1. Re-resolve the target and freshen its latest stored generation.
/// 2. Acquire mutation locks, re-resolve the target, and require the exact
///    validated full-content generation plus a valid config backup.
/// 3. Read and **back up the outgoing login** — the account currently live (resolved
///    from `~/.claude.json`'s `oauthAccount`, not from a possibly-stale
///    `activeAccountNumber`) has its credential and config snapshot written
///    to its backup slot before anything about the target is installed. An unmanaged/unattributable live credential (no
///    resolvable slot) is preserved via
///    [`CredentialStore::write_unclaimed_credential`] instead of a normal
///    slot backup, so a fresh-machine or drifted-login switch still never
///    silently destroys it.
/// 4. **Install** the target credential (composed with the machine's live
///    shared OAuth fields, mirroring `_prepare_credentials_for_activation`)
///    and splice its `oauthAccount` block into the global config.
/// 5. Update `sequence.json`'s `activeAccountNumber`.
///
/// CLI-only self-switch and `--force` modes are not reproduced. The normal GUI
/// path does reproduce upstream's foreign-credential classification and adds a
/// durable cross-file journal through [`crate::switch_transaction`].
pub async fn switch_to(target: &Account) -> Result<(), SwitchError> {
    let timeout = crate::locking::DEFAULT_TIMEOUT;
    let coordinator = production_refresh_coordinator(timeout);
    switch_to_with_coordinator(target, &coordinator, timeout).await
}

async fn switch_to_with_coordinator(
    target: &Account,
    coordinator: &RefreshCoordinator,
    timeout: Duration,
) -> Result<(), SwitchError> {
    // A refresh winner may land between network validation and acquisition of
    // the complete mutation lock set. Re-resolve and retry from that winner;
    // never activate the stale callback's bytes.
    for _ in 0..3 {
        let identity = target_identity_with_timeout(target.number, timeout)?;
        let validated = coordinator.freshen_for_activation(&identity).await?;
        let provenance = prefetch_live_provenance().await;
        match switch_to_validated_with_timeout(target, &validated, &provenance, timeout) {
            Err(SwitchError::TargetGenerationChanged(_)) => continue,
            result => return result,
        }
    }
    Err(SwitchError::TargetGenerationChanged(
        target.number.to_string(),
    ))
}

#[derive(Debug, Clone, Default)]
struct LiveProvenance {
    live: String,
    resolved: Option<oauth::TokenAccount>,
}

async fn prefetch_live_provenance() -> LiveProvenance {
    let mut store = CredentialStore::new(GuiStoreHost);
    let live = store.read_active_credentials().value.unwrap_or_default();
    let resolved = match oauth::extract_access_token(&live) {
        Some(token) => oauth::fetch_oauth_profile(&token).await,
        None => None,
    };
    LiveProvenance { live, resolved }
}

fn classify_outgoing_destination(
    store: &mut CredentialStore<GuiStoreHost>,
    data: &mut Map<String, Value>,
    current_num: &str,
    current_email: &str,
    live: &str,
    provenance: &LiveProvenance,
) -> crate::switch_transaction::OutgoingDestination {
    let own_backup = store.read_account_credentials(current_num, current_email);
    if !own_backup.is_empty()
        && (own_backup == live
            || oauth::credential_fingerprint(&own_backup) == oauth::credential_fingerprint(live))
    {
        return crate::switch_transaction::OutgoingDestination::Managed {
            number: current_num.to_string(),
            email: current_email.to_string(),
            config_backup_path: account_config_path(current_num, current_email),
        };
    }

    let tokens_wiped = oauth::extract_oauth_data(live).is_some_and(|oauth| {
        !oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty())
            && !oauth
                .get("refreshToken")
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
    });
    if tokens_wiped {
        log::warn!(
            "live credential tokens are wiped; preserving them outside account {current_num}"
        );
        return crate::switch_transaction::OutgoingDestination::Unclaimed;
    }

    let Some(resolved) = provenance
        .resolved
        .as_ref()
        .filter(|_| provenance.live == live)
    else {
        // Same fail-open rule as current cswap: an unavailable/advisory
        // identity oracle must not discard a legitimate local rotation.
        return crate::switch_transaction::OutgoingDestination::Managed {
            number: current_num.to_string(),
            email: current_email.to_string(),
            config_backup_path: account_config_path(current_num, current_email),
        };
    };

    let accounts = data.get("accounts").and_then(Value::as_object);
    let own = accounts.and_then(|accounts| accounts.get(current_num));
    let own_uuid = own
        .and_then(|record| record.get("uuid"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let own_org = own
        .and_then(|record| record.get("organizationUuid"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let resolved_org = resolved.organization_uuid.as_deref().unwrap_or_default();
    if !own_uuid.is_empty()
        && resolved.uuid == own_uuid
        && (resolved_org.is_empty() || own_org.is_empty() || resolved_org == own_org)
    {
        return crate::switch_transaction::OutgoingDestination::Managed {
            number: current_num.to_string(),
            email: current_email.to_string(),
            config_backup_path: account_config_path(current_num, current_email),
        };
    }

    let mut matched_slot = accounts.and_then(|accounts| {
        resolved.email.as_deref().and_then(|resolved_email| {
            accounts.iter().find_map(|(number, record)| {
                let email = record.get("email")?.as_str()?;
                let org = record
                    .get("organizationUuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                (email.trim().eq_ignore_ascii_case(resolved_email.trim()) && org == resolved_org)
                    .then(|| number.clone())
            })
        })
    });
    if !resolved.uuid.is_empty() {
        if let Some(stored_uuid) = matched_slot.as_deref().and_then(|slot| {
            accounts
                .and_then(|accounts| accounts.get(slot))
                .and_then(|record| record.get("uuid"))
                .and_then(Value::as_str)
                .filter(|uuid| !uuid.is_empty())
        }) {
            if stored_uuid != resolved.uuid {
                // Same email/org with a conflicting account UUID is a
                // recycled identity, never ownership of that slot.
                matched_slot = None;
            }
        }
    }
    if matched_slot.is_none() && !resolved.uuid.is_empty() {
        matched_slot = accounts.and_then(|accounts| {
            accounts.iter().find_map(|(number, record)| {
                let uuid = record.get("uuid")?.as_str()?;
                let org = record
                    .get("organizationUuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                (uuid == resolved.uuid && org == resolved_org).then(|| number.clone())
            })
        });
    }

    if matched_slot.as_deref() == Some(current_num) {
        if own_uuid.is_empty() && !resolved.uuid.is_empty() {
            if let Some(record) = data
                .get_mut("accounts")
                .and_then(Value::as_object_mut)
                .and_then(|accounts| accounts.get_mut(current_num))
                .and_then(Value::as_object_mut)
            {
                record.insert("uuid".to_string(), Value::String(resolved.uuid.clone()));
            }
        }
        return crate::switch_transaction::OutgoingDestination::Managed {
            number: current_num.to_string(),
            email: current_email.to_string(),
            config_backup_path: account_config_path(current_num, current_email),
        };
    }

    let structurally_complete = resolved.email.is_some() && resolved.organization_uuid.is_some();
    let foreign_uuid_confirmed = matched_slot.as_deref().is_some_and(|slot| {
        accounts
            .and_then(|accounts| accounts.get(slot))
            .and_then(|record| record.get("uuid"))
            .and_then(Value::as_str)
            .is_some_and(|uuid| !uuid.is_empty() && uuid == resolved.uuid)
    });
    if foreign_uuid_confirmed || structurally_complete {
        log::warn!(
            "live credential identity does not belong to configured account {current_num}; \
             preserving it in the unclaimed safety store"
        );
        crate::switch_transaction::OutgoingDestination::Unclaimed
    } else {
        crate::switch_transaction::OutgoingDestination::Managed {
            number: current_num.to_string(),
            email: current_email.to_string(),
            config_backup_path: account_config_path(current_num, current_email),
        }
    }
}

fn target_identity_with_timeout(
    target_number: u32,
    timeout: Duration,
) -> Result<AccountIdentity, SwitchError> {
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    let account = read_accounts()?
        .into_iter()
        .find(|account| account.number == target_number)
        .ok_or_else(|| SwitchError::UnknownAccount(target_number.to_string()))?;
    Ok(AccountIdentity {
        number: target_number.to_string(),
        email: account.email.clone(),
        stable_key: account.stable_key(),
    })
}

fn switch_to_validated_with_timeout(
    target: &Account,
    validated: &ValidatedCredential,
    provenance: &LiveProvenance,
    timeout: Duration,
) -> Result<(), SwitchError> {
    let num = target.number.to_string();

    // Rule 1: lock before touching anything, for the whole mutation. This
    // function writes Claude Code's official files (step 4 below), so it
    // needs the complete cross-process lock set.
    let _locks = crate::switch_transaction::acquire_live_state_locks(timeout)?;

    let mut data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;

    // Source of truth for the target's email is the registry, not whatever
    // the caller's (possibly stale) `Account` says.
    let target_account = accounts_from_sequence(&data)
        .into_iter()
        .find(|account| account.number == target.number)
        .ok_or_else(|| SwitchError::UnknownAccount(num.clone()))?;
    let email = data
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|accounts| accounts.get(&num))
        .and_then(|record| record.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| SwitchError::UnknownAccount(num.clone()))?;

    let mut store = CredentialStore::new(GuiStoreHost);

    // Validate every target artifact, including the exact credential
    // generation returned by the network phase, before reading or backing up
    // the outgoing login. This is the mutation boundary's core invariant.
    if validated.identity.number != num
        || validated.identity.email != email
        || validated.identity.stable_key != target_account.stable_key()
    {
        return Err(SwitchError::TargetGenerationChanged(num));
    }
    let target_creds = store.read_account_credentials(&num, &email);
    if target_creds.is_empty() {
        return Err(SwitchError::NoStoredCredentials(num));
    }
    if oauth_refresh::credential_generation(&target_creds) != validated.generation
        || target_creds != validated.credentials
    {
        return Err(SwitchError::TargetGenerationChanged(num));
    }
    let target_config_text = read_account_config(&num, &email)
        .ok_or_else(|| SwitchError::NoStoredConfig(num.clone()))?;
    let target_config_value: Value = serde_json::from_str(&target_config_text)?;
    let target_oauth = target_config_value
        .get("oauthAccount")
        .cloned()
        .ok_or_else(|| SwitchError::InvalidBackupConfig(num.clone()))?;

    let current_num = current_account_number(&data);
    let outgoing = match &current_num {
        Some(cur_num) => {
            let cur_email = data
                .get("accounts")
                .and_then(Value::as_object)
                .and_then(|accounts| accounts.get(cur_num))
                .and_then(|record| record.get("email"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let live = store
                .read_active_credentials()
                .value
                .ok_or(SwitchError::CredentialRead)?;
            classify_outgoing_destination(
                &mut store, &mut data, cur_num, &cur_email, &live, provenance,
            )
        }
        None => crate::switch_transaction::OutgoingDestination::Unclaimed,
    };

    crate::switch_transaction::execute_locked(
        &mut store,
        crate::switch_transaction::SwitchPlan {
            target: crate::switch_journal::JournalTarget {
                number: num,
                email,
                stable_key: target_account.stable_key(),
                credential_generation: validated.generation.clone(),
            },
            target_credentials: validated.credentials.clone(),
            target_oauth,
            sequence: data,
            sequence_path: accounts_file(),
            global_config_path: paths::global_config_path(),
            outgoing,
        },
        &crate::switch_transaction::NoFaults,
    )?;
    Ok(())
}

#[cfg(test)]
fn switch_to_with_timeout(target: &Account, timeout: Duration) -> Result<(), SwitchError> {
    let identity = target_identity_with_timeout(target.number, timeout)?;
    let stored = GuiGenerationStore::new(timeout)
        .read(&identity)
        .map_err(oauth_refresh::RefreshCoordinatorError::PersistenceFailed)?
        .ok_or(oauth_refresh::RefreshCoordinatorError::Missing)?;
    let validated = ValidatedCredential {
        identity,
        credentials: stored.credentials,
        generation: stored.generation,
    };
    let mut active_store = CredentialStore::new(GuiStoreHost);
    let provenance = LiveProvenance {
        live: active_store
            .read_active_credentials()
            .value
            .unwrap_or_default(),
        resolved: None,
    };
    switch_to_validated_with_timeout(target, &validated, &provenance, timeout)
}

// ---------------------------------------------------------------------------
// Account management: add / add-token / enable-disable.
//
// Ported from `claude_swap.switcher.ClaudeAccountSwitcher.add_account`,
// `.add_account_from_token`, and `.set_account_disabled`. Each mutating
// function here follows the same two rules as `switch_to`: hold the lock for
// the whole mutation, and never make a network call while holding it.
// ---------------------------------------------------------------------------

/// Ensure `data["accounts"]` is an object, replacing anything else (missing,
/// wrong type, corrupt) with an empty one so callers can always
/// `and_then(Value::as_object_mut)` without a fallible step of their own.
fn ensure_accounts_object(data: &mut Map<String, Value>) {
    if !matches!(data.get("accounts"), Some(Value::Object(_))) {
        data.insert("accounts".to_string(), Value::Object(Map::new()));
    }
}

fn refuse_pending_recovery() -> Result<(), SwitchError> {
    if crate::switch_transaction::recovery_requirement().is_some() {
        Err(crate::switch_transaction::TransactionError::RecoveryRequired.into())
    } else {
        Ok(())
    }
}

/// The lowest slot number `>= 1` not already used as an `accounts` key.
///
/// Deliberately *not* upstream's `max(existing) + 1` (`_get_next_account_number`):
/// reusing the lowest free slot means a freed slot is recycled instead of
/// leaving a permanent gap, which matters more here since removal isn't
/// (yet) exposed by this port and slots are a comparatively scarcer, more
/// visible resource in the GUI's account list than in the CLI.
fn next_free_slot(data: &Map<String, Value>) -> u32 {
    let used: std::collections::HashSet<u32> = data
        .get("accounts")
        .and_then(Value::as_object)
        .map(|accounts| {
            accounts
                .keys()
                .filter_map(|k| k.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();
    let mut candidate = 1u32;
    while used.contains(&candidate) {
        candidate += 1;
    }
    candidate
}

/// Append `slot` to `data["sequence"]` (creating the array if absent),
/// skipping it if already present.
fn add_to_sequence(data: &mut Map<String, Value>, slot: u32) {
    match data.get_mut("sequence").and_then(Value::as_array_mut) {
        Some(arr) => {
            if !arr.iter().any(|v| v.as_u64() == Some(u64::from(slot))) {
                arr.push(Value::from(slot));
            }
        }
        None => {
            data.insert(
                "sequence".to_string(),
                Value::Array(vec![Value::from(slot)]),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Duplicate detection by account identity (not credential bytes).
//
// Comparing raw credential bytes is not enough on its own:
// `oauth::try_refresh_oauth_credentials`
// rotates the refresh token whenever the server issues a new one, and
// `oauth::credential_fingerprint` hashes the refresh token when one is
// present — so the SAME account's fingerprint changes across a refresh-token
// rotation. This caused a confirmed real duplicate registration: two stored
// credentials for one real account (`charlie@example.com`, one
// `organizationUuid`) fingerprinted as `e9938586d217fcad` (slot 1) and
// `0b3c888d8bf0b1b9` (slot 2, added later) — different enough that the old
// fingerprint-only check let the second slot through.
//
// [`find_registered_slot_by_identity`] is the fix: it compares account
// identity (`uuid`, then `organizationUuid` + email) resolved via
// [`oauth::fetch_oauth_profile`], and only falls back to a fingerprint
// comparison when neither side of a given pair has resolvable identity at
// all.
// ---------------------------------------------------------------------------

/// Identity used to detect a duplicate account registration, independent of
/// credential bytes. `None` fields mean "unknown", not "empty" — two
/// identities that are each entirely unknown must never be treated as
/// matching each other (see [`identity_matches`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ResolvedIdentity {
    uuid: Option<String>,
    organization_uuid: Option<String>,
    email: Option<String>,
}

impl From<oauth::TokenAccount> for ResolvedIdentity {
    fn from(account: oauth::TokenAccount) -> Self {
        ResolvedIdentity {
            uuid: Some(account.uuid).filter(|s| !s.is_empty()),
            organization_uuid: account.organization_uuid.filter(|s| !s.is_empty()),
            email: account.email.filter(|s| !s.is_empty()),
        }
    }
}

/// The identity a registry record already carries, read straight out of its
/// `sequence.json` fields (`uuid`, `organizationUuid`, `email`) — the same
/// shape as [`ResolvedIdentity`] so both sides of a comparison line up. A
/// record written before this fix may have no `uuid` key at all; that gap
/// is exactly why [`find_registered_slot_by_identity`] falls back to
/// `organizationUuid` + email rather than requiring `uuid` on both sides.
fn identity_from_record(record: &Map<String, Value>) -> ResolvedIdentity {
    ResolvedIdentity {
        uuid: record
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        organization_uuid: record
            .get("organizationUuid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        email: record
            .get("email")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

/// Whether `new` and `existing` denote the same Claude account, using the
/// most specific signal both sides actually have:
///
/// 1. Account `uuid`, when both sides have one.
/// 2. Else `organization_uuid` + email (case-insensitive, trimmed), when both
///    sides have an `organization_uuid`.
///
/// Returns `false` (no opinion, not "match") when neither pairing is
/// available — the caller ([`find_registered_slot_by_identity`]) falls back
/// to the credential fingerprint for that pair instead of treating "both
/// unknown" as a match.
fn identity_matches(new: &ResolvedIdentity, existing: &ResolvedIdentity) -> bool {
    if let (Some(a), Some(b)) = (new.uuid.as_deref(), existing.uuid.as_deref()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (
        new.organization_uuid.as_deref(),
        existing.organization_uuid.as_deref(),
    ) {
        if a != b {
            return false;
        }
        return match (new.email.as_deref(), existing.email.as_deref()) {
            (Some(e1), Some(e2)) => e1.trim().eq_ignore_ascii_case(e2.trim()),
            _ => false,
        };
    }
    false
}

/// Return the slot number of an already-registered account that denotes the
/// same Claude account as `new_identity`, or `None`. See the module section
/// above this function for why identity (not just fingerprint) is compared.
///
/// For each registered account, in priority order: compare `uuid` (when both
/// sides have one), else `organization_uuid` + email (when both sides have
/// one), else fall back to comparing `live_fingerprint` against that
/// account's own stored credential fingerprint — the only signal left once
/// neither side offers resolvable identity for that particular pair. Once
/// identity IS resolved on both sides of a pair, it is authoritative for
/// that pair — a non-match there is never second-guessed by also checking
/// the fingerprint.
fn find_registered_slot_by_identity(
    store: &mut CredentialStore<GuiStoreHost>,
    accounts: &Map<String, Value>,
    new_identity: &ResolvedIdentity,
    live_fingerprint: Option<&str>,
) -> Option<String> {
    for (num, value) in accounts {
        let Some(record) = value.as_object() else {
            continue;
        };
        let existing_identity = identity_from_record(record);

        let has_uuid_pair = new_identity.uuid.is_some() && existing_identity.uuid.is_some();
        let has_org_pair = new_identity.organization_uuid.is_some()
            && existing_identity.organization_uuid.is_some();

        if has_uuid_pair || has_org_pair {
            if identity_matches(new_identity, &existing_identity) {
                return Some(num.clone());
            }
            continue;
        }

        if let Some(fp) = live_fingerprint {
            let email = record
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let existing_creds = store.read_account_credentials(num, email);
            if !existing_creds.is_empty()
                && oauth::credential_fingerprint(&existing_creds).as_deref() == Some(fp)
            {
                return Some(num.clone());
            }
        }
    }
    None
}

/// Production identity resolver for [`add_current_account`] / [`add_token`]:
/// bridges to [`oauth::fetch_oauth_profile`], which is `async` and strictly
/// advisory (`None` on any failure — network blip, timeout, non-2xx, ... —
/// per its own doc comment).
///
/// Both callers must stay synchronous: the Tauri command layer (`commands.rs`)
/// calls `switcher::add_current_account` / `switcher::add_token` without
/// `.await`, so changing either to `async fn` is not possible from this file
/// alone. This function instead hops onto whichever Tokio runtime is already
/// driving the caller — always a multi-thread runtime here (see
/// `Cargo.toml`'s `rt-multi-thread` feature, which is what makes
/// `block_in_place` legal) — via `block_in_place`, which lets sibling tasks
/// keep running on other worker threads while this one blocks on the HTTP
/// round trip. Outside any ambient runtime (this module's own tests never
/// reach this function — every test injects a fake resolver instead, to
/// satisfy the "no network calls in tests" rule) a disposable one-off
/// runtime is spun up instead, so this never panics regardless of caller.
fn default_identity_resolver(access_token: &str) -> Option<oauth::TokenAccount> {
    let fut = oauth::fetch_oauth_profile(access_token);
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Runtime::new().ok()?.block_on(fut),
    }
}

/// Register the currently active Claude Code login as a new managed slot.
///
/// This needs no interactive auth from us: the user logs in with Claude Code
/// normally (outside this app), and this call captures whatever is live right
/// now into the store — mirroring `cswap add`. Returns the newly allocated
/// slot number.
///
/// Refuses via [`SwitchError::NoLiveCredential`] if there is no live
/// credential (or it reads empty), and via [`SwitchError::AlreadyRegistered`]
/// if the live login denotes the same Claude account as an already
/// registered slot (see [`find_registered_slot_by_identity`]) — registering
/// the same login twice would leave two slots fighting over one credential
/// on every future switch.
///
/// Duplicate detection prefers account identity (`uuid`, then
/// `organizationUuid` + email) resolved via [`oauth::fetch_oauth_profile`],
/// falling back to [`oauth::credential_fingerprint`] only when identity can't
/// be resolved for a given comparison. This matters because the fingerprint
/// hashes the refresh token when one is present, and
/// `oauth::try_refresh_oauth_credentials` rotates that token whenever the
/// server issues a new one — fingerprint-only comparison therefore misses
/// the same account across a refresh-token rotation (a confirmed real-world
/// bug; see the module section above [`find_registered_slot_by_identity`]).
/// The resolved identity (`uuid`, `organizationUuid`) is also persisted onto
/// the new record so a future add has something stable to match on even
/// without reaching the network.
///
/// The identity lookup is advisory and network-bound, and runs BEFORE the
/// vault lock is taken (rule 2 — never hold a lock across a network call).
/// Failure to resolve it (offline, timeout, ...) never blocks a legitimate
/// add — it only degrades the duplicate check down to the fingerprint
/// comparison, logged at `warn`. Holds the lock (rule 1) for the
/// read-check-write sequence once that starts.
pub fn add_current_account(alias: Option<&str>) -> Result<u32, SwitchError> {
    add_current_account_with_timeout(
        alias,
        crate::locking::DEFAULT_TIMEOUT,
        &default_identity_resolver,
    )
}

fn add_current_account_with_timeout(
    alias: Option<&str>,
    timeout: Duration,
    resolve_identity: &dyn Fn(&str) -> Option<oauth::TokenAccount>,
) -> Result<u32, SwitchError> {
    // Read the live login and resolve its identity BEFORE taking any lock —
    // rule 2 (never hold a lock across a network call). `resolve_identity`
    // is advisory: a failure here only degrades the duplicate check below,
    // it never blocks the add.
    let mut store = CredentialStore::new(GuiStoreHost);
    let live_creds = store.read_active_credentials().value.unwrap_or_default();
    if live_creds.is_empty() {
        return Err(SwitchError::NoLiveCredential);
    }
    let live_fingerprint = oauth::credential_fingerprint(&live_creds);
    let resolved_account = oauth::extract_access_token(&live_creds)
        .as_deref()
        .and_then(resolve_identity);
    let identity_resolved = resolved_account.is_some();
    let new_identity: ResolvedIdentity = resolved_account
        .map(ResolvedIdentity::from)
        .unwrap_or_default();

    // Only our own vault is written here (the live login is read, never
    // written) — no Claude Code lock needed, see the module-level locking
    // section above.
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().unwrap_or_default();
    ensure_accounts_object(&mut data);

    {
        // Cloned rather than borrowed: `store` needs `&mut self` inside the
        // lookup, and `data` is mutated further down — decoupling here avoids
        // holding an immutable borrow of `data` across that later mutation.
        let accounts_snapshot = data
            .get("accounts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(existing_num) = find_registered_slot_by_identity(
            &mut store,
            &accounts_snapshot,
            &new_identity,
            live_fingerprint.as_deref(),
        ) {
            return Err(SwitchError::AlreadyRegistered(existing_num));
        }
    }
    if !identity_resolved {
        log::warn!(
            "add_current_account: could not resolve account identity via the OAuth profile \
             lookup (offline, or the endpoint failed); duplicate check degraded to \
             credential-fingerprint comparison only"
        );
    }

    let config_text = std::fs::read_to_string(paths::global_config_path())?;
    let config_value: Value = serde_json::from_str(&config_text)?;
    let oauth_account = config_value
        .get("oauthAccount")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let email = oauth_account
        .get("emailAddress")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let organization_uuid = oauth_account
        .get("organizationUuid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let organization_name = oauth_account
        .get("organizationName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let slot = next_free_slot(&data);
    let num = slot.to_string();

    // Install the backup before touching the registry (same ordering
    // discipline as `switch_to`: the credential/config write is the
    // recoverable-on-retry part, so it happens before the registry commits
    // to the new slot existing).
    store.write_account_credentials(&num, &email, &live_creds)?;
    write_account_config(&num, &email, &config_text)?;

    let mut record = Map::new();
    record.insert("email".to_string(), Value::String(email.clone()));
    record.insert(
        "organizationUuid".to_string(),
        Value::String(organization_uuid.clone()),
    );
    record.insert(
        "organizationName".to_string(),
        Value::String(organization_name),
    );
    record.insert(
        "added".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    // Persist the resolved identity so a future add — even one that can't
    // reach the network — has something stable to match this slot on. The
    // absence of `uuid` on a slot is precisely what let the confirmed
    // duplicate through; every newly added slot must carry it when known.
    if let Some(uuid) = new_identity.uuid.clone() {
        record.insert("uuid".to_string(), Value::String(uuid));
    }
    if let Some(a) = alias.map(str::trim).filter(|s| !s.is_empty()) {
        record.insert("alias".to_string(), Value::String(a.to_string()));
    }

    data.get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("ensure_accounts_object guarantees this")
        .insert(num, Value::Object(record));
    add_to_sequence(&mut data, slot);
    // The captured login is what's live right now, so it is also the
    // registry's notion of "active" — mirrors upstream `add_account` setting
    // `activeAccountNumber` to the freshly added slot.
    data.insert("activeAccountNumber".to_string(), Value::from(slot));
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;
    clear_replaced_quarantine(&email, Some(&organization_uuid), &live_creds);

    Ok(slot)
}

/// Reject a token that is obviously not a Claude credential, returning the
/// trimmed token on success.
///
/// Every real Claude API key and OAuth setup-token upstream ever issues
/// starts with `sk-ant-` (`sk-ant-api...` / `sk-ant-oat...`); this is a
/// coarse sanity check, not a full format validator, so it only rejects
/// input that is empty, obviously JSON (a common paste mistake — pasting a
/// whole credentials blob instead of just the token), or missing that
/// prefix/short enough to be garbage.
fn validate_token(token: &str) -> Result<String, SwitchError> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(SwitchError::InvalidToken("token is empty".to_string()));
    }
    if trimmed.starts_with('{') {
        return Err(SwitchError::InvalidToken(
            "expected a raw token, not a JSON credentials blob".to_string(),
        ));
    }
    if !trimmed.starts_with("sk-ant-") {
        return Err(SwitchError::InvalidToken(
            "does not look like a Claude API key or setup token (expected a value starting \
             with \"sk-ant-\")"
                .to_string(),
        ));
    }
    if trimmed.len() < 20 {
        return Err(SwitchError::InvalidToken(
            "token is too short to be valid".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Register a setup-token or managed API key as a new slot.
///
/// Useful without any prior Claude Code login on this machine — e.g. a
/// headless install, or a token copied from another machine. The kind is
/// auto-detected via [`credentials::looks_like_api_key`]: an `sk-ant-api...`
/// value is stored raw and activated on Claude Code's managed-key axis;
/// anything else is treated as an OAuth setup-token and wrapped in the same
/// `claudeAiOauth` JSON shape Claude Code itself uses. The token itself is
/// trusted as given for its *content* — exactly like upstream
/// `add_account_from_token` — but is now also tried, as-is, as a bearer token
/// against [`oauth::fetch_oauth_profile`] purely to resolve duplicate-account
/// identity (see below); this never rejects or alters the token.
///
/// `email` defaults to `setup-token-{slot}@token.local` /
/// `api-key-{slot}@token.local` when omitted, mirroring the CLI's own
/// convention for tokens that carry no real identity metadata. Returns the
/// newly allocated slot number.
///
/// A setup-token for an already-registered account is the same duplicate
/// situation [`add_current_account`] guards against, so the same identity
/// comparison applies here (see [`find_registered_slot_by_identity`]):
/// account `uuid`/`organizationUuid` + email first, credential fingerprint
/// only as a fallback. A raw token has no previously-installed backup to
/// fingerprint against its own bytes, so that fallback only ever catches
/// literally re-pasting the same token twice. Many tokens — API keys
/// especially — resolve no identity at all (the profile endpoint expects an
/// OAuth bearer token); that degrades exactly like an offline
/// [`add_current_account`] does: allow the add, log at `warn`. The resolved
/// identity, when there is one, is persisted onto the new record the same
/// way. Identity resolution runs before the vault lock is taken (rule 2).
pub fn add_token(
    token: &str,
    email: Option<&str>,
    alias: Option<&str>,
) -> Result<u32, SwitchError> {
    add_token_with_timeout(
        token,
        email,
        alias,
        crate::locking::DEFAULT_TIMEOUT,
        &default_identity_resolver,
    )
}

fn add_token_with_timeout(
    token: &str,
    email: Option<&str>,
    alias: Option<&str>,
    timeout: Duration,
    resolve_identity: &dyn Fn(&str) -> Option<oauth::TokenAccount>,
) -> Result<u32, SwitchError> {
    // Validated before the lock is taken: a malformed token should never
    // block on (or take) the cross-process lock at all.
    let trimmed = validate_token(token)?;
    let is_api_key = credentials::looks_like_api_key(Some(&trimmed));

    // Resolve identity BEFORE the lock (rule 2 — never hold a lock across a
    // network call). Advisory: a failure here (or an API key that simply
    // can't authenticate against the profile endpoint) only degrades the
    // duplicate check below to the fingerprint fallback.
    let resolved_account = resolve_identity(&trimmed);
    let identity_resolved = resolved_account.is_some();
    let new_identity: ResolvedIdentity = resolved_account
        .map(ResolvedIdentity::from)
        .unwrap_or_default();

    let credentials_payload = if is_api_key {
        trimmed.clone()
    } else {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": trimmed,
                "scopes": ["user:inference"],
            }
        })
        .to_string()
    };
    let live_fingerprint = oauth::credential_fingerprint(&credentials_payload);

    // Vault-only write (never touches the official files) — vault lock alone
    // suffices, same reasoning as `add_current_account_with_timeout`.
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().unwrap_or_default();
    ensure_accounts_object(&mut data);

    let mut store = CredentialStore::new(GuiStoreHost);
    {
        let accounts_snapshot = data
            .get("accounts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(existing_num) = find_registered_slot_by_identity(
            &mut store,
            &accounts_snapshot,
            &new_identity,
            live_fingerprint.as_deref(),
        ) {
            return Err(SwitchError::AlreadyRegistered(existing_num));
        }
    }
    if !identity_resolved {
        log::warn!(
            "add_token: could not resolve account identity via the OAuth profile lookup \
             (offline, an API key, or the endpoint failed); duplicate check degraded to \
             credential-fingerprint comparison only"
        );
    }

    let slot = next_free_slot(&data);
    let num = slot.to_string();

    let resolved_email = match email.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => e.to_string(),
        None => {
            let label = if is_api_key { "api-key" } else { "setup-token" };
            format!("{label}-{slot}@token.local")
        }
    };

    let config_payload = serde_json::json!({
        "oauthAccount": {
            "emailAddress": resolved_email,
            "accountUuid": "",
            "organizationUuid": Value::Null,
            "organizationName": Value::Null,
        }
    })
    .to_string();

    store.write_account_credentials(&num, &resolved_email, &credentials_payload)?;
    write_account_config(&num, &resolved_email, &config_payload)?;

    let mut record = Map::new();
    record.insert("email".to_string(), Value::String(resolved_email.clone()));
    record.insert(
        "organizationUuid".to_string(),
        Value::String(new_identity.organization_uuid.clone().unwrap_or_default()),
    );
    record.insert("organizationName".to_string(), Value::String(String::new()));
    record.insert(
        "added".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    // Persist the resolved identity — same reasoning as `add_current_account`.
    if let Some(uuid) = new_identity.uuid.clone() {
        record.insert("uuid".to_string(), Value::String(uuid));
    }
    if is_api_key {
        record.insert("kind".to_string(), Value::String("api_key".to_string()));
    }
    if let Some(a) = alias.map(str::trim).filter(|s| !s.is_empty()) {
        record.insert("alias".to_string(), Value::String(a.to_string()));
    }

    data.get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("ensure_accounts_object guarantees this")
        .insert(num, Value::Object(record));
    add_to_sequence(&mut data, slot);
    // Unlike `add_current_account`, this does not become the active account —
    // it is only registered, not activated, mirroring upstream
    // `add_account_from_token` (which never touches `activeAccountNumber`).
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;
    clear_replaced_quarantine(
        &resolved_email,
        new_identity.organization_uuid.as_deref(),
        &credentials_payload,
    );

    Ok(slot)
}

/// Register a captured OAuth credential (from
/// [`crate::login::interactive_login`]) as a new managed slot, WITHOUT
/// activating it.
///
/// Mirrors [`add_current_account`] closely — same identity-based duplicate
/// detection ([`find_registered_slot_by_identity`]), same injectable-resolver
/// pattern so tests never touch the network, same lowest-free-slot
/// allocation via [`next_free_slot`], same "resolve identity before the lock"
/// discipline (rule 2). The input is a credential blob handed in by the
/// caller rather than whatever happens to be live in `~/.claude` — this is
/// what [`add_token`] can't do, since it explicitly refuses anything starting
/// with `{` as a paste mistake. Two differences from [`add_current_account`]
/// matter and are both deliberate:
///
/// 1. **Never touches the live credential or config.**
///    `add_current_account` reads `~/.claude/.credentials.json` and
///    `~/.claude.json` because it is capturing what is already active; the
///    credential handed to this function instead comes from an isolated
///    login flow (a throwaway temp `CLAUDE_CONFIG_DIR` — see `login.rs`)
///    that was never installed as the live login in the first place. There
///    is nothing live to read and nothing live to leave alone, so this
///    function simply never opens either path.
/// 2. **Never sets `activeAccountNumber`.** The user is adding an account,
///    not switching to it. `add_current_account`'s write to
///    `activeAccountNumber` only makes sense because it captures what is
///    already live and therefore already active on this machine; that
///    reasoning does not apply to a credential that was never installed
///    anywhere here.
///
/// `credentials_json` must parse as JSON and carry a non-empty
/// `claudeAiOauth.accessToken`, or this refuses with
/// [`SwitchError::InvalidCredential`] before taking any lock — same
/// "validate before the lock" discipline [`validate_token`] applies for
/// [`add_token`].
///
/// `email` is resolved in priority order: the caller's `email` argument,
/// else the identity resolved via [`oauth::fetch_oauth_profile`], else the
/// same `setup-token-{slot}@token.local` synthetic convention [`add_token`]
/// uses for a token carrying no email of its own (this credential is the
/// same `claudeAiOauth` JSON shape as that path's non-API-key branch). The
/// resolved `uuid` / `organizationUuid`, when known, are persisted onto the
/// new record — same reasoning as [`add_current_account`]'s doc comment:
/// that is what stops this class of duplicate recurring on a future add.
///
/// Never logs or echoes `credentials_json` itself — every error path below
/// carries only a fixed, content-free description, matching `login.rs`'s
/// "never logged" rule for credential bytes.
pub fn add_oauth_credential(
    credentials_json: &str,
    email: Option<&str>,
    alias: Option<&str>,
) -> Result<u32, SwitchError> {
    add_oauth_credential_with_timeout(
        credentials_json,
        email,
        alias,
        crate::locking::DEFAULT_TIMEOUT,
        &default_identity_resolver,
    )
}

/// Replace one existing slot's OAuth credential after an isolated re-login.
///
/// This mirrors cswap's add-existing-account behavior: keep the slot and all
/// registry metadata, replace only its credential generation, and clear the
/// obsolete dead-token verdict after the writes commit. Replacement is stricter
/// than a new add: an unresolved or mismatched identity fails closed because a
/// credential must never be written into a user-selected slot on guesswork.
pub fn replace_oauth_credential(
    account_number: u32,
    credentials_json: &str,
    uuid: Option<&str>,
    email: Option<&str>,
    organization_uuid: Option<&str>,
) -> Result<(), SwitchError> {
    replace_oauth_credential_with_timeout(
        account_number,
        credentials_json,
        uuid,
        email,
        organization_uuid,
        crate::locking::DEFAULT_TIMEOUT,
    )
}

fn replace_oauth_credential_with_timeout(
    account_number: u32,
    credentials_json: &str,
    uuid: Option<&str>,
    email: Option<&str>,
    organization_uuid: Option<&str>,
    timeout: Duration,
) -> Result<(), SwitchError> {
    let trimmed = credentials_json.trim();
    let oauth = oauth::extract_oauth_data(trimmed).ok_or_else(|| {
        SwitchError::InvalidCredential("credential is not valid OAuth JSON".to_string())
    })?;
    let has_token_pair = ["accessToken", "refreshToken"].into_iter().all(|field| {
        oauth
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    });
    if !has_token_pair {
        return Err(SwitchError::InvalidCredential(
            "credential lacks a complete OAuth token pair".to_string(),
        ));
    }

    let replacement_identity = ResolvedIdentity {
        uuid: uuid.filter(|value| !value.is_empty()).map(str::to_string),
        organization_uuid: organization_uuid
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        email: email.filter(|value| !value.is_empty()).map(str::to_string),
    };
    if replacement_identity.uuid.is_none()
        && (replacement_identity.organization_uuid.is_none()
            || replacement_identity.email.is_none())
    {
        return Err(SwitchError::InvalidCredential(
            "the signed-in account identity could not be verified; try again while online"
                .to_string(),
        ));
    }

    // This may update both the vault and Claude Code's active credential, so
    // use the complete canonical lock set even for an inactive target.
    let _locks = crate::switch_transaction::acquire_live_state_locks(timeout)?;
    if crate::switch_transaction::recovery_requirement().is_some() {
        return Err(crate::switch_transaction::TransactionError::RecoveryRequired.into());
    }

    let data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;
    let number = account_number.to_string();
    let record = data
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|accounts| accounts.get(&number))
        .and_then(Value::as_object)
        .ok_or_else(|| SwitchError::UnknownAccount(number.clone()))?;
    let existing_identity = identity_from_record(record);
    if !identity_matches(&replacement_identity, &existing_identity) {
        return Err(SwitchError::InvalidCredential(format!(
            "the signed-in account does not match account {number}"
        )));
    }
    let stored_email = record
        .get("email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| SwitchError::InvalidCredential("stored account has no email".into()))?
        .to_string();
    let stable_key = accounts_from_sequence(&data)
        .into_iter()
        .find(|account| account.number == account_number)
        .ok_or_else(|| SwitchError::UnknownAccount(number.clone()))?
        .stable_key();

    let mut store = CredentialStore::new(GuiStoreHost);
    let previous = store.read_account_credentials(&number, &stored_email);
    store.write_account_credentials(&number, &stored_email, trimmed)?;

    if current_account_number(&data).as_deref() == Some(number.as_str()) {
        if let Err(error) = store.write_refreshed_oauth_credentials(trimmed) {
            if !previous.is_empty() {
                if let Err(restore_error) =
                    store.write_account_credentials(&number, &stored_email, &previous)
                {
                    return Err(SwitchError::Credential(CredentialError::Write(format!(
                        "active re-login failed and the previous slot backup could not be restored: {restore_error}"
                    ))));
                }
            }
            return Err(error.into());
        }
    }

    let fingerprint = oauth::credential_fingerprint(trimmed)
        .unwrap_or_else(|| oauth_refresh::credential_generation(trimmed));
    if let Err(error) =
        OAuthQuarantine::new(paths::backup_root()).clear_obsolete(&stable_key, &fingerprint)
    {
        log::warn!("could not clear obsolete OAuth quarantine after re-login: {error}");
    }
    Ok(())
}

fn add_oauth_credential_with_timeout(
    credentials_json: &str,
    email: Option<&str>,
    alias: Option<&str>,
    timeout: Duration,
    resolve_identity: &dyn Fn(&str) -> Option<oauth::TokenAccount>,
) -> Result<u32, SwitchError> {
    // Validate before anything else — a malformed blob must never block on
    // (or take) the vault lock, and must never appear verbatim in the error
    // it produces.
    let trimmed = credentials_json.trim();
    if trimmed.is_empty() {
        return Err(SwitchError::InvalidCredential(
            "credential is empty".to_string(),
        ));
    }
    if serde_json::from_str::<Value>(trimmed).is_err() {
        return Err(SwitchError::InvalidCredential(
            "credential is not valid JSON".to_string(),
        ));
    }
    let access_token = oauth::extract_access_token(trimmed)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            SwitchError::InvalidCredential(
                "credential is missing a non-empty claudeAiOauth.accessToken".to_string(),
            )
        })?;

    // Resolve identity BEFORE any lock is taken — rule 2 (never hold a lock
    // across a network call), same as `add_current_account`/`add_token`.
    // Advisory: a failure here only degrades the duplicate check below, it
    // never blocks the add.
    let live_fingerprint = oauth::credential_fingerprint(trimmed);
    let resolved_account = resolve_identity(&access_token);
    let identity_resolved = resolved_account.is_some();
    let new_identity: ResolvedIdentity = resolved_account
        .map(ResolvedIdentity::from)
        .unwrap_or_default();

    // Vault-only write (the live credential/config are never touched here) —
    // vault lock alone suffices, same reasoning as
    // `add_current_account_with_timeout`.
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().unwrap_or_default();
    ensure_accounts_object(&mut data);

    let mut store = CredentialStore::new(GuiStoreHost);
    {
        let accounts_snapshot = data
            .get("accounts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(existing_num) = find_registered_slot_by_identity(
            &mut store,
            &accounts_snapshot,
            &new_identity,
            live_fingerprint.as_deref(),
        ) {
            return Err(SwitchError::AlreadyRegistered(existing_num));
        }
    }
    if !identity_resolved {
        log::warn!(
            "add_oauth_credential: could not resolve account identity via the OAuth profile \
             lookup (offline, or the endpoint failed); duplicate check degraded to \
             credential-fingerprint comparison only"
        );
    }

    let slot = next_free_slot(&data);
    let num = slot.to_string();

    let resolved_email = match email.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => e.to_string(),
        None => match new_identity.email.clone() {
            Some(e) => e,
            None => format!("setup-token-{slot}@token.local"),
        },
    };

    // Install the backup before touching the registry — same ordering
    // discipline as `add_current_account`: the recoverable-on-retry part
    // happens before the registry commits to the new slot existing.
    store.write_account_credentials(&num, &resolved_email, trimmed)?;

    let config_payload = serde_json::json!({
        "oauthAccount": {
            "emailAddress": resolved_email,
            "accountUuid": new_identity.uuid.clone().unwrap_or_default(),
            "organizationUuid": new_identity
                .organization_uuid
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
            "organizationName": Value::Null,
        }
    })
    .to_string();
    write_account_config(&num, &resolved_email, &config_payload)?;

    let mut record = Map::new();
    record.insert("email".to_string(), Value::String(resolved_email.clone()));
    record.insert(
        "organizationUuid".to_string(),
        Value::String(new_identity.organization_uuid.clone().unwrap_or_default()),
    );
    record.insert("organizationName".to_string(), Value::String(String::new()));
    record.insert(
        "added".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    // Persist the resolved identity — same reasoning as `add_current_account`:
    // the absence of `uuid` on a slot is exactly what let the confirmed
    // duplicate through.
    if let Some(uuid) = new_identity.uuid.clone() {
        record.insert("uuid".to_string(), Value::String(uuid));
    }
    if let Some(a) = alias.map(str::trim).filter(|s| !s.is_empty()) {
        record.insert("alias".to_string(), Value::String(a.to_string()));
    }

    data.get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("ensure_accounts_object guarantees this")
        .insert(num, Value::Object(record));
    add_to_sequence(&mut data, slot);
    // Deliberately NOT setting `activeAccountNumber` — see the doc comment
    // above: this registers the account, it does not switch to it.
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;
    clear_replaced_quarantine(
        &resolved_email,
        new_identity.organization_uuid.as_deref(),
        trimmed,
    );

    Ok(slot)
}

/// Hold an account out of (`enabled = false`) or return it to
/// (`enabled = true`) automatic rotation.
///
/// A disabled slot stays managed and remains a valid explicit switch target —
/// only auto-switch and the usage-aware strategies skip it (this mirrors
/// `Account::is_switchable` on the read side, which already excludes
/// [`crate::model::UsageStatus::Disabled`]).
///
/// Refuses via [`SwitchError::CannotDisableActive`] to disable the account
/// that is *currently* live: doing so would leave auto-switch with no valid
/// home to land on next, since the active account keeps running until the
/// user explicitly switches away from it regardless of this flag.
pub fn set_account_enabled(number: u32, enabled: bool) -> Result<(), SwitchError> {
    set_account_enabled_with_timeout(number, enabled, crate::locking::DEFAULT_TIMEOUT)
}

fn set_account_enabled_with_timeout(
    number: u32,
    enabled: bool,
    timeout: Duration,
) -> Result<(), SwitchError> {
    // `disabled` lives only in our own sequence.json — vault-only, same
    // reasoning as `add_current_account_with_timeout`.
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;
    let num = number.to_string();

    let exists = data
        .get("accounts")
        .and_then(Value::as_object)
        .map(|accounts| accounts.contains_key(&num))
        .unwrap_or(false);
    if !exists {
        return Err(SwitchError::UnknownAccount(num));
    }

    if !enabled && current_account_number(&data).as_deref() == Some(num.as_str()) {
        return Err(SwitchError::CannotDisableActive(num));
    }

    if let Some(record) = data
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .and_then(|accounts| accounts.get_mut(&num))
        .and_then(Value::as_object_mut)
    {
        if enabled {
            record.remove("disabled");
        } else {
            record.insert("disabled".to_string(), Value::Bool(true));
        }
    }
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Target selection.
// ---------------------------------------------------------------------------

/// Target-selection strategy, mirroring `cswap switch --strategy`'s `best` /
/// `next-available` and the auto-switch engine's `consume-first`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// `best`: the switchable account with the most remaining headroom.
    MostHeadroom,
    /// `next-available`: the first switchable account (in `accounts` order)
    /// that isn't provably at its limit.
    NextAvailable,
    /// `consume-first`: among switchable accounts with real headroom, the one
    /// whose weekly (7-day) window resets soonest — spend down the
    /// soonest-to-refill quota first.
    ConsumeFirst,
}

/// Pick a switch target from `accounts` under `strategy`. Pure — no I/O, no
/// network. `accounts` is expected in rotation order (as returned by
/// [`read_accounts`] / [`read_snapshot`]); [`Strategy::NextAvailable`] and
/// [`Strategy::ConsumeFirst`] use that order to break ties.
///
/// Unlike upstream's `best` / `next-available`, this takes no "current
/// account" baseline (the function signature has none to give it) — so
/// `MostHeadroom` doesn't require beating a specific current account, only
/// that some switchable candidate provably has real headroom, and
/// `NextAvailable` scans from the front of `accounts` rather than from just
/// after wherever "current" is. `Account::is_automatic_target` excludes the
/// active account, disabled/dead slots, stale or unavailable measurements,
/// unknown headroom, and exhausted accounts. Manual switching retains a
/// separate, more permissive validation path.
pub fn pick_target(accounts: &[Account], strategy: Strategy) -> Option<&Account> {
    match strategy {
        Strategy::MostHeadroom => pick_most_headroom(accounts),
        Strategy::NextAvailable => pick_next_available(accounts),
        Strategy::ConsumeFirst => pick_consume_first(accounts),
    }
}

/// `best`: the known-headroom switchable account with the most headroom.
/// Unknown-usage candidates are never chosen — there's nothing to compare —
/// but they are not "skipped" in the sense of being excluded from
/// eligibility; if every switchable candidate is unknown, this returns `None`
/// (no candidate can be *proven* better) rather than guessing. Also `None`
/// when the best known headroom is `<= 0` (every switchable account is at its
/// limit — switching would not help).
fn pick_most_headroom(accounts: &[Account]) -> Option<&Account> {
    let mut best: Option<(&Account, f64)> = None;
    for account in accounts {
        if !account.is_automatic_target() {
            continue;
        }
        if let Some(headroom) = account.headroom() {
            match best {
                Some((_, best_headroom)) if best_headroom >= headroom => {}
                _ => best = Some((account, headroom)),
            }
        }
    }
    let (account, headroom) = best?;
    if headroom <= 0.0 {
        return None;
    }
    Some(account)
}

/// `next-available`: the first switchable account, in `accounts` order, that
/// isn't provably exhausted. An unknown headroom is *not* skipped — mirroring
/// upstream's `if headroom is not None and headroom <= 0: skip` — only a
/// known `<= 0` headroom is. `None` when every switchable account is known to
/// be exhausted, or there are no switchable accounts at all.
fn pick_next_available(accounts: &[Account]) -> Option<&Account> {
    for account in accounts {
        if !account.is_automatic_target() {
            continue;
        }
        return Some(account);
    }
    None
}

/// `consume-first`: among switchable accounts with *known, positive*
/// headroom, the one whose 7-day window resets soonest (ties: more headroom,
/// then `accounts` order). Unlike `next-available`, an unknown headroom is
/// skipped here — there is nothing to rank it by, and upstream's own
/// `_rank_candidates` does the same (`if h is None: continue`). `None` when
/// no switchable account has known, positive headroom.
fn pick_consume_first(accounts: &[Account]) -> Option<&Account> {
    let mut candidates: Vec<(f64, f64, &Account)> = Vec::new();
    for account in accounts {
        if !account.is_automatic_target() {
            continue;
        }
        let Some(headroom) = account.headroom() else {
            continue;
        };
        if headroom <= 0.0 {
            continue;
        }
        let reset_ts = seven_day_reset_ts(account).unwrap_or(f64::INFINITY);
        candidates.push((reset_ts, -headroom, account));
    }
    // Stable sort: ties (equal reset_ts and headroom) preserve `accounts`
    // order, matching upstream's "list order (sequence order) breaks ties".
    candidates.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    candidates.into_iter().next().map(|(_, _, account)| account)
}

/// Epoch seconds of `account`'s 7-day window reset, or `None` if unknown or
/// already past (a stale, already-elapsed `resets_at` must never sort as
/// "soonest" — that would rank the just-rolled-over account, the *least*
/// perishable quota of all, first).
fn seven_day_reset_ts(account: &Account) -> Option<f64> {
    let resets_at = account
        .usage
        .as_ref()?
        .seven_day
        .as_ref()?
        .resets_at
        .as_deref()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(resets_at).ok()?;
    let ts = parsed.timestamp() as f64;
    let now = chrono::Utc::now().timestamp() as f64;
    if ts > now {
        Some(ts)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{OAuthFuture, OAuthNetwork, RefreshError, RefreshOutcome, UsageFetchError};
    use crate::oauth_refresh::{Clock, LeaseGuard, RefreshLeaseProvider};
    use crate::test_support::{env_lock, EnvGuard, StoreRootGuard};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct ActivationNetwork {
        successor: String,
        refresh_calls: AtomicUsize,
    }

    struct ActiveUsageNetwork {
        refreshes: Mutex<VecDeque<RefreshOutcome>>,
        usages: Mutex<VecDeque<Result<Value, UsageFetchError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl OAuthNetwork for ActiveUsageNetwork {
        fn refresh<'a>(&'a self, credentials: &'a str) -> OAuthFuture<'a, RefreshOutcome> {
            let refresh = oauth::extract_oauth_data(credentials)
                .and_then(|data| {
                    data.get("refreshToken")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("refresh:{refresh}"));
                self.refreshes.lock().unwrap().pop_front().unwrap()
            })
        }

        fn fetch_usage<'a>(
            &'a self,
            access_token: &'a str,
        ) -> OAuthFuture<'a, Result<Value, UsageFetchError>> {
            let access_token = access_token.to_string();
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("usage:{access_token}"));
                self.usages.lock().unwrap().pop_front().unwrap()
            })
        }
    }

    impl OAuthNetwork for ActivationNetwork {
        fn refresh<'a>(&'a self, _: &'a str) -> OAuthFuture<'a, RefreshOutcome> {
            Box::pin(async move {
                self.refresh_calls.fetch_add(1, Ordering::SeqCst);
                RefreshOutcome {
                    credentials: Some(self.successor.clone()),
                    error: None,
                    token_account: None,
                }
            })
        }

        fn fetch_usage<'a>(
            &'a self,
            _: &'a str,
        ) -> OAuthFuture<'a, Result<Value, UsageFetchError>> {
            Box::pin(async { panic!("activation validation must not fetch usage") })
        }
    }

    struct ImmediateLease;
    struct ImmediateLeaseGuard;
    impl RefreshLeaseProvider for ImmediateLease {
        fn acquire<'a>(
            &'a self,
            _: &'a str,
        ) -> OAuthFuture<'a, Result<Box<dyn LeaseGuard>, String>> {
            Box::pin(async { Ok(Box::new(ImmediateLeaseGuard) as Box<dyn LeaseGuard>) })
        }
    }

    struct ActivationClock(f64);
    impl Clock for ActivationClock {
        fn now_ms(&self) -> f64 {
            self.0
        }
    }

    // -- pick_target ----------------------------------------------------------

    fn switchable_account(number: u32, pct: Option<f64>) -> Account {
        let usage = pct.map(|p| Usage {
            five_hour: None,
            seven_day: Some(UsageWindow {
                pct: p,
                ..Default::default()
            }),
            scoped: None,
        });
        Account {
            number,
            email: format!("acct{number}@example.com"),
            active: false,
            usage_status: if pct.is_some() {
                UsageStatus::Ok
            } else {
                UsageStatus::Unknown
            },
            usage,
            ..Default::default()
        }
    }

    fn active_account(number: u32) -> Account {
        Account {
            number,
            email: format!("acct{number}@example.com"),
            active: true,
            usage_status: UsageStatus::Ok,
            ..Default::default()
        }
    }

    #[test]
    fn active_usage_provenance_rejects_a_conflicting_uuid() {
        let account = Account {
            uuid: Some("slot-uuid".into()),
            organization_uuid: Some("org-1".into()),
            ..active_account(1)
        };
        let resolved = oauth::TokenAccount {
            uuid: "foreign-uuid".into(),
            email: Some(account.email.clone()),
            organization_uuid: Some("org-1".into()),
        };

        assert_eq!(
            active_usage_provenance(&account, &resolved),
            ProvenanceVerdict::Foreign
        );
    }

    #[test]
    fn active_usage_provenance_accepts_uuid_with_a_partial_profile() {
        let account = Account {
            uuid: Some("slot-uuid".into()),
            organization_uuid: Some("org-1".into()),
            ..active_account(1)
        };
        let resolved = oauth::TokenAccount {
            uuid: "slot-uuid".into(),
            email: None,
            organization_uuid: None,
        };

        assert_eq!(
            active_usage_provenance(&account, &resolved),
            ProvenanceVerdict::Owned
        );
    }

    #[test]
    fn active_usage_provenance_is_unresolved_without_uuid_or_complete_identity() {
        let account = active_account(1);
        let resolved = oauth::TokenAccount {
            uuid: "resolved-uuid".into(),
            email: Some(account.email.clone()),
            organization_uuid: None,
        };

        assert_eq!(
            active_usage_provenance(&account, &resolved),
            ProvenanceVerdict::Unresolved
        );
    }

    fn disabled_account(number: u32, pct: f64) -> Account {
        let mut a = switchable_account(number, Some(pct));
        a.usage_status = UsageStatus::Disabled;
        a
    }

    #[test]
    fn most_headroom_picks_the_highest_known_headroom() {
        let accounts = vec![
            switchable_account(1, Some(80.0)), // headroom 20
            switchable_account(2, Some(30.0)), // headroom 70
            switchable_account(3, Some(50.0)), // headroom 50
        ];
        assert_eq!(
            pick_target(&accounts, Strategy::MostHeadroom)
                .unwrap()
                .number,
            2
        );
    }

    #[test]
    fn most_headroom_never_targets_the_active_account() {
        let accounts = vec![active_account(1), switchable_account(2, Some(10.0))];
        assert_eq!(
            pick_target(&accounts, Strategy::MostHeadroom)
                .unwrap()
                .number,
            2
        );
    }

    #[test]
    fn most_headroom_ignores_unknown_usage_when_a_known_candidate_exists() {
        let accounts = vec![
            switchable_account(1, None), // unknown usage — not the winner, not excluded either
            switchable_account(2, Some(40.0)), // headroom 60
        ];
        assert_eq!(
            pick_target(&accounts, Strategy::MostHeadroom)
                .unwrap()
                .number,
            2
        );
    }

    #[test]
    fn most_headroom_returns_none_when_every_switchable_candidate_is_unknown() {
        let accounts = vec![switchable_account(1, None), switchable_account(2, None)];
        assert!(pick_target(&accounts, Strategy::MostHeadroom).is_none());
    }

    #[test]
    fn most_headroom_returns_none_when_all_switchable_accounts_are_exhausted() {
        let accounts = vec![
            switchable_account(1, Some(100.0)),
            switchable_account(2, Some(100.0)),
        ];
        assert!(pick_target(&accounts, Strategy::MostHeadroom).is_none());
    }

    #[test]
    fn next_available_requires_fresh_known_positive_headroom() {
        let accounts = vec![
            switchable_account(1, Some(100.0)), // known-exhausted: skip
            switchable_account(2, None),        // unknown: untrusted for automation
            switchable_account(3, Some(10.0)),
        ];
        assert_eq!(
            pick_target(&accounts, Strategy::NextAvailable)
                .unwrap()
                .number,
            3
        );
    }

    #[test]
    fn every_strategy_excludes_non_ok_automatic_targets() {
        for strategy in [
            Strategy::MostHeadroom,
            Strategy::NextAvailable,
            Strategy::ConsumeFirst,
        ] {
            for status in [
                UsageStatus::Stale,
                UsageStatus::Unknown,
                UsageStatus::Unavailable,
                UsageStatus::ForeignCredential,
                UsageStatus::Error,
                UsageStatus::ReloginRequired,
                UsageStatus::Disabled,
            ] {
                let mut untrusted = switchable_account(1, Some(0.0));
                untrusted.usage_status = status;
                let healthy = switchable_account(2, Some(20.0));
                assert_eq!(
                    pick_target(&[untrusted, healthy], strategy).unwrap().number,
                    2,
                    "status {status:?} must not be selected by {strategy:?}"
                );
            }
        }
    }

    #[test]
    fn next_available_returns_none_when_every_switchable_account_is_known_exhausted() {
        let accounts = vec![
            switchable_account(1, Some(100.0)),
            switchable_account(2, Some(100.0)),
        ];
        assert!(pick_target(&accounts, Strategy::NextAvailable).is_none());
    }

    #[test]
    fn next_available_never_targets_active_or_disabled_accounts() {
        let accounts = vec![
            active_account(1),
            disabled_account(2, 0.0),
            switchable_account(3, Some(0.0)),
        ];
        assert_eq!(
            pick_target(&accounts, Strategy::NextAvailable)
                .unwrap()
                .number,
            3
        );
    }

    #[test]
    fn consume_first_prefers_the_soonest_weekly_reset() {
        let soon = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let later = (chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339();

        let mut a = switchable_account(1, Some(50.0));
        a.usage
            .as_mut()
            .unwrap()
            .seven_day
            .as_mut()
            .unwrap()
            .resets_at = Some(later);
        let mut b = switchable_account(2, Some(50.0));
        b.usage
            .as_mut()
            .unwrap()
            .seven_day
            .as_mut()
            .unwrap()
            .resets_at = Some(soon);

        let accounts = vec![a, b];
        assert_eq!(
            pick_target(&accounts, Strategy::ConsumeFirst)
                .unwrap()
                .number,
            2
        );
    }

    #[test]
    fn consume_first_skips_unknown_usage_accounts() {
        let accounts = vec![
            switchable_account(1, None),
            switchable_account(2, Some(20.0)),
        ];
        assert_eq!(
            pick_target(&accounts, Strategy::ConsumeFirst)
                .unwrap()
                .number,
            2
        );
    }

    #[test]
    fn consume_first_returns_none_when_all_switchable_accounts_are_exhausted() {
        let accounts = vec![
            switchable_account(1, Some(100.0)),
            switchable_account(2, Some(100.0)),
        ];
        assert!(pick_target(&accounts, Strategy::ConsumeFirst).is_none());
    }

    // -- switch_to --------------------------------------------------------------
    //
    // These exercise real filesystem state under a temp HOME/CLAUDE_CONFIG_DIR,
    // the same isolation pattern `paths.rs` and `credentials.rs` already use.
    // Env vars are process-global, so every test here is serialized on
    // `crate::test_support::ENV_LOCK`.

    // `_lock` is declared LAST: struct fields drop in declaration order, and
    // this must be the last thing released — after every env var this guard
    // protects has been restored — or another thread could start mutating
    // HOME/CLAUDE_CONFIG_DIR while this scope's `EnvGuard`s are still being
    // torn down.
    struct TestEnv {
        _home: EnvGuard,
        _userprofile: EnvGuard,
        _config: EnvGuard,
        _xdg: EnvGuard,
        _wsl_distro: EnvGuard,
        _store_root: StoreRootGuard,
        _home_dir: TempDir,
        _config_dir: TempDir,
        _store_root_dir: TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    /// Redirect every path this module touches (OUR vault via
    /// `paths::backup_root`, `global_config_path`, `credentials_path`,
    /// `claude_config_home`) into fresh temp directories, isolated from the
    /// real machine.
    ///
    /// `XDG_DATA_HOME` and `WSL_DISTRO_NAME` are pinned too: both can steer
    /// path resolution away from the temp `HOME` on Linux, and CI runners set
    /// them.
    ///
    /// Serialized on `crate::test_support::ENV_LOCK`, the single crate-wide
    /// lock shared with `paths.rs` and `credentials.rs`, so this module's
    /// env-touching tests cannot race a different module's under the default
    /// parallel `cargo test` runner.
    fn setup_env() -> TestEnv {
        let lock = env_lock();
        let home_dir = TempDir::new().unwrap();
        let config_dir = TempDir::new().unwrap();
        let store_root_dir = TempDir::new().unwrap();
        // HOME (unix) and USERPROFILE (windows) both drive `paths::home_dir`;
        // setting both is harmless on every platform.
        let home_guard = EnvGuard::set("HOME", home_dir.path().to_str().unwrap());
        let userprofile_guard = EnvGuard::set("USERPROFILE", home_dir.path().to_str().unwrap());
        let config_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", config_dir.path().to_str().unwrap());
        let xdg_guard = EnvGuard::unset("XDG_DATA_HOME");
        let wsl_guard = EnvGuard::unset("WSL_DISTRO_NAME");
        // `paths::backup_root()` (OUR vault) is redirected via the test-only
        // override rather than an env var — see `StoreRootGuard`'s doc for
        // why it can't reuse the production `set_store_root` OnceLock.
        let store_root_guard = StoreRootGuard::set(store_root_dir.path().to_path_buf());
        TestEnv {
            _home: home_guard,
            _userprofile: userprofile_guard,
            _config: config_guard,
            _xdg: xdg_guard,
            _wsl_distro: wsl_guard,
            _store_root: store_root_guard,
            _home_dir: home_dir,
            _config_dir: config_dir,
            _store_root_dir: store_root_dir,
            _lock: lock,
        }
    }

    fn write_json_file(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    /// Seed a two-account registry: Account-1 (alpha) is live and active,
    /// Account-2 (bravo) has a valid stored backup and is the switch target.
    fn seed_two_accounts() {
        let seq = serde_json::json!({
            "activeAccountNumber": 1,
            "lastUpdated": "2026-01-01T00:00:00Z",
            "sequence": [1, 2],
            "accounts": {
                "1": {"email": "alpha@example.com", "organizationUuid": "org-1", "organizationName": "Alpha"},
                "2": {"email": "bravo@example.com", "organizationUuid": "org-2", "organizationName": "Bravo"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("2", "bravo@example.com", "target-creds-2")
            .unwrap();
        write_json_file(
            &account_config_path("2", "bravo@example.com"),
            &serde_json::json!({"oauthAccount": {"emailAddress": "bravo@example.com", "organizationUuid": "org-2"}}),
        );

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "alpha@example.com", "organizationUuid": "org-1"},
                "someLocalSetting": true
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        std::fs::write(
            paths::credentials_path(),
            "original-active-creds-for-account-1",
        )
        .unwrap();
    }

    fn bravo_target() -> Account {
        Account {
            number: 2,
            email: "bravo@example.com".to_string(),
            ..Default::default()
        }
    }

    fn bravo_identity() -> AccountIdentity {
        let account = read_accounts()
            .unwrap()
            .into_iter()
            .find(|account| account.number == 2)
            .unwrap();
        AccountIdentity {
            number: "2".to_string(),
            email: account.email.clone(),
            stable_key: account.stable_key(),
        }
    }

    #[test]
    fn generation_store_compare_and_store_never_overwrites_a_newer_winner() {
        let _env = setup_env();
        seed_two_accounts();
        let store = GuiGenerationStore::new(Duration::from_secs(5));
        let identity = bravo_identity();
        let first = store.read(&identity).unwrap().unwrap();

        let persisted = store
            .compare_and_store(&identity, &first.generation, "successor-creds")
            .unwrap();
        assert!(matches!(persisted, CompareAndStore::Persisted(_)));

        let stale = store
            .compare_and_store(&identity, &first.generation, "stale-callback-creds")
            .unwrap();
        let CompareAndStore::Superseded(winner) = stale else {
            panic!("stale callback must observe the winner");
        };
        assert_eq!(winner.credentials, "successor-creds");
        assert_eq!(
            store.read(&identity).unwrap().unwrap().credentials,
            "successor-creds"
        );
    }

    #[test]
    fn generation_store_quarantines_only_the_exact_current_generation() {
        let _env = setup_env();
        seed_two_accounts();
        let store = GuiGenerationStore::new(Duration::from_secs(5));
        let identity = bravo_identity();
        let first = store.read(&identity).unwrap().unwrap();

        assert!(store
            .reject_if_current(&identity, &first.generation, &first.credentials)
            .unwrap());
        assert!(store.is_rejected(&identity, &first.credentials).unwrap());
        assert!(!store
            .reject_if_current(&identity, "sha256-full:stale", &first.credentials)
            .unwrap());
    }

    #[test]
    fn switch_to_installs_the_target_and_backs_up_the_outgoing_account() {
        let _env = setup_env();
        seed_two_accounts();

        switch_to_with_timeout(&bravo_target(), Duration::from_secs(5)).unwrap();

        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "target-creds-2"
        );
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(paths::global_config_path()).unwrap())
                .unwrap();
        assert_eq!(cfg["oauthAccount"]["emailAddress"], "bravo@example.com");
        assert_eq!(
            cfg["someLocalSetting"], true,
            "untouched keys must survive the splice"
        );

        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "original-active-creds-for-account-1"
        );

        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["activeAccountNumber"], 2);
    }

    #[test]
    fn target_validation_failure_is_a_strict_no_op_before_outgoing_backup() {
        let _env = setup_env();
        seed_two_accounts();
        std::fs::remove_file(account_config_path("2", "bravo@example.com")).unwrap();

        let err = switch_to_with_timeout(&bravo_target(), Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, SwitchError::NoStoredConfig(_)));

        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "",
            "an invalid target must abort before mutating the outgoing backup"
        );

        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "original-active-creds-for-account-1",
            "the live login must never be touched when the switch fails"
        );
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["activeAccountNumber"], 1);
    }

    #[test]
    fn generation_change_after_freshening_aborts_before_outgoing_backup() {
        let _env = setup_env();
        seed_two_accounts();
        let identity = bravo_identity();
        let mut store = CredentialStore::new(GuiStoreHost);
        let stale_credentials = store.read_account_credentials(&identity.number, &identity.email);
        let validated = ValidatedCredential {
            identity,
            generation: oauth_refresh::credential_generation(&stale_credentials),
            credentials: stale_credentials,
        };
        store
            .write_account_credentials("2", "bravo@example.com", "newer-winner")
            .unwrap();

        let error = switch_to_validated_with_timeout(
            &bravo_target(),
            &validated,
            &LiveProvenance::default(),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(error, SwitchError::TargetGenerationChanged(ref n) if n == "2"));
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "",
            "a stale validation must never back up or otherwise mutate the outgoing slot"
        );
        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "original-active-creds-for-account-1"
        );
    }

    #[test]
    fn proven_foreign_live_credential_is_never_routed_into_the_configured_slot() {
        let _env = setup_env();
        seed_two_accounts();
        let mut data = read_sequence_data().unwrap();
        data.get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|accounts| accounts.get_mut("2"))
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("uuid".to_string(), Value::String("uuid-bravo".to_string()));
        let mut store = CredentialStore::new(GuiStoreHost);
        let live = store.read_active_credentials().value.unwrap();
        let provenance = LiveProvenance {
            live: live.clone(),
            resolved: Some(oauth::TokenAccount {
                uuid: "uuid-bravo".to_string(),
                email: Some("bravo@example.com".to_string()),
                organization_uuid: Some("org-2".to_string()),
            }),
        };

        assert!(matches!(
            classify_outgoing_destination(
                &mut store,
                &mut data,
                "1",
                "alpha@example.com",
                &live,
                &provenance,
            ),
            crate::switch_transaction::OutgoingDestination::Unclaimed
        ));
    }

    #[test]
    fn switch_stashes_proven_foreign_live_bytes_without_poisoning_any_slot() {
        let _env = setup_env();
        seed_two_accounts();
        let mut data = read_sequence_data().unwrap();
        data.get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|accounts| accounts.get_mut("2"))
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("uuid".to_string(), Value::String("uuid-bravo".to_string()));
        write_sequence_data(&data).unwrap();
        let identity = bravo_identity();
        let mut store = CredentialStore::new(GuiStoreHost);
        let target = store.read_account_credentials("2", "bravo@example.com");
        let validated = ValidatedCredential {
            identity,
            generation: oauth_refresh::credential_generation(&target),
            credentials: target,
        };
        let live = store.read_active_credentials().value.unwrap();
        let provenance = LiveProvenance {
            live: live.clone(),
            resolved: Some(oauth::TokenAccount {
                uuid: "uuid-bravo".to_string(),
                email: Some("bravo@example.com".to_string()),
                organization_uuid: Some("org-2".to_string()),
            }),
        };

        switch_to_validated_with_timeout(
            &bravo_target(),
            &validated,
            &provenance,
            Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "",
            "foreign live bytes must never overwrite the configured outgoing slot"
        );
        assert_eq!(store.list_unclaimed_credentials().len(), 1);
        assert!(!account_config_path("1", "alpha@example.com").exists());
        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "target-creds-2"
        );
    }

    #[test]
    fn unresolved_or_moved_live_credential_keeps_cswaps_fail_open_backup_rule() {
        let _env = setup_env();
        seed_two_accounts();
        let mut data = read_sequence_data().unwrap();
        let mut store = CredentialStore::new(GuiStoreHost);
        let live = store.read_active_credentials().value.unwrap();
        for provenance in [
            LiveProvenance::default(),
            LiveProvenance {
                live: "older-prefetch-generation".to_string(),
                resolved: Some(oauth::TokenAccount {
                    uuid: "foreign".to_string(),
                    email: Some("foreign@example.com".to_string()),
                    organization_uuid: Some("foreign-org".to_string()),
                }),
            },
        ] {
            assert!(matches!(
                classify_outgoing_destination(
                    &mut store,
                    &mut data,
                    "1",
                    "alpha@example.com",
                    &live,
                    &provenance,
                ),
                crate::switch_transaction::OutgoingDestination::Managed { .. }
            ));
        }
    }

    #[test]
    fn recycled_email_with_conflicting_uuid_is_treated_as_alien_not_own() {
        let _env = setup_env();
        seed_two_accounts();
        let mut data = read_sequence_data().unwrap();
        data.get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|accounts| accounts.get_mut("1"))
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert(
                "uuid".to_string(),
                Value::String("uuid-original".to_string()),
            );
        let mut store = CredentialStore::new(GuiStoreHost);
        let live = store.read_active_credentials().value.unwrap();
        let provenance = LiveProvenance {
            live: live.clone(),
            resolved: Some(oauth::TokenAccount {
                uuid: "uuid-recycled".to_string(),
                email: Some("alpha@example.com".to_string()),
                organization_uuid: Some("org-1".to_string()),
            }),
        };
        assert!(matches!(
            classify_outgoing_destination(
                &mut store,
                &mut data,
                "1",
                "alpha@example.com",
                &live,
                &provenance,
            ),
            crate::switch_transaction::OutgoingDestination::Unclaimed
        ));
    }

    #[test]
    fn wiped_live_tokens_are_not_allowed_to_replace_a_slot_backup() {
        let _env = setup_env();
        seed_two_accounts();
        let wiped = serde_json::json!({"claudeAiOauth": {
            "accessToken": "",
            "refreshToken": "",
            "expiresAt": 1
        }})
        .to_string();
        let mut data = read_sequence_data().unwrap();
        let mut store = CredentialStore::new(GuiStoreHost);
        assert!(matches!(
            classify_outgoing_destination(
                &mut store,
                &mut data,
                "1",
                "alpha@example.com",
                &wiped,
                &LiveProvenance::default(),
            ),
            crate::switch_transaction::OutgoingDestination::Unclaimed
        ));
    }

    #[tokio::test]
    async fn switch_to_refreshes_an_expired_target_before_installing_it() {
        let _env = setup_env();
        seed_two_accounts();
        let expired = oauth_creds_json("old-refresh", "old-access");
        let mut expired_value: Value = serde_json::from_str(&expired).unwrap();
        expired_value["claudeAiOauth"]["expiresAt"] = Value::from(1);
        let expired = expired_value.to_string();
        let successor = serde_json::json!({"claudeAiOauth": {
            "accessToken": "fresh-access",
            "refreshToken": "fresh-refresh",
            "expiresAt": 9_999_999_999_999_f64
        }})
        .to_string();
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("2", "bravo@example.com", &expired)
            .unwrap();
        let network = Arc::new(ActivationNetwork {
            successor: successor.clone(),
            refresh_calls: AtomicUsize::new(0),
        });
        let coordinator = RefreshCoordinator::new(
            network.clone(),
            Arc::new(GuiGenerationStore::new(Duration::from_secs(5))),
            Arc::new(ImmediateLease),
            Arc::new(ActivationClock(10_000.0)),
        );

        switch_to_with_coordinator(&bravo_target(), &coordinator, Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(network.refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.read_account_credentials("2", "bravo@example.com"),
            successor,
            "the consumed refresh generation must be persisted before activation"
        );
        assert!(std::fs::read_to_string(paths::credentials_path())
            .unwrap()
            .contains("fresh-access"));
    }

    #[test]
    fn switch_fails_cleanly_rather_than_half_applying_when_the_vault_lock_cannot_be_acquired() {
        let _env = setup_env();
        seed_two_accounts();
        // The pre-network target-identity read uses only our vault lock and
        // therefore fails before the complete mutation lock set is entered.

        let _held =
            crate::locking::acquire_or_err(vault_lock_path(), Duration::from_secs(5)).unwrap();

        let err = switch_to_with_timeout(&bravo_target(), Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, SwitchError::Locking(_)));

        // Nothing was touched: the lock guards the whole mutation, so a
        // failed acquire must be a strict no-op on every file.
        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "original-active-creds-for-account-1"
        );
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["activeAccountNumber"], 1);
        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(store.read_account_credentials("1", "alpha@example.com"), "");
    }

    // -- lock ordering: Claude Code's locks, then ours, always ------------------

    #[test]
    fn switch_to_is_blocked_by_an_externally_held_claude_credential_lock() {
        let _env = setup_env();
        seed_two_accounts();

        // Claude Code's credential lock is acquired FIRST, so failing to get
        // it must short-circuit before any effect on OUR vault or the live
        // login — proving the ordering, not just that both locks exist.
        std::fs::create_dir_all(paths::oauth_refresh_lock_dir()).unwrap();

        let err = switch_to_with_timeout(&bravo_target(), Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, SwitchError::LiveStateLock(_)));

        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "original-active-creds-for-account-1",
            "the live login must never be touched when Claude's lock can't be acquired"
        );
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["activeAccountNumber"], 1);
        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "",
            "our vault must be untouched — the vault lock is acquired last"
        );
    }

    // -- read_accounts ----------------------------------------------------------

    #[test]
    fn read_accounts_marks_the_live_login_as_active() {
        let _env = setup_env();
        seed_two_accounts();

        let accounts = read_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        let alpha = accounts.iter().find(|a| a.number == 1).unwrap();
        let bravo = accounts.iter().find(|a| a.number == 2).unwrap();
        assert!(alpha.active);
        assert!(!bravo.active);
        assert_eq!(alpha.organization_uuid.as_deref(), Some("org-1"));
        assert_eq!(alpha.is_organization, Some(true));
    }

    #[test]
    fn read_accounts_is_empty_when_no_registry_exists() {
        let _env = setup_env();
        assert!(read_accounts().unwrap().is_empty());
    }

    // -- next_free_slot -----------------------------------------------------
    // Pure function, no filesystem/env involved.

    #[test]
    fn next_free_slot_is_one_when_no_accounts_exist() {
        let data = Map::new();
        assert_eq!(next_free_slot(&data), 1);
    }

    #[test]
    fn next_free_slot_reuses_a_freed_slot_instead_of_only_ever_growing() {
        let data = serde_json::json!({
            "accounts": {"1": {}, "3": {}}
        })
        .as_object()
        .unwrap()
        .clone();
        // Slot 2 was freed (never allocated between 1 and 3) and must be
        // reused rather than skipped straight to 4.
        assert_eq!(next_free_slot(&data), 2);
    }

    #[test]
    fn next_free_slot_grows_past_the_highest_slot_when_none_are_free() {
        let data = serde_json::json!({
            "accounts": {"1": {}, "2": {}}
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(next_free_slot(&data), 3);
    }

    // -- add_current_account --------------------------------------------------
    //
    // Every test below injects a resolver instead of calling the public
    // `add_current_account`/`add_token` (which default to
    // `default_identity_resolver`, a REAL network call): this module must
    // never make a network call in a test. `no_identity` stands in for a
    // resolver that couldn't determine anything (offline, or — for the
    // pre-existing tests below that predate identity resolution — simply
    // "no resolver was wired up yet"), which is also what makes those
    // pre-existing tests keep exercising exactly the fingerprint-only
    // behavior they always did.

    fn oauth_creds_json(refresh_token: &str, access_token: &str) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "scopes": ["user:inference"],
            }
        })
        .to_string()
    }

    fn expiring_oauth_creds_json(
        refresh_token: &str,
        access_token: &str,
        expires_at: f64,
    ) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "expiresAt": expires_at,
                "scopes": ["user:inference"],
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn active_expired_owned_lineage_refreshes_and_persists_both_stores() {
        let _env = setup_env();
        let original = expiring_oauth_creds_json("old-refresh", "old-access", 1.0);
        let successor =
            expiring_oauth_creds_json("new-refresh", "new-access", 9_999_999_999_999_f64);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({
                "sequence": [1],
                "accounts": {"1": {"email": "alpha@example.com", "organizationUuid": "org-1"}}
            }),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount": {"emailAddress": "alpha@example.com", "organizationUuid": "org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_refreshed_oauth_credentials(&original).unwrap();
        store
            .write_account_credentials("1", "alpha@example.com", &original)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(successor.clone()),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::from([Ok(
                serde_json::json!({"five_hour":{"utilization":17.0}}),
            )])),
            calls: Mutex::new(Vec::new()),
        };

        let usage = fetch_active_usage_with_network(
            &account,
            &original,
            &original,
            &network,
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(usage.five_hour.unwrap().pct, 17.0);
        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(store.read_active_credentials().value.unwrap(), successor);
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            successor
        );
        assert_eq!(
            *network.calls.lock().unwrap(),
            vec!["refresh:old-refresh", "usage:new-access"]
        );
        assert_eq!(
            cached_active_usage_provenance(&account, &successor),
            Some(ProvenanceVerdict::Owned)
        );
    }

    #[tokio::test]
    async fn active_wiped_live_credentials_restore_a_usable_slot_backup() {
        let _env = setup_env();
        let backup =
            expiring_oauth_creds_json("backup-refresh", "backup-access", 9_999_999_999_999_f64);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("1", "alpha@example.com", &backup)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::from([Ok(
                serde_json::json!({"five_hour":{"utilization":13.0}}),
            )])),
            calls: Mutex::new(Vec::new()),
        };

        let usage = fetch_active_usage_with_network(
            &account,
            "",
            &backup,
            &network,
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(usage.five_hour.unwrap().pct, 13.0);
        assert_eq!(*network.calls.lock().unwrap(), vec!["usage:backup-access"]);
        assert_eq!(
            CredentialStore::new(GuiStoreHost)
                .read_active_credentials()
                .value
                .unwrap(),
            backup
        );
    }

    #[tokio::test]
    async fn active_non_oauth_live_value_is_never_replaced_from_an_oauth_backup() {
        let _env = setup_env();
        let account = active_account(1);
        let backup =
            expiring_oauth_creds_json("backup-refresh", "backup-access", 9_999_999_999_999_f64);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };

        assert_eq!(
            fetch_active_usage_with_network(
                &account,
                "sk-ant-api03-managed",
                &backup,
                &network,
                Duration::from_secs(1),
            )
            .await,
            Err(ActiveUsageError::Missing)
        );
        assert!(network.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_refresh_never_overwrites_a_managed_key_that_landed_while_waiting() {
        let _env = setup_env();
        let observed = expiring_oauth_creds_json("old-refresh", "old-access", 1.0);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("1", "alpha@example.com", &observed)
            .unwrap();
        std::fs::create_dir_all(paths::credentials_path().parent().unwrap()).unwrap();
        std::fs::write(paths::credentials_path(), "sk-ant-api03-concurrent").unwrap();
        let account = read_accounts().unwrap().remove(0);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };

        assert_eq!(
            fetch_active_usage_with_network(
                &account,
                &observed,
                &observed,
                &network,
                Duration::from_secs(1),
            )
            .await,
            Err(ActiveUsageError::Unavailable)
        );
        assert!(network.calls.lock().unwrap().is_empty());
        assert_eq!(
            CredentialStore::new(GuiStoreHost)
                .read_active_credentials()
                .value,
            Some("sk-ant-api03-concurrent".into())
        );
    }

    #[tokio::test]
    async fn active_wipe_never_restores_a_quarantined_backup_generation() {
        let _env = setup_env();
        let backup =
            expiring_oauth_creds_json("dead-refresh", "backup-access", 9_999_999_999_999_f64);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("1", "alpha@example.com", &backup)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        OAuthQuarantine::new(paths::backup_root())
            .reject(
                &account.stable_key(),
                &oauth::credential_fingerprint(&backup).unwrap(),
                chrono::Utc::now(),
            )
            .unwrap();
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };

        assert_eq!(
            fetch_active_usage_with_network(
                &account,
                "",
                &backup,
                &network,
                Duration::from_secs(1),
            )
            .await,
            Err(ActiveUsageError::ReloginRequired)
        );
        assert!(network.calls.lock().unwrap().is_empty());
        assert_eq!(
            CredentialStore::new(GuiStoreHost)
                .read_active_credentials()
                .value,
            Some(String::new())
        );
    }

    #[tokio::test]
    async fn active_refresh_waits_for_the_same_per_account_lease_as_inactive_refresh() {
        let _env = setup_env();
        let original = expiring_oauth_creds_json("old-refresh", "old-access", 1.0);
        let successor =
            expiring_oauth_creds_json("new-refresh", "new-access", 9_999_999_999_999_f64);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_refreshed_oauth_credentials(&original).unwrap();
        store
            .write_account_credentials("1", "alpha@example.com", &original)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        let leases =
            oauth_refresh::FileRefreshLeases::new(paths::backup_root(), Duration::from_secs(1));
        let held = leases.acquire(&account.stable_key()).await.unwrap();
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(successor),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::from([Ok(
                serde_json::json!({"five_hour":{"utilization":29.0}}),
            )])),
            calls: Mutex::new(Vec::new()),
        };
        let refresh = fetch_active_usage_with_network(
            &account,
            &original,
            &original,
            &network,
            Duration::from_secs(1),
        );
        tokio::pin!(refresh);

        tokio::select! {
            result = &mut refresh => panic!("active refresh bypassed the held account lease: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert!(network.calls.lock().unwrap().is_empty());

        let vault_path = paths::backup_root().join(".lock");
        let vault_guard = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || {
                crate::locking::acquire_or_err(vault_path, Duration::from_millis(250))
            }),
        )
        .await
        .expect("active refresh held the vault while waiting for the account lease")
        .unwrap()
        .expect("inactive persistence could not acquire the vault behind the active waiter");
        drop(vault_guard);

        drop(held);
        assert_eq!(refresh.await.unwrap().five_hour.unwrap().pct, 29.0);
        assert_eq!(
            *network.calls.lock().unwrap(),
            vec!["refresh:old-refresh", "usage:new-access"]
        );
    }

    #[tokio::test]
    async fn active_usage_401_forces_refresh_of_locally_fresh_owned_generation() {
        let _env = setup_env();
        let original =
            expiring_oauth_creds_json("old-refresh", "old-access", 9_999_999_999_999_f64);
        let successor =
            expiring_oauth_creds_json("new-refresh", "new-access", 9_999_999_999_999_f64);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_refreshed_oauth_credentials(&original).unwrap();
        store
            .write_account_credentials("1", "alpha@example.com", &original)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(successor),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::from([
                Err(UsageFetchError::Http {
                    status: 401,
                    retry_after_s: None,
                }),
                Ok(serde_json::json!({"five_hour":{"utilization":19.0}})),
            ])),
            calls: Mutex::new(Vec::new()),
        };

        let usage = fetch_active_usage_with_network(
            &account,
            &original,
            &original,
            &network,
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(usage.five_hour.unwrap().pct, 19.0);
        assert_eq!(
            *network.calls.lock().unwrap(),
            vec![
                "usage:old-access",
                "refresh:old-refresh",
                "usage:new-access"
            ]
        );
    }

    #[tokio::test]
    async fn active_invalid_grant_quarantines_only_the_current_owned_generation() {
        let _env = setup_env();
        let original = expiring_oauth_creds_json("dead-refresh", "old-access", 1.0);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_refreshed_oauth_credentials(&original).unwrap();
        store
            .write_account_credentials("1", "alpha@example.com", &original)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::InvalidGrant),
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };

        let result = fetch_active_usage_with_network(
            &account,
            &original,
            &original,
            &network,
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(result, Err(ActiveUsageError::ReloginRequired));
        let fingerprint = oauth::credential_fingerprint(&original).unwrap();
        assert!(OAuthQuarantine::new(paths::backup_root())
            .is_rejected(&account.stable_key(), &fingerprint));
        assert_eq!(*network.calls.lock().unwrap(), vec!["refresh:dead-refresh"]);

        let no_retry_network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            fetch_active_usage_with_network(
                &account,
                &original,
                &original,
                &no_retry_network,
                Duration::from_secs(1),
            )
            .await,
            Err(ActiveUsageError::ReloginRequired)
        );
        assert!(no_retry_network.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_refresh_never_consumes_a_known_foreign_lineage() {
        let _env = setup_env();
        let foreign = expiring_oauth_creds_json("foreign-refresh", "foreign-access", 1.0);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let account = read_accounts().unwrap().remove(0);
        let key = active_provenance_cache_key(&account, &foreign).unwrap();
        active_provenance_cache()
            .lock()
            .unwrap()
            .insert(key, ProvenanceVerdict::Foreign);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };

        let result = fetch_active_usage_with_network(
            &account,
            &foreign,
            "",
            &network,
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(result, Err(ActiveUsageError::ForeignCredential));
        assert!(network.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_refresh_rechecks_config_identity_before_consuming_a_grant() {
        let _env = setup_env();
        let original = expiring_oauth_creds_json("owned-refresh", "owned-access", 1.0);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"someone-else@example.com","organizationUuid":"org-2"}}),
        );
        let account = Account {
            number: 1,
            email: "alpha@example.com".into(),
            organization_uuid: Some("org-1".into()),
            active: true,
            ..Default::default()
        };
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_refreshed_oauth_credentials(&original).unwrap();
        store
            .write_account_credentials("1", "alpha@example.com", &original)
            .unwrap();
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        };

        let result = fetch_active_usage_with_network(
            &account,
            &original,
            &original,
            &network,
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(result, Err(ActiveUsageError::Unavailable));
        assert!(network.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_refresh_adopts_a_concurrent_fresh_generation_without_a_post() {
        let _env = setup_env();
        let observed = expiring_oauth_creds_json("same-refresh", "old-access", 1.0);
        let concurrent =
            expiring_oauth_creds_json("same-refresh", "fresh-access", 9_999_999_999_999_f64);
        write_json_file(
            &accounts_file(),
            &serde_json::json!({"sequence":[1],"accounts":{"1":{"email":"alpha@example.com","organizationUuid":"org-1"}}}),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount":{"emailAddress":"alpha@example.com","organizationUuid":"org-1"}}),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_refreshed_oauth_credentials(&concurrent)
            .unwrap();
        store
            .write_account_credentials("1", "alpha@example.com", &observed)
            .unwrap();
        let account = read_accounts().unwrap().remove(0);
        let network = ActiveUsageNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::from([Ok(
                serde_json::json!({"five_hour":{"utilization":23.0}}),
            )])),
            calls: Mutex::new(Vec::new()),
        };

        let usage = fetch_active_usage_with_network(
            &account,
            &observed,
            &observed,
            &network,
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(usage.five_hour.unwrap().pct, 23.0);
        assert_eq!(*network.calls.lock().unwrap(), vec!["usage:fresh-access"]);
        assert_eq!(
            CredentialStore::new(GuiStoreHost).read_account_credentials("1", "alpha@example.com"),
            concurrent
        );
    }

    fn no_identity(_access_token: &str) -> Option<oauth::TokenAccount> {
        None
    }

    #[test]
    fn add_current_account_captures_the_live_login_into_a_new_slot() {
        let _env = setup_env();
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {
                    "emailAddress": "fresh@example.com",
                    "organizationUuid": "org-fresh",
                    "organizationName": "Fresh Org"
                }
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        let live = oauth_creds_json("refresh-fresh", "access-fresh");
        std::fs::write(paths::credentials_path(), &live).unwrap();

        let slot = add_current_account_with_timeout(
            Some("  work  "),
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(slot, 1);

        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["activeAccountNumber"], 1);
        assert_eq!(seq["accounts"]["1"]["email"], "fresh@example.com");
        assert_eq!(seq["accounts"]["1"]["organizationUuid"], "org-fresh");
        // Alias must be trimmed before storage.
        assert_eq!(seq["accounts"]["1"]["alias"], "work");
        // Identity was unresolved (`no_identity`), so nothing fabricated.
        assert!(seq["accounts"]["1"].get("uuid").is_none());

        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "fresh@example.com"),
            live
        );
    }

    #[test]
    fn add_current_account_reuses_a_freed_slot() {
        let _env = setup_env();
        // Seed a registry where slot 1 is occupied but slot 2 is free (never
        // allocated), so the newly captured login should land in slot 2, not 3.
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {"email": "alpha@example.com", "organizationUuid": "org-1"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "second@example.com", "organizationUuid": "org-2"}
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        std::fs::write(
            paths::credentials_path(),
            oauth_creds_json("refresh-2", "access-2"),
        )
        .unwrap();

        let slot =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &no_identity)
                .unwrap();
        assert_eq!(slot, 2);
    }

    #[test]
    fn add_current_account_refuses_when_no_live_credential() {
        let _env = setup_env();
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({"oauthAccount": {"emailAddress": "nobody@example.com"}}),
        );
        // Deliberately no .credentials.json written: nothing is live.

        let err =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &no_identity)
                .unwrap_err();
        assert!(matches!(err, SwitchError::NoLiveCredential));
    }

    #[test]
    fn add_current_account_refuses_a_duplicate_registration_by_credential_identity() {
        let _env = setup_env();
        // Account 1 is already registered, with a stored backup carrying the
        // same refresh-token lineage as what is about to be captured — even
        // though the access token differs (simulating a token that has
        // since rotated), the fingerprint must still recognise it. Identity
        // resolution is offline here (`no_identity`), so this exercises the
        // fingerprint fallback exactly as before this fix.
        let seq = serde_json::json!({
            "activeAccountNumber": 1,
            "sequence": [1],
            "accounts": {
                "1": {"email": "alpha@example.com", "organizationUuid": "org-1"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials(
                "1",
                "alpha@example.com",
                &oauth_creds_json("shared-refresh", "old-access"),
            )
            .unwrap();

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "alpha@example.com", "organizationUuid": "org-1"}
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        std::fs::write(
            paths::credentials_path(),
            oauth_creds_json("shared-refresh", "new-rotated-access"),
        )
        .unwrap();

        let err =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &no_identity)
                .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));

        // Must be a strict no-op: no second slot created.
        let seq_after: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq_after["accounts"].as_object().unwrap().len(), 1);
    }

    // -- add_current_account: identity-based duplicate detection (the fix) ----
    //
    // These reproduce the confirmed real bug and lock in the fix's exact
    // priority order: account `uuid` first, then `organizationUuid` + email,
    // then the credential fingerprint only as a last resort — plus the
    // "advisory, never block on a network failure" degrade path.

    #[test]
    fn add_current_account_refuses_duplicate_when_refresh_token_rotated_but_uuid_resolves() {
        // THE regression test: a live login whose refresh token has rotated
        // server-side (`oauth::try_refresh_oauth_credentials` overwrites
        // `refreshToken` whenever the server issues a new one) fingerprints
        // completely differently from anything on record — under the old,
        // fingerprint-only check this exact case slipped through and created
        // a real duplicate slot on a real machine. It must now be refused
        // because the account's `uuid` is known and matches on both sides,
        // regardless of what the credential bytes look like.
        let _env = setup_env();
        let seq = serde_json::json!({
            "activeAccountNumber": 1,
            "sequence": [1],
            "accounts": {
                "1": {
                    "email": "charlie@example.com",
                    "organizationUuid": "7f94011f-org",
                    "uuid": "acct-uuid-stable"
                }
            }
        });
        write_json_file(&accounts_file(), &seq);

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "charlie@example.com", "organizationUuid": "7f94011f-org"}
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        // A brand-new refresh token: the fingerprint of this credential is
        // provably different from whatever slot 1 was originally registered
        // with (no stored backup for slot 1 even exists in this test — the
        // fix must not need one to catch this).
        std::fs::write(
            paths::credentials_path(),
            oauth_creds_json("rotated-refresh-token", "rotated-access-token"),
        )
        .unwrap();

        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "acct-uuid-stable".to_string(),
                email: Some("charlie@example.com".to_string()),
                organization_uuid: Some("7f94011f-org".to_string()),
            })
        };

        let err =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &resolver)
                .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));

        let seq_after: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(
            seq_after["accounts"].as_object().unwrap().len(),
            1,
            "must be a strict no-op: no second slot created"
        );
    }

    #[test]
    fn add_current_account_refuses_duplicate_matched_by_org_and_email_when_existing_slot_has_no_uuid(
    ) {
        // A record written before this fix carries
        // `organizationUuid` + email but no `uuid` at all — this is exactly
        // the gap the confirmed bug exploited ("slot 1 carries a `uuid`
        // field, slot 2 has none"). The duplicate check must still catch it
        // via the `organizationUuid` + email fallback.
        let _env = setup_env();
        let seq = serde_json::json!({
            "activeAccountNumber": 1,
            "sequence": [1],
            "accounts": {
                "1": {"email": "Alpha@Example.com", "organizationUuid": "org-1"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "alpha@example.com", "organizationUuid": "org-1"}
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        std::fs::write(
            paths::credentials_path(),
            oauth_creds_json("fresh-refresh", "fresh-access"),
        )
        .unwrap();

        // Resolves a brand-new uuid — the existing side has none to compare
        // against, so this can't match on `uuid` — but the SAME org + email,
        // differing only in case/whitespace, which must still be recognised.
        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "brand-new-uuid".to_string(),
                email: Some("  alpha@example.com  ".to_string()),
                organization_uuid: Some("org-1".to_string()),
            })
        };

        let err =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &resolver)
                .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));
    }

    #[test]
    fn add_current_account_allows_the_add_when_identity_is_unresolved_and_fingerprint_does_not_match(
    ) {
        // Simulates a total network outage during identity resolution: the
        // resolver always returns `None`, exactly like `fetch_oauth_profile`
        // degrading on a failure. With no identity to compare and no
        // fingerprint match against the one registered account, this must be
        // treated as a legitimate new registration — refusing it would be the
        // worse failure (blocking a real login because the network hiccuped).
        let _env = setup_env();
        let seq = serde_json::json!({
            "activeAccountNumber": 1,
            "sequence": [1],
            "accounts": {
                "1": {"email": "someone-else@example.com", "organizationUuid": "org-other"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "new-person@example.com", "organizationUuid": "org-new"}
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        std::fs::write(
            paths::credentials_path(),
            oauth_creds_json("new-refresh", "new-access"),
        )
        .unwrap();

        let slot =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &no_identity)
                .unwrap();
        assert_eq!(
            slot, 2,
            "the add must proceed rather than being blocked by a degraded check"
        );
    }

    #[test]
    fn add_current_account_accepts_a_genuinely_different_account() {
        // A fully resolved identity that simply doesn't match anything on
        // record must not be refused — and the newly resolved `uuid` must be
        // persisted onto the new slot for next time.
        let _env = setup_env();
        let seq = serde_json::json!({
            "activeAccountNumber": 1,
            "sequence": [1],
            "accounts": {
                "1": {"email": "alpha@example.com", "organizationUuid": "org-1", "uuid": "uuid-alpha"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": "beta@example.com", "organizationUuid": "org-2"}
            }),
        );
        std::fs::create_dir_all(paths::claude_config_home()).unwrap();
        std::fs::write(
            paths::credentials_path(),
            oauth_creds_json("beta-refresh", "beta-access"),
        )
        .unwrap();

        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "uuid-beta".to_string(),
                email: Some("beta@example.com".to_string()),
                organization_uuid: Some("org-2".to_string()),
            })
        };

        let slot =
            add_current_account_with_timeout(None, crate::locking::DEFAULT_TIMEOUT, &resolver)
                .unwrap();
        assert_eq!(slot, 2);

        let seq_after: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq_after["accounts"]["2"]["uuid"], "uuid-beta");
    }

    // -- add_token ------------------------------------------------------------

    #[test]
    fn add_token_rejects_empty_json_and_unrecognised_input() {
        let _env = setup_env();
        assert!(matches!(
            add_token("", None, None).unwrap_err(),
            SwitchError::InvalidToken(_)
        ));
        assert!(matches!(
            add_token("   ", None, None).unwrap_err(),
            SwitchError::InvalidToken(_)
        ));
        assert!(matches!(
            add_token(r#"{"claudeAiOauth": {}}"#, None, None).unwrap_err(),
            SwitchError::InvalidToken(_)
        ));
        assert!(matches!(
            add_token("not-a-real-token", None, None).unwrap_err(),
            SwitchError::InvalidToken(_)
        ));
        assert!(matches!(
            add_token("sk-ant-short", None, None).unwrap_err(),
            SwitchError::InvalidToken(_)
        ));
    }

    #[test]
    fn add_token_detects_api_key_vs_setup_token_and_applies_default_emails() {
        let _env = setup_env();

        let api_slot = add_token_with_timeout(
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(api_slot, 1);
        let setup_slot = add_token_with_timeout(
            "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz",
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(setup_slot, 2);

        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["1"]["email"], "api-key-1@token.local");
        assert_eq!(seq["accounts"]["1"]["kind"], "api_key");
        assert_eq!(seq["accounts"]["2"]["email"], "setup-token-2@token.local");
        assert!(seq["accounts"]["2"].get("kind").is_none());

        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "api-key-1@token.local"),
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz"
        );
        let setup_creds = store.read_account_credentials("2", "setup-token-2@token.local");
        let parsed: Value = serde_json::from_str(&setup_creds).unwrap();
        assert_eq!(
            parsed["claudeAiOauth"]["accessToken"],
            "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz"
        );

        // add_token never activates the token — it only registers it.
        assert!(seq.get("activeAccountNumber").is_none());
    }

    #[test]
    fn add_token_honours_an_explicit_email_over_the_default_convention() {
        let _env = setup_env();
        let slot = add_token_with_timeout(
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            Some("custom@example.com"),
            Some("ci-key"),
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(slot, 1);

        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["1"]["email"], "custom@example.com");
        assert_eq!(seq["accounts"]["1"]["alias"], "ci-key");
    }

    #[test]
    fn add_token_refuses_a_duplicate_registration_by_resolved_identity() {
        // A setup-token for an already-registered account is the same
        // duplicate situation as `add_current_account` — the same identity
        // comparison must apply.
        let _env = setup_env();
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {"email": "org-owner@example.com", "organizationUuid": "org-1", "uuid": "acct-uuid-1"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "acct-uuid-1".to_string(),
                email: Some("org-owner@example.com".to_string()),
                organization_uuid: Some("org-1".to_string()),
            })
        };

        let err = add_token_with_timeout(
            "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz",
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &resolver,
        )
        .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));

        let seq_after: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq_after["accounts"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn add_token_allows_the_add_when_identity_cannot_be_resolved() {
        // Tokens routinely resolve no identity at all (an API key isn't an
        // OAuth bearer token the profile endpoint understands) — that must
        // degrade to "allow the add", exactly like the offline
        // `add_current_account` path.
        let _env = setup_env();
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {"email": "org-owner@example.com", "organizationUuid": "org-1", "uuid": "acct-uuid-1"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let slot = add_token_with_timeout(
            "sk-ant-api03-zzzzzzzzzzzzzzzzzzzzzz",
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(slot, 2);
    }

    // -- add_oauth_credential ---------------------------------------------------
    //
    // Same "inject a resolver, never touch the network" discipline as the
    // `add_current_account`/`add_token` tests above.

    #[test]
    fn add_oauth_credential_registers_a_slot_without_activating_or_touching_the_live_login() {
        let _env = setup_env();
        let creds = oauth_creds_json("refresh-1", "access-1");

        let slot = add_oauth_credential_with_timeout(
            &creds,
            Some("captured@example.com"),
            Some("  work  "),
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(slot, 1);

        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["1"]["email"], "captured@example.com");
        // Alias must be trimmed before storage, same as `add_current_account`.
        assert_eq!(seq["accounts"]["1"]["alias"], "work");
        assert!(
            seq.get("activeAccountNumber").is_none(),
            "adding an oauth credential must not activate it — the user is adding, not switching"
        );

        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "captured@example.com"),
            creds
        );
        assert!(
            read_account_config("1", "captured@example.com").is_some(),
            "a config backup must exist so a future switch_to has something to install"
        );

        // Must never touch the live credential/config — this credential was
        // never installed as the live login anywhere on this machine.
        assert!(!paths::credentials_path().exists());
        assert!(!paths::global_config_path().exists());
    }

    #[test]
    fn successful_relogin_clears_only_the_replaced_generation_quarantine() {
        let _env = setup_env();
        let email = "recovered@example.com";
        let old = oauth_creds_json("dead-refresh", "dead-access");
        let replacement = oauth_creds_json("fresh-refresh", "fresh-access");
        let stable_key = Account {
            email: email.to_string(),
            ..Account::default()
        }
        .stable_key();
        let old_fingerprint = oauth::credential_fingerprint(&old).unwrap();
        let quarantine = OAuthQuarantine::new(paths::backup_root());
        quarantine
            .reject(&stable_key, &old_fingerprint, chrono::Utc::now())
            .unwrap();

        add_oauth_credential_with_timeout(
            &replacement,
            Some(email),
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();

        assert!(
            !quarantine.is_rejected(&stable_key, &old_fingerprint),
            "a committed replacement must release the prior dead-token lineage"
        );
    }

    #[test]
    fn relogin_replaces_the_selected_active_slot_and_preserves_its_registry_record() {
        let _env = setup_env();
        let email = "recovered@example.com";
        let old = oauth_creds_json("dead-refresh", "dead-access");
        let replacement = oauth_creds_json("fresh-refresh", "fresh-access");
        write_json_file(
            &accounts_file(),
            &serde_json::json!({
                "sequence": [1],
                "activeAccountNumber": 1,
                "accounts": {
                    "1": {
                        "email": email,
                        "organizationUuid": "org-1",
                        "uuid": "uuid-1",
                        "alias": "Work",
                        "added": "2026-01-01T00:00:00Z"
                    }
                }
            }),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": email, "organizationUuid": "org-1"}
            }),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_account_credentials("1", email, &old).unwrap();
        store.write_refreshed_oauth_credentials(&old).unwrap();
        let account = read_accounts().unwrap().remove(0);
        let quarantine = OAuthQuarantine::new(paths::backup_root());
        let old_fingerprint = oauth::credential_fingerprint(&old).unwrap();
        quarantine
            .reject(&account.stable_key(), &old_fingerprint, chrono::Utc::now())
            .unwrap();

        replace_oauth_credential_with_timeout(
            1,
            &replacement,
            Some("uuid-1"),
            Some(email),
            Some("org-1"),
            Duration::from_secs(1),
        )
        .unwrap();

        let registry: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(registry["accounts"]["1"]["alias"], "Work");
        assert_eq!(registry["accounts"]["1"]["added"], "2026-01-01T00:00:00Z");
        assert_eq!(registry["activeAccountNumber"], 1);
        assert_eq!(store.read_account_credentials("1", email), replacement);
        assert_eq!(
            store.read_active_credentials().value.as_deref(),
            Some(replacement.as_str())
        );
        assert!(!quarantine.is_rejected(&account.stable_key(), &old_fingerprint));
    }

    #[test]
    fn relogin_replaces_an_inactive_slot_without_touching_the_live_credential() {
        let _env = setup_env();
        let active_email = "active@example.com";
        let inactive_email = "inactive@example.com";
        let live = oauth_creds_json("live-refresh", "live-access");
        let old = oauth_creds_json("old-refresh", "old-access");
        let replacement = oauth_creds_json("fresh-refresh", "fresh-access");
        write_json_file(
            &accounts_file(),
            &serde_json::json!({
                "sequence": [1, 2],
                "activeAccountNumber": 1,
                "accounts": {
                    "1": {"email": active_email, "organizationUuid": "org-1", "uuid": "uuid-1"},
                    "2": {"email": inactive_email, "organizationUuid": "org-2", "uuid": "uuid-2", "alias": "Spare"}
                }
            }),
        );
        write_json_file(
            &paths::global_config_path(),
            &serde_json::json!({
                "oauthAccount": {"emailAddress": active_email, "organizationUuid": "org-1"}
            }),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("2", inactive_email, &old)
            .unwrap();
        store.write_refreshed_oauth_credentials(&live).unwrap();

        replace_oauth_credential_with_timeout(
            2,
            &replacement,
            Some("uuid-2"),
            Some(inactive_email),
            Some("org-2"),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(
            store.read_account_credentials("2", inactive_email),
            replacement
        );
        assert_eq!(
            store.read_active_credentials().value.as_deref(),
            Some(live.as_str())
        );
        let registry: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(registry["accounts"]["2"]["alias"], "Spare");
        assert_eq!(registry["activeAccountNumber"], 1);
    }

    #[test]
    fn relogin_refuses_a_different_identity_without_changing_credentials() {
        let _env = setup_env();
        let email = "owner@example.com";
        let old = oauth_creds_json("old-refresh", "old-access");
        let replacement = oauth_creds_json("other-refresh", "other-access");
        write_json_file(
            &accounts_file(),
            &serde_json::json!({
                "sequence": [1],
                "accounts": {
                    "1": {"email": email, "organizationUuid": "org-1", "uuid": "uuid-1"}
                }
            }),
        );
        let mut store = CredentialStore::new(GuiStoreHost);
        store.write_account_credentials("1", email, &old).unwrap();

        let error = replace_oauth_credential_with_timeout(
            1,
            &replacement,
            Some("uuid-2"),
            Some("other@example.com"),
            Some("org-2"),
            Duration::from_secs(1),
        )
        .unwrap_err();

        assert!(matches!(error, SwitchError::InvalidCredential(_)));
        assert_eq!(store.read_account_credentials("1", email), old);
    }

    #[test]
    fn registry_mutations_refuse_pending_switch_recovery() {
        let _env = setup_env();
        write_json_file(
            &accounts_file(),
            &serde_json::json!({
                "sequence": [1],
                "accounts": {"1": {"email": "owner@example.com"}}
            }),
        );
        crate::switch_transaction::set_recovery_requirement(Some("repair required".into()));

        let error = set_account_enabled_with_timeout(1, false, Duration::from_secs(1)).unwrap_err();

        crate::switch_transaction::set_recovery_requirement(None);
        assert!(matches!(
            error,
            SwitchError::Transaction(crate::switch_transaction::TransactionError::RecoveryRequired)
        ));
    }

    #[test]
    fn add_oauth_credential_rejects_malformed_or_empty_blobs_as_a_strict_no_op() {
        let _env = setup_env();

        for bad in [
            "",
            "   ",
            "not json",
            r#"{"claudeAiOauth": {}}"#,
            r#"{"claudeAiOauth": {"accessToken": ""}}"#,
            r#"{"somethingElse": true}"#,
        ] {
            let err = add_oauth_credential_with_timeout(
                bad,
                None,
                None,
                crate::locking::DEFAULT_TIMEOUT,
                &no_identity,
            )
            .unwrap_err();
            assert!(
                matches!(err, SwitchError::InvalidCredential(_)),
                "input {bad:?} got {err:?}"
            );
        }

        assert!(
            read_accounts().unwrap().is_empty(),
            "no slot may be created by a rejected blob"
        );
    }

    #[test]
    fn add_oauth_credential_refuses_a_duplicate_registration_by_resolved_identity() {
        let _env = setup_env();
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {"email": "org-owner@example.com", "organizationUuid": "org-1", "uuid": "acct-uuid-1"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "acct-uuid-1".to_string(),
                email: Some("org-owner@example.com".to_string()),
                organization_uuid: Some("org-1".to_string()),
            })
        };

        let creds = oauth_creds_json("dup-refresh", "dup-access");
        let err = add_oauth_credential_with_timeout(
            &creds,
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &resolver,
        )
        .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));

        // Strict no-op: no second slot created, nothing activated.
        let seq_after: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq_after["accounts"].as_object().unwrap().len(), 1);
        assert!(seq_after.get("activeAccountNumber").is_none());
    }

    #[test]
    fn add_oauth_credential_refuses_a_second_interactive_sign_in_to_the_same_account() {
        // The real-world shape of the interactive-login path, and the case the
        // fingerprint check provably CANNOT catch.
        //
        // Signing in again always mints a brand-new credential: a different
        // access token AND a different refresh token from whatever is already
        // stored for that account. So the fingerprints differ by construction,
        // and only the resolved account identity can tell that this is the
        // same account. This is the same failure that created a duplicate slot
        // on a real machine, reached through a different door.
        let _env = setup_env();
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {
                    "email": "charlie@example.com",
                    "organizationUuid": "7f94011f-org",
                    "uuid": "acct-uuid-stable"
                }
            }
        });
        write_json_file(&accounts_file(), &seq);

        // Slot 1 has a stored credential on disk, so the fingerprint path has
        // something real to compare against — and must still not be what saves
        // us here.
        let stored = oauth_creds_json("original-refresh", "original-access");
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("1", "charlie@example.com", &stored)
            .unwrap();
        assert_ne!(
            oauth::credential_fingerprint(&stored),
            oauth::credential_fingerprint(&oauth_creds_json("fresh-refresh", "fresh-access")),
            "precondition: a fresh sign-in must fingerprint differently, or this \
             test would pass for the wrong reason"
        );

        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "acct-uuid-stable".to_string(),
                email: Some("charlie@example.com".to_string()),
                organization_uuid: Some("7f94011f-org".to_string()),
            })
        };

        let err = add_oauth_credential_with_timeout(
            &oauth_creds_json("fresh-refresh", "fresh-access"),
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &resolver,
        )
        .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));

        let seq_after: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(
            seq_after["accounts"].as_object().unwrap().len(),
            1,
            "strict no-op: no second slot created"
        );
        // And the original stored credential must be untouched — a refused add
        // must not have overwritten the account it refused to duplicate.
        assert_eq!(
            store.read_account_credentials("1", "charlie@example.com"),
            stored
        );
    }

    #[test]
    fn add_oauth_credential_refuses_a_duplicate_via_org_and_email_when_the_slot_predates_uuids() {
        // Accounts registered before identity was persisted carry no `uuid`.
        // A fresh sign-in to one of those must still be caught, by falling
        // through to organizationUuid + email.
        let _env = setup_env();
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {"email": "Charlie@Example.com", "organizationUuid": "7f94011f-org"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "acct-uuid-stable".to_string(),
                // Deliberately different casing and padding: the same address
                // typed differently is the same account.
                email: Some("  charlie@example.com  ".to_string()),
                organization_uuid: Some("7f94011f-org".to_string()),
            })
        };

        let err = add_oauth_credential_with_timeout(
            &oauth_creds_json("fresh-refresh", "fresh-access"),
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &resolver,
        )
        .unwrap_err();
        assert!(matches!(err, SwitchError::AlreadyRegistered(ref n) if n == "1"));
    }

    #[test]
    fn add_oauth_credential_prefers_caller_email_then_resolved_identity_then_synthetic() {
        let _env = setup_env();
        let creds = oauth_creds_json("r1", "a1");

        // No caller email, no resolved identity: falls back to the same
        // synthetic convention `add_token` uses for a bare setup-token.
        let slot1 = add_oauth_credential_with_timeout(
            &creds,
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(slot1, 1);
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["1"]["email"], "setup-token-1@token.local");

        // No caller email, but identity resolves one: that wins over synthetic.
        let creds2 = oauth_creds_json("r2", "a2");
        let resolver = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "uuid-2".to_string(),
                email: Some("resolved@example.com".to_string()),
                organization_uuid: Some("org-2".to_string()),
            })
        };
        let slot2 = add_oauth_credential_with_timeout(
            &creds2,
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &resolver,
        )
        .unwrap();
        assert_eq!(slot2, 2);
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["2"]["email"], "resolved@example.com");
        assert_eq!(seq["accounts"]["2"]["uuid"], "uuid-2");
        assert_eq!(seq["accounts"]["2"]["organizationUuid"], "org-2");

        // An explicit caller email wins over both — resolved via a distinct
        // identity so this doesn't collide with the account just registered
        // above under `resolver`'s identity.
        let creds3 = oauth_creds_json("r3", "a3");
        let resolver3 = |_: &str| {
            Some(oauth::TokenAccount {
                uuid: "uuid-3".to_string(),
                email: Some("ignored-because-explicit@example.com".to_string()),
                organization_uuid: Some("org-3".to_string()),
            })
        };
        let slot3 = add_oauth_credential_with_timeout(
            &creds3,
            Some("explicit@example.com"),
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &resolver3,
        )
        .unwrap();
        assert_eq!(slot3, 3);
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["3"]["email"], "explicit@example.com");
    }

    #[test]
    fn add_oauth_credential_allows_the_add_when_identity_cannot_be_resolved() {
        // Same degrade-to-fingerprint-only behavior as `add_current_account`
        // and `add_token`: an offline identity lookup must never block a
        // legitimate add.
        let _env = setup_env();
        let seq = serde_json::json!({
            "sequence": [1],
            "accounts": {
                "1": {"email": "someone-else@example.com", "organizationUuid": "org-other"}
            }
        });
        write_json_file(&accounts_file(), &seq);

        let creds = oauth_creds_json("fresh-refresh", "fresh-access");
        let slot = add_oauth_credential_with_timeout(
            &creds,
            None,
            None,
            crate::locking::DEFAULT_TIMEOUT,
            &no_identity,
        )
        .unwrap();
        assert_eq!(slot, 2);
    }

    // -- set_account_enabled ----------------------------------------------------

    #[test]
    fn set_account_enabled_refuses_to_disable_the_active_account() {
        let _env = setup_env();
        seed_two_accounts(); // account 1 (alpha) is active

        let err = set_account_enabled(1, false).unwrap_err();
        assert!(matches!(err, SwitchError::CannotDisableActive(ref n) if n == "1"));

        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert!(
            seq["accounts"]["1"].get("disabled").is_none(),
            "must be a strict no-op"
        );
    }

    #[test]
    fn set_account_enabled_toggles_the_disabled_flag_on_a_non_active_account() {
        let _env = setup_env();
        seed_two_accounts(); // account 2 (bravo) is not active

        set_account_enabled(2, false).unwrap();
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["accounts"]["2"]["disabled"], true);

        set_account_enabled(2, true).unwrap();
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert!(seq["accounts"]["2"].get("disabled").is_none());
    }

    #[test]
    fn set_account_enabled_errors_on_an_unknown_account() {
        let _env = setup_env();
        seed_two_accounts();

        let err = set_account_enabled(99, false).unwrap_err();
        assert!(matches!(err, SwitchError::UnknownAccount(ref n) if n == "99"));
    }
}
