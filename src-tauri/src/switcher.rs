//! The account-switch core: backup-then-install account activation, and the
//! account-listing/read path used to build a [`Snapshot`].
//!
//! Ported from claude-swap (MIT) — <https://github.com/realiti4/claude-swap>,
//! `claude_swap/switcher.py` (`ClaudeAccountSwitcher`). This is a narrow slice
//! of a 4900-line module: only the switch path (backup the active credential,
//! install a target account's credential, splice the global config's
//! `oauthAccount` block) and the local, network-free account enumeration
//! needed to build a [`Snapshot`]. Everything else in `switcher.py` — aliasing,
//! adding/removing accounts, session-mode directories, the auto-switch daemon,
//! import/export, foreign-credential provenance resolution, transactional
//! rollback across multiple files, and Claude Code's own `~/.claude.lock` /
//! `~/.claude.json.lock` directory-locks — is out of scope here and not
//! reproduced. See the crate-level port report for what that means in
//! practice.
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
//! [`import_from_cswap`] is not a port of anything upstream — it is this
//! app's own bridge for a user who already has accounts registered with the
//! `cswap` CLI: it copies them into OUR vault ([`crate::paths::backup_root`])
//! without ever mutating the CLI's own store, so both tools keep working
//! afterward.
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
//!   this app and `cswap` — see the "Locking" section further down this file
//!   (just above [`acquire_cswap_and_vault_locks`]) for the full split.
//! - [`crate::paths`] — every on-disk location comes from here, never
//!   hand-rolled: [`crate::paths::backup_root`] for OUR vault,
//!   [`crate::paths::cswap_store_root`] for the CLI's (read-only interop
//!   only), and `global_config_path`/`credentials_path`/`claude_config_home`
//!   for Claude Code's official files.
//! - [`crate::oauth`] — usage fetch, token refresh, and (new) profile lookup.
//!   `try_fetch_usage_for_account` is called with `is_active` set correctly,
//!   which is what keeps this port honest about never refreshing the active
//!   account's token (Claude Code owns those bytes). [`add_current_account`]
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
//!    holds it for the entire operation. [`switch_to`] (and
//!    [`import_from_cswap`], which also reads the CLI's store) hold two locks
//!    — see the "Locking" section below.
//! 2. **Never hold a lock across a network call.** [`read_snapshot`] fetches
//!    usage with no lock held at all. [`add_current_account`],
//!    [`add_token`], and [`add_oauth_credential`] are the exception to "no
//!    mutating function makes a network call": each resolves account
//!    identity via `oauth::fetch_oauth_profile` for duplicate detection, but
//!    does so strictly BEFORE acquiring the vault lock, and treats a failed
//!    lookup as advisory (degrade, don't block) — see
//!    [`find_registered_slot_by_identity`]. Every other mutating function
//!    still makes no network call at all.
//! 3. **Back up the outgoing credential before installing the new one.** See
//!    [`switch_to`]'s doc comment and the `backup_happens_before_target_validation…`
//!    test below.
//! 4. **Atomic writes.** All local writes in this module go through
//!    [`atomic_write`] (write-temp-then-rename), matching `credentials.rs`.
//! 5. **Never refresh the active account's token.** Enforced by
//!    `oauth::try_fetch_usage_for_account`'s `is_active` flag, not by this
//!    module — but this module is the caller that must (and does) pass it
//!    correctly.
//! 6. **`.claude.json` lives at the home dir, not inside `.claude/`.** Always
//!    resolved via [`crate::paths::global_config_path`], never hand-rolled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::credentials::{
    self, merge_shared_credential_fields, shared_credential_fields, CredentialError,
    CredentialStore, Platform as CredPlatform, StoreHost,
};
use crate::model::{
    Account, EnvKind, EnvStatus, Environment, Snapshot, Usage, UsageStatus, UsageWindow,
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
/// [`acquire_cswap_and_vault_locks`] for how this relates to the *other* lock
/// this module sometimes also takes.
fn vault_lock_path() -> PathBuf {
    paths::backup_root().join(".lock")
}

fn account_config_path(account_num: &str, email: &str) -> PathBuf {
    account_config_path_at(&paths::backup_root(), account_num, email)
}

/// Same layout as [`account_config_path`], parameterized on the store root so
/// it can also address an account config backup inside the `cswap` CLI's
/// store (see [`import_from_cswap`]).
fn account_config_path_at(root: &Path, account_num: &str, email: &str) -> PathBuf {
    root.join("configs")
        .join(format!(".claude-config-{account_num}-{email}.json"))
}

/// [`StoreHost`] for this crate's [`CredentialStore`]: platform is detected
/// live (never cached across calls, matching the trait's contract), and
/// `credentials_dir` is OUR OWN `<backup_root>/credentials` — this app's
/// vault, never the `cswap` CLI's. [`CswapStoreHost`] (below, near
/// [`import_from_cswap`]) is the read-only counterpart pointed at the CLI's
/// own credentials directory.
struct GuiStoreHost;

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
    /// Our own namespace. The default is the CLI's, and sharing it made
    /// `import_from_cswap` overwrite the CLI's own Keychain backups on macOS.
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
    Credential(#[from] CredentialError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

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
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("switcher");
    let tmp_path = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        next_tmp_suffix()
    ));

    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        // 0600 at creation, not after the rename — see the twin of this
        // function in `credentials.rs::atomic_write`.
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp_path)?;
        f.write_all(contents)?;
        f.sync_all()
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp_path, target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        // Not `?`: the rename has committed, so a chmod failure must not
        // report a completed switch as failed.
        if let Err(e) = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600)) {
            log::warn!("could not tighten permissions on {}: {e}", target.display());
        }
    }

    Ok(())
}

fn next_tmp_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// sequence.json access.
// ---------------------------------------------------------------------------

fn read_sequence_data() -> Option<Map<String, Value>> {
    read_sequence_data_at(&paths::backup_root())
}

/// Same as [`read_sequence_data`], parameterized on the store root so it can
/// also read the `cswap` CLI's registry (see [`import_from_cswap`]) without
/// ever writing there.
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

// ---------------------------------------------------------------------------
// Snapshot (accounts + freshly-fetched usage).
// ---------------------------------------------------------------------------

/// Accounts plus freshly-fetched usage, wrapped in the single `Native`
/// [`Environment`] this port produces (WSL/profile environments are out of
/// scope here; see `crate::wsl`).
///
/// No lock is held anywhere in this function — rule 2 (never hold the
/// credential/config lock across a network call). A per-account usage-fetch
/// failure degrades that account to [`UsageStatus::Stale`] rather than
/// failing the whole snapshot; a disabled account's status is left alone
/// either way. (This port has no persistent usage cache — `oauth.rs` and
/// `credentials.rs` are the only reused modules in scope — so "last-known"
/// degrades to "no reading, marked Stale" rather than serving a genuinely
/// cached prior measurement; see the port report.)
pub async fn read_snapshot() -> Result<Snapshot, SwitchError> {
    let accounts = read_accounts()?;

    // Phase 1 — read every credential up front, then DROP the store before any
    // network call happens.
    //
    // Two reasons, and they point the same way. It mirrors the rule inherited
    // from upstream that no store or lock may be held across I/O. And
    // `CredentialStore` is not `Send`, so holding it across an `.await` makes
    // this future non-`Send`, which Tauri rejects outright for async commands.
    let mut pending: Vec<(Account, String)> = Vec::with_capacity(accounts.len());
    {
        let mut store = CredentialStore::new(GuiStoreHost);
        for account in accounts {
            let num = account.number.to_string();
            // Never refresh the active account's token (rule 5): the
            // credential for the active slot comes from the *active* store,
            // and `try_fetch_usage_for_account`'s `is_active` flag (passed
            // below) is what keeps refresh/retry off it.
            let creds = if account.active {
                store.read_active_credentials().value.unwrap_or_default()
            } else {
                store.read_account_credentials(&num, &account.email)
            };
            pending.push((account, creds));
        }
    }

    // Phase 2 — network work, with no credential store held.
    let mut measured = Vec::with_capacity(pending.len());
    for (mut account, creds) in pending {
        let num = account.number.to_string();

        if !creds.is_empty() {
            let outcome = oauth::try_fetch_usage_for_account(
                &num,
                &account.email,
                &creds,
                account.active,
                None,
            )
            .await;
            match outcome.usage {
                Some(result) => {
                    account.usage = Some(to_model_usage(&result));
                    account.usage_fetched_at = Some(chrono::Utc::now().to_rfc3339());
                    account.usage_age_seconds = Some(0.0);
                    if account.usage_status != UsageStatus::Disabled {
                        account.usage_status = UsageStatus::Ok;
                    }
                }
                None => {
                    if account.usage_status != UsageStatus::Disabled {
                        account.usage_status = UsageStatus::Stale;
                    }
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
//   also written by the `cswap` CLI. Mutual exclusion against it therefore
//   cannot use a lock file of our choosing — it requires locking a path the
//   CLI itself honours, which is `<cswap_store_root>/.lock` (see
//   `crate::locking`'s module doc: both tools lock that exact path with the
//   exact same OS primitive, which is the entire interop contract).
//
// So any function that writes the official files — today, only [`switch_to`]
// — must hold BOTH locks for the whole mutation, and every such caller must
// acquire them in the SAME order, or two processes taking the two locks in
// opposite orders could deadlock each other. [`acquire_cswap_and_vault_locks`]
// is that one order, enforced structurally by being the only way any function
// in this module acquires both: cswap-compat lock first, our vault lock
// second. A function that only ever touches our own vault (`add_current_account`,
// `add_token`, `set_account_enabled`) has no reason to take the cswap lock at
// all — there is nothing there for another process to race it on — so those
// take [`vault_lock_path`] alone.
//
// The cswap lock is acquired only when `<cswap_store_root>` already exists on
// disk. If a user has no `cswap` install, there is no directory and nothing
// to coordinate with — creating one purely to place a `.lock` file inside it
// would conjure a fake `cswap` installation out of nothing. This is also the
// one write this module ever makes under the `cswap` directory at all:
// taking a lock cannot corrupt anything (unlike writing account data there
// would), which is exactly why it is safe to share a *lock file* in a place
// this app otherwise never touches — see [`import_from_cswap`]'s doc for the
// read side of that same directory.

/// Acquire, in order, the `cswap`-compatible lock (only if that CLI's store
/// directory already exists) and then our own vault lock. See the module
/// section above this function for the full reasoning; this is the single
/// choke point every caller that needs both locks must go through, so the
/// acquisition order can never drift out of sync between call sites.
fn acquire_cswap_and_vault_locks(
    timeout: Duration,
) -> Result<(Option<crate::locking::FileLock>, crate::locking::FileLock), SwitchError> {
    let cswap_root = paths::cswap_store_root();
    let cswap_lock = if cswap_root.exists() {
        Some(crate::locking::acquire_or_err(
            cswap_root.join(".lock"),
            timeout,
        )?)
    } else {
        None
    };
    let vault_lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;
    Ok((cswap_lock, vault_lock))
}

// ---------------------------------------------------------------------------
// Switch.
// ---------------------------------------------------------------------------

/// Switch the live login to `target`'s stored credential.
///
/// Holds BOTH the `cswap`-compat lock (when that CLI's store exists) and our
/// own vault lock for the whole mutation — see
/// [`acquire_cswap_and_vault_locks`] for why two locks and why this order.
/// No network call is made anywhere in this function (rules 2 and 5 fall out
/// for free). Order of operations, matching upstream's `_perform_switch`:
///
/// 1. Read the active credential (local I/O only).
/// 2. **Back up the outgoing login** — the account currently live (resolved
///    from `~/.claude.json`'s `oauthAccount`, not from a possibly-stale
///    `activeAccountNumber`) has its credential and config snapshot written
///    to its backup slot *before* anything about the target is written or
///    even validated. An unmanaged/unattributable live credential (no
///    resolvable slot) is preserved via
///    [`CredentialStore::write_unclaimed_credential`] instead of a normal
///    slot backup, so a fresh-machine or drifted-login switch still never
///    silently destroys it.
/// 3. Validate and read the target's stored credential + config backup.
/// 4. **Install** the target credential (composed with the machine's live
///    shared OAuth fields, mirroring `_prepare_credentials_for_activation`)
///    and splice its `oauthAccount` block into the global config.
/// 5. Update `sequence.json`'s `activeAccountNumber`.
///
/// A failure at step 3 or 4 leaves step 2's backup in place and never reaches
/// the write in step 4, so a switch that can't complete fails without ever
/// touching the live login — see the
/// `backup_happens_before_target_validation…` test.
///
/// Not reproduced from upstream: cross-file transactional rollback (Python's
/// `SwitchTransaction`), self-switch no-op short-circuiting, `--force`
/// direct-activation, and foreign-credential provenance classification
/// (network-based ownership resolution before backing up divergent live
/// bytes). See the port report.
pub fn switch_to(target: &Account) -> Result<(), SwitchError> {
    switch_to_with_timeout(target, crate::locking::DEFAULT_TIMEOUT)
}

fn switch_to_with_timeout(target: &Account, timeout: Duration) -> Result<(), SwitchError> {
    let num = target.number.to_string();

    // Rule 1: lock before touching anything, for the whole mutation. This
    // function writes Claude Code's official files (step 4 below), so it
    // needs both locks — see `acquire_cswap_and_vault_locks`.
    let (_cswap_lock, _lock) = acquire_cswap_and_vault_locks(timeout)?;

    let mut data = read_sequence_data().ok_or(SwitchError::NoAccountsManaged)?;

    // Source of truth for the target's email is the registry, not whatever
    // the caller's (possibly stale) `Account` says.
    let email = data
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|accounts| accounts.get(&num))
        .and_then(|record| record.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| SwitchError::UnknownAccount(num.clone()))?;

    let mut store = CredentialStore::new(GuiStoreHost);

    let active = store.read_active_credentials();
    let original_creds = active.value.ok_or(SwitchError::CredentialRead)?;

    let current_num = current_account_number(&data);

    // Step 2: back up the outgoing login BEFORE anything about the target is
    // written (rule 3).
    match &current_num {
        Some(cur_num) => {
            if original_creds.is_empty() {
                // An empty read (e.g. a settling Keychain) must never be
                // written over the departing account's backup.
                return Err(SwitchError::EmptyActiveCredential(cur_num.clone()));
            }
            let cur_email = data
                .get("accounts")
                .and_then(Value::as_object)
                .and_then(|accounts| accounts.get(cur_num))
                .and_then(|record| record.get("email"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            let config_text = std::fs::read_to_string(paths::global_config_path())?;

            store.write_account_credentials(cur_num, &cur_email, &original_creds)?;
            write_account_config(cur_num, &cur_email, &config_text)?;
        }
        None => {
            if !original_creds.is_empty() {
                // Live credential with no resolvable managed slot (fresh
                // machine, or a login that drifted out from under cswap's
                // records). Preserve it rather than silently overwrite it.
                let mut context = Map::new();
                context.insert(
                    "reason".to_string(),
                    Value::String("displaced-live-login".to_string()),
                );
                store
                    .write_unclaimed_credential(&original_creds, context)
                    .map_err(|e| SwitchError::Stash(e.to_string()))?;
            }
        }
    }

    // Step 3: validate and read the target's stored backups.
    let target_creds = store.read_account_credentials(&num, &email);
    if target_creds.is_empty() {
        return Err(SwitchError::NoStoredCredentials(num));
    }
    let target_config_text = read_account_config(&num, &email)
        .ok_or_else(|| SwitchError::NoStoredConfig(num.clone()))?;
    let target_config_value: Value = serde_json::from_str(&target_config_text)?;
    let target_oauth = target_config_value
        .get("oauthAccount")
        .cloned()
        .ok_or_else(|| SwitchError::InvalidBackupConfig(num.clone()))?;

    // Step 4: install. Compose the target's stored login with the machine's
    // live shared OAuth fields (mcpOAuth, pluginSecrets, ...) so activation
    // doesn't regress those to the target's last-seen generation.
    let shared = shared_credential_fields(Some(&original_creds)).unwrap_or_default();
    let prepared = merge_shared_credential_fields(&target_creds, &shared);
    store.write_credentials(&prepared)?;
    write_oauth_account(&target_oauth)?;

    // Step 5: record the new active slot.
    data.insert(
        "activeAccountNumber".to_string(),
        Value::from(target.number),
    );
    data.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    write_sequence_data(&data)?;

    Ok(())
}

/// Splice `oauth_account` into `~/.claude.json`'s `oauthAccount` key,
/// preserving every other key untouched (rule 6: the path itself comes from
/// `paths::global_config_path`, never hand-rolled).
fn write_oauth_account(oauth_account: &Value) -> Result<(), SwitchError> {
    let path = paths::global_config_path();
    let mut config: Map<String, Value> = if path.exists() {
        match serde_json::from_str::<Value>(&std::fs::read_to_string(&path)?)? {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    } else {
        Map::new()
    };
    config.insert("oauthAccount".to_string(), oauth_account.clone());
    let body = serde_json::to_string_pretty(&Value::Object(config))?;
    atomic_write(&path, body.as_bytes())?;
    Ok(())
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

/// Return the slot number of an already-registered account whose stored
/// backup credential has the same identity fingerprint as `live_fingerprint`,
/// or `None` if no such slot exists.
///
/// Comparing by [`oauth::credential_fingerprint`] rather than by email is the
/// point: it survives OAuth access-token rotation (fingerprint prefers the
/// refresh-token hash), so re-adding an account whose access token has since
/// refreshed is still correctly recognised as a duplicate.
fn find_registered_slot_by_fingerprint(
    store: &mut CredentialStore<GuiStoreHost>,
    accounts: &Map<String, Value>,
    live_fingerprint: &str,
) -> Option<String> {
    for (num, record) in accounts {
        let email = record
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let existing = store.read_account_credentials(num, email);
        if existing.is_empty() {
            continue;
        }
        if oauth::credential_fingerprint(&existing).as_deref() == Some(live_fingerprint) {
            return Some(num.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Duplicate detection by account identity (not credential bytes).
//
// `find_registered_slot_by_fingerprint` above compares raw credential bytes,
// and that is not enough on its own: `oauth::try_refresh_oauth_credentials`
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
// [`oauth::fetch_oauth_profile`], and only falls back to the fingerprint
// comparison above when neither side of a given pair has resolvable
// identity at all.
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
/// record written before this fix (or copied in by [`import_from_cswap`]
/// from a source that never had one) may have no `uuid` key at all; that gap
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
    // written) — no cswap-compat lock needed, see the module-level locking
    // section above `acquire_cswap_and_vault_locks`.
    let _lock = crate::locking::acquire_or_err(vault_lock_path(), timeout)?;

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
    record.insert("email".to_string(), Value::String(email));
    record.insert(
        "organizationUuid".to_string(),
        Value::String(organization_uuid),
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
    record.insert("email".to_string(), Value::String(resolved_email));
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
    record.insert("email".to_string(), Value::String(resolved_email));
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
// Import from the cswap CLI's store.
// ---------------------------------------------------------------------------

/// [`StoreHost`] pointed at the `cswap` CLI's OWN credential backups
/// (`<cswap_store_root>/credentials`), used only to *read* — see
/// [`import_from_cswap`]. Never constructed as the destination of a write in
/// this module; [`GuiStoreHost`] is always the write side.
struct CswapStoreHost;

impl StoreHost for CswapStoreHost {
    /// Pinned to the file backend under test, same as [`GuiStoreHost`].
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
        paths::cswap_store_root().join("credentials")
    }
}

/// Result of [`import_from_cswap`]: how many accounts were copied in, and how
/// many were left alone because they were already present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub imported: u32,
    pub skipped: u32,
}

/// Copy every account from the `cswap` CLI's store into ours.
///
/// # Read-only on the source, always
///
/// This function only ever reads `<cswap_store_root>/sequence.json` and the
/// credential/config backups it references, and only ever writes into OUR OWN
/// vault ([`paths::backup_root`]). It never moves, deletes, or rewrites
/// anything under the `cswap` directory — the one exception, taking the
/// cswap-compat lock (see below), is not a data write and cannot corrupt
/// anything there. A user who has both tools installed must be able to keep
/// using the `cswap` CLI exactly as before, immediately after running this.
///
/// # No-op when there is nothing to import
///
/// If `<cswap_store_root>` does not exist at all, this returns
/// `Ok(ImportOutcome::default())` without taking any lock and without
/// touching the filesystem — same reasoning as
/// `acquire_cswap_and_vault_locks`: no directory means no `cswap` install to
/// import from, and we must not go looking for one by creating it.
///
/// # Locking
///
/// When the source directory does exist, this acquires the cswap-compat lock
/// (for a consistent read of a registry the CLI might be concurrently
/// mutating) and then our own vault lock (we are about to write into it) —
/// via [`acquire_cswap_and_vault_locks`], the same order [`switch_to`] uses.
///
/// # Duplicate detection
///
/// An account already present in our vault is skipped rather than imported a
/// second time, compared by [`oauth::credential_fingerprint`] exactly the way
/// [`add_current_account`] already detects a duplicate registration — this
/// survives OAuth access-token rotation, so an account added to one store and
/// later refreshed is still recognised as the same login when found in the
/// other.
///
/// A source account whose registry entry exists but whose credential backup
/// is missing or unreadable is also counted as skipped rather than failing
/// the whole import — one bad slot in the CLI's store must not block every
/// other account from coming across.
pub fn import_from_cswap() -> Result<ImportOutcome, SwitchError> {
    import_from_cswap_with_timeout(crate::locking::DEFAULT_TIMEOUT)
}

fn import_from_cswap_with_timeout(timeout: Duration) -> Result<ImportOutcome, SwitchError> {
    let cswap_root = paths::cswap_store_root();
    if !cswap_root.exists() {
        return Ok(ImportOutcome::default());
    }

    let (_cswap_lock, _lock) = acquire_cswap_and_vault_locks(timeout)?;

    let Some(source_data) = read_sequence_data_at(&cswap_root) else {
        return Ok(ImportOutcome::default());
    };
    let source_accounts = match source_data.get("accounts").and_then(Value::as_object) {
        Some(m) => m.clone(),
        None => return Ok(ImportOutcome::default()),
    };

    let mut dest_data = read_sequence_data().unwrap_or_default();
    ensure_accounts_object(&mut dest_data);
    let dest_accounts_snapshot = dest_data
        .get("accounts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut source_store = CredentialStore::new(CswapStoreHost);
    let mut dest_store = CredentialStore::new(GuiStoreHost);

    // Deterministic order (numeric slot), so a re-run over a partially
    // imported store processes accounts the same way every time.
    let mut nums: Vec<String> = source_accounts.keys().cloned().collect();
    nums.sort_by_key(|s| s.parse::<u64>().unwrap_or(u64::MAX));

    let mut imported = 0u32;
    let mut skipped = 0u32;

    for source_num in nums {
        let Some(record) = source_accounts.get(&source_num).and_then(Value::as_object) else {
            continue;
        };
        let email = record
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let credential = source_store.read_account_credentials(&source_num, &email);
        if credential.is_empty() {
            skipped += 1;
            continue;
        }

        if let Some(fp) = oauth::credential_fingerprint(&credential) {
            if find_registered_slot_by_fingerprint(&mut dest_store, &dest_accounts_snapshot, &fp)
                .is_some()
            {
                skipped += 1;
                continue;
            }
        }

        let config_text = read_account_config_at(&cswap_root, &source_num, &email);

        let slot = next_free_slot(&dest_data);
        let dest_num = slot.to_string();

        // Same ordering discipline as `add_current_account`: the recoverable
        // backup write happens before the registry commits to the new slot.
        dest_store.write_account_credentials(&dest_num, &email, &credential)?;
        if let Some(config_text) = &config_text {
            write_account_config(&dest_num, &email, config_text)?;
        }

        let mut new_record = record.clone();
        new_record.insert(
            "importedFrom".to_string(),
            Value::String("cswap".to_string()),
        );
        new_record.insert(
            "imported".to_string(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );

        dest_data
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .expect("ensure_accounts_object guarantees this")
            .insert(dest_num, Value::Object(new_record));
        add_to_sequence(&mut dest_data, slot);

        imported += 1;
    }

    if imported > 0 {
        dest_data.insert(
            "lastUpdated".to_string(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
        write_sequence_data(&dest_data)?;
    }

    Ok(ImportOutcome { imported, skipped })
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
/// after wherever "current" is. `Account::is_switchable` already excludes the
/// active account itself, disabled slots, and expired logins, so none of
/// those are ever returned. An account with `UNKNOWN` usage
/// (`Account::headroom() == None`) is never treated as ineligible by
/// `is_switchable` — see each strategy's handling of that case below.
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
        if !account.is_switchable() {
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
        if !account.is_switchable() {
            continue;
        }
        match account.headroom() {
            Some(headroom) if headroom <= 0.0 => continue, // known-exhausted: skip
            _ => return Some(account),                     // unknown or real headroom: take it
        }
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
        if !account.is_switchable() {
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
    fn next_available_does_not_skip_unknown_usage_but_does_skip_known_exhaustion() {
        let accounts = vec![
            switchable_account(1, Some(100.0)), // known-exhausted: skip
            switchable_account(2, None),        // unknown: must NOT be auto-skipped
            switchable_account(3, Some(10.0)),
        ];
        assert_eq!(
            pick_target(&accounts, Strategy::NextAvailable)
                .unwrap()
                .number,
            2
        );
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
    /// `paths::cswap_store_root()` is not separately redirected: it derives
    /// from `HOME`/`USERPROFILE`, so pointing those at a temp dir isolates it
    /// too — *provided* `XDG_DATA_HOME` is unset. On Linux that variable is
    /// consulted first, and CI runners set it, so leaving it alone let a real
    /// `$XDG_DATA_HOME/claude-swap` escape the sandbox: `guard_real_store`
    /// would panic, and `acquire_cswap_and_vault_locks` could create a `.lock`
    /// in the user's actual directory. `WSL_DISTRO_NAME` is pinned for the
    /// same reason — it flips `Platform::detect()`.
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

    /// The backup-before-install ordering rule (rule 3), proven by making the
    /// target invalid: if the outgoing account were only backed up *after*
    /// validating/installing the target, this failure would leave Account-1's
    /// login without a fresh backup. Instead the backup must already be there,
    /// and the live login must be untouched.
    #[test]
    fn backup_happens_before_target_validation_so_a_failed_switch_still_preserves_the_outgoing_login(
    ) {
        let _env = setup_env();
        seed_two_accounts();
        std::fs::remove_file(account_config_path("2", "bravo@example.com")).unwrap();

        let err = switch_to_with_timeout(&bravo_target(), Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, SwitchError::NoStoredConfig(_)));

        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "original-active-creds-for-account-1",
            "outgoing account must be backed up even though the switch ultimately failed"
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
    fn switch_fails_cleanly_rather_than_half_applying_when_the_vault_lock_cannot_be_acquired() {
        let _env = setup_env();
        seed_two_accounts();
        // No cswap store exists in this test, so switch_to skips that lock
        // entirely and its only contention point is our own vault lock.
        assert!(!paths::cswap_store_root().exists());

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

    // -- lock ordering (task 2): cswap-compat lock, then ours, always -----------

    #[test]
    fn switch_to_is_blocked_by_an_externally_held_cswap_compat_lock_when_a_cswap_store_exists() {
        let _env = setup_env();
        seed_two_accounts();

        // Simulate a machine that also has a `cswap` install: the directory
        // exists, so switch_to must coordinate with it.
        let cswap_root = paths::cswap_store_root();
        std::fs::create_dir_all(&cswap_root).unwrap();
        let _held =
            crate::locking::acquire_or_err(cswap_root.join(".lock"), Duration::from_secs(5))
                .unwrap();

        let err = switch_to_with_timeout(&bravo_target(), Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, SwitchError::Locking(_)));

        // The cswap-compat lock is acquired FIRST (see
        // `acquire_cswap_and_vault_locks`), so failing to get it must
        // short-circuit before any effect on OUR vault or the live login —
        // proving the ordering, not just that both locks exist.
        assert_eq!(
            std::fs::read_to_string(paths::credentials_path()).unwrap(),
            "original-active-creds-for-account-1",
            "the live login must never be touched when the cswap-compat lock can't be acquired"
        );
        let seq: Value =
            serde_json::from_str(&std::fs::read_to_string(accounts_file()).unwrap()).unwrap();
        assert_eq!(seq["activeAccountNumber"], 1);
        let mut store = CredentialStore::new(GuiStoreHost);
        assert_eq!(
            store.read_account_credentials("1", "alpha@example.com"),
            "",
            "our vault must be untouched — the vault lock is acquired second, after cswap"
        );
    }

    #[test]
    fn switch_to_never_creates_a_cswap_directory_that_does_not_exist() {
        let _env = setup_env();
        seed_two_accounts();
        let cswap_root = paths::cswap_store_root();
        assert!(!cswap_root.exists());

        switch_to_with_timeout(&bravo_target(), Duration::from_secs(5)).unwrap();

        assert!(
            !cswap_root.exists(),
            "switch_to must never create a directory for a tool the user doesn't have, \
             just to place a lock file in it"
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
        // A record written before this fix (or copied in via
        // `import_from_cswap` from a source that never had one) carries
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

    // -- import_from_cswap -------------------------------------------------------

    /// Seed a `cswap`-shaped store (registry + per-account credential/config
    /// backups) at `paths::cswap_store_root()`, using the exact on-disk
    /// layout `import_from_cswap` reads. Returns that root for convenience.
    fn seed_cswap_store(accounts: &[(&str, &str, &str)]) -> PathBuf {
        let cswap_root = paths::cswap_store_root();

        let mut accounts_map = Map::new();
        let mut sequence = Vec::new();
        for (num, email, _) in accounts {
            accounts_map.insert(
                num.to_string(),
                serde_json::json!({
                    "email": email,
                    "organizationUuid": format!("org-{num}"),
                    "organizationName": format!("Org {num}"),
                }),
            );
            sequence.push(Value::from(num.parse::<u64>().unwrap()));
        }
        write_json_file(
            &cswap_root.join("sequence.json"),
            &serde_json::json!({
                "activeAccountNumber": Value::Null,
                "sequence": sequence,
                "accounts": accounts_map,
            }),
        );

        let mut store = CredentialStore::new(CswapStoreHost);
        for (num, email, creds) in accounts {
            store.write_account_credentials(num, email, creds).unwrap();
            write_json_file(
                &account_config_path_at(&cswap_root, num, email),
                &serde_json::json!({"oauthAccount": {"emailAddress": email, "organizationUuid": format!("org-{num}")}}),
            );
        }

        cswap_root
    }

    #[test]
    fn import_from_cswap_is_a_no_op_when_the_cswap_store_does_not_exist() {
        let _env = setup_env();
        let cswap_root = paths::cswap_store_root();
        assert!(!cswap_root.exists());

        let outcome = import_from_cswap().unwrap();
        assert_eq!(
            outcome,
            ImportOutcome {
                imported: 0,
                skipped: 0
            }
        );

        assert!(
            !cswap_root.exists(),
            "must not create a directory for a tool the user doesn't have"
        );
        assert!(read_accounts().unwrap().is_empty());
    }

    #[test]
    fn import_from_cswap_copies_accounts_without_mutating_the_source() {
        let _env = setup_env();
        let cswap_root = seed_cswap_store(&[
            ("1", "alpha@example.com", "alpha-live-creds"),
            ("2", "bravo@example.com", "bravo-live-creds"),
        ]);

        let source_seq_before = std::fs::read_to_string(cswap_root.join("sequence.json")).unwrap();
        let source_alpha_config_before = std::fs::read_to_string(account_config_path_at(
            &cswap_root,
            "1",
            "alpha@example.com",
        ))
        .unwrap();

        let outcome = import_from_cswap().unwrap();
        assert_eq!(
            outcome,
            ImportOutcome {
                imported: 2,
                skipped: 0
            }
        );

        // The source is byte-for-byte untouched: sequence.json, the config
        // backup, and (via a fresh read through the source-side store) the
        // credential backup itself.
        assert_eq!(
            std::fs::read_to_string(cswap_root.join("sequence.json")).unwrap(),
            source_seq_before
        );
        assert_eq!(
            std::fs::read_to_string(account_config_path_at(
                &cswap_root,
                "1",
                "alpha@example.com"
            ))
            .unwrap(),
            source_alpha_config_before
        );
        let mut source_store = CredentialStore::new(CswapStoreHost);
        assert_eq!(
            source_store.read_account_credentials("1", "alpha@example.com"),
            "alpha-live-creds"
        );
        assert_eq!(
            source_store.read_account_credentials("2", "bravo@example.com"),
            "bravo-live-creds"
        );

        // Our vault now has both accounts, with matching credentials, under
        // whatever slots it chose to allocate.
        let dest_accounts = read_accounts().unwrap();
        assert_eq!(dest_accounts.len(), 2);
        let mut dest_store = CredentialStore::new(GuiStoreHost);
        let alpha = dest_accounts
            .iter()
            .find(|a| a.email == "alpha@example.com")
            .unwrap();
        assert_eq!(
            dest_store.read_account_credentials(&alpha.number.to_string(), "alpha@example.com"),
            "alpha-live-creds"
        );
        let bravo = dest_accounts
            .iter()
            .find(|a| a.email == "bravo@example.com")
            .unwrap();
        assert_eq!(
            dest_store.read_account_credentials(&bravo.number.to_string(), "bravo@example.com"),
            "bravo-live-creds"
        );
    }

    #[test]
    fn import_from_cswap_skips_an_account_already_registered_by_credential_fingerprint() {
        let _env = setup_env();
        // Same login (same refresh-token lineage), registered in OUR vault
        // under slot 5 already.
        let shared_creds = oauth_creds_json("shared-refresh", "our-access-token");
        write_json_file(
            &accounts_file(),
            &serde_json::json!({
                "sequence": [5],
                "accounts": {"5": {"email": "dup@example.com", "organizationUuid": "org-dup"}}
            }),
        );
        let mut dest_store = CredentialStore::new(GuiStoreHost);
        dest_store
            .write_account_credentials("5", "dup@example.com", &shared_creds)
            .unwrap();

        // cswap has the same login (rotated access token) plus one genuinely
        // new account.
        seed_cswap_store(&[
            (
                "1",
                "dup@example.com",
                &oauth_creds_json("shared-refresh", "cswap-rotated-access"),
            ),
            ("2", "fresh@example.com", "fresh-creds"),
        ]);

        let outcome = import_from_cswap().unwrap();
        assert_eq!(
            outcome,
            ImportOutcome {
                imported: 1,
                skipped: 1
            }
        );

        // Still exactly two accounts total: the pre-existing slot 5, plus
        // the one genuinely new import. No duplicate of "dup@example.com".
        let accounts = read_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(
            accounts
                .iter()
                .filter(|a| a.email == "dup@example.com")
                .count(),
            1
        );
        assert!(accounts.iter().any(|a| a.email == "fresh@example.com"));
    }

    #[test]
    fn import_from_cswap_is_blocked_by_an_externally_held_cswap_compat_lock() {
        let _env = setup_env();
        let cswap_root = seed_cswap_store(&[("1", "alpha@example.com", "alpha-creds")]);

        let _held =
            crate::locking::acquire_or_err(cswap_root.join(".lock"), Duration::from_secs(5))
                .unwrap();

        let err = import_from_cswap_with_timeout(Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, SwitchError::Locking(_)));

        // No partial import: our vault must still be empty.
        assert!(read_accounts().unwrap().is_empty());
    }
}
