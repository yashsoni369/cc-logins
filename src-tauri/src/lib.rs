//! cc-logins.
//!
//! A tray application that reports Claude Code account quota and switches
//! between accounts. It refreshes OAuth tokens and reads usage telemetry, and
//! hands credentials to the official Claude Code binary. It never proxies model
//! traffic and has no server.
//!
//! Portions of the credential, path and usage logic are ported from
//! claude-swap (MIT) — https://github.com/realiti4/claude-swap

pub mod claude_locks;
pub mod commands;
pub mod credentials;
pub mod durable_fs;
pub mod hex;
pub mod history;
pub mod linux;
pub mod locking;
pub mod login;
pub mod migrate;
pub mod model;
pub mod oauth;
pub mod oauth_quarantine;
pub mod oauth_refresh;
pub mod paths;
pub mod poller;
pub mod recovery_store;
pub mod resilience;
pub mod runtime;
pub mod settings;
pub mod switch_journal;
pub mod switch_transaction;
pub mod switcher;
pub mod tray;
pub mod wsl;

#[cfg(test)]
mod test_support;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
};
use tray::{AmbientTheme, IconSpec};

// The tray redraw cache (`poller::TrayCache`), the icon rasterisation size
// (`poller::TRAY_PX`), and the tooltip text (`poller::tray_tooltip`) all used
// to be duplicated here — a second, independent copy of exactly the same
// "paint the tray" logic that only this module's one-time startup call used.
// Two implementations of one on-screen icon is a drift hazard by
// construction (this is how the tooltip here ended up `Switching`-aware
// while the poller's own copy silently wasn't), so painting now goes through
// `poller::paint_icon`/`publish_snapshot`/`publish_switching`, which are also
// the only functions that touch `poller::TrayCache` — see that type's doc
// comment for the full rationale.

/// Show and focus the main window. The window always exists; it is only hidden.
fn reveal(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        // No-op off Linux. See `linux::nudge_window` for the focus bug this
        // works around and why a plain `set_focus` is not enough there.
        linux::nudge_window(w);
    }
}

/// Toggle the tray popover, anchored to the tray icon.
///
/// The popover is the product's primary surface — the dashboard is the rare
/// case — so a tray left-click opens this, not the main window.
///
/// Positioning has to happen in Rust: the tray icon's screen rectangle arrives
/// on the click event and there is no reliable cross-platform way to obtain it
/// from JavaScript.
fn toggle_popover(app: &tauri::AppHandle, anchor: tauri::Rect) {
    let Some(w) = app.get_webview_window("popover") else {
        return;
    };

    // Second click closes it, like a real menu.
    if w.is_visible().unwrap_or(false) {
        let _ = w.hide();
        return;
    }

    if let Some(pos) = popover_position(&w, anchor) {
        let _ = w.set_position(tauri::Position::Physical(pos));
    }
    let _ = w.show();
    let _ = w.set_focus();
    linux::nudge_window(w);
}

/// Show the popover without a tray-icon anchor, centred on screen.
///
/// Linux's AppIndicator backend emits no `TrayIconEvent` at all, so the
/// left-click route below is dead there and this menu route is the only way to
/// reach the popover.
fn toggle_popover_centered(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("popover") else {
        return;
    };
    if w.is_visible().unwrap_or(false) {
        let _ = w.hide();
        return;
    }
    let _ = w.center();
    let _ = w.show();
    let _ = w.set_focus();
    linux::nudge_window(w);
}

/// Work out where to put the popover given the tray icon's rectangle.
///
/// Returns `None` if any of the geometry is unavailable, in which case the
/// window keeps its last position — visibly wrong beats not opening at all.
fn popover_position(
    w: &tauri::WebviewWindow,
    anchor: tauri::Rect,
) -> Option<tauri::PhysicalPosition<i32>> {
    let scale = w.scale_factor().ok()?;
    let win = w.outer_size().ok()?;
    let icon: tauri::PhysicalPosition<f64> = anchor.position.to_physical(scale);
    let icon_size: tauri::PhysicalSize<f64> = anchor.size.to_physical(scale);

    let monitor = w.current_monitor().ok().flatten()?;
    let screen = monitor.size();
    let screen_pos = monitor.position();

    // Centre horizontally on the icon, then clamp inside the monitor so a
    // tray icon near the screen edge does not push the panel off-screen.
    let margin = (8.0 * scale) as i32;
    let centred = icon.x as i32 + (icon_size.width as i32 / 2) - (win.width as i32 / 2);
    let min_x = screen_pos.x + margin;
    let max_x = screen_pos.x + screen.width as i32 - win.width as i32 - margin;
    let x = centred.clamp(min_x.min(max_x), max_x.max(min_x));

    // Flip above or below the icon depending on which half of the screen the
    // tray sits in — the Windows taskbar is usually at the bottom, but not
    // always, and macOS puts the menu bar at the top.
    let icon_mid_y = icon.y as i32 + (icon_size.height as i32 / 2);
    let screen_mid_y = screen_pos.y + (screen.height as i32 / 2);
    let y = if icon_mid_y > screen_mid_y {
        icon.y as i32 - win.height as i32 - margin // tray at bottom: open upward
    } else {
        icon.y as i32 + icon_size.height as i32 + margin // tray at top: open downward
    };

    Some(tauri::PhysicalPosition::new(x, y))
}

/// Where the log file lives. Returned so the UI and bug reports can point at it.
pub fn log_path() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("cc-logins")
        .join("app.log")
}

/// Send logs to a file rather than stderr.
///
/// A GUI process has no terminal attached in the normal case, so stderr output
/// is simply lost — which is how a usage-fetch failure became invisible and
/// surfaced as a wrong number on screen instead of a diagnosable error. A file
/// also survives the session, so a user can attach it to a bug report.
fn init_logging() {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("cc_logins_lib=debug,info"),
    );
    builder.format_timestamp_millis();

    // On failure, fall through and let env_logger keep its default stderr
    // target: losing the log is better than not starting, and must never stop
    // the app booting.
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    let _ = builder.try_init();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Without this, every `log::warn!` in oauth.rs, poller.rs and
    // credentials.rs is silently discarded — which is exactly how a usage-fetch
    // failure became invisible and surfaced as a wrong number in the UI instead
    // of a diagnosable error. Default to `info` for our own crate; override
    // with RUST_LOG for more.
    // Before logging, so a panic during logging setup is still captured.
    //
    // The release profile now unwinds rather than aborts (see Cargo.toml):
    // `poller.rs` builds its "one bad tick cannot kill the daemon" guarantee on
    // `catch_unwind`, which an abort makes unreachable. This hook is what turns
    // a crash from silent into diagnosable.
    resilience::install();

    init_logging();
    log::info!("cc-logins starting");

    // Everything below happens BEFORE the builder, because windows declared in
    // tauri.conf.json start loading their webview before `setup()` runs. The
    // popover is one of them: it is hidden, but its JS still executes and calls
    // `snapshot` on mount, which produced "state not managed for field `state`"
    // when `AppState` was only managed inside `setup`.
    let context = tauri::generate_context!();

    // Same derivation Tauri's `app_data_dir()` uses — `dirs::data_dir()` joined
    // with the identifier — taken from the generated context rather than
    // hardcoded, so it cannot drift from tauri.conf.json. Verified against
    // Tauri's own resolver in `setup` below.
    let data_dir = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(&context.config().identifier);

    // One-time migration across the bundle identifier rename
    // (dev.apex36.cc-logins -> cc-logins). Tauri
    // derives app_data_dir() from the identifier, so the rename alone would
    // move this app's data tree to a new, empty directory and silently orphan
    // a real user's accounts one directory over. Must run before
    // `set_store_root`/`AppState::new`, since both read from `data_dir`. See
    // migrate.rs for the safety model (copy-verify-retire, never delete).
    if let Some(parent) = data_dir.parent() {
        let old_data_dirs = [
            parent.join("dev.apex36.cc-logins"),
            parent.join("dev.apex36.claude-account-switcher"),
        ];
        migrate::migrate_app_data_chain(&old_data_dirs, &data_dir);
    }

    // Point the vault at our own directory before anything reads it. Not a
    // setting, deliberately: this app once wrote its registry and credential
    // backups into another tool's directory, which coupled the two blast
    // radii and destroyed a user's accounts there. The vault is ours
    // unconditionally.
    paths::set_store_root(data_dir.join("accounts"));
    log::info!("account vault: {}", paths::backup_root().display());

    match switch_transaction::recover_pending_switch() {
        Ok(switch_transaction::RecoveryDisposition::NothingToRecover) => {}
        Ok(disposition) => log::warn!("recovered interrupted account switch: {disposition:?}"),
        Err(error) => {
            log::error!("automatic switch recovery failed; switching remains disabled: {error}")
        }
    }

    // Backstop for isolated-login temp dirs. They clean up on Drop, but an
    // abort runs no destructors, and one of those briefly holds a real
    // credential. Only sweeps dirs older than an hour so a login running in
    // another instance is never removed mid-flight.
    let swept = login::sweep_stale_login_dirs(std::time::Duration::from_secs(3600));
    if swept > 0 {
        log::info!(
            "swept {swept} stale login director{}",
            if swept == 1 { "y" } else { "ies" }
        );
    }

    let app_state = commands::AppState::new(data_dir.clone());

    tauri::Builder::default()
        // Must be registered first so a second launch is rejected before it
        // touches any credential state.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            reveal(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Nothing here reaches the network on its own. The check runs only when
        // the user asks for it from Settings -> About, which keeps the "no
        // telemetry, no phoning home" claim in the README true.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(poller::TrayCache::default())
        // Managed here, not in `setup`: a config-declared window's webview can
        // invoke a command before `setup` has run.
        .manage(app_state)
        // Read-only commands are safe on a timer; `switch_account` mutates
        // credentials and must only ever be user-initiated. See commands.rs.
        .invoke_handler(tauri::generate_handler![
            commands::accounts,
            commands::snapshot,
            commands::refresh_snapshot,
            commands::environments,
            commands::switch_account,
            commands::add_current_account,
            commands::interactive_login,
            commands::relogin_account,
            commands::add_token,
            commands::set_account_enabled,
            commands::history_summary,
            commands::history_series,
            commands::history_samples,
            commands::history_available,
            commands::get_settings,
            commands::get_daemon_status,
            commands::update_settings,
            commands::snooze_auto_switch,
            commands::resume_auto_switch,
            commands::data_locations,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // The data dir was computed before the builder so state could be
            // managed early. Confirm that derivation still matches Tauri's own
            // resolver, so a change on their side surfaces as a log line rather
            // than a silently split data directory.
            match app.path().app_data_dir() {
                Ok(resolved) if resolved != data_dir => log::warn!(
                    "app_data_dir mismatch: using {} but Tauri resolves {}",
                    data_dir.display(),
                    resolved.display()
                ),
                _ => {}
            }

            // Start the background poller.
            //
            // It polls READ-ONLY commands and records history. It is allowed to
            // call `switcher::switch_to` only when the user has explicitly
            // enabled auto-switch, which defaults to false — software that
            // starts moving credentials before being asked is not trustworthy.
            {
                let policy_rx = app
                    .state::<commands::AppState>()
                    .settings
                    .subscribe_policy();
                let poller_handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    poller::run(poller_handle, policy_rx).await;
                });
            }

            // A hard process termination leaves Claude Code's proper-lockfile
            // directories behind. Claude Code deliberately protects a fresh
            // credential lock for 60 seconds, so the synchronous startup
            // attempt may truthfully defer recovery. Retry off the UI thread
            // until that compatibility boundary passes; stop after six
            // attempts and leave the durable recoveryRequired gate in place.
            if switch_transaction::recovery_requirement().is_some() {
                tauri::async_runtime::spawn(async move {
                    for attempt in 1..=6 {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        let result = tokio::task::spawn_blocking(|| {
                            switch_transaction::recover_pending_switch()
                        })
                        .await;
                        match result {
                            Ok(Ok(disposition)) => {
                                log::warn!(
                                    "background switch recovery succeeded on attempt {attempt}: \
                                     {disposition:?}"
                                );
                                break;
                            }
                            Ok(Err(error)) => log::warn!(
                                "background switch recovery attempt {attempt} deferred: {error}"
                            ),
                            Err(error) => log::warn!(
                                "background switch recovery task {attempt} failed: {error}"
                            ),
                        }
                    }
                });
            }

            let quota_i = MenuItem::with_id(app, "quota", "Show quota panel", true, None::<&str>)?;
            let open_i = MenuItem::with_id(app, "open", "Open dashboard", true, None::<&str>)?;
            let switch_i = MenuItem::with_id(app, "switch", "Switch account…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&quota_i, &open_i, &switch_i, &sep, &quit_i])?;

            // The tray is built in Rust rather than declared in tauri.conf.json;
            // declaring it in both produces two tray icons.
            TrayIconBuilder::with_id("main")
                .menu(&menu)
                // Left click belongs to the popover, not the context menu.
                .show_menu_on_left_click(false)
                .icon_as_template(cfg!(target_os = "macos"))
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quota" => toggle_popover_centered(app),
                    "open" => reveal(app),
                    "switch" => reveal(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|icon, event| {
                    // Left click opens the popover, not the dashboard: the
                    // popover is what people actually use, ~20 times a day.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_popover(icon.app_handle(), rect);
                    }
                })
                .build(app)?;

            // TODO(M1): replace with a real reading from the credential store.
            // A failure here must not abort startup — see `poller::paint_icon`'s
            // "never fails the caller" contract, the same one every mutating
            // command's tray paint relies on.
            poller::paint_icon(&handle, IconSpec::resting(61.0, AmbientTheme::detect()));

            // The main window is created by Tauri from tauri.conf.json with
            // `visible: true`, so it never passed through `reveal()` and never
            // got nudged — exactly the "first click after launch does nothing"
            // case linux.rs exists to fix. No-op off Linux.
            if let Some(w) = app.get_webview_window("main") {
                linux::nudge_window(w);
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the app alive in the tray; quitting is an
            // explicit action from the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(context)
        .expect("error while running tauri application");
}
