// Prevents an additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Must run before `cc_logins_lib::run()` builds the Tauri webview:
    // WebKitGTK reads its `WEBKIT_DISABLE_*` environment variables once, at
    // library-init time, so setting them from inside `.setup()` (i.e. after
    // a webview already exists) would be too late. See
    // `cc_logins_lib::linux` for what each variable fixes, the sources for
    // each, and the confidence level — no-op on Windows/macOS.
    cc_logins_lib::linux::apply_webkit_env_workarounds();

    cc_logins_lib::run()
}
