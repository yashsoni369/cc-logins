//! The account registry and the snapshot read path.
//!
//! Originally ported from claude-swap (MIT) —
//! <https://github.com/realiti4/claude-swap>, `claude_swap/switcher.py`.
//! Since v0.4 this module no longer switches accounts by swapping credential
//! files, and never registers, copies or refreshes a token:
//!
//! - Registry edits (enable/disable, alias, remove, reorder) are here, each
//!   under the vault lock with atomic writes. Profile registration and
//!   selection live in [`crate::profile_registry`], which uses the helpers
//!   this module exposes.
//! - [`read_snapshot`] reads each profile account's folder in place
//!   (identity from `.claude.json`, token via [`crate::usage_reader`]) and
//!   makes at most one usage request per account. Accounts not yet moved
//!   from v0.3 are never read; they show their last reading.
//! - The v0.3 vault is only read or deleted: [`delete_vault_copy`] removes a
//!   moved account's stored copy, and [`crate::switch_transaction`] keeps
//!   recovering an interrupted v0.3 switch until v0.5.
//!
//! Slot allocation reuses the lowest free slot number, so a freed slot is
//! recycled rather than left as a permanent gap. `.claude.json` lives at the
//! home dir, not inside `.claude/`; always resolve it via [`crate::paths`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::credentials::{CredentialError, CredentialStore, Platform as CredPlatform, StoreHost};
use crate::model::{
    Account, EnvKind, EnvStatus, Environment, Snapshot, SpendWindow, Usage, UsageStatus,
    UsageWindow,
};
use crate::oauth;
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
pub(crate) fn vault_lock_path() -> PathBuf {
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

    #[error("account {0} is the active account; switch to another account before removing it")]
    CannotRemoveActive(String),

    /// The account is already gone from the registry, so nothing points at
    /// the files that were left behind; the startup sweep
    /// ([`sweep_orphaned_slot_files`]) retries them.
    #[error(
        "account {0} was removed, but some of its stored files could not be deleted ({1}); they will be cleaned up the next time CC Logins starts"
    )]
    RemovedWithLeftovers(String, String),

    /// A request the backend refuses on its face (too long, not a
    /// permutation, not a WSL environment). The text is shown to the user.
    #[error("{0}")]
    InvalidInput(String),
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

pub(crate) fn read_sequence_data() -> Option<Map<String, Value>> {
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

pub(crate) fn write_sequence_data(data: &Map<String, Value>) -> Result<(), SwitchError> {
    let body = serde_json::to_string_pretty(&Value::Object(data.clone()))?;
    atomic_write(&accounts_file(), body.as_bytes())?;
    Ok(())
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

    // Once any account is selected for new sessions, that selection is what
    // "active" means. Before that (a v0.3 registry), it is the live login.
    let active_num = match crate::profile_registry::selected_number(data) {
        Some(selected) => Some(selected.to_string()),
        None => current_account_number(data),
    };

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
            profile: crate::profile_registry::profile_of(record),
            usage_freshness: None,
        });
    }
    out
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
/// A snapshot, plus what the usage fetch behind it actually did.
///
/// The snapshot alone collapses every fetch failure into
/// [`UsageStatus::Stale`]/[`UsageStatus::Unavailable`], which is right for the
/// UI — a reader does not care whether the network dropped or the endpoint
/// refused — and wrong for the poller, which has to back off for one of those
/// and not the other. Only the cadence planner needs this; everything else
/// keeps calling [`read_snapshot`].
#[derive(Debug)]
pub struct SnapshotFetch {
    pub snapshot: Snapshot,
    /// Any account's usage fetch came back HTTP 429.
    pub rate_limited: bool,
}

/// [`read_snapshot`], keeping the fetch diagnostics.
pub async fn read_snapshot_reporting() -> Result<SnapshotFetch, SwitchError> {
    read_snapshot_inner().await
}

pub async fn read_snapshot() -> Result<Snapshot, SwitchError> {
    read_snapshot_inner().await.map(|f| f.snapshot)
}

async fn read_snapshot_inner() -> Result<SnapshotFetch, SwitchError> {
    let mut rate_limited = false;
    // Last-good readings, so an account nobody is using right now shows what
    // was last true rather than a blank. See usage_projection for why that
    // stays accurate.
    let mut cache = crate::usage_cache::UsageCache::load();
    let accounts = read_accounts()?;

    // Phase 1: local reads only. A profile account's folder is read in place;
    // a v0.3 account is never read at all: its stored copy is not used any
    // more, and it shows its last reading until it is moved.
    let pending: Vec<(Account, Option<ProfileRead>)> = accounts
        .into_iter()
        .map(|account| {
            let read = account
                .profile
                .as_ref()
                .map(|profile| read_profile(&account, profile));
            (account, read)
        })
        .collect();

    // Phase 2: at most one usage request per account with a current token.
    let mut measured = Vec::with_capacity(pending.len());
    for (mut account, read) in pending {
        match read {
            Some(read) => measure_profile(&mut account, read, &mut cache, &mut rate_limited).await,
            None => {
                let key = account.stable_key();
                serve_projected(&mut account, &cache, &key);
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
        // Probed, so first run can tell "you have not added an account yet"
        // apart from "you have no Claude login at all". Those read identically
        // on an empty vault, and conflating them told a user who was signed
        // into Claude Code that no account existed.
        has_credentials: paths::claude_login_present(),
    };

    cache.save();

    Ok(SnapshotFetch {
        snapshot: Snapshot::new(vec![environment]),
        rate_limited,
    })
}

/// What phase 1 learned about a profile account, without any network.
enum ProfileRead {
    /// The folder is signed in as this account; here is its store.
    Signed(crate::usage_reader::Access),
    /// The folder has no login.
    SignedOut,
    /// The folder is signed in as someone else.
    Mismatch,
}

/// Read a profile's folder: who it is signed in as (from `.claude.json`,
/// cheap), then its access token in place. Never writes or refreshes.
fn read_profile(account: &Account, profile: &crate::model::AccountProfile) -> ProfileRead {
    if profile.state == crate::model::ProfileState::MigrationPending {
        return ProfileRead::SignedOut;
    }
    let config_dir = profile.config_dir.as_deref();
    let global = crate::auth_status::global_config_for(config_dir);
    let Some(identity) = crate::auth_status::read_folder_identity(&global) else {
        return ProfileRead::SignedOut;
    };
    if !folder_matches(account, &identity) {
        return ProfileRead::Mismatch;
    }
    ProfileRead::Signed(crate::usage_reader::read_access(config_dir))
}

/// Whether a folder's signed-in identity is this account: account UUID when
/// both are known, else email plus organization.
pub(crate) fn folder_matches(
    account: &Account,
    identity: &crate::auth_status::FolderIdentity,
) -> bool {
    if let (Some(a), Some(b)) = (account.uuid.as_deref(), identity.account_uuid.as_deref()) {
        return a == b;
    }
    let account_org = account
        .organization_uuid
        .as_deref()
        .filter(|s| !s.is_empty());
    let folder_org = identity
        .organization_uuid
        .as_deref()
        .filter(|s| !s.is_empty());
    account
        .email
        .trim()
        .eq_ignore_ascii_case(identity.email.trim())
        && account_org == folder_org
}

/// Measure a profile account: one usage request when Claude Code holds a
/// current token for it, otherwise its last reading projected onto now.
async fn measure_profile(
    account: &mut Account,
    read: ProfileRead,
    cache: &mut crate::usage_cache::UsageCache,
    rate_limited: &mut bool,
) {
    let key = account.stable_key();
    let token = match read {
        ProfileRead::Signed(crate::usage_reader::Access::Token(token)) => token,
        ProfileRead::Mismatch => {
            if let Some(profile) = account.profile.as_mut() {
                profile.state = crate::model::ProfileState::IdentityMismatch;
            }
            set_status(account, UsageStatus::Error);
            return;
        }
        ProfileRead::SignedOut => {
            if let Some(profile) = account.profile.as_mut() {
                if profile.state == crate::model::ProfileState::Ready {
                    profile.state = crate::model::ProfileState::LoginRequired;
                }
            }
            serve_projected(account, cache, &key);
            set_status(account, UsageStatus::ReloginRequired);
            return;
        }
        ProfileRead::Signed(_) => {
            // Expired, missing or unreadable: nobody is using this account
            // right now, so its last reading still holds.
            serve_projected(account, cache, &key);
            return;
        }
    };

    match oauth::read_usage_once(&token).await {
        Ok(Some(result)) => {
            let now = chrono::Utc::now();
            let usage = to_model_usage(&result);
            cache.record_success(&key, &usage, now);
            account.usage = Some(usage);
            account.usage_fetched_at = Some(now.to_rfc3339());
            account.usage_age_seconds = Some(0.0);
            account.usage_freshness = Some(crate::usage_projection::UsageFreshness::Live);
            set_status(account, UsageStatus::Ok);
        }
        Ok(None) => serve_projected(account, cache, &key),
        Err(error) => {
            if error == oauth::UsageError::Http(429) {
                *rate_limited = true;
                cache.record_failure(&key, crate::usage_cache::RATE_LIMITED);
            }
            // A 401 means the token was renewed or revoked since it was read.
            // Claude Code owns renewal; show the last reading meanwhile.
            serve_projected(account, cache, &key);
        }
    }
}

/// Set a usage status unless the user has held the account out of rotation.
fn set_status(account: &mut Account, status: UsageStatus) {
    if account.usage_status != UsageStatus::Disabled {
        account.usage_status = status;
    }
}

/// Show the last good reading, with windows that have reset since shown at
/// zero. Status `Stale`, freshness says how old.
fn serve_projected(account: &mut Account, cache: &crate::usage_cache::UsageCache, key: &str) {
    let Some((usage, fetched_at)) = cache.last_good(key) else {
        return;
    };
    let now = chrono::Utc::now();
    let (projected, freshness) = crate::usage_projection::project(&usage, fetched_at, now);
    account.usage = Some(projected);
    account.usage_fetched_at = Some(fetched_at.to_rfc3339());
    account.usage_age_seconds =
        Some(now.signed_duration_since(fetched_at).num_seconds().max(0) as f64);
    account.usage_freshness = Some(freshness);
    set_status(account, UsageStatus::Stale);
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
        // Parsed since the port was written and dropped on this line ever
        // since, so an enterprise account reached the UI with no usage at all.
        spend: u.spend.as_ref().map(|sp| SpendWindow {
            used: sp.used,
            limit: sp.limit,
            pct: sp.pct,
            currency: sp.currency.clone(),
            severity: sp.severity.clone(),
            resets_at: sp.resets_at.clone(),
            countdown: sp.countdown.clone(),
            clock: sp.clock.clone(),
        }),
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
// Since v0.4 nothing in this module writes Claude Code's official files, so
// every mutation here takes [`vault_lock_path`] alone. Only
// `crate::switch_transaction`'s recovery of an interrupted v0.3 switch still
// takes Claude's locks.

// ---------------------------------------------------------------------------
// Registry helpers, shared with `crate::profile_registry`.
// ---------------------------------------------------------------------------

pub(crate) fn ensure_accounts_object(data: &mut Map<String, Value>) {
    if !matches!(data.get("accounts"), Some(Value::Object(_))) {
        data.insert("accounts".to_string(), Value::Object(Map::new()));
    }
}

pub(crate) fn refuse_pending_recovery() -> Result<(), SwitchError> {
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
/// leaving a permanent gap, since slots are a comparatively scarcer, more
/// visible resource in the GUI's account list than in the CLI. Recycling is
/// why [`remove_account`] and [`sweep_orphaned_slot_files`] must leave no
/// backup behind for a freed number.
pub(crate) fn next_free_slot(data: &Map<String, Value>) -> u32 {
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
pub(crate) fn add_to_sequence(data: &mut Map<String, Value>, slot: u32) {
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
// Account management: alias / remove / reorder.
//
// Registry-only edits follow `set_account_enabled_with_timeout` exactly: vault
// lock, recovery gate, read, mutate, stamp `lastUpdated`, atomic write.
// ---------------------------------------------------------------------------

/// Longest alias accepted, in characters. Long enough for a real name, short
/// enough to fit a tray menu row.
pub const MAX_ALIAS_CHARS: usize = 40;

/// Set or clear the display alias of `number`.
///
/// Whitespace is trimmed; `None` or an empty result removes the alias so the
/// account falls back to its (masked) email.
pub fn set_account_alias(number: u32, alias: Option<&str>) -> Result<(), SwitchError> {
    set_account_alias_with_timeout(number, alias, crate::locking::DEFAULT_TIMEOUT)
}

fn set_account_alias_with_timeout(
    number: u32,
    alias: Option<&str>,
    timeout: Duration,
) -> Result<(), SwitchError> {
    // Validated before the lock: a refusal must cost nothing.
    let alias = alias.map(str::trim).filter(|a| !a.is_empty());
    if alias.is_some_and(|a| a.chars().count() > MAX_ALIAS_CHARS) {
        return Err(SwitchError::InvalidInput(format!(
            "an alias can be at most {MAX_ALIAS_CHARS} characters"
        )));
    }

    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;
    let num = number.to_string();

    let Some(record) = data
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .and_then(|accounts| accounts.get_mut(&num))
        .and_then(Value::as_object_mut)
    else {
        return Err(SwitchError::UnknownAccount(num));
    };
    match alias {
        Some(a) => {
            record.insert("alias".to_string(), Value::String(a.to_string()));
        }
        None => {
            record.remove("alias");
        }
    }
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;

    Ok(())
}

/// Whether a `sequence` / `activeAccountNumber` entry names `number`. Both
/// numbers and numeric strings are accepted, matching [`accounts_from_sequence`].
fn registry_entry_is(value: &Value, number: u32) -> bool {
    value.as_u64() == Some(u64::from(number))
        || value.as_str().and_then(|s| s.trim().parse::<u32>().ok()) == Some(number)
}

/// The registered email of slot `num`, or `None` when there is no such slot.
fn registered_email(data: &Map<String, Value>, num: &str) -> Option<String> {
    let record = data.get("accounts")?.as_object()?.get(num)?;
    let email = record
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(email.to_string())
}

/// Stop managing `number` and delete everything stored for it.
///
/// Refuses the live account ([`SwitchError::CannotRemoveActive`]): removing
/// it would leave Claude Code running on a login nothing manages.
///
/// Order is the safety property. The registry is durably rewritten without
/// the slot **first**, so there is never a registry entry pointing at deleted
/// files. Only then are the slot's credential backup (`.enc`, `.prev`, macOS
/// Keychain) and config backup deleted. If that second step fails the account
/// is still removed and [`SwitchError::RemovedWithLeftovers`] says so; the
/// files are unreferenced by then and [`sweep_orphaned_slot_files`] deletes
/// them at the next launch, before [`next_free_slot`] can hand the number to
/// a new account.
///
/// Usage history is keyed by `Account::stable_key`, not the slot, and is
/// left alone.
pub fn remove_account(number: u32) -> Result<(), SwitchError> {
    remove_account_with_timeout(number, crate::locking::DEFAULT_TIMEOUT)
}

fn remove_account_with_timeout(number: u32, timeout: Duration) -> Result<(), SwitchError> {
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;
    let num = number.to_string();

    let Some(email) = registered_email(&data, &num) else {
        return Err(SwitchError::UnknownAccount(num));
    };

    // A profile account can always be removed: nothing live depends on this
    // app for it. Its folder, and the history in it, stays on disk.
    let is_profile = data
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|accounts| accounts.get(&num))
        .and_then(Value::as_object)
        .is_some_and(|record| record.contains_key("configDir"));
    if !is_profile && current_account_number(&data).as_deref() == Some(num.as_str()) {
        return Err(SwitchError::CannotRemoveActive(num));
    }
    if crate::profile_registry::selected_number(&data) == Some(number) {
        data.remove("selectedAccountNumber");
    }

    // 1. The registry, durably, so nothing references the files below.
    if let Some(accounts) = data.get_mut("accounts").and_then(Value::as_object_mut) {
        accounts.remove(&num);
    }
    if let Some(sequence) = data.get_mut("sequence").and_then(Value::as_array_mut) {
        sequence.retain(|entry| !registry_entry_is(entry, number));
    }
    if data
        .get("activeAccountNumber")
        .is_some_and(|entry| registry_entry_is(entry, number))
    {
        data.remove("activeAccountNumber");
    }
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;

    // 2. The files. Every step is attempted and every failure reported.
    let mut failures: Vec<String> = Vec::new();
    let mut store = CredentialStore::new(GuiStoreHost);
    if let Err(e) = store.delete_account_credentials_strict(&num, &email) {
        failures.push(e.to_string());
    }
    if let Err(e) = remove_file_if_present(&account_config_path(&num, &email)) {
        failures.push(format!("config backup: {e}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        let detail = failures.join("; ");
        Err(SwitchError::RemovedWithLeftovers(num, detail))
    }
}

/// Delete a moved account's v0.3 vault copy (credential backup and config
/// backup). Called only after its token-free login has been verified and
/// registered; the registry record itself stays.
pub(crate) fn delete_vault_copy(number: u32, email: &str) -> Result<(), SwitchError> {
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), crate::locking::DEFAULT_TIMEOUT)?;
    let num = number.to_string();
    let mut store = CredentialStore::new(GuiStoreHost);
    store.delete_account_credentials_strict(&num, email)?;
    remove_file_if_present(&account_config_path(&num, email))?;
    Ok(())
}

/// `remove_file` that treats an already-absent file as success.
fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Replace the rotation order with `order`.
///
/// `order` must name every managed slot exactly once; anything else is
/// [`SwitchError::InvalidInput`] and changes nothing. `sequence` is both the
/// display order and the order `Strategy::NextAvailable` walks, so this is a
/// real behavioural change, not a view preference.
pub fn reorder_accounts(order: &[u32]) -> Result<(), SwitchError> {
    reorder_accounts_with_timeout(order, crate::locking::DEFAULT_TIMEOUT)
}

fn reorder_accounts_with_timeout(order: &[u32], timeout: Duration) -> Result<(), SwitchError> {
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let mut data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;

    let mut existing: Vec<u32> = data
        .get("accounts")
        .and_then(Value::as_object)
        .map(|accounts| {
            accounts
                .keys()
                .filter_map(|k| k.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();
    existing.sort_unstable();
    let mut requested = order.to_vec();
    requested.sort_unstable();
    if requested != existing {
        return Err(SwitchError::InvalidInput(
            "the new order must list every account exactly once".to_string(),
        ));
    }

    data.insert(
        "sequence".to_string(),
        Value::Array(order.iter().copied().map(Value::from).collect()),
    );
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Orphaned slot files.
//
// `next_free_slot` recycles numbers, so a credential or config backup left
// behind by a removal that could not finish (or by a crash between an add's
// file write and its registry write) would otherwise be picked up by the next
// account to land on that number.
// ---------------------------------------------------------------------------

/// Which per-slot backup a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotFileKind {
    /// `credentials/.creds-{num}-{email}.enc` or its `.enc.prev`.
    Credentials,
    /// `configs/.claude-config-{num}-{email}.json`.
    Config,
}

impl SlotFileKind {
    fn dir(self, root: &Path) -> PathBuf {
        match self {
            Self::Credentials => root.join("credentials"),
            Self::Config => root.join("configs"),
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Credentials => ".creds-",
            Self::Config => ".claude-config-",
        }
    }

    /// Longest first: `.enc.prev` must win over `.enc`.
    fn suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Credentials => &[".enc.prev", ".enc"],
            Self::Config => &[".json"],
        }
    }

    /// `(slot, email)` from a file name of this kind. `None` for anything
    /// else, including the legacy `None` slot and in-flight `.stage` files.
    fn parse(self, name: &str) -> Option<(u32, String)> {
        let rest = name.strip_prefix(self.prefix())?;
        let rest = self
            .suffixes()
            .iter()
            .find_map(|suffix| rest.strip_suffix(*suffix))?;
        let (number, email) = rest.split_once('-')?;
        if email.is_empty() {
            return None;
        }
        Some((number.parse().ok()?, email.to_string()))
    }
}

/// A per-slot backup file whose slot number is not in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanedSlotFile {
    kind: SlotFileKind,
    number: u32,
    email: String,
    path: PathBuf,
}

/// Every slot backup under `root` whose number is not an `accounts` key in
/// `data`. Reads directory listings only; deletes nothing.
///
/// Only `root/credentials` and `root/configs` are looked at — never the live
/// `~/.claude.json` or Claude Code's own credential store. A registry without
/// an `accounts` object yields nothing rather than "everything is orphaned".
fn orphaned_slot_files(root: &Path, data: &Map<String, Value>) -> Vec<OrphanedSlotFile> {
    let Some(accounts) = data.get("accounts").and_then(Value::as_object) else {
        return Vec::new();
    };
    let registered: std::collections::HashSet<u32> = accounts
        .keys()
        .filter_map(|k| k.parse::<u32>().ok())
        .collect();

    let mut out = Vec::new();
    for kind in [SlotFileKind::Credentials, SlotFileKind::Config] {
        let Ok(entries) = std::fs::read_dir(kind.dir(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((number, email)) = kind.parse(&name) else {
                continue;
            };
            if !registered.contains(&number) {
                out.push(OrphanedSlotFile {
                    kind,
                    number,
                    email,
                    path: entry.path(),
                });
            }
        }
    }
    out
}

/// Delete one orphan through the same helpers a removal uses, so a
/// credential orphan also loses its `.prev` and (macOS) Keychain copy.
fn delete_orphan(
    store: &mut CredentialStore<GuiStoreHost>,
    orphan: &OrphanedSlotFile,
) -> Result<(), SwitchError> {
    if orphan.kind == SlotFileKind::Config {
        remove_file_if_present(&orphan.path)?;
    } else {
        let number = orphan.number.to_string();
        store.delete_account_credentials_strict(&number, &orphan.email)?;
    }
    Ok(())
}

/// Delete slot backups no registry slot references. Best-effort, run once at
/// startup; returns how many orphaned files were found.
///
/// Does nothing at all when the registry is missing or unreadable — "no
/// registry" must never be read as "every file is an orphan" — or when a
/// switch recovery is pending, since recovery may still need those files.
pub fn sweep_orphaned_slot_files() -> Result<usize, SwitchError> {
    sweep_orphaned_slot_files_with_timeout(crate::locking::DEFAULT_TIMEOUT)
}

fn sweep_orphaned_slot_files_with_timeout(timeout: Duration) -> Result<usize, SwitchError> {
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    refuse_pending_recovery()?;

    let Some(data) = read_sequence_data() else {
        return Ok(0);
    };
    let orphans = orphaned_slot_files(&paths::backup_root(), &data);

    let mut store = CredentialStore::new(GuiStoreHost);
    for orphan in &orphans {
        match delete_orphan(&mut store, orphan) {
            Ok(()) => log::info!("removed orphaned backup {}", orphan.path.display()),
            Err(e) => log::warn!(
                "could not remove orphaned backup {}: {e}",
                orphan.path.display()
            ),
        }
    }
    Ok(orphans.len())
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
    use crate::test_support::{env_lock, EnvGuard, StoreRootGuard};
    use tempfile::TempDir;

    // -- pick_target ----------------------------------------------------------

    fn switchable_account(number: u32, pct: Option<f64>) -> Account {
        let usage = pct.map(|p| Usage {
            spend: None,
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

    // -- test environment ---------------------------------------------------------
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

    // -- registry mutations ------------------------------------------------------

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

    // -- set_account_alias ------------------------------------------------------

    fn read_registry() -> Value {
        serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap()
    }

    #[test]
    fn set_account_alias_trims_sets_and_clears() {
        let _env = setup_env();
        seed_two_accounts();

        set_account_alias(2, Some("  Work  ")).unwrap();
        assert_eq!(read_registry()["accounts"]["2"]["alias"], "Work");
        let bravo = read_accounts()
            .unwrap()
            .into_iter()
            .find(|account| account.number == 2)
            .unwrap();
        assert_eq!(bravo.display_name(), "Work");

        set_account_alias(2, Some("   ")).unwrap();
        assert!(read_registry()["accounts"]["2"].get("alias").is_none());

        set_account_alias(2, Some("Personal")).unwrap();
        assert_eq!(read_registry()["accounts"]["2"]["alias"], "Personal");
        set_account_alias(2, None).unwrap();
        assert!(read_registry()["accounts"]["2"].get("alias").is_none());
    }

    #[test]
    fn set_account_alias_refuses_an_overlong_alias_as_a_strict_no_op() {
        let _env = setup_env();
        seed_two_accounts();

        let longest = "x".repeat(MAX_ALIAS_CHARS);
        set_account_alias(2, Some(&longest)).unwrap();

        let too_long = "y".repeat(MAX_ALIAS_CHARS + 1);
        let err = set_account_alias(2, Some(&too_long)).unwrap_err();
        assert!(matches!(err, SwitchError::InvalidInput(_)), "got {err:?}");
        let registry = read_registry();
        assert_eq!(registry["accounts"]["2"]["alias"], longest.as_str());
    }

    #[test]
    fn set_account_alias_errors_on_an_unknown_account() {
        let _env = setup_env();
        seed_two_accounts();

        let err = set_account_alias(99, Some("Nobody")).unwrap_err();
        assert!(matches!(err, SwitchError::UnknownAccount(ref n) if n == "99"));
    }

    // -- remove_account ---------------------------------------------------------

    #[test]
    fn remove_account_refuses_the_active_account() {
        let _env = setup_env();
        seed_two_accounts(); // account 1 (alpha) is live

        let err = remove_account(1).unwrap_err();
        assert!(matches!(err, SwitchError::CannotRemoveActive(ref n) if n == "1"));

        let registry = read_registry();
        assert!(
            registry["accounts"].get("1").is_some(),
            "must be a strict no-op"
        );
        assert_eq!(registry["sequence"], serde_json::json!([1, 2]));
    }

    #[test]
    fn remove_account_drops_the_registry_entry_then_every_backup_of_that_slot_only() {
        let _env = setup_env();
        seed_two_accounts();
        // A registry that still remembers slot 2 as active (as a string, the
        // way older writers stored it) must not keep pointing at it.
        let mut registry = read_registry();
        registry["activeAccountNumber"] = Value::from("2");
        write_json_file(&accounts_file(), &registry);

        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("1", "alpha@example.com", "alpha-backup")
            .unwrap();
        // A second write leaves a `.prev` generation behind for slot 2.
        store
            .write_account_credentials("2", "bravo@example.com", "target-creds-2b")
            .unwrap();
        let prev = credentials_dir().join(".creds-2-bravo@example.com.enc.prev");
        assert!(prev.exists(), "precondition: slot 2 has a .prev backup");

        remove_account(2).unwrap();

        let registry = read_registry();
        assert!(registry["accounts"].get("2").is_none());
        assert_eq!(registry["sequence"], serde_json::json!([1]));
        assert!(registry.get("activeAccountNumber").is_none());
        let leftover = store.read_account_credentials("2", "bravo@example.com");
        assert!(leftover.is_empty());
        assert!(!prev.exists());
        assert!(!account_config_path("2", "bravo@example.com").exists());

        // Everything else is untouched: the other slot and the live login.
        assert!(registry["accounts"].get("1").is_some());
        let alpha = store.read_account_credentials("1", "alpha@example.com");
        assert_eq!(alpha, "alpha-backup");
        let live = std::fs::read_to_string(paths::credentials_path()).unwrap();
        assert_eq!(live, "original-active-creds-for-account-1");
    }

    #[test]
    fn remove_account_errors_on_an_unknown_account() {
        let _env = setup_env();
        seed_two_accounts();

        let err = remove_account(99).unwrap_err();
        assert!(matches!(err, SwitchError::UnknownAccount(ref n) if n == "99"));
    }

    #[test]
    fn remove_account_refuses_pending_switch_recovery() {
        let _env = setup_env();
        seed_two_accounts();
        crate::switch_transaction::set_recovery_requirement(Some("repair required".into()));

        let error = remove_account_with_timeout(2, Duration::from_secs(1)).unwrap_err();

        crate::switch_transaction::set_recovery_requirement(None);
        assert!(matches!(
            error,
            SwitchError::Transaction(crate::switch_transaction::TransactionError::RecoveryRequired)
        ));
        assert!(read_registry()["accounts"].get("2").is_some());
    }

    // -- reorder_accounts -------------------------------------------------------

    #[test]
    fn reorder_accounts_rewrites_the_rotation_order() {
        let _env = setup_env();
        seed_two_accounts();

        reorder_accounts(&[2, 1]).unwrap();

        assert_eq!(read_registry()["sequence"], serde_json::json!([2, 1]));
        let numbers: Vec<u32> = read_accounts()
            .unwrap()
            .into_iter()
            .map(|account| account.number)
            .collect();
        assert_eq!(numbers, vec![2, 1]);
    }

    #[test]
    fn reorder_accounts_refuses_anything_but_a_permutation_of_the_slots() {
        let _env = setup_env();
        seed_two_accounts();

        for bad in [vec![1], vec![1, 2, 3], vec![1, 1], Vec::new()] {
            let err = reorder_accounts(&bad).unwrap_err();
            assert!(matches!(err, SwitchError::InvalidInput(_)), "{bad:?}");
        }
        assert_eq!(read_registry()["sequence"], serde_json::json!([1, 2]));
    }

    // -- orphaned slot files ----------------------------------------------------

    #[test]
    fn slot_file_names_parse_only_numbered_backups() {
        let creds = SlotFileKind::Credentials;
        assert_eq!(
            creds.parse(".creds-12-first-last@example.com.enc"),
            Some((12, "first-last@example.com".to_string()))
        );
        assert_eq!(
            creds.parse(".creds-3-a@example.com.enc.prev"),
            Some((3, "a@example.com".to_string()))
        );
        assert_eq!(creds.parse(".creds-None-a@example.com.enc"), None);
        assert_eq!(creds.parse("..creds-1-a@x.io.enc.1.stage"), None);
        assert_eq!(creds.parse(".unclaimed-abc.enc"), None);
        assert_eq!(
            SlotFileKind::Config.parse(".claude-config-4-Mixed@Example.com.json"),
            Some((4, "Mixed@Example.com".to_string()))
        );
    }

    #[test]
    fn sweep_deletes_backups_no_registry_slot_references_and_nothing_else() {
        let _env = setup_env();
        seed_two_accounts(); // slots 1 and 2; slot 2 has backups

        // Slot 3 is gone from the registry but its files survived.
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("3", "Carol@Example.com", "orphan-creds")
            .unwrap();
        store
            .write_account_credentials("3", "Carol@Example.com", "orphan-creds-2")
            .unwrap();
        let orphan_prev = credentials_dir().join(".creds-3-carol@example.com.enc.prev");
        assert!(orphan_prev.exists(), "precondition: slot 3 has a .prev");
        let orphan_config = account_config_path("3", "Carol@Example.com");
        write_json_file(&orphan_config, &serde_json::json!({"oauthAccount": {}}));
        let unrelated = credentials_dir().join(".unclaimed-keep.enc");
        std::fs::write(&unrelated, "not a slot").unwrap();

        let found = sweep_orphaned_slot_files().unwrap();

        assert_eq!(found, 3, "the .enc, its .prev, and the config");
        let leftover = store.read_account_credentials("3", "Carol@Example.com");
        assert!(leftover.is_empty());
        assert!(!orphan_prev.exists());
        assert!(!orphan_config.exists());
        assert!(unrelated.exists());
        let bravo = store.read_account_credentials("2", "bravo@example.com");
        assert_eq!(bravo, "target-creds-2");
        assert!(account_config_path("2", "bravo@example.com").exists());
        let live = std::fs::read_to_string(paths::credentials_path()).unwrap();
        assert_eq!(live, "original-active-creds-for-account-1");
    }

    #[test]
    fn sweep_never_reads_a_missing_registry_as_every_file_being_orphaned() {
        let _env = setup_env();
        let mut store = CredentialStore::new(GuiStoreHost);
        store
            .write_account_credentials("1", "alpha@example.com", "only-copy")
            .unwrap();

        assert_eq!(sweep_orphaned_slot_files().unwrap(), 0);
        let kept = store.read_account_credentials("1", "alpha@example.com");
        assert_eq!(kept, "only-copy");
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
    fn next_free_slot_is_one_when_no_accounts_exist() {
        let data = Map::new();
        assert_eq!(next_free_slot(&data), 1);
    }

    #[test]
    fn read_accounts_is_empty_when_no_registry_exists() {
        let _env = setup_env();
        assert!(read_accounts().unwrap().is_empty());
    }

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
}
