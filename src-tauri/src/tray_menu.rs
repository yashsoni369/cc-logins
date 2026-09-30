//! The tray icon's context menu.
//!
//! One checkable row per native account (`"{name} — {pct}%"`, checked on the
//! active one), then the fixed actions. `tray.rs` stays the icon rasteriser;
//! this module owns only the menu.
//!
//! The menu is rebuilt with `set_menu` only when its *structure* changes — the
//! account set, their order, which one is active, which ones can be clicked.
//! Replacing the native menu closes it under the user's pointer, and the
//! percentages move on every poll, so those are edited in place on the stored
//! item handles instead.
//!
//! What the menu should show is decided by pure functions ([`account_rows`],
//! [`account_label`], [`menu_change`], [`click_accepted`]) with unit tests;
//! the Tauri calls that apply it are kept thin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::{AppHandle, Manager, Wry};

use crate::model::{Account, EnvKind, Snapshot, UsageStatus};
use crate::settings::DisplayMode;

/// The tray icon's id, as built in `lib.rs`.
pub const TRAY_ID: &str = "main";

/// Menu item ids. Account rows are [`ids::ACCOUNT_PREFIX`] + slot number.
pub mod ids {
    pub const QUOTA: &str = "quota";
    pub const OPEN: &str = "open";
    pub const QUIT: &str = "quit";
    pub const ACCOUNT_PREFIX: &str = "account:";
}

/// Updates arriving within this window collapse into one. A switch publishes
/// twice in quick succession (in-flight, then landed), and each application
/// round-trips through the main thread.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// A second click on an account row this soon after the last is dropped, so
/// a double click cannot queue two switches.
const CLICK_DEBOUNCE: Duration = Duration::from_millis(500);

/// No keyboard accelerators on tray rows.
const NO_ACCEL: Option<&str> = None;

pub fn account_item_id(number: u32) -> String {
    format!("{}{number}", ids::ACCOUNT_PREFIX)
}

/// The slot number behind an account row's id; `None` for any other item.
pub fn parse_account_item_id(id: &str) -> Option<u32> {
    id.strip_prefix(ids::ACCOUNT_PREFIX)?.parse().ok()
}

/// One account row as it should currently appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub number: u32,
    pub label: String,
    /// The active account.
    pub checked: bool,
    /// Clickable: [`Account::is_switchable`].
    pub enabled: bool,
}

/// `"{name} — {pct}%"`, or `"{name} — {pct}% left"` in [`DisplayMode::Left`].
///
/// The percentage is the binding window's, the same number the tray icon
/// draws. An account with no reading says so rather than showing 0%, and one
/// that needs a fresh sign-in says that instead of a stale number.
pub fn account_label(account: &Account, mode: DisplayMode) -> String {
    // `&` marks a mnemonic in native menu text; `&&` is a literal ampersand.
    let name = account.display_name().replace('&', "&&");
    if account.usage_status == UsageStatus::ReloginRequired {
        return format!("{name} — sign in again");
    }
    match account.binding_utilisation() {
        Some(used) => {
            let used = used.clamp(0.0, 100.0);
            match mode {
                DisplayMode::Used => format!("{name} — {used:.0}%"),
                DisplayMode::Left => format!("{name} — {:.0}% left", 100.0 - used),
            }
        }
        None => format!("{name} — no reading yet"),
    }
}

/// The rows for `snapshot`, in its (rotation) order. Only the native realm
/// carries switchable accounts; WSL realms never appear here.
pub fn account_rows(snapshot: &Snapshot, mode: DisplayMode) -> Vec<AccountRow> {
    snapshot
        .environments
        .iter()
        .filter(|env| env.kind == EnvKind::Native)
        .flat_map(|env| env.accounts.iter())
        .map(|account| AccountRow {
            number: account.number,
            label: account_label(account, mode),
            checked: account.active,
            enabled: account.is_switchable(),
        })
        .collect()
}

/// Everything about the rows except their labels. A difference here needs a
/// new native menu; a difference only in labels does not.
fn structure(rows: &[AccountRow]) -> Vec<(u32, bool, bool)> {
    rows.iter()
        .map(|row| (row.number, row.checked, row.enabled))
        .collect()
}

/// How to bring the native menu from `previous` to `next`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuChange {
    /// No menu yet, or the structure changed: build a new one and `set_menu`.
    Rebuild,
    /// Same structure: relabel these rows (by index) in place. Check marks
    /// are re-asserted on every in-place update too, because native menus
    /// flip a check item's mark on click before any switch has happened.
    InPlace(Vec<(usize, String)>),
}

pub fn menu_change(previous: Option<&[AccountRow]>, next: &[AccountRow]) -> MenuChange {
    let Some(previous) = previous else {
        return MenuChange::Rebuild;
    };
    if structure(previous) != structure(next) {
        return MenuChange::Rebuild;
    }
    let relabel = previous
        .iter()
        .zip(next)
        .enumerate()
        .filter(|(_, (old, new))| old.label != new.label)
        .map(|(index, (_, new))| (index, new.label.clone()))
        .collect();
    MenuChange::InPlace(relabel)
}

/// Whether a click at `now` is far enough from the last accepted one.
pub fn click_accepted(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= CLICK_DEBOUNCE)
}

/// The rows the native menu currently shows, and the handles to edit them.
#[derive(Default)]
struct MenuModel {
    /// `None` until the first menu built from a snapshot is installed.
    rows: Option<Vec<AccountRow>>,
    items: Vec<CheckMenuItem<Wry>>,
}

/// Managed state behind the tray menu. Every lock here recovers from poison:
/// a panic elsewhere must not take the tray menu down with it.
#[derive(Default)]
pub struct TrayMenuState {
    model: Mutex<MenuModel>,
    /// Newest rows not yet applied.
    pending: Mutex<Option<Vec<AccountRow>>>,
    /// An application is already scheduled and will take `pending`.
    scheduled: AtomicBool,
    last_click: Mutex<Option<Instant>>,
}

impl TrayMenuState {
    fn accept_click(&self, now: Instant) -> bool {
        let mut last = lock(&self.last_click);
        let accepted = click_accepted(*last, now);
        if accepted {
            *last = Some(now);
        }
        accepted
    }

    fn is_active_row(&self, number: u32) -> bool {
        lock(&self.model)
            .rows
            .iter()
            .flatten()
            .any(|row| row.number == number && row.checked)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Build the whole menu: account rows (if any) and a separator, then the
/// fixed actions. Returns the account items so later updates can edit them.
pub fn build_menu(
    app: &AppHandle,
    rows: &[AccountRow],
) -> tauri::Result<(Menu<Wry>, Vec<CheckMenuItem<Wry>>)> {
    let mut accounts = Vec::with_capacity(rows.len());
    for row in rows {
        let id = account_item_id(row.number);
        let text = row.label.as_str();
        let item = CheckMenuItem::with_id(app, id, text, row.enabled, row.checked, NO_ACCEL)?;
        accounts.push(item);
    }

    let quota = MenuItem::with_id(app, ids::QUOTA, "Show quota panel", true, NO_ACCEL)?;
    let open = MenuItem::with_id(app, ids::OPEN, "Open CC Logins", true, NO_ACCEL)?;
    let quit = MenuItem::with_id(app, ids::QUIT, "Quit", true, NO_ACCEL)?;
    let accounts_end = PredefinedMenuItem::separator(app)?;
    let quit_sep = PredefinedMenuItem::separator(app)?;

    let mut items: Vec<&dyn IsMenuItem<Wry>> = accounts
        .iter()
        .map(|item| item as &dyn IsMenuItem<Wry>)
        .collect();
    if !accounts.is_empty() {
        items.push(&accounts_end);
    }
    items.push(&quota);
    items.push(&open);
    items.push(&quit_sep);
    items.push(&quit);
    let menu = Menu::with_items(app, &items)?;
    Ok((menu, accounts))
}

/// Queue a refresh of the account rows from `snapshot`.
///
/// Called from [`crate::poller::publish_snapshot`] on poller and command
/// threads. Bursts collapse into one application [`COALESCE_WINDOW`] later,
/// always with the newest rows. Never blocks and never fails the caller.
pub fn schedule_update(app: &AppHandle, snapshot: &Snapshot) {
    let Some(state) = app.try_state::<TrayMenuState>() else {
        return;
    };
    let mode = app
        .try_state::<crate::commands::AppState>()
        .map(|app_state| app_state.settings.snapshot().settings.display_mode)
        .unwrap_or_default();
    *lock(&state.pending) = Some(account_rows(snapshot, mode));
    if state.scheduled.swap(true, Ordering::AcqRel) {
        // Already on its way; it will pick up the rows just stored.
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(COALESCE_WINDOW).await;
        let state = app.state::<TrayMenuState>();
        // Cleared before taking, so rows stored after this point schedule a
        // fresh application rather than being stranded.
        state.scheduled.store(false, Ordering::Release);
        let rows = lock(&state.pending).take();
        if let Some(rows) = rows {
            apply(&app, &state, rows);
        }
    });
}

/// Bring the native menu in line with `rows`.
///
/// Never called on the main thread: each menu call below is dispatched to the
/// main thread and waits for it, with `model` held. So nothing that runs on
/// the main thread may take `model` — the click handler takes only
/// `last_click`.
fn apply(app: &AppHandle, state: &TrayMenuState, rows: Vec<AccountRow>) {
    let mut model = lock(&state.model);
    match menu_change(model.rows.as_deref(), &rows) {
        MenuChange::Rebuild => {
            let (menu, items) = match build_menu(app, &rows) {
                Ok(built) => built,
                Err(e) => {
                    log::warn!("tray menu: rebuild failed: {e}");
                    return;
                }
            };
            let Some(tray) = app.tray_by_id(TRAY_ID) else {
                return;
            };
            if let Err(e) = tray.set_menu(Some(menu)) {
                log::warn!("tray menu: set_menu failed: {e}");
                return;
            }
            model.items = items;
        }
        MenuChange::InPlace(relabel) => {
            for (index, label) in relabel {
                if let Some(item) = model.items.get(index) {
                    if let Err(e) = item.set_text(label) {
                        log::debug!("tray menu: relabel failed: {e}");
                    }
                }
            }
            reassert_checks(&model.items, &rows);
        }
    }
    model.rows = Some(rows);
}

fn reassert_checks(items: &[CheckMenuItem<Wry>], rows: &[AccountRow]) {
    for (item, row) in items.iter().zip(rows) {
        if let Err(e) = item.set_checked(row.checked) {
            log::debug!("tray menu: set_checked failed: {e}");
        }
    }
}

/// Put every row's check mark back to what the model says.
fn resync_checks(app: &AppHandle) {
    if let Some(state) = app.try_state::<TrayMenuState>() {
        let model = lock(&state.model);
        if let Some(rows) = &model.rows {
            reassert_checks(&model.items, rows);
        }
    }
}

/// Handle a click on a menu item that is not one of the fixed actions.
///
/// Runs on the main thread, so it only spawns: waiting there on a switch, or
/// on any menu call, would freeze the UI. A click within [`CLICK_DEBOUNCE`] of
/// the last accepted one is dropped. A successful switch republishes through
/// [`crate::poller::publish_snapshot`], which rebuilds the menu; after a
/// dropped or failed one the check marks are put back, since the native menu
/// has already flipped the clicked row's mark by itself.
pub fn on_account_click(app: &AppHandle, id: &str) {
    let Some(number) = parse_account_item_id(id) else {
        return;
    };
    let Some(state) = app.try_state::<TrayMenuState>() else {
        return;
    };
    let accepted = state.accept_click(Instant::now());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Checked here, not on the main thread: `apply` holds `model` while
        // it waits on the main thread.
        let state = app.state::<TrayMenuState>();
        if accepted && !state.is_active_row(number) {
            match crate::commands::switch_account_for(&app, number).await {
                Ok(_) => return,
                Err(e) => log::warn!("tray menu: switch to account {number} failed: {e:?}"),
            }
        }
        resync_checks(&app);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EnvStatus, Environment, Usage, UsageWindow};

    fn account(number: u32, pct: Option<f64>) -> Account {
        Account {
            number,
            email: format!("user{number}@example.com"),
            usage_status: UsageStatus::Ok,
            usage: pct.map(|pct| Usage {
                five_hour: Some(UsageWindow {
                    pct,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn environment(kind: EnvKind, accounts: Vec<Account>) -> Environment {
        Environment {
            id: format!("{kind:?}"),
            label: format!("{kind:?}"),
            path: String::new(),
            kind,
            status: EnvStatus::Live,
            accounts,
            last_seen_seconds: None,
            has_credentials: None,
        }
    }

    fn row(number: u32, label: &str, checked: bool) -> AccountRow {
        AccountRow {
            number,
            label: label.to_string(),
            checked,
            enabled: !checked,
        }
    }

    #[test]
    fn labels_show_used_or_left_for_the_binding_window() {
        let mut work = account(1, Some(72.0));
        work.alias = Some("Work".to_string());

        assert_eq!(account_label(&work, DisplayMode::Used), "Work — 72%");
        assert_eq!(account_label(&work, DisplayMode::Left), "Work — 28% left");
    }

    #[test]
    fn labels_never_show_a_missing_reading_as_zero() {
        let unread = account(2, None);
        assert_eq!(
            account_label(&unread, DisplayMode::Used),
            "u•••@example.com — no reading yet"
        );

        let mut expired = account(3, Some(10.0));
        expired.usage_status = UsageStatus::ReloginRequired;
        assert!(account_label(&expired, DisplayMode::Used).ends_with("sign in again"));
    }

    #[test]
    fn labels_escape_ampersands_and_clamp_the_percentage() {
        let mut team = account(4, Some(130.0));
        team.alias = Some("R&D".to_string());

        assert_eq!(account_label(&team, DisplayMode::Used), "R&&D — 100%");
        assert_eq!(account_label(&team, DisplayMode::Left), "R&&D — 0% left");
    }

    #[test]
    fn account_ids_round_trip_and_reject_everything_else() {
        assert_eq!(account_item_id(7), "account:7");
        assert_eq!(parse_account_item_id(&account_item_id(7)), Some(7));
        assert_eq!(parse_account_item_id(ids::QUOTA), None);
        assert_eq!(parse_account_item_id("account:x"), None);
    }

    #[test]
    fn rows_list_native_accounts_in_order_with_the_active_one_checked() {
        let mut active = account(2, Some(50.0));
        active.active = true;
        let mut held = account(1, Some(10.0));
        held.usage_status = UsageStatus::Disabled;
        let native = environment(EnvKind::Native, vec![active, held, account(3, None)]);
        let wsl = environment(EnvKind::Wsl, vec![account(9, Some(1.0))]);

        let rows = account_rows(&Snapshot::new(vec![native, wsl]), DisplayMode::Used);

        assert_eq!(rows.len(), 3, "the WSL realm's accounts never appear");
        assert_eq!(
            structure(&rows),
            vec![(2, true, false), (1, false, false), (3, false, true)]
        );
        assert_eq!(rows[0].label, "u•••@example.com — 50%");
    }

    #[test]
    fn a_first_menu_or_a_structural_change_rebuilds() {
        let prev = [row(1, "a 10%", true), row(2, "b 20%", false)];
        assert_eq!(menu_change(None, &prev), MenuChange::Rebuild);

        let flip = [row(1, "a 10%", false), row(2, "b 20%", true)];
        assert_eq!(menu_change(Some(&prev[..]), &flip), MenuChange::Rebuild);

        let moved = [row(2, "b 20%", false), row(1, "a 10%", true)];
        assert_eq!(menu_change(Some(&prev[..]), &moved), MenuChange::Rebuild);

        let grown = [
            row(1, "a 10%", true),
            row(2, "b 20%", false),
            row(3, "c 30%", false),
        ];
        assert_eq!(menu_change(Some(&prev[..]), &grown), MenuChange::Rebuild);
    }

    #[test]
    fn a_percentage_change_relabels_in_place_without_rebuilding() {
        let prev = [row(1, "a 10%", true), row(2, "b 20%", false)];
        let next = [row(1, "a 11%", true), row(2, "b 20%", false)];

        assert_eq!(
            menu_change(Some(&prev[..]), &next),
            MenuChange::InPlace(vec![(0, "a 11%".to_string())])
        );
        assert_eq!(
            menu_change(Some(&prev[..]), &prev),
            MenuChange::InPlace(Vec::new())
        );
    }

    #[test]
    fn clicks_inside_the_debounce_window_are_dropped() {
        let now = Instant::now();
        assert!(click_accepted(None, now));
        let soon = now + Duration::from_millis(100);
        assert!(!click_accepted(Some(now), soon));
        assert!(click_accepted(Some(now), now + CLICK_DEBOUNCE));
    }
}
