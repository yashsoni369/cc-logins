//! The Tauri IPC surface.
//!
//! Everything the frontend can ask the backend to do lives here, and nothing
//! else is exposed. Serde emits camelCase throughout (see [`crate::model`]), so
//! the payloads match `src/types.ts` without a translation layer.
//!
//! # Read and write are deliberately separated
//!
//! Commands that only observe state ([`snapshot`], [`accounts`],
//! [`environments`]) are safe to call on any timer, from any screen, at any
//! time. Commands that mutate credentials ([`switch_account`]) are not, and are
//! grouped separately below so the boundary is impossible to miss when reading
//! this file. No polling path may ever call a mutating command.

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

use crate::login::{self, LoginError};
use crate::model::{Account, Environment, Snapshot};
use crate::switcher::{self, SwitchError};

/// Errors cross the IPC boundary as a tagged object rather than a bare string,
/// so the UI can distinguish "nothing is set up yet" (show onboarding) from
/// "the network is down" (show stale data) from "something is genuinely wrong".
///
/// Every variant here is a structural signal the frontend is meant to branch
/// on directly (`err.kind`, or the `is*` accessors in `src/lib/api.ts`) —
/// never by inspecting `detail`. `detail` stays free text for humans and
/// logs; wording it differently must never change how the UI behaves. That
/// is the whole point of this enum being a closed, tagged set rather than a
/// single `String`: a rewording in `login.rs`/`switcher.rs` cannot silently
/// change what the UI does, because nothing in the UI reads those words.
#[derive(Debug, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind",
    content = "detail"
)]
pub enum IpcError {
    /// No accounts are managed yet — the first-run screen, not an error state.
    NotConfigured,
    /// Usage could not be read. Any accompanying data is last-known.
    Unreachable(String),
    /// The credential store could not be read or written.
    Credential(String),
    /// A lock is held by another process (very likely Claude Code itself).
    Busy(String),
    /// The interactive login's terminal window closed before any credential
    /// appeared. The ordinary "the user changed their mind" outcome, not a
    /// failure — the UI must render nothing for this, not a banner.
    Cancelled,
    /// The interactive login did not complete within its time budget.
    TimedOut(String),
    /// Something the requested operation depends on is missing from this
    /// machine — today, specifically the `claude` binary not being on PATH.
    PrerequisiteMissing(String),
    /// No terminal emulator could be launched to run the interactive login
    /// (Linux only). The UI should fall back to the paste-a-token flow.
    NoTerminalAvailable(String),
    /// The login/credential being added is already registered under a
    /// different slot.
    AlreadyRegistered(String),
    /// Refused to disable the currently-active account — auto-switch would
    /// have nowhere valid to land.
    CannotDisableActive(String),
    /// Refused to remove the currently-active account — Claude Code would be
    /// left running on a login nothing manages. Switch away first.
    CannotRemoveActive(String),
    /// The request itself is invalid (an alias that is too long, an order
    /// that is not a permutation of the accounts, an environment that is not
    /// WSL). `detail` is a sentence fit to show the user.
    InvalidInput(String),
    /// The server proved this account's refresh-token lineage is dead.
    ReloginRequired(String),
    /// An interrupted switch could not be recovered automatically.
    RecoveryRequired(String),
    /// A settings editor tried to overwrite a newer canonical revision.
    SettingsConflict {
        expected_revision: u64,
        actual_revision: u64,
    },
    /// Anything else.
    Internal(String),
}

impl From<crate::settings::SettingsUpdateError> for IpcError {
    fn from(error: crate::settings::SettingsUpdateError) -> Self {
        match error {
            crate::settings::SettingsUpdateError::Conflict {
                expected_revision,
                actual_revision,
            } => Self::SettingsConflict {
                expected_revision,
                actual_revision,
            },
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<SwitchError> for IpcError {
    fn from(e: SwitchError) -> Self {
        // Map by meaning, not by convenience: the UI branches on these.
        // Listed exhaustively (no catch-all `_`) so a variant added to
        // `SwitchError` later is a compile error here, not a silent
        // `Internal`.
        match &e {
            SwitchError::NoAccountsManaged => IpcError::NotConfigured,
            SwitchError::Locking(_) | SwitchError::LiveStateLock(_) => {
                IpcError::Busy(e.to_string())
            }
            SwitchError::Transaction(_)
                if crate::switch_transaction::recovery_requirement().is_some() =>
            {
                IpcError::RecoveryRequired(e.to_string())
            }
            SwitchError::Transaction(
                crate::switch_transaction::TransactionError::RecoveryRequired,
            )
            | SwitchError::Transaction(
                crate::switch_transaction::TransactionError::RollbackIncomplete { .. },
            ) => IpcError::RecoveryRequired(e.to_string()),
            SwitchError::TargetGenerationChanged(_) => IpcError::Busy(e.to_string()),
            // Credential-store problems: the store itself is unreadable,
            // missing, empty, or otherwise not trustworthy — as distinct
            // from a business-rule refusal below, where the store is fine
            // and the requested mutation just isn't valid right now.
            // `RemovedWithLeftovers` belongs here too: the account is already
            // out of the registry, and what failed is deleting its files.
            SwitchError::Credential(_)
            | SwitchError::Transaction(
                crate::switch_transaction::TransactionError::Credential(_)
                | crate::switch_transaction::TransactionError::CredentialRead
                | crate::switch_transaction::TransactionError::EmptyActiveCredential,
            )
            | SwitchError::CredentialRead
            | SwitchError::NoStoredCredentials(_)
            | SwitchError::NoStoredConfig(_)
            | SwitchError::InvalidBackupConfig(_)
            | SwitchError::EmptyActiveCredential(_)
            | SwitchError::Stash(_)
            | SwitchError::NoLiveCredential
            | SwitchError::InvalidCredential(_)
            | SwitchError::RemovedWithLeftovers(..) => IpcError::Credential(e.to_string()),
            // Business-rule refusals: the requested mutation is invalid
            // given the current state, not an I/O or credential-store
            // failure. Each gets its own structural kind so the UI can
            // branch without reading the message.
            SwitchError::AlreadyRegistered(_) => IpcError::AlreadyRegistered(e.to_string()),
            SwitchError::CannotDisableActive(_) => IpcError::CannotDisableActive(e.to_string()),
            SwitchError::CannotRemoveActive(_) => IpcError::CannotRemoveActive(e.to_string()),
            SwitchError::InvalidInput(_) => IpcError::InvalidInput(e.to_string()),
            // Malformed user input (an obviously-bad pasted token) and
            // everything else genuinely uncategorized fall to `Internal` —
            // the UI still gets the full, specific message via `e.to_string()`.
            SwitchError::UnknownAccount(_)
            | SwitchError::InvalidToken(_)
            | SwitchError::Transaction(_)
            | SwitchError::Io(_)
            | SwitchError::Json(_) => IpcError::Internal(e.to_string()),
        }
    }
}

/// Maps [`login::LoginError`] to [`IpcError`] by meaning, not convenience —
/// the UI (`describeInteractiveLoginError` in `src/App.tsx`) branches on the
/// tagged `kind` alone, never on `detail` text:
///
/// - [`LoginError::Cancelled`] → [`IpcError::Cancelled`] — the ordinary
///   "closed the terminal without logging in" outcome, not a failure. The UI
///   checks `err.isCancelled` and returns `null` (render nothing) for this
///   one specifically. Rewording `LoginError::Cancelled`'s `Display` text can
///   never affect this again, because the UI never reads it.
/// - [`LoginError::TimedOut`] → [`IpcError::TimedOut`].
/// - [`LoginError::ClaudeNotInstalled`] → [`IpcError::PrerequisiteMissing`] —
///   the `claude` binary isn't on PATH.
/// - [`LoginError::NoTerminalAvailable`] → [`IpcError::NoTerminalAvailable`]
///   — lets the UI point the user at the "Add token" fallback for this one
///   specifically.
/// - [`LoginError::BadCredential`] — a credential landed but did not
///   validate, which is a credential-store problem rather than a generic
///   internal failure, so this maps to `IpcError::Credential` (matching how
///   [`SwitchError::NoLiveCredential`] etc. are already categorized above).
///   The wrapped `&'static str` is fixed and content-free by construction
///   (see `LoginError::BadCredential`'s own doc comment), so this can never
///   leak the credential blob.
/// - [`LoginError::Io`] — a filesystem/process-spawn failure unrelated to the
///   login flow itself; `Internal`, same as every uncategorized `SwitchError`.
///
/// Listed exhaustively (no catch-all `_`) so a variant added to `LoginError`
/// later is a compile error here, not a silent `Internal`.
///
/// The credential blob itself is never part of any [`LoginError`] variant (see
/// `login.rs`'s "never logged" rule), so no arm here can ever surface it.
impl From<LoginError> for IpcError {
    fn from(e: LoginError) -> Self {
        match &e {
            LoginError::Cancelled => IpcError::Cancelled,
            LoginError::TimedOut => IpcError::TimedOut(e.to_string()),
            LoginError::ClaudeNotInstalled(_) => IpcError::PrerequisiteMissing(e.to_string()),
            LoginError::NoTerminalAvailable => IpcError::NoTerminalAvailable(e.to_string()),
            LoginError::BadCredential(_) => IpcError::Credential(e.to_string()),
            LoginError::Io(_) => IpcError::Internal(e.to_string()),
        }
    }
}

type IpcResult<T> = Result<T, IpcError>;

// ─── read-only commands ──────────────────────────────────────────────────────
// Safe to call on a timer. These never mutate credential state.

/// Accounts with no usage data. Fast and offline — this is what paints the UI
/// on first frame, before any network call has completed.
#[tauri::command]
pub fn accounts() -> IpcResult<Vec<Account>> {
    switcher::read_accounts().map_err(Into::into)
}

/// Accounts plus freshly-fetched usage, and every detected environment.
///
/// Degrades rather than fails: if usage cannot be fetched, accounts still come
/// back carrying their last-known values and a stale status. A blank UI is
/// worse than an old number that is labelled old.
#[tauri::command]
pub async fn snapshot(state: tauri::State<'_, AppState>) -> IpcResult<Snapshot> {
    // Serve a recent result rather than spending another request against the
    // per-token usage budget. Every window fetches once on mount, and dev-mode
    // StrictMode doubles that — without this, opening the app is a burst of
    // simultaneous identical fetches that earns an HTTP 429.
    if let Some(cached) = state.cached_snapshot(SNAPSHOT_CACHE_TTL) {
        return Ok(cached);
    }

    match switcher::read_snapshot().await {
        Ok(mut snap) => {
            merge_environments(&mut snap);
            state.store_snapshot(&snap);
            Ok(snap)
        }
        Err(SwitchError::NoAccountsManaged) => Err(IpcError::NotConfigured),
        Err(e) => Err(e.into()),
    }
}

/// Fetch a snapshot ignoring the cache, and refresh the cache with it.
///
/// Used by the mutating commands: after a switch or a registration the cached
/// value is stale by definition, and returning it would show the user the state
/// from before their own action.
async fn snapshot_uncached(state: &AppState) -> IpcResult<Snapshot> {
    match switcher::read_snapshot().await {
        Ok(mut snap) => {
            merge_environments(&mut snap);
            state.store_snapshot(&snap);
            Ok(snap)
        }
        Err(SwitchError::NoAccountsManaged) => Err(IpcError::NotConfigured),
        Err(e) => Err(e.into()),
    }
}

/// Outcome of a user-pressed Refresh.
///
/// `refreshed` is deliberately explicit rather than inferred from whether the
/// numbers changed: a genuine fetch that returns identical usage is not the
/// same event as a request that was never sent, and the UI must be able to
/// tell the user which happened instead of implying freshness it did not get.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResult {
    pub snapshot: Snapshot,
    /// `false` when the cooldown was still running and `snapshot` is the
    /// previously-held value.
    pub refreshed: bool,
    /// Seconds until pressing Refresh will actually fetch again.
    pub retry_after_seconds: u64,
}

/// How long after a manual refresh the next one is refused.
///
/// Matched to [`crate::poller::poll_policy::URGENT_INTERVAL_S`], the fastest
/// cadence the daemon will ever choose for itself: the user gets a control as
/// responsive as the poller's own most aggressive mode, and no faster.
///
/// Be clear about what this does and does not buy. It stops burst clicking,
/// which is the realistic failure. It does **not** by itself guarantee the
/// endpoint's rolling budget of ~28-30 requests per hour per token: someone
/// pressing this every 60 seconds for a solid hour, on top of the poller's own
/// spend, would exceed it. That case is handled where it was always handled —
/// [`crate::poller::poll_policy`]'s 429 backoff, which widens the cadence
/// after a rate limit rather than pretending it cannot happen.
pub const MANUAL_REFRESH_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// Fetch usage now, at the user's request, subject to a cooldown.
///
/// This is the escape hatch for the fixed poll cadence: the daemon's interval
/// is no longer configurable, so this is how someone who wants a number *right
/// now* gets one. It is throttled because it is the only fetch path a user can
/// trigger arbitrarily fast, and it spends from the same per-token budget that
/// [`snapshot`]'s cache exists to protect.
#[tauri::command]
pub async fn refresh_snapshot(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> IpcResult<RefreshResult> {
    if let Some(remaining) = state.manual_refresh_cooldown(MANUAL_REFRESH_COOLDOWN) {
        // Hand back what we already hold rather than spending a request. Any
        // age is acceptable here — the point is precisely not to fetch — and
        // the UI labels staleness from the snapshot's own timestamps.
        if let Some(cached) = state.cached_snapshot(std::time::Duration::MAX) {
            return Ok(RefreshResult {
                snapshot: cached,
                refreshed: false,
                // Round up: reporting 0 while still refusing would invite an
                // immediate retry that is also refused.
                retry_after_seconds: remaining.as_secs().saturating_add(1),
            });
        }
    }

    // Marked before the await, not after: two clicks landing together must not
    // both observe an expired cooldown and both fetch.
    state.mark_manual_refresh();
    let snapshot = snapshot_uncached(&state).await?;
    // Repaint the tray and tell the other window, so a refresh pressed in the
    // popover is visible on the dashboard too.
    crate::poller::publish_snapshot(&app, &snapshot);

    Ok(RefreshResult {
        snapshot,
        refreshed: true,
        retry_after_seconds: MANUAL_REFRESH_COOLDOWN.as_secs(),
    })
}

/// Detected credential realms: native, plus any WSL distro.
///
/// Never starts a stopped WSL distro — see [`crate::wsl`]. A stopped distro
/// comes back as `Asleep` with no filesystem access performed at all.
#[tauri::command]
pub fn environments() -> Vec<Environment> {
    crate::wsl::detect_environments()
}

/// Fold detected environments into a snapshot, attaching the accounts we read
/// to the native realm and leaving other realms as detected.
fn merge_environments(snap: &mut Snapshot) {
    let detected = crate::wsl::detect_environments();
    if detected.is_empty() {
        return;
    }

    // The accounts we just read belong to the native realm.
    let native_accounts: Vec<Account> = snap
        .environments
        .iter()
        .flat_map(|e| e.accounts.iter().cloned())
        .collect();

    snap.environments = detected
        .into_iter()
        .map(|mut env| {
            if env.kind == crate::model::EnvKind::Native {
                env.accounts = native_accounts.clone();
            }
            env
        })
        .collect();
}

// ─── mutating commands ───────────────────────────────────────────────────────
// These change which login Claude Code will use. Never call from a poller.

fn refuse_if_recovery_required() -> IpcResult<()> {
    match crate::switch_transaction::recovery_requirement() {
        Some(detail) => Err(IpcError::RecoveryRequired(detail)),
        None => Ok(()),
    }
}

/// Switch the live login to `account_number`.
///
/// Explicitly user-initiated. Takes the credential lock for the whole mutation
/// and backs up the outgoing login before writing anything about the target, so
/// a crash mid-switch cannot lose an account.
///
/// Paints the tray's [`crate::tray::State::Switching`] icon before touching
/// any credential, then [`crate::poller::publish_snapshot`]s the fresh result
/// once the swap has landed — otherwise the tray and the popover would both
/// keep showing the pre-switch state until the poller's next tick, which on
/// the adaptive cadence can be minutes away.
#[tauri::command]
pub async fn switch_account(app: tauri::AppHandle, account_number: u32) -> IpcResult<Snapshot> {
    switch_account_for(&app, account_number).await
}

/// Body of [`switch_account`], shared with the tray menu's account rows so
/// both paths take exactly the same guards and publish the same way.
pub async fn switch_account_for(
    app: &tauri::AppHandle,
    account_number: u32,
) -> IpcResult<Snapshot> {
    let state = app.state::<AppState>();
    refuse_if_recovery_required()?;
    let accounts = switcher::read_accounts()?;
    let target = accounts
        .iter()
        .find(|a| a.number == account_number)
        .ok_or_else(|| IpcError::Internal(format!("no account in slot {account_number}")))?;

    // With the opt-in live swap, a profile account with its own folder is
    // swapped into `~/.claude`, so running sessions follow it too.
    if crate::live_swap::enabled()
        && target
            .profile
            .as_ref()
            .is_some_and(|profile| profile.config_dir.is_some())
    {
        crate::poller::publish_switching(app);
        let swapped = off_runtime(move || crate::live_swap::swap_to(account_number)).await?;
        let snap = snapshot_uncached(&state).await;
        if let Ok(snap) = &snap {
            crate::poller::publish_snapshot(app, snap);
        }
        swapped?;
        return snap;
    }

    // Otherwise a profile account is never swapped in: new sessions are
    // pointed at its folder and running sessions keep theirs. Nothing is
    // written but the registry and shim.json.
    if target.profile.is_some() {
        off_runtime(move || crate::profile_registry::select(account_number)).await??;
        let snap = snapshot_uncached(&state).await?;
        crate::poller::publish_snapshot(app, &snap);
        return Ok(snap);
    }

    // A v0.3 account is never swapped into Claude Code again. It moves by
    // signing in once into its own folder.
    Err(IpcError::InvalidInput(
        "Sign in to this account once to move it to its own folder, then use it.".to_string(),
    ))
}

/// Register the currently active Claude Code login as a new managed slot.
///
/// The user is expected to have logged in with Claude Code normally first —
/// this call takes no credentials of its own and makes no network call, it
/// only captures whatever is live right now. Refuses if nothing is live
/// ([`IpcError::Credential`]) or if that login is already registered under a
/// different slot, compared by credential identity rather than email so a
/// rotated access token can't fool the check ([`IpcError::Internal`]).
#[tauri::command]
pub async fn add_current_account(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    alias: Option<String>,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let setting = claude_binary_setting(&state);
    off_runtime(move || adopt_default_login(setting, alias)).await??;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Open an isolated, visible terminal running `claude auth login`, wait for
/// the user to complete the browser OAuth round trip, and register the
/// captured credential as a new managed slot.
///
/// Unlike [`add_current_account`], this takes no dependency on anything
/// already live on this machine — [`login::interactive_login`] runs the whole
/// flow against a throwaway `CLAUDE_CONFIG_DIR`, so the user's existing
/// active login (if any) is never read, never touched, and never at risk.
/// [`switcher::add_oauth_credential`] then registers the captured blob
/// exactly like [`add_current_account`] registers a live login — same
/// identity-based duplicate detection — except it never activates the new
/// slot (the user is adding an account, not switching to it) and never
/// writes anything to the live credential/config.
///
/// Failure modes the UI is expected to branch on (see the
/// `From<login::LoginError> for IpcError` impl above for the full mapping):
/// a closed terminal (calm, not an error), a 10-minute timeout, `claude` not
/// on PATH, no terminal emulator available (Linux only — the UI falls back
/// to [`add_token`] here), a credential that landed but didn't validate, or
/// the resulting account already being registered.
///
/// Always returns a freshly-read [`Snapshot`] via [`snapshot_uncached`] — the
/// cache predates this call's own effect by definition, so serving it would
/// show the user the state from before their own sign-in.
#[tauri::command]
pub async fn interactive_login(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    alias: Option<String>,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let setting = state
        .settings
        .snapshot()
        .settings
        .claude_binary_path
        .clone()
        .map(std::path::PathBuf::from);
    let dir = off_runtime(new_profile_dir).await??;
    let signed_in = match login::sign_in_to_profile(setting, dir.clone()).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            discard_new_profile_dir(&dir);
            return Err(error.into());
        }
    };
    refuse_if_recovery_required()?;
    let identity = signed_in.identity;
    let new = crate::profile_registry::NewProfile {
        email: identity.email,
        account_uuid: identity.account_uuid,
        organization_uuid: identity.organization_uuid,
        organization_name: identity.organization_name,
        config_dir: Some(dir.clone()),
        alias,
    };
    if let Err(error) = off_runtime(move || crate::profile_registry::register(&new)).await? {
        // Already registered elsewhere: this folder is a duplicate login.
        discard_new_profile_dir(&dir);
        return Err(error.into());
    }
    off_runtime(refresh_claude_command).await?;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Re-authenticate one existing slot without creating a duplicate account.
/// The login runs in the same isolated temporary config used by
/// [`interactive_login`]; the captured identity must match `account_number`
/// before the backend writes any credential bytes.
#[tauri::command]
pub async fn relogin_account(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    account_number: u32,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let setting = state
        .settings
        .snapshot()
        .settings
        .claude_binary_path
        .clone()
        .map(std::path::PathBuf::from);
    let target = switcher::read_accounts()?
        .into_iter()
        .find(|a| a.number == account_number)
        .ok_or_else(|| IpcError::Internal(format!("no account in slot {account_number}")))?;
    if let Some(profile) = target.profile.clone() {
        let Some(dir) = profile.config_dir else {
            return Err(IpcError::InvalidInput(
                "This account uses Claude Code's own folder. Run claude and sign in with /login."
                    .to_string(),
            ));
        };
        let signed_in = login::sign_in_to_profile(setting, dir.clone()).await?;
        if !switcher::folder_matches(&target, &signed_in.identity) {
            return Err(IpcError::InvalidInput(format!(
                "That signed in as a different account. Sign in again as {}.",
                crate::model::mask_email(&target.email)
            )));
        }
        let identity = signed_in.identity;
        let new = crate::profile_registry::NewProfile {
            email: identity.email,
            account_uuid: identity.account_uuid,
            organization_uuid: identity.organization_uuid,
            organization_name: identity.organization_name,
            config_dir: Some(dir),
            alias: None,
        };
        off_runtime(move || crate::profile_registry::register(&new)).await??;
        off_runtime(refresh_claude_command).await?;
        let snap = snapshot_uncached(&state).await?;
        crate::poller::publish_snapshot(&app, &snap);
        return Ok(snap);
    }
    // A v0.3 account: sign in once into its own folder. Its stored copy is
    // deleted only after the new login is verified as this same account.
    let dir = off_runtime(new_profile_dir).await??;
    let signed_in = match login::sign_in_to_profile(setting, dir.clone()).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            discard_new_profile_dir(&dir);
            return Err(error.into());
        }
    };
    if !switcher::folder_matches(&target, &signed_in.identity) {
        discard_new_profile_dir(&dir);
        return Err(IpcError::InvalidInput(format!(
            "That signed in as a different account. Sign in as {} to move this one.",
            crate::model::mask_email(&target.email)
        )));
    }
    let new = crate::migration::new_profile(&signed_in.identity, Some(dir));
    off_runtime(move || crate::profile_registry::register(&new)).await??;
    off_runtime(|| {
        clean_up_moved_vault_copies();
        refresh_claude_command();
    })
    .await?;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Hold an account out of, or return it to, automatic switch rotation.
///
/// The account stays managed and remains a valid explicit switch target
/// either way — this only affects auto-switch and the usage-aware
/// strategies. Refuses to disable the currently active account: that would
/// leave auto-switch with no valid home to land on next.
#[tauri::command]
pub async fn set_account_enabled(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    account_number: u32,
    enabled: bool,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    switcher::set_account_enabled(account_number, enabled)?;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Run blocking work (a registry mutation may wait up to
/// [`crate::locking::DEFAULT_TIMEOUT`] on the vault lock; waking WSL boots a
/// VM) off the async runtime's worker threads.
async fn off_runtime<T, F>(work: F) -> IpcResult<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| IpcError::Internal(format!("background task failed: {error}")))
}

/// Set (or, with `None`/blank, clear) an account's display alias.
///
/// Trimmed; longer than [`switcher::MAX_ALIAS_CHARS`] characters is
/// [`IpcError::InvalidInput`].
#[tauri::command]
pub async fn set_account_alias(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    account_number: u32,
    alias: Option<String>,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let task = move || switcher::set_account_alias(account_number, alias.as_deref());
    off_runtime(task).await??;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Stop managing an account and delete its stored credential and config.
///
/// Refuses the active account ([`IpcError::CannotRemoveActive`]). The
/// registry is rewritten before any file is deleted; if a file then cannot be
/// deleted the account is still gone, the fresh state is published anyway,
/// and the error ([`IpcError::Credential`]) says the files remain.
#[tauri::command]
pub async fn remove_account(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    account_number: u32,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let task = move || {
        let result = switcher::remove_account(account_number);
        refresh_claude_command();
        result
    };
    if let Err(error) = off_runtime(task).await? {
        if matches!(error, SwitchError::RemovedWithLeftovers(..)) {
            // The registry did change: show that before reporting the rest.
            if let Ok(snap) = snapshot_uncached(&state).await {
                crate::poller::publish_snapshot(&app, &snap);
            }
        }
        return Err(error.into());
    }
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Replace the rotation order. `order` must list every account number
/// exactly once ([`IpcError::InvalidInput`] otherwise). Both the account list
/// and the "next available" strategy follow it.
#[tauri::command]
pub async fn reorder_accounts(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    order: Vec<u32>,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let task = move || switcher::reorder_accounts(&order);
    off_runtime(task).await??;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Start a sleeping WSL distro and re-read whether it holds Claude Code
/// credentials, then return a fresh snapshot.
///
/// **User-initiated only** — this boots a Linux VM, which no polling path is
/// ever allowed to do (see [`crate::wsl::wake_and_read`]). `env_id` is the
/// `wsl:{name}` id from the snapshot; anything else, a distro that is not
/// detected, or any platform other than Windows is [`IpcError::InvalidInput`].
#[tauri::command]
pub async fn wake_environment(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    env_id: String,
) -> IpcResult<Snapshot> {
    if !cfg!(target_os = "windows") {
        return Err(IpcError::InvalidInput(
            "WSL is only available on Windows".to_string(),
        ));
    }
    let Some(name) = env_id.strip_prefix("wsl:").map(str::to_string) else {
        return Err(IpcError::InvalidInput(format!(
            "{env_id} is not a WSL environment"
        )));
    };
    let found = off_runtime(move || wake_distro(&name)).await??;
    log::info!("woke {env_id}; Claude Code credentials found: {found}");
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

fn is_wsl_named(env: &Environment, name: &str) -> bool {
    env.kind == crate::model::EnvKind::Wsl && env.id.strip_prefix("wsl:") == Some(name)
}

/// Blocking body of [`wake_environment`]: confirm `name` is a detected
/// distro, then wake it.
fn wake_distro(name: &str) -> IpcResult<bool> {
    let environments = crate::wsl::detect_environments();
    if !environments.iter().any(|env| is_wsl_named(env, name)) {
        return Err(IpcError::InvalidInput(format!(
            "no WSL distro named {name} was found"
        )));
    }
    crate::wsl::wake_and_read(name).map_err(|error| match &error {
        crate::wsl::WslError::Unsupported => IpcError::InvalidInput(error.to_string()),
        crate::wsl::WslError::Spawn { .. } => IpcError::Internal(error.to_string()),
    })
}

// ─── application state ───────────────────────────────────────────────────────

/// Long-lived state, created once at startup and injected into commands.
pub struct AppState {
    /// Most recent snapshot, with the instant it was fetched.
    ///
    /// The usage endpoint budgets requests **per access token**, and callers
    /// are plural: the poller, the dashboard window, and the tray popover —
    /// each of which React StrictMode double-invokes in dev. A live run
    /// produced five HTTP 429s inside 1.5 seconds from exactly that pile-up.
    ///
    /// Removing the frontend's polling timers was necessary but not
    /// sufficient, because every window still fetches once on mount. So the
    /// coalescing lives here, at the process that actually owns the budget:
    /// concurrent readers share one recent result instead of each spending a
    /// request.
    pub snapshot_cache: std::sync::Mutex<Option<(std::time::Instant, Snapshot)>>,
    /// Where settings and the history database live.
    pub data_dir: std::path::PathBuf,
    /// Local usage history. `None` if the database could not be opened — the
    /// app must still run without history rather than refusing to start.
    pub history: Option<crate::history::HistoryStore>,
    pub settings: crate::settings::SettingsStore,
    /// Canonical daemon phase used by hydration and global status events.
    pub daemon_status: crate::runtime::DaemonStatusStore,
    /// When the user last forced a fetch with the Refresh control.
    ///
    /// The Refresh button spends a real request against the same per-token
    /// budget the poller is carefully rationing, and it is the one path a user
    /// can trigger as fast as they can click. Held here rather than in the
    /// frontend because both windows can press it: two per-window cooldowns
    /// would let the popover and the dashboard alternate and defeat each other.
    pub last_manual_refresh: std::sync::Mutex<Option<std::time::Instant>>,
}

impl AppState {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        let now = chrono::Utc::now();
        let settings = crate::settings::SettingsStore::new(data_dir.clone(), now);
        let policy = {
            let receiver = settings.subscribe_policy();
            let current = receiver.borrow().clone();
            current
        };
        crate::live_swap::set_enabled(settings.snapshot().settings.switch_running_sessions);
        let daemon_status = crate::runtime::DaemonStatusStore::new(&policy, now);
        if let Some(detail) = crate::switch_transaction::recovery_requirement() {
            let _ = daemon_status.transition(
                policy.revision,
                crate::runtime::DaemonPhase::RecoveryRequired { detail },
                now,
            );
        }
        let history = match crate::history::HistoryStore::open(&data_dir) {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("history unavailable ({e}); charts will be empty this session");
                None
            }
        };
        Self {
            data_dir,
            history,
            settings,
            daemon_status,
            snapshot_cache: std::sync::Mutex::new(None),
            last_manual_refresh: std::sync::Mutex::new(None),
        }
    }

    /// `Some(remaining)` while a manual refresh is still on cooldown.
    ///
    /// A poisoned lock reports "cooling down" rather than "go ahead": the
    /// conservative answer protects the request budget, and the alternative
    /// would turn a panic elsewhere into an unthrottled refresh path.
    pub fn manual_refresh_cooldown(
        &self,
        window: std::time::Duration,
    ) -> Option<std::time::Duration> {
        let guard = match self.last_manual_refresh.lock() {
            Ok(g) => g,
            Err(_) => return Some(window),
        };
        let last = (*guard)?;
        window.checked_sub(last.elapsed())
    }

    /// Start the manual-refresh cooldown from now.
    pub fn mark_manual_refresh(&self) {
        if let Ok(mut guard) = self.last_manual_refresh.lock() {
            *guard = Some(std::time::Instant::now());
        }
    }

    /// A cached snapshot, if it is fresh enough to serve.
    pub fn cached_snapshot(&self, max_age: std::time::Duration) -> Option<Snapshot> {
        let guard = self.snapshot_cache.lock().ok()?;
        let (at, snap) = guard.as_ref()?;
        (at.elapsed() <= max_age).then(|| snap.clone())
    }

    /// Record a freshly-fetched snapshot for other callers to reuse.
    pub fn store_snapshot(&self, snap: &Snapshot) {
        if let Ok(mut guard) = self.snapshot_cache.lock() {
            *guard = Some((std::time::Instant::now(), snap.clone()));
        }
    }
}

/// How long a snapshot may be reused before another fetch is worth spending.
///
/// Sized to absorb the startup pile-up (multiple windows mounting at once)
/// without meaningfully staling the display — the poller refreshes on its own
/// adaptive cadence regardless, and pushes the result to every window.
const SNAPSHOT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(20);

// ─── history (read-only) ─────────────────────────────────────────────────────

/// Headline history figures backing the stat row on the History screen.
#[tauri::command]
pub fn history_summary(
    state: tauri::State<'_, AppState>,
    days: Option<i64>,
) -> IpcResult<Option<crate::history::HistorySummary>> {
    let Some(h) = state.history.as_ref() else {
        return Ok(None);
    };
    h.summary(days.unwrap_or(30))
        .map(Some)
        .map_err(|e| IpcError::Internal(e.to_string()))
}

/// Per-day min/max/avg for one account, for the burn-rate charts.
///
/// Returns an empty series rather than an error when there is no history yet —
/// a fresh install has nothing to chart, which is a normal state, not a fault.
#[tauri::command]
pub fn history_series(
    state: tauri::State<'_, AppState>,
    account_key: String,
    days: Option<i64>,
) -> IpcResult<Vec<crate::history::DayStat>> {
    let Some(h) = state.history.as_ref() else {
        return Ok(Vec::new());
    };
    h.daily_rollup(&account_key, days.unwrap_or(30))
        .map_err(|e| IpcError::Internal(e.to_string()))
}

/// Raw samples for one account over a recent window, for the Dashboard.
///
/// Unlike [`history_series`], this keeps the intraday shape, the 5h/7d split
/// and the per-model scoped windows instead of averaging them into one number
/// per day. Same failure posture: no history yet is an empty series, not an
/// error, so the UI can say "no history yet" honestly.
#[tauri::command]
pub fn history_samples(
    state: tauri::State<'_, AppState>,
    account_key: String,
    hours: Option<i64>,
) -> IpcResult<Vec<crate::history::Sample>> {
    let Some(h) = state.history.as_ref() else {
        return Ok(Vec::new());
    };
    // Clamped to the raw retention window the poller prunes to: asking for
    // more would silently return only what survived pruning, which reads as
    // a gap in usage.
    let retention_days = state.settings.snapshot().settings.history_retention_days;
    let hours = hours.unwrap_or(24).clamp(1, retention_days * 24);
    let until = chrono::Utc::now();
    let since = until - chrono::Duration::hours(hours);
    h.series(&account_key, since, until)
        .map_err(|e| IpcError::Internal(e.to_string()))
}

/// Whether history is actually available this session.
///
/// The UI must be able to say "no history yet" honestly instead of rendering an
/// empty chart that looks like zero usage.
#[tauri::command]
pub fn history_available(state: tauri::State<'_, AppState>) -> bool {
    state.history.is_some()
}

// ─── about ───────────────────────────────────────────────────────────────────

/// Absolute paths to the files this app owns, for the About section. Resolved
/// here because only this side knows where they actually landed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataLocations {
    /// This app's own account vault — never another tool's directory.
    pub account_vault: String,
    /// Settings file and history database.
    pub data_dir: String,
    pub log_file: String,
}

/// Where this app keeps its files. Pure path resolution with no I/O, so it
/// cannot fail and returns directly rather than an [`IpcResult`].
#[tauri::command]
pub fn data_locations(state: tauri::State<'_, AppState>) -> DataLocations {
    data_locations_in(&state.data_dir)
}

/// Body of [`data_locations`], taking the dir directly so it is testable
/// without a live `tauri::State`.
fn data_locations_in(data_dir: &std::path::Path) -> DataLocations {
    DataLocations {
        account_vault: crate::paths::backup_root().display().to_string(),
        data_dir: data_dir.display().to_string(),
        log_file: crate::log_path().display().to_string(),
    }
}

/// What resolving the `claude` binary turned up, for the About section and
/// the settings field itself — the same answer login itself would act on,
/// never a hypothetical.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeBinaryStatus {
    pub found: bool,
    /// Resolved path, present only when `found`.
    pub path: Option<String>,
    /// [`crate::claude_cli::Source::label`], present only when `found`.
    pub source: Option<String>,
    /// The [`crate::claude_cli::NotFound`] sentence, present only when not
    /// `found` — this is the same text `login`'s own error carries.
    pub message: Option<String>,
}

/// Report where this app would launch `claude` from right now, and how.
///
/// Reads the persisted setting internally on purpose, rather than taking it
/// as a parameter: About must show the same truth login acts on, never a
/// hypothetical value the caller happens to be holding. Direct return, no
/// [`IpcResult`] — "not found" is the answer this command gives, not an
/// error it fails with.
#[tauri::command]
pub fn claude_binary_status(state: tauri::State<'_, AppState>) -> ClaudeBinaryStatus {
    let setting = state
        .settings
        .snapshot()
        .settings
        .claude_binary_path
        .clone()
        .map(std::path::PathBuf::from);
    claude_binary_status_from(crate::claude_cli::resolve(setting))
}

/// Body of [`claude_binary_status`], taking the resolution result directly
/// so it is testable without a live `tauri::State`.
fn claude_binary_status_from(
    result: Result<crate::claude_cli::Resolved, crate::claude_cli::NotFound>,
) -> ClaudeBinaryStatus {
    match result {
        Ok(resolved) => ClaudeBinaryStatus {
            found: true,
            path: Some(resolved.path.display().to_string()),
            source: Some(resolved.source.label().to_string()),
            message: None,
        },
        Err(not_found) => ClaudeBinaryStatus {
            found: false,
            path: None,
            source: None,
            message: Some(not_found.to_string()),
        },
    }
}

// ─── the claude command ──────────────────────────────────────────────────────
// Installing the `claude` shim on PATH. Explicit and reversible; nothing here
// reads or writes a Claude credential.

/// One `claude-<slug>` launcher, for the settings list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliLauncher {
    pub slug: String,
    pub account_number: u32,
    pub command: String,
}

/// Whether the `claude` command is installed, and where.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    pub health: crate::cli_install::CliHealth,
    /// This build ships the shim. False in development builds, where the
    /// install button explains instead of failing.
    pub shim_available: bool,
    pub bin_dir: String,
    /// Full path of the `claude` copy, for editor settings that need one.
    pub command_path: String,
    pub launchers: Vec<CliLauncher>,
}

fn cli_status_now(launchers: Vec<CliLauncher>) -> CliStatus {
    let bin_dir = crate::paths::cc_logins_bin_dir();
    let command = bin_dir.join(crate::cli_install::exe_name(crate::shim_core::SHIM_STEM));
    CliStatus {
        health: crate::cli_install::health(&bin_dir),
        shim_available: crate::cli_install::bundled_shim().is_some(),
        bin_dir: bin_dir.display().to_string(),
        command_path: command.display().to_string(),
        launchers,
    }
}

fn claude_binary_setting(state: &AppState) -> Option<std::path::PathBuf> {
    state
        .settings
        .snapshot()
        .settings
        .claude_binary_path
        .clone()
        .map(std::path::PathBuf::from)
}

/// Register the login already signed in to Claude Code's default folder as
/// the default profile, after `claude auth status` confirms it. Nothing is
/// copied: the login stays where Claude Code keeps it. A v0.3 record for the
/// same account is upgraded in place and its stored copy deleted.
pub(crate) fn adopt_default_login(
    claude_setting: Option<std::path::PathBuf>,
    alias: Option<String>,
) -> IpcResult<u32> {
    let global = crate::paths::default_global_config_path();
    let identity = crate::auth_status::read_folder_identity(&global).ok_or_else(|| {
        IpcError::InvalidInput(
            "Claude Code isn't signed in on this computer. Add the account with Sign in instead."
                .to_string(),
        )
    })?;
    let claude = crate::claude_cli::resolve(claude_setting)
        .map_err(|not_found| IpcError::PrerequisiteMissing(not_found.to_string()))?;
    let status = crate::auth_status::check(&claude.path, None)
        .map_err(|error| IpcError::Internal(error.to_string()))?;
    let confirmed = status.is_claude_account()
        && status
            .email
            .as_deref()
            .is_some_and(|email| email.eq_ignore_ascii_case(&identity.email));
    if !confirmed {
        return Err(IpcError::InvalidInput(
            "Claude Code's default login couldn't be confirmed. Run claude once, then try again."
                .to_string(),
        ));
    }
    let mut new = crate::migration::new_profile(&identity, None);
    new.alias = alias;
    let slot = crate::profile_registry::register(&new)?;
    clean_up_moved_vault_copies();
    refresh_claude_command();
    Ok(slot)
}

/// Delete the v0.3 stored copies of accounts that have moved to a folder
/// (flagged `legacyVault` when their record was upgraded). A failure keeps
/// the flag, so the next start tries again.
pub(crate) fn clean_up_moved_vault_copies() {
    let data = switcher::read_sequence_data().unwrap_or_default();
    for (number, email) in crate::migration::vault_cleanup_due(&data) {
        match switcher::delete_vault_copy(number, &email) {
            Ok(()) => {
                let cleared = crate::profile_registry::edit(|data| {
                    crate::migration::set_legacy_vault(data, number, false);
                    Ok(())
                });
                if let Err(error) = cleared {
                    log::warn!("vault copy of account {number} deleted; flag not cleared: {error}");
                }
            }
            Err(error) => log::warn!("vault copy of account {number} not deleted yet: {error}"),
        }
    }
}

/// Startup: finish what migration can do without the user. Adopts the
/// default login when a v0.3 record matches it, then retries any pending
/// vault deletions. Never runs while an interrupted v0.3 switch is still
/// being recovered, since that touches the same files.
pub(crate) fn startup_migration(claude_setting: Option<std::path::PathBuf>) {
    if crate::switch_transaction::recovery_requirement().is_some() {
        return;
    }
    let data = switcher::read_sequence_data().unwrap_or_default();
    let global = crate::paths::default_global_config_path();
    let candidate = crate::auth_status::read_folder_identity(&global)
        .and_then(|identity| crate::migration::default_adoption_candidate(&data, &identity));
    if candidate.is_some() {
        match adopt_default_login(claude_setting, None) {
            Ok(slot) => log::info!("account {slot} now uses Claude Code's default folder"),
            Err(error) => log::info!("default login not adopted yet: {error:?}"),
        }
    } else {
        clean_up_moved_vault_copies();
    }
}

/// Launchers of every ready profile account.
fn current_launchers() -> Vec<CliLauncher> {
    let data = switcher::read_sequence_data().unwrap_or_default();
    crate::profile_registry::launchers(&data)
        .into_iter()
        .map(|(slug, account_number)| CliLauncher {
            command: format!("{}-{slug}", crate::shim_core::SHIM_STEM),
            slug,
            account_number,
        })
        .collect()
}

/// Keep an installed `claude` command in step with this build and with the
/// accounts: refresh `shim.json` and the shim copies (one per launcher).
/// Never installs anything the user has not installed.
pub(crate) fn refresh_claude_command() {
    let data = switcher::read_sequence_data().unwrap_or_default();
    if crate::profile_registry::selected_number(&data).is_some() {
        if let Err(error) = crate::profile_registry::sync_shim(&data) {
            log::warn!("shim.json not updated: {error}");
        }
    }
    resync_shared_files(&data);
    let bin_dir = crate::paths::cc_logins_bin_dir();
    let installed = bin_dir
        .join(crate::cli_install::exe_name(crate::shim_core::SHIM_STEM))
        .exists();
    let Some(shim) = crate::cli_install::bundled_shim() else {
        return;
    };
    if !installed {
        return;
    }
    let slugs: Vec<String> = crate::profile_registry::launchers(&data)
        .into_iter()
        .map(|(slug, _)| slug)
        .collect();
    match crate::cli_install::materialize(&bin_dir, &shim, &slugs) {
        Ok(changed) if !changed.written.is_empty() || !changed.removed.is_empty() => {
            log::info!(
                "claude command copies refreshed: {} written, {} removed",
                changed.written.len(),
                changed.removed.len()
            )
        }
        Ok(_) => {}
        Err(error) => log::warn!("claude command refresh skipped: {error}"),
    }
}

/// Bring every profile's shared files (settings, CLAUDE.md, keybindings) up
/// to date with the default folder.
fn resync_shared_files(data: &serde_json::Map<String, serde_json::Value>) {
    let default_dir = crate::sys_env::default_claude_config_dir();
    let dirs = data
        .get("accounts")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|accounts| accounts.values())
        .filter_map(serde_json::Value::as_object)
        .filter_map(crate::profile_registry::profile_of)
        .filter_map(|profile| profile.config_dir);
    for dir in dirs {
        match crate::sharing::resync(std::path::Path::new(&dir), &default_dir) {
            Ok(conflicts) if !conflicts.is_empty() => log::warn!(
                "{} shared file(s) changed in both places; the profile's copies were saved beside them",
                conflicts.len()
            ),
            Ok(_) => {}
            Err(error) => log::warn!("shared file resync skipped: {error}"),
        }
    }
}

/// Create the folder a new account signs in to:
/// `~/.cc-logins/profiles/p<slot>-<suffix>`, seeded from the default profile.
fn new_profile_dir() -> IpcResult<String> {
    let data = switcher::read_sequence_data().unwrap_or_default();
    let slot = switcher::next_free_slot(&data);
    let name = crate::profiles::profile_dir_name(slot, &crate::profiles::random_suffix());
    let dir = crate::paths::profiles_root().join(name);
    let text = crate::profiles::normalize_profile_path(&crate::sys_env::home_dir(), &dir)
        .map_err(|error| IpcError::InvalidInput(error.to_string()))?;
    std::fs::create_dir_all(&text).map_err(io_internal)?;
    let path = std::path::Path::new(&text);
    crate::profiles::seed_profile(path).map_err(io_internal)?;
    // Settings, CLAUDE.md, agents and friends come along. A failure here
    // costs convenience, not the account, so it never blocks a sign-in.
    if let Err(error) =
        crate::sharing::share_into(path, &crate::sys_env::default_claude_config_dir())
    {
        log::warn!("could not share settings into the new profile: {error}");
    }
    Ok(text)
}

/// Remove a folder created for a sign-in that did not produce a new account.
/// Only ever a folder this app just created under its own profiles root.
fn discard_new_profile_dir(dir: &str) {
    let path = std::path::Path::new(dir);
    if !path.starts_with(crate::paths::profiles_root()) {
        return;
    }
    // Links first, so the recursive delete below can never reach through one.
    if let Err(error) = crate::sharing::unshare(path) {
        log::warn!("could not unlink shared folders: {error}");
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(path) {
        log::warn!("could not remove unused profile folder: {error}");
    }
}

fn io_internal(error: std::io::Error) -> IpcError {
    IpcError::Internal(error.to_string())
}

/// Report whether new terminals get this app's `claude`. Runs the user's
/// login shell on macOS and Linux, so it is off the async runtime.
#[tauri::command]
pub async fn cli_status() -> IpcResult<CliStatus> {
    off_runtime(|| cli_status_now(current_launchers())).await
}

/// Copy the shim into `~/.cc-logins/bin` and put that first on `PATH`.
#[tauri::command]
pub async fn install_cli(state: tauri::State<'_, AppState>) -> IpcResult<CliStatus> {
    let claude_binary = state
        .settings
        .snapshot()
        .settings
        .claude_binary_path
        .clone()
        .map(std::path::PathBuf::from);
    off_runtime(move || install_cli_blocking(claude_binary)).await?
}

fn install_cli_blocking(claude_binary: Option<std::path::PathBuf>) -> IpcResult<CliStatus> {
    let shim = crate::cli_install::bundled_shim().ok_or_else(|| {
        IpcError::PrerequisiteMissing(
            "This build doesn't include the claude command. Install CC Logins from a              release to use it."
                .to_string(),
        )
    })?;
    let bin_dir = crate::paths::cc_logins_bin_dir();
    // The shim reads this before anything else; write it first so the very
    // first `claude` after install already finds the configured binary.
    crate::profiles::update_shim_config(|config| config.claude_binary = claude_binary)
        .map_err(io_internal)?;
    let launchers = current_launchers();
    let slugs: Vec<String> = launchers.iter().map(|l| l.slug.clone()).collect();
    crate::cli_install::materialize(&bin_dir, &shim, &slugs).map_err(io_internal)?;
    crate::cli_install::install_path(&bin_dir).map_err(io_internal)?;
    Ok(cli_status_now(launchers))
}

/// Take the `claude` command off `PATH` and remove the copies. Profile
/// folders and their history are untouched.
#[tauri::command]
pub async fn uninstall_cli() -> IpcResult<CliStatus> {
    off_runtime(|| {
        let bin_dir = crate::paths::cc_logins_bin_dir();
        crate::cli_install::uninstall_path(&bin_dir).map_err(io_internal)?;
        crate::cli_install::clear_bin_dir(&bin_dir).map_err(io_internal)?;
        Ok(cli_status_now(current_launchers()))
    })
    .await?
}

// ─── settings ────────────────────────────────────────────────────────────────

const SETTINGS_UPDATED_EVENT: &str = "settings://updated";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateSettingsInput {
    pub expected_revision: u64,
    pub patch: crate::settings::SettingsPatch,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnoozeAutoSwitchInput {
    pub duration_seconds: u64,
}

#[tauri::command]
pub fn get_settings(state: tauri::State<'_, AppState>) -> crate::settings::SettingsSnapshot {
    get_settings_from(&state)
}

#[tauri::command]
pub fn get_daemon_status(state: tauri::State<'_, AppState>) -> crate::runtime::DaemonStatus {
    state.daemon_status.snapshot()
}

#[tauri::command]
pub fn update_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    input: UpdateSettingsInput,
) -> IpcResult<crate::settings::SettingsSnapshot> {
    let updated = update_settings_at(&state, input, chrono::Utc::now(), |snapshot| {
        app.emit(SETTINGS_UPDATED_EVENT, snapshot)
    })?;
    // The tray menu's percentages follow the used/left display setting; the
    // last snapshot, of any age, is enough to relabel them.
    if let Some(snapshot) = state.cached_snapshot(std::time::Duration::MAX) {
        crate::tray_menu::schedule_update(&app, &snapshot);
    }
    Ok(updated)
}

/// Turn the opt-in live swap (Settings -> Switch running sessions too) on or
/// off. See [`crate::live_swap`].
///
/// On: the default account first gets a folder of its own (its login is
/// copied there; nothing is signed out), then the selected account is
/// swapped into `~/.claude`. Off: the live login goes back to its folder and
/// plain `claude` follows the folder selection again.
#[tauri::command]
pub async fn set_switch_running_sessions(
    app: tauri::AppHandle,
    enabled: bool,
) -> IpcResult<crate::settings::SettingsSnapshot> {
    let state = app.state::<AppState>();
    refuse_if_recovery_required()?;
    if enabled {
        let data = switcher::read_sequence_data().unwrap_or_default();
        if crate::live_swap::default_without_folder(&data).is_some() {
            let dir = off_runtime(new_profile_dir).await??;
            let moving = dir.clone();
            match off_runtime(move || crate::live_swap::move_default_into(&moving)).await? {
                Ok(Some(_)) => {}
                Ok(None) => discard_new_profile_dir(&dir),
                Err(error) => {
                    discard_new_profile_dir(&dir);
                    return Err(error.into());
                }
            }
        }
        crate::live_swap::set_enabled(true);
        let selected = crate::profile_registry::selected_number(
            &switcher::read_sequence_data().unwrap_or_default(),
        );
        if let Some(number) = selected {
            crate::poller::publish_switching(&app);
            if let Err(error) = off_runtime(move || crate::live_swap::swap_to(number)).await? {
                crate::live_swap::set_enabled(false);
                return Err(error.into());
            }
        }
    } else {
        crate::live_swap::set_enabled(false);
        if let Err(error) = off_runtime(crate::live_swap::send_live_home).await? {
            log::warn!("live login not handed back to its folder: {error}");
        }
    }
    off_runtime(refresh_claude_command).await?;

    let current = state.settings.snapshot();
    let patch = crate::settings::SettingsPatch {
        switch_running_sessions: Some(enabled),
        ..Default::default()
    };
    let updated = match state
        .settings
        .update(current.revision, patch, chrono::Utc::now())
    {
        Ok(updated) => updated,
        Err(error) => {
            crate::live_swap::set_enabled(current.settings.switch_running_sessions);
            off_runtime(refresh_claude_command).await?;
            return Err(error.into());
        }
    };
    let _ = app.emit(SETTINGS_UPDATED_EVENT, &updated);
    if let Ok(snap) = snapshot_uncached(&state).await {
        crate::poller::publish_snapshot(&app, &snap);
    }
    Ok(updated)
}

#[tauri::command]
pub fn snooze_auto_switch(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    input: SnoozeAutoSwitchInput,
) -> IpcResult<crate::settings::SettingsSnapshot> {
    snooze_auto_switch_at(&state, input, chrono::Utc::now(), |snapshot| {
        app.emit(SETTINGS_UPDATED_EVENT, snapshot)
    })
}

#[tauri::command]
pub fn resume_auto_switch(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> IpcResult<crate::settings::SettingsSnapshot> {
    resume_auto_switch_at(&state, chrono::Utc::now(), |snapshot| {
        app.emit(SETTINGS_UPDATED_EVENT, snapshot)
    })
}

fn get_settings_from(state: &AppState) -> crate::settings::SettingsSnapshot {
    state.settings.snapshot()
}

fn update_settings_at<E, F>(
    state: &AppState,
    input: UpdateSettingsInput,
    now: chrono::DateTime<chrono::Utc>,
    emit: F,
) -> IpcResult<crate::settings::SettingsSnapshot>
where
    E: std::fmt::Display,
    F: FnOnce(&crate::settings::SettingsSnapshot) -> Result<(), E>,
{
    // The live swap moves logins when it changes, so it only ever changes
    // through `set_switch_running_sessions`.
    let mut patch = input.patch;
    patch.switch_running_sessions = None;
    let snapshot = state.settings.update(input.expected_revision, patch, now)?;
    emit(&snapshot).map_err(|error| IpcError::Internal(error.to_string()))?;
    Ok(snapshot)
}

fn snooze_auto_switch_at<E, F>(
    state: &AppState,
    input: SnoozeAutoSwitchInput,
    now: chrono::DateTime<chrono::Utc>,
    emit: F,
) -> IpcResult<crate::settings::SettingsSnapshot>
where
    E: std::fmt::Display,
    F: FnOnce(&crate::settings::SettingsSnapshot) -> Result<(), E>,
{
    if input.duration_seconds == 0 {
        return Err(IpcError::Internal(
            "snooze duration must be greater than zero".to_string(),
        ));
    }
    let snapshot = state
        .settings
        .snooze(std::time::Duration::from_secs(input.duration_seconds), now)?;
    emit(&snapshot).map_err(|error| IpcError::Internal(error.to_string()))?;
    Ok(snapshot)
}

fn resume_auto_switch_at<E, F>(
    state: &AppState,
    now: chrono::DateTime<chrono::Utc>,
    emit: F,
) -> IpcResult<crate::settings::SettingsSnapshot>
where
    E: std::fmt::Display,
    F: FnOnce(&crate::settings::SettingsSnapshot) -> Result<(), E>,
{
    let snapshot = state.settings.resume(now)?;
    emit(&snapshot).map_err(|error| IpcError::Internal(error.to_string()))?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use chrono::{DateTime, Duration as ChronoDuration, Utc};

    use super::*;

    /// An `AppState` rooted in a temp dir.
    ///
    /// `AppState::new` opens the history database and reads settings, so it
    /// must never be pointed at a real data directory from a test — see
    /// `test_support::guard_real_store` for what that cost us once already.
    fn temp_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = AppState::new(dir.path().to_path_buf());
        (dir, state)
    }

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn update_settings_hydrates_a_revisioned_snapshot() {
        let (_dir, state) = temp_state();

        let snapshot = get_settings_from(&state);

        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.settings, crate::settings::Settings::default());
    }

    #[test]
    fn update_settings_rejects_stale_overwrites_with_a_structural_conflict() {
        let (_dir, state) = temp_state();
        update_settings_at(
            &state,
            UpdateSettingsInput {
                expected_revision: 0,
                patch: crate::settings::SettingsPatch {
                    threshold: Some(77),
                    ..Default::default()
                },
            },
            fixed_now(),
            |_| Ok::<(), String>(()),
        )
        .unwrap();

        let error = update_settings_at(
            &state,
            UpdateSettingsInput {
                expected_revision: 0,
                patch: crate::settings::SettingsPatch {
                    grace_seconds: Some(5),
                    ..Default::default()
                },
            },
            fixed_now(),
            |_| Ok::<(), String>(()),
        )
        .unwrap_err();

        assert!(matches!(
            &error,
            IpcError::SettingsConflict {
                expected_revision: 0,
                actual_revision: 1
            }
        ));
        assert_eq!(state.settings.snapshot().settings.threshold, 77);
        assert_eq!(state.settings.snapshot().settings.grace_seconds, 60);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "kind": "settingsConflict",
                "detail": { "expectedRevision": 0, "actualRevision": 1 }
            })
        );
    }

    #[test]
    fn update_settings_emits_only_after_the_successful_state_is_canonical() {
        let (dir, state) = temp_state();
        let emitted = Cell::new(false);

        let result = update_settings_at(
            &state,
            UpdateSettingsInput {
                expected_revision: 0,
                patch: crate::settings::SettingsPatch {
                    threshold: Some(79),
                    ..Default::default()
                },
            },
            fixed_now(),
            |snapshot| {
                assert_eq!(state.settings.snapshot(), *snapshot);
                assert_eq!(crate::settings::load(dir.path()), snapshot.settings);
                emitted.set(true);
                Ok::<(), String>(())
            },
        )
        .unwrap();

        assert!(emitted.get());
        assert_eq!(result.revision, 1);
    }

    #[test]
    fn update_settings_does_not_emit_after_a_failed_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("settings-parent-is-a-file");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let state = AppState {
            snapshot_cache: std::sync::Mutex::new(None),
            data_dir: blocked.clone(),
            history: None,
            settings: crate::settings::SettingsStore::new(blocked, fixed_now()),
            daemon_status: crate::runtime::DaemonStatusStore::new(
                &crate::runtime::RuntimePolicy::from_settings(
                    0,
                    &crate::settings::Settings::default(),
                    fixed_now(),
                ),
                fixed_now(),
            ),
            last_manual_refresh: std::sync::Mutex::new(None),
        };
        let emitted = Cell::new(false);

        let result = update_settings_at(
            &state,
            UpdateSettingsInput {
                expected_revision: 0,
                patch: crate::settings::SettingsPatch {
                    threshold: Some(79),
                    ..Default::default()
                },
            },
            fixed_now(),
            |_| {
                emitted.set(true);
                Ok::<(), String>(())
            },
        );

        assert!(result.is_err());
        assert!(!emitted.get());
    }

    #[test]
    fn snooze_auto_switch_uses_the_exact_requested_deadline() {
        let (_dir, state) = temp_state();

        let result = snooze_auto_switch_at(
            &state,
            SnoozeAutoSwitchInput {
                duration_seconds: 3600,
            },
            fixed_now(),
            |_| Ok::<(), String>(()),
        )
        .unwrap();

        assert_eq!(
            result.settings.auto_switch_paused_until,
            Some(fixed_now() + ChronoDuration::hours(1))
        );
        assert_eq!(result.revision, 1);
    }

    #[test]
    fn snooze_auto_switch_rejects_zero_without_emitting() {
        let (_dir, state) = temp_state();
        let emitted = Cell::new(false);

        let result = snooze_auto_switch_at(
            &state,
            SnoozeAutoSwitchInput {
                duration_seconds: 0,
            },
            fixed_now(),
            |_| {
                emitted.set(true);
                Ok::<(), String>(())
            },
        );

        assert!(result.is_err());
        assert!(!emitted.get());
        assert_eq!(state.settings.snapshot().revision, 0);
    }

    #[test]
    fn snooze_resume_clears_the_persisted_pause_and_emits_the_new_snapshot() {
        let (_dir, state) = temp_state();
        snooze_auto_switch_at(
            &state,
            SnoozeAutoSwitchInput {
                duration_seconds: 60,
            },
            fixed_now(),
            |_| Ok::<(), String>(()),
        )
        .unwrap();
        let emitted = Cell::new(false);

        let result = resume_auto_switch_at(&state, fixed_now(), |snapshot| {
            assert_eq!(snapshot.settings.auto_switch_paused_until, None);
            emitted.set(true);
            Ok::<(), String>(())
        })
        .unwrap();

        assert!(emitted.get());
        assert_eq!(result.settings.auto_switch_paused_until, None);
        assert_eq!(result.revision, 2);
    }

    #[test]
    fn a_fresh_state_allows_a_manual_refresh_immediately() {
        let (_dir, state) = temp_state();
        assert_eq!(
            state.manual_refresh_cooldown(MANUAL_REFRESH_COOLDOWN),
            None,
            "the first press must not be refused; nothing has been spent yet"
        );
    }

    #[test]
    fn a_manual_refresh_starts_a_cooldown_that_refuses_the_next_one() {
        let (_dir, state) = temp_state();
        state.mark_manual_refresh();

        let remaining = state
            .manual_refresh_cooldown(MANUAL_REFRESH_COOLDOWN)
            .expect("a refresh immediately after another must be refused");

        // Bounded on both sides: a zero remainder would let the UI report
        // "retry in 0s" while still refusing, and anything above the window
        // would mean the clock ran backwards.
        assert!(
            remaining > std::time::Duration::ZERO && remaining <= MANUAL_REFRESH_COOLDOWN,
            "remaining {remaining:?} outside (0, {MANUAL_REFRESH_COOLDOWN:?}]"
        );
    }

    #[test]
    fn the_cooldown_expires_rather_than_latching() {
        let (_dir, state) = temp_state();
        state.mark_manual_refresh();

        // A zero-length window is the same code path an elapsed one takes:
        // `checked_sub` returns None once elapsed >= window.
        assert_eq!(
            state.manual_refresh_cooldown(std::time::Duration::ZERO),
            None,
            "an elapsed cooldown must release, or Refresh would never work again"
        );
    }

    /// The user must never be able to out-poll the daemon's own most
    /// aggressive mode. `URGENT_INTERVAL_S` is the tightest cadence
    /// `poll_policy` will ever choose for itself, having been derived against
    /// the real endpoint; a manual control allowed to fire faster than that
    /// would be spending the budget on a schedule nothing reasoned about.
    #[test]
    fn a_held_down_refresh_cannot_beat_the_pollers_own_fastest_cadence() {
        let cooldown = MANUAL_REFRESH_COOLDOWN.as_secs_f64();
        let urgent = crate::poller::poll_policy::URGENT_INTERVAL_S;
        assert!(
            cooldown >= urgent,
            "manual refresh every {cooldown}s is faster than the poller's own \
             urgent cadence of {urgent}s"
        );
    }

    #[test]
    fn no_accounts_maps_to_not_configured_not_an_error() {
        // The first-run screen depends on this distinction: an unconfigured
        // machine is a normal state, not a failure to report.
        let mapped: IpcError = SwitchError::NoAccountsManaged.into();
        assert!(matches!(mapped, IpcError::NotConfigured));
    }

    #[test]
    fn ipc_errors_serialise_tagged_so_the_ui_can_branch() {
        let json = serde_json::to_string(&IpcError::NotConfigured).unwrap();
        assert!(json.contains("notConfigured"), "got {json}");

        let json = serde_json::to_string(&IpcError::Busy("locked".into())).unwrap();
        assert!(
            json.contains("busy") && json.contains("locked"),
            "got {json}"
        );
    }

    // -- LoginError -> IpcError mapping ---------------------------------------
    //
    // The UI (`describeInteractiveLoginError` in `src/App.tsx`) branches on
    // the tagged `kind` alone — these tests pin that structural contract,
    // not any message wording.

    #[test]
    fn login_cancelled_maps_to_cancelled_kind_specifically() {
        // This is the one regression that is silent and user-visible: if a
        // cancelled login stopped mapping to `Cancelled`, closing the
        // terminal would start rendering as an alarming failure instead of
        // quietly returning to rest.
        let mapped: IpcError = LoginError::Cancelled.into();
        assert!(matches!(mapped, IpcError::Cancelled), "got {mapped:?}");

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("cancelled"), "got {json}");
    }

    #[test]
    fn login_timed_out_maps_to_timed_out_kind_not_cancelled() {
        let mapped: IpcError = LoginError::TimedOut.into();
        assert!(matches!(mapped, IpcError::TimedOut(_)), "got {mapped:?}");
        assert!(!matches!(mapped, IpcError::Cancelled));

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("timedOut"), "got {json}");
    }

    #[test]
    fn login_claude_not_installed_maps_to_prerequisite_missing() {
        let mapped: IpcError = LoginError::ClaudeNotInstalled(Default::default()).into();
        assert!(
            matches!(mapped, IpcError::PrerequisiteMissing(_)),
            "got {mapped:?}"
        );

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("prerequisiteMissing"), "got {json}");
    }

    /// The whole point of giving `ClaudeNotInstalled` a payload: the actionable
    /// part of the message has to survive all the way to the frontend, which
    /// renders `detail` verbatim.
    #[test]
    fn login_claude_not_installed_detail_names_the_override_env_var() {
        let mapped: IpcError = LoginError::ClaudeNotInstalled(Default::default()).into();
        let json = serde_json::to_string(&mapped).unwrap();
        assert!(
            json.contains(crate::claude_cli::OVERRIDE_ENV),
            "detail should tell the user how to point the app at their install; got {json}"
        );
    }

    #[test]
    fn login_no_terminal_available_maps_to_its_own_kind_for_the_add_token_fallback() {
        let mapped: IpcError = LoginError::NoTerminalAvailable.into();
        assert!(
            matches!(mapped, IpcError::NoTerminalAvailable(_)),
            "got {mapped:?}"
        );

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("noTerminalAvailable"), "got {json}");
    }

    #[test]
    fn login_bad_credential_maps_to_credential_kind_and_stays_content_free() {
        let mapped: IpcError =
            LoginError::BadCredential("credential file did not contain a usable access token")
                .into();
        match mapped {
            IpcError::Credential(msg) => {
                assert!(msg.contains("could not be validated"), "got {msg}");
                assert!(
                    !msg.contains("accessToken\":\""),
                    "must never echo raw credential bytes"
                );
            }
            other => panic!("expected Credential, got {other:?}"),
        }
    }

    #[test]
    fn login_io_error_maps_to_internal() {
        let mapped: IpcError = LoginError::Io(std::io::Error::other("spawn failed")).into();
        assert!(matches!(mapped, IpcError::Internal(_)));
    }

    // -- SwitchError -> IpcError mapping (business-rule refusals) ------------

    #[test]
    fn switch_already_registered_maps_to_its_own_kind() {
        let mapped: IpcError = SwitchError::AlreadyRegistered("1".to_string()).into();
        assert!(
            matches!(mapped, IpcError::AlreadyRegistered(_)),
            "got {mapped:?}"
        );

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("alreadyRegistered"), "got {json}");
    }

    #[test]
    fn switch_cannot_disable_active_maps_to_its_own_kind() {
        let mapped: IpcError = SwitchError::CannotDisableActive("1".to_string()).into();
        assert!(
            matches!(mapped, IpcError::CannotDisableActive(_)),
            "got {mapped:?}"
        );

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("cannotDisableActive"), "got {json}");
    }

    #[test]
    fn switch_cannot_remove_active_maps_to_its_own_kind() {
        let mapped: IpcError = SwitchError::CannotRemoveActive("1".to_string()).into();
        assert!(
            matches!(mapped, IpcError::CannotRemoveActive(_)),
            "got {mapped:?}"
        );

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("cannotRemoveActive"), "got {json}");
    }

    #[test]
    fn switch_invalid_input_maps_to_its_own_kind_with_the_sentence_as_detail() {
        let mapped: IpcError = SwitchError::InvalidInput("too long".to_string()).into();
        assert_eq!(
            serde_json::to_value(mapped).unwrap(),
            serde_json::json!({ "kind": "invalidInput", "detail": "too long" })
        );
    }

    #[test]
    fn a_removal_that_left_files_behind_is_a_credential_error() {
        let mapped: IpcError =
            SwitchError::RemovedWithLeftovers("2".to_string(), "disk busy".to_string()).into();
        match mapped {
            IpcError::Credential(detail) => {
                assert!(detail.contains("was removed"), "got {detail}");
                assert!(detail.contains("disk busy"), "got {detail}");
            }
            other => panic!("expected Credential, got {other:?}"),
        }
    }

    #[test]
    fn switch_locking_maps_to_busy() {
        let underlying = crate::locking::LockingError::Timeout {
            path: std::path::PathBuf::from("/tmp/lock"),
        };
        let mapped: IpcError = SwitchError::Locking(underlying).into();
        assert!(matches!(mapped, IpcError::Busy(_)), "got {mapped:?}");
    }

    #[test]
    fn pending_switch_recovery_is_a_structured_ipc_error() {
        let mapped: IpcError =
            SwitchError::Transaction(crate::switch_transaction::TransactionError::RecoveryRequired)
                .into();
        assert!(matches!(mapped, IpcError::RecoveryRequired(_)));
        assert_eq!(
            serde_json::to_value(mapped).unwrap(),
            serde_json::json!({
                "kind": "recoveryRequired",
                "detail": "another switch transaction requires recovery"
            })
        );
    }

    #[test]
    fn switch_credential_store_problems_map_to_credential_kind() {
        for e in [
            SwitchError::Credential(crate::credentials::CredentialError::Write(
                "disk full".to_string(),
            )),
            SwitchError::CredentialRead,
            SwitchError::NoStoredCredentials("1".to_string()),
            SwitchError::NoStoredConfig("1".to_string()),
            SwitchError::InvalidBackupConfig("1".to_string()),
            SwitchError::EmptyActiveCredential("1".to_string()),
            SwitchError::Stash("write failed".to_string()),
            SwitchError::NoLiveCredential,
            SwitchError::InvalidCredential("bad json".to_string()),
        ] {
            let mapped: IpcError = e.into();
            assert!(matches!(mapped, IpcError::Credential(_)), "got {mapped:?}");
        }
    }

    // -- data locations (About section) --------------------------------------

    #[test]
    fn data_locations_reports_the_vault_and_data_dir_it_resolves() {
        // Takes the env lock and a vault override because `backup_root()`
        // refuses to resolve outside a temp dir under `cfg(test)`.
        let _lock = crate::test_support::env_lock();
        let vault = tempfile::tempdir().expect("temp dir");
        let _store = crate::test_support::StoreRootGuard::set(vault.path().to_path_buf());
        let data = tempfile::tempdir().expect("temp dir");

        let locations = data_locations_in(data.path());
        assert_eq!(locations.account_vault, vault.path().display().to_string());
        assert_eq!(locations.data_dir, data.path().display().to_string());
        assert!(
            locations.log_file.ends_with("app.log"),
            "got {}",
            locations.log_file
        );

        // The UI reads these by camelCase name.
        let json = serde_json::to_string(&locations).unwrap();
        assert!(json.contains("accountVault"), "got {json}");
        assert!(json.contains("logFile"), "got {json}");
    }

    // -- claude binary status (About / Settings) -----------------------------

    #[test]
    fn claude_binary_status_reports_the_path_and_source_label() {
        let resolved = crate::claude_cli::Resolved {
            path: std::path::PathBuf::from("/x/claude"),
            source: crate::claude_cli::Source::Setting,
        };

        let status = claude_binary_status_from(Ok(resolved));

        assert!(status.found);
        assert_eq!(status.path, Some("/x/claude".to_string()));
        assert_eq!(status.source, Some("setting".to_string()));
        assert_eq!(status.message, None);

        // The UI reads these by camelCase name.
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"found\":true"), "got {json}");
        assert!(json.contains("\"source\":\"setting\""), "got {json}");
    }

    #[test]
    fn claude_binary_status_carries_the_not_found_sentence() {
        let status = claude_binary_status_from(Err(crate::claude_cli::NotFound::default()));

        assert!(!status.found);
        assert_eq!(status.path, None);
        assert_eq!(status.source, None);
        let message = status.message.expect("carries the not-found sentence");
        assert!(
            message.contains(crate::claude_cli::OVERRIDE_ENV),
            "got {message}"
        );
    }

    #[test]
    fn switch_uncategorized_variants_map_to_internal() {
        for e in [
            SwitchError::UnknownAccount("99".to_string()),
            SwitchError::InvalidToken("token is empty".to_string()),
        ] {
            let mapped: IpcError = e.into();
            assert!(matches!(mapped, IpcError::Internal(_)), "got {mapped:?}");
        }
    }
}
