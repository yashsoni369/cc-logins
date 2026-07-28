//! Linux-only workarounds for known Tauri 2 / WebKitGTK usability bugs.
//!
//! Everything in this module is a no-op on Windows and macOS: every function
//! that touches process/window state is gated by `#[cfg(target_os = "linux")]`
//! internally, so callers on other platforms can call these functions
//! unconditionally and nothing about their behaviour changes. This was
//! written and reviewed with **no Linux machine available** — see the
//! per-workaround confidence notes below, and re-verify against upstream
//! issue trackers before trusting this blindly on a new Tauri/WebKitGTK
//! version pairing.
//!
//! ## Why this exists
//!
//! `RESILIENCE.md` §5 documents three WebKitGTK-on-Linux bugs that
//! `farion1231/cc-switch` (a Tauri 2 app in the same problem domain) had to
//! work around, citing Tauri's own issue tracker rather than anything
//! cc-switch-specific:
//!
//! 1. White/black screen at launch on some GPU+driver combinations.
//! 2. The webview not receiving focus after `show()`, so the first click
//!    after launch does nothing.
//! 3. Compositor surface-negotiation failures that leave the whole window
//!    permanently unresponsive to clicks after a resize.
//!
//! This module addresses (1) with environment variables set before Tauri
//! initialises ([`apply_webkit_env_workarounds`]), and (2)/(3) with a
//! focus-and-resize nudge applied to a window right after it is shown
//! ([`nudge_window`]).

use std::env;

/// One environment-variable workaround: the variable, the value to set it
/// to, and the symptom it addresses. Kept as data (rather than inline in
/// [`apply_webkit_env_workarounds`]) so the "don't overwrite an existing
/// value" logic has exactly one call site to audit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct EnvWorkaround {
    key: &'static str,
    value: &'static str,
}

/// WebKitGTK environment-variable workarounds.
///
/// Both entries are cited by Tauri's own documentation
/// (<https://v2.tauri.app/develop/debug/linux-graphics/>) and by
/// `tauri-apps/tauri#9394`, and were independently hit by several unrelated
/// Tauri/Electron-adjacent apps (`Zackriya-Solutions/meetily#435`,
/// `visnkmr/netspeed_pc#3`, `tauri-apps/tauri#13151`) — this is a general
/// "Tauri/WebKitGTK on Linux" problem, not specific to any one app.
///
/// **Confidence: well-established for the symptom -> variable pairing.**
/// Tauri's own docs carry an explicit caveat, though: "Only ship an
/// unconditional override like this if you have verified your app is
/// affected. It disables a faster path for everyone, including users on
/// working setups." We cannot verify affectedness without Linux hardware, so
/// this is a deliberate judgment call, not a certainty — see the module-level
/// doc comment. The mitigating factors: (a) both variables only give up a
/// *faster* rendering path, they do not correctness-degrade a *working*
/// setup, matching `cc-switch`'s own choice to set both unconditionally, and
/// (b) [`set_env_if_unset`] never overrides a value the user already set, so
/// anyone who has diagnosed a regression from this workaround can opt back
/// out with a single exported variable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WEBKIT_ENV_WORKAROUNDS: &[EnvWorkaround] = &[
    EnvWorkaround {
        // Symptom: white or black screen at launch on some GPU+driver
        // combinations (reported with Nvidia proprietary drivers on Debian
        // 13; also seen on other distros in the issues cited above).
        // WebKitGTK's DMA-BUF render path fails to initialise, silently.
        key: "WEBKIT_DISABLE_DMABUF_RENDERER",
        value: "1",
    },
    EnvWorkaround {
        // Symptom: the webview crashes or blanks on window resize; on some
        // Wayland compositors the compositing surface never renegotiates
        // and the window becomes permanently unresponsive to input until
        // something forces a re-layout (e.g. manually maximising and
        // restoring). Tauri's docs describe this variable as a "last
        // resort" because it disables GPU-accelerated compositing entirely,
        // trading a slower rendering path for not crashing/hanging.
        key: "WEBKIT_DISABLE_COMPOSITING_MODE",
        value: "1",
    },
];

/// Set `key=value` in the process environment unless `key` is already set —
/// so a user (or a launcher script) who has deliberately exported, say,
/// `WEBKIT_DISABLE_COMPOSITING_MODE=0` to undo this workaround on a setup
/// where it causes a regression is never overridden. Returns whether it
/// actually set the variable, so callers/tests can observe the decision.
///
/// Pure enough to unit test on any OS (see the `tests` module below); only
/// ever *called* from Linux-gated code in this crate, so it has no effect on
/// Windows/macOS in a real build.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn set_env_if_unset(key: &str, value: &str) -> bool {
    if env::var_os(key).is_some() {
        return false;
    }

    // SAFETY: `env::set_var` is only unsound when it races another thread
    // reading or writing the environment. The one production call site
    // ([`apply_webkit_env_workarounds`]) is invoked as the very first
    // statement of `main()`, before Tauri, tokio, or any other thread of
    // this process exists — see that function's doc comment for why that
    // ordering is required for correctness, not just safety.
    unsafe { env::set_var(key, value) };
    true
}

/// Apply the WebKitGTK environment-variable workarounds documented on
/// [`WEBKIT_ENV_WORKAROUNDS`].
///
/// # Ordering: must run before Tauri builds anything
///
/// WebKitGTK reads these variables once, at library-init time, when the
/// first `WebKitWebView` (or the GTK main loop backing it) is created.
/// Setting them later — inside `.setup()`, or anywhere after
/// `tauri::Builder::default()` has started building windows — is too late:
/// the webview has already picked its render path. That is why this must be
/// called from `main()`, before `cc_logins_lib::run()`, rather than from
/// anywhere inside `lib.rs`.
///
/// No-op on every platform except Linux.
pub fn apply_webkit_env_workarounds() {
    #[cfg(target_os = "linux")]
    {
        for w in WEBKIT_ENV_WORKAROUNDS {
            set_env_if_unset(w.key, w.value);
        }
    }
}

// ── Focus / surface-reactivation nudge ─────────────────────────────────────

/// Delay after `show()` before the first re-focus attempt, to give GTK's
/// main loop time to finish realizing the webview. 200ms is the empirical
/// value `cc-switch` settled on (`linux_fix.rs`): shorter and the second
/// `set_focus()` is still a no-op; longer and the delay becomes perceptible
/// as the window looking ready before it actually is.
#[cfg(target_os = "linux")]
const REALIZE_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// Gap between the two halves of the ±1px pseudo-resize, so the compositor
/// processes the first `size_allocate` before the second resize request
/// arrives — Tao's Linux size API is asynchronous (it goes through
/// `gtk_window_resize` and then the compositor's own `configure` event), so
/// too short a gap risks the two requests being coalesced into one no-op.
#[cfg(target_os = "linux")]
const RESIZE_GAP: std::time::Duration = std::time::Duration::from_millis(100);

/// Extra wait before reading the window size back to check it settled at the
/// original value. 200ms + 100ms + 500ms = ~800ms total, which `cc-switch`
/// found sufficient for every compositor it tested against to finish
/// processing the resize queue.
#[cfg(target_os = "linux")]
const RECONCILE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Bump one dimension of `size` by 1 physical pixel, saturating rather than
/// overflowing at `u32::MAX`. Pure so it can be unit tested without a real
/// window; the saturating case is exercised explicitly below since a plain
/// `+1` would panic in debug builds on the boundary value.
#[cfg(any(target_os = "linux", test))]
fn bump_width(size: (u32, u32)) -> (u32, u32) {
    (size.0.saturating_add(1), size.1)
}

/// Apply a Linux-only "focus + surface reactivation" nudge to a window right
/// after it has been shown.
///
/// This mimics a user manually maximising and restoring the window — the
/// known manual workaround for two related bugs where a freshly shown
/// window looks normal but silently does not respond to input:
///
/// - **First click swallowed** (`tauri-apps/wry#637`, `tauri-apps/tauri#10746`):
///   on some X11/Wayland setups, `show()` followed by `set_focus()` does not
///   actually hand keyboard/pointer focus to the webview — the first click
///   after launch is consumed as "activate this window" by the window
///   manager rather than delivered to the page.
/// - **GTK surface never reallocates**: the WebKitWebView's input region can
///   be negotiated incorrectly on the `visible:false -> show()` transition,
///   leaving the window permanently unresponsive to clicks until something
///   forces GTK to run `size-allocate` again.
///
/// The fix: a second `set_focus()` after giving the webview time to realize,
/// then a same-session ±1px resize-and-restore that forces GTK to
/// reallocate the surface, with a delayed read-back that re-corrects the
/// size if a compositor coalesced the two resize requests.
///
/// # Confidence: contested / possibly already fixed upstream
///
/// `tauri-apps/tauri#10746` was ultimately closed as resolved by an
/// upstream `tao`/`winit` update, and `tauri-apps/wry#637` points at
/// `tauri-apps/tao#575` for further work — i.e. this class of bug has been
/// actively, if incompletely, fixed over time. This crate depends on `tao`
/// 0.35.3 / `wry` 0.55.1 (see `Cargo.lock`), both considerably newer than
/// when `cc-switch` wrote its own copy of this workaround, so it is
/// plausible the underlying bug is already gone for us. Nobody has verified
/// that on real Linux hardware in this session either way. The nudge is
/// cheap, fire-and-forget, and invisible if the bug is already fixed (a
/// ±1px resize-and-restore on an already-responsive window is not
/// user-visible), so it errs on the side of keeping it rather than assuming
/// it is unnecessary — but this is exactly the kind of thing that should be
/// deleted the first time someone with real Linux hardware confirms it no
/// longer reproduces.
///
/// # Wiring
///
/// This function cannot wire itself in — see "Wiring needed in lib.rs" in
/// this change's report. It must be called after every "make a window
/// appear" path: normal startup (`setup()`), single-instance re-activation
/// (`reveal()`), and the tray popover toggle (`toggle_popover()`), in each
/// case right after the existing `set_focus()` call.
///
/// No-op on every platform except Linux. Fire-and-forget: the delayed steps
/// run on Tauri's async runtime, so this returns immediately and never
/// blocks the caller (including the tray-click event handler, which must
/// stay fast).
#[cfg(target_os = "linux")]
pub fn nudge_window(window: tauri::WebviewWindow) {
    let _ = window.set_focus();

    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(REALIZE_WAIT).await;
        let _ = window.set_focus();

        let original = match window.inner_size() {
            Ok(size) => size,
            Err(e) => {
                // Read failed; a focus-only nudge is still better than
                // nothing, so don't let this abort the whole sequence.
                log::warn!(
                    "linux: could not read window size for nudge, skipping resize step: {e}"
                );
                return;
            }
        };

        let (bumped_w, bumped_h) = bump_width((original.width, original.height));
        let bumped = tauri::PhysicalSize::new(bumped_w, bumped_h);

        let _ = window.set_size(bumped);
        tokio::time::sleep(RESIZE_GAP).await;
        let _ = window.set_size(original);
        log::info!("linux: nudged window focus + surface after show()");

        // Reconciliation read-back: Tao's Linux resize API is
        // asynchronous, and a compositor that coalesces the two
        // `set_size` calls above can leave the window permanently 1px
        // off. If drift is observed, correct it once and log either way
        // so a real drift-after-correction failure is diagnosable from
        // app.log rather than silently wrong forever.
        tokio::time::sleep(RECONCILE_WAIT).await;
        if let Ok(after) = window.inner_size() {
            if after.width != original.width || after.height != original.height {
                log::info!(
                    "linux: window size drifted after nudge (expected {}x{}, got {}x{}), correcting",
                    original.width,
                    original.height,
                    after.width,
                    after.height
                );
                let _ = window.set_size(original);
            }
        }
    });
}

/// No-op stub for every platform except Linux — see the Linux
/// implementation above for what this does and why.
#[cfg(not(target_os = "linux"))]
pub fn nudge_window(_window: tauri::WebviewWindow) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvGuard};

    #[test]
    fn set_env_if_unset_sets_an_absent_variable() {
        let _lock = env_lock();
        let key = "CC_LOGINS_TEST_LINUX_ABSENT";
        let _guard = EnvGuard::unset(key);

        let set = set_env_if_unset(key, "1");

        assert!(set, "should report that it set the variable");
        assert_eq!(std::env::var(key).as_deref(), Ok("1"));
    }

    #[test]
    fn set_env_if_unset_never_overwrites_an_existing_value() {
        let _lock = env_lock();
        let key = "CC_LOGINS_TEST_LINUX_PRESENT";
        let _guard = EnvGuard::set(key, "user-chosen-value");

        let set = set_env_if_unset(key, "1");

        assert!(!set, "should report that it left the variable alone");
        assert_eq!(
            std::env::var(key).as_deref(),
            Ok("user-chosen-value"),
            "a user-set value must never be clobbered"
        );
    }

    #[test]
    fn set_env_if_unset_treats_an_empty_string_as_already_set() {
        // Someone who has exported `WEBKIT_DISABLE_COMPOSITING_MODE=` (empty,
        // not unset) has still made a deliberate choice about the variable's
        // presence; `var_os` sees it as present, and this must not overwrite
        // it either.
        let _lock = env_lock();
        let key = "CC_LOGINS_TEST_LINUX_EMPTY";
        let _guard = EnvGuard::set(key, "");

        let set = set_env_if_unset(key, "1");

        assert!(!set);
        assert_eq!(std::env::var(key).as_deref(), Ok(""));
    }

    #[test]
    fn webkit_workarounds_list_has_no_accidental_duplicate_keys() {
        // A duplicate would mean the second entry's `set_env_if_unset` call
        // is always a no-op, silently — cheap to guard against.
        let mut keys: Vec<&str> = WEBKIT_ENV_WORKAROUNDS.iter().map(|w| w.key).collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            before,
            "duplicate key in WEBKIT_ENV_WORKAROUNDS"
        );
    }

    #[test]
    fn bump_width_increments_by_one_pixel() {
        assert_eq!(bump_width((800, 600)), (801, 600));
    }

    #[test]
    fn bump_width_saturates_instead_of_overflowing() {
        assert_eq!(bump_width((u32::MAX, 600)), (u32::MAX, 600));
    }

    #[test]
    fn apply_webkit_env_workarounds_is_callable_on_any_platform() {
        // The whole point of the internal `#[cfg(target_os = "linux")]` gate
        // is that callers never need to know the platform. This just proves
        // it links and returns on whatever OS is running the test suite.
        apply_webkit_env_workarounds();
    }
}
