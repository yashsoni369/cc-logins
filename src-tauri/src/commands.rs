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
use tauri::Emitter;

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
    /// A lock is held by another process (very likely the `cswap` CLI).
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
            SwitchError::Refresh(crate::oauth_refresh::RefreshCoordinatorError::Lease(_)) => {
                IpcError::Busy(e.to_string())
            }
            SwitchError::Refresh(
                crate::oauth_refresh::RefreshCoordinatorError::ReloginRequired,
            ) => IpcError::ReloginRequired(e.to_string()),
            SwitchError::Refresh(
                crate::oauth_refresh::RefreshCoordinatorError::RefreshFailed(_)
                | crate::oauth_refresh::RefreshCoordinatorError::Usage(_),
            ) => IpcError::Unreachable(e.to_string()),
            // Credential-store problems: the store itself is unreadable,
            // missing, empty, or otherwise not trustworthy — as distinct
            // from a business-rule refusal below, where the store is fine
            // and the requested mutation just isn't valid right now.
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
            | SwitchError::Refresh(
                crate::oauth_refresh::RefreshCoordinatorError::Missing
                | crate::oauth_refresh::RefreshCoordinatorError::PersistenceFailed(_)
                | crate::oauth_refresh::RefreshCoordinatorError::InvalidCredential,
            ) => IpcError::Credential(e.to_string()),
            // Business-rule refusals: the requested mutation is invalid
            // given the current state, not an I/O or credential-store
            // failure. Each gets its own structural kind so the UI can
            // branch without reading the message.
            SwitchError::AlreadyRegistered(_) => IpcError::AlreadyRegistered(e.to_string()),
            SwitchError::CannotDisableActive(_) => IpcError::CannotDisableActive(e.to_string()),
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
            LoginError::ClaudeNotInstalled => IpcError::PrerequisiteMissing(e.to_string()),
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
pub async fn switch_account(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    account_number: u32,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    let accounts = switcher::read_accounts()?;
    let target = accounts
        .iter()
        .find(|a| a.number == account_number)
        .ok_or_else(|| IpcError::Internal(format!("no account in slot {account_number}")))?;

    // Show the switch in flight before mutating anything, so a slow swap
    // doesn't leave the tray sitting on the outgoing account's stale number.
    crate::poller::publish_switching(&app);

    switcher::switch_to(target).await?;

    // Bypass the cache: it holds the pre-switch state by definition, and
    // showing the user the state from before their own action is a lie.
    let snap = snapshot_uncached(&state).await?;

    // The credential change already succeeded above; publish_snapshot never
    // fails the caller, so a tray/emit hiccup here can't misreport this
    // switch as failed.
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
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
    switcher::add_current_account(alias.as_deref())?;
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
    let outcome = login::interactive_login().await?;
    refuse_if_recovery_required()?;
    switcher::add_oauth_credential(
        &outcome.credentials,
        outcome.email.as_deref(),
        alias.as_deref(),
    )?;
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
    let outcome = login::interactive_login().await?;
    refuse_if_recovery_required()?;
    switcher::replace_oauth_credential(
        account_number,
        &outcome.credentials,
        outcome.uuid.as_deref(),
        outcome.email.as_deref(),
        outcome.organization_uuid.as_deref(),
    )?;
    let snap = snapshot_uncached(&state).await?;
    crate::poller::publish_snapshot(&app, &snap);
    Ok(snap)
}

/// Register a setup-token or managed API key as a new slot.
///
/// Useful without any prior Claude Code login on this machine. The token kind
/// (managed API key vs OAuth setup-token) is auto-detected; an omitted
/// `email` gets a synthesized `setup-token-{slot}@token.local` /
/// `api-key-{slot}@token.local` address, matching the CLI's own convention.
/// An obviously malformed token is rejected before the credential lock is
/// even taken.
#[tauri::command]
pub async fn add_token(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    token: String,
    email: Option<String>,
    alias: Option<String>,
) -> IpcResult<Snapshot> {
    refuse_if_recovery_required()?;
    switcher::add_token(&token, email.as_deref(), alias.as_deref())?;
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

// NOTE: `switcher::import_from_cswap` is deliberately NOT exposed as a command.
//
// It exists as a one-off migration utility, not a product feature. Offering
// "import from cswap" in the UI would frame this app as a companion to that
// CLI rather than a standalone tool, which is not what it is. The function
// stays (it is tested, and useful for seeding a vault during development) but
// nothing user-facing reaches it.

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
    // Clamped to the raw retention window: asking for more would silently
    // return only what survived pruning, which reads as a gap in usage.
    let hours = hours
        .unwrap_or(24)
        .clamp(1, crate::history::DEFAULT_RAW_RETENTION_DAYS * 24);
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
    /// This app's own account vault — never the `cswap` CLI's directory.
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
    update_settings_at(&state, input, chrono::Utc::now(), |snapshot| {
        app.emit(SETTINGS_UPDATED_EVENT, snapshot)
    })
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
    let snapshot = state
        .settings
        .update(input.expected_revision, input.patch, now)?;
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
        let mapped: IpcError = LoginError::ClaudeNotInstalled.into();
        assert!(
            matches!(mapped, IpcError::PrerequisiteMissing(_)),
            "got {mapped:?}"
        );

        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("prerequisiteMissing"), "got {json}");
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
    fn switch_locking_maps_to_busy() {
        let underlying = crate::locking::LockingError::Timeout {
            path: std::path::PathBuf::from("/tmp/lock"),
        };
        let mapped: IpcError = SwitchError::Locking(underlying).into();
        assert!(matches!(mapped, IpcError::Busy(_)), "got {mapped:?}");
    }

    #[test]
    fn manual_relogin_required_is_a_structured_ipc_error() {
        let mapped: IpcError =
            SwitchError::Refresh(crate::oauth_refresh::RefreshCoordinatorError::ReloginRequired)
                .into();
        assert!(matches!(mapped, IpcError::ReloginRequired(_)));
        assert_eq!(
            serde_json::to_value(mapped).unwrap(),
            serde_json::json!({
                "kind": "reloginRequired",
                "detail": "account requires re-login"
            })
        );
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
