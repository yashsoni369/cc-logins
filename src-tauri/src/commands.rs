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

use serde::Serialize;

use crate::login::{self, LoginError};
use crate::model::{Account, Environment, Snapshot};
use crate::switcher::{self, Strategy, SwitchError};

/// Errors cross the IPC boundary as a tagged object rather than a bare string,
/// so the UI can distinguish "nothing is set up yet" (show onboarding) from
/// "the network is down" (show stale data) from "something is genuinely wrong".
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum IpcError {
    /// No accounts are managed yet — the first-run screen, not an error state.
    NotConfigured,
    /// Usage could not be read. Any accompanying data is last-known.
    Unreachable(String),
    /// The credential store could not be read or written.
    Credential(String),
    /// A lock is held by another process (very likely the `cswap` CLI).
    Busy(String),
    /// Anything else.
    Internal(String),
}

impl From<SwitchError> for IpcError {
    fn from(e: SwitchError) -> Self {
        // Map by meaning, not by convenience: the UI branches on these.
        match &e {
            SwitchError::NoAccountsManaged => IpcError::NotConfigured,
            SwitchError::Locking(_) => IpcError::Busy(e.to_string()),
            SwitchError::CredentialRead
            | SwitchError::NoStoredCredentials(_)
            | SwitchError::NoStoredConfig(_)
            | SwitchError::EmptyActiveCredential(_)
            | SwitchError::NoLiveCredential => IpcError::Credential(e.to_string()),
            // Business-rule refusals, not I/O/credential-store failures: the
            // store itself is fine, the requested mutation just isn't valid
            // right now. No dedicated `IpcError` kind exists for these yet,
            // so they fall to `Internal` — the UI still gets the full,
            // specific message via `e.to_string()`.
            SwitchError::AlreadyRegistered(_)
            | SwitchError::InvalidToken(_)
            | SwitchError::InvalidCredential(_)
            | SwitchError::CannotDisableActive(_) => IpcError::Internal(e.to_string()),
            _ => IpcError::Internal(e.to_string()),
        }
    }
}

/// Maps [`login::LoginError`] to [`IpcError`] by meaning, not convenience —
/// the UI (`describeInteractiveLoginError` in `src/App.tsx`) branches on the
/// resulting message text, not just the tagged `kind`:
///
/// - [`LoginError::Cancelled`] — the ordinary "closed the terminal without
///   logging in" outcome, not a failure. Its `Display` text ("login was
///   cancelled") is what the UI matches via a `.includes("cancel")` check to
///   return `null` and render nothing, rather than an alarming banner. No
///   dedicated `IpcError` kind exists for "calm, non-error" today, so this
///   rides `Internal` — the same place every other business-rule outcome
///   without its own kind lands (see [`SwitchError::AlreadyRegistered`] etc.
///   above) — and the message text alone carries the distinction.
/// - [`LoginError::TimedOut`], [`LoginError::ClaudeNotInstalled`],
///   [`LoginError::NoTerminalAvailable`] — each variant's own `Display` text
///   is already a distinct, specific, content-free description (see
///   `login.rs`), so these also ride `Internal` unchanged; the UI's substring
///   matching (`"time"`, `"not installed"`/`"not found"`/`"path"`,
///   `"terminal"`) keys directly off that wording.
///   [`LoginError::NoTerminalAvailable`]'s message already names "terminal
///   emulator", which is what lets the UI point the user at the "Add token"
///   fallback for this one specifically.
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
/// The credential blob itself is never part of any [`LoginError`] variant (see
/// `login.rs`'s "never logged" rule), so no arm here can ever surface it.
impl From<LoginError> for IpcError {
    fn from(e: LoginError) -> Self {
        match &e {
            LoginError::BadCredential(_) => IpcError::Credential(e.to_string()),
            LoginError::Cancelled
            | LoginError::TimedOut
            | LoginError::ClaudeNotInstalled
            | LoginError::NoTerminalAvailable
            | LoginError::Io(_) => IpcError::Internal(e.to_string()),
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
    let accounts = switcher::read_accounts()?;
    let target = accounts
        .iter()
        .find(|a| a.number == account_number)
        .ok_or_else(|| IpcError::Internal(format!("no account in slot {account_number}")))?;

    // Show the switch in flight before mutating anything, so a slow swap
    // doesn't leave the tray sitting on the outgoing account's stale number.
    crate::poller::publish_switching(&app);

    switcher::switch_to(target)?;

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
    let outcome = login::interactive_login().await?;
    switcher::add_oauth_credential(&outcome.credentials, outcome.email.as_deref(), alias.as_deref())?;
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

/// The account the auto-switcher would move to right now, without moving.
///
/// Read-only on purpose: it powers the "next" hint in the popover, and lets the
/// switch path be inspected before it is ever trusted to run on its own.
#[tauri::command]
pub fn preview_target(strategy: Option<String>) -> IpcResult<Option<Account>> {
    let strategy = match strategy.as_deref() {
        Some("next-available") => Strategy::NextAvailable,
        Some("consume-first") => Strategy::ConsumeFirst,
        _ => Strategy::MostHeadroom,
    };
    let accounts = switcher::read_accounts()?;
    Ok(switcher::pick_target(&accounts, strategy).cloned())
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
    pub settings: std::sync::Mutex<crate::settings::Settings>,
}

impl AppState {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        let settings = crate::settings::load(&data_dir);
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
            settings: std::sync::Mutex::new(settings),
            snapshot_cache: std::sync::Mutex::new(None),
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
    let Some(h) = state.history.as_ref() else { return Ok(None) };
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
    let Some(h) = state.history.as_ref() else { return Ok(Vec::new()) };
    h.daily_rollup(&account_key, days.unwrap_or(30))
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

// ─── settings ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn get_settings(state: tauri::State<'_, AppState>) -> crate::settings::Settings {
    state.settings.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Persist settings. Values are clamped on save, so a bad number cannot brick
/// the next launch.
#[tauri::command]
pub fn set_settings(
    state: tauri::State<'_, AppState>,
    settings: crate::settings::Settings,
) -> IpcResult<crate::settings::Settings> {
    let clean = settings.sanitised();
    crate::settings::save(&state.data_dir, &clean)
        .map_err(|e| IpcError::Internal(e.to_string()))?;
    if let Ok(mut guard) = state.settings.lock() {
        *guard = clean.clone();
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(json.contains("busy") && json.contains("locked"), "got {json}");
    }

    // -- LoginError -> IpcError mapping ---------------------------------------
    //
    // The UI (`describeInteractiveLoginError` in `src/App.tsx`) branches on
    // substrings of the message text, not the tagged `kind` — these tests
    // pin the exact wording contract it depends on.

    #[test]
    fn login_cancelled_produces_a_message_the_ui_can_recognise_as_calm() {
        let mapped: IpcError = LoginError::Cancelled.into();
        match mapped {
            IpcError::Internal(msg) => {
                assert!(msg.to_lowercase().contains("cancel"), "got {msg}")
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn login_timed_out_message_is_distinct_from_cancellation() {
        let mapped: IpcError = LoginError::TimedOut.into();
        match mapped {
            IpcError::Internal(msg) => {
                let lower = msg.to_lowercase();
                assert!(lower.contains("time"), "got {msg}");
                assert!(!lower.contains("cancel"), "must not read as a cancellation: {msg}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn login_claude_not_installed_message_points_at_installation() {
        let mapped: IpcError = LoginError::ClaudeNotInstalled.into();
        match mapped {
            IpcError::Internal(msg) => {
                let lower = msg.to_lowercase();
                assert!(lower.contains("not found") || lower.contains("path"), "got {msg}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn login_no_terminal_available_message_names_terminal_for_the_add_token_fallback() {
        let mapped: IpcError = LoginError::NoTerminalAvailable.into();
        match mapped {
            IpcError::Internal(msg) => {
                assert!(msg.to_lowercase().contains("terminal"), "got {msg}")
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn login_bad_credential_maps_to_credential_kind_and_stays_content_free() {
        let mapped: IpcError = LoginError::BadCredential(
            "credential file did not contain a usable access token",
        )
        .into();
        match mapped {
            IpcError::Credential(msg) => {
                assert!(msg.contains("could not be validated"), "got {msg}");
                assert!(!msg.contains("accessToken\":\""), "must never echo raw credential bytes");
            }
            other => panic!("expected Credential, got {other:?}"),
        }
    }

    #[test]
    fn login_io_error_maps_to_internal() {
        let mapped: IpcError = LoginError::Io(std::io::Error::other("spawn failed")).into();
        assert!(matches!(mapped, IpcError::Internal(_)));
    }
}
