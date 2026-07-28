# Resilience findings: cc-switch vs. cc-logins

Reference: [`farion1231/cc-switch`](https://github.com/farion1231/cc-switch) (MIT), read via `gh api`
at the `main` branch as of 2026-07-28: `panic_hook.rs`, `linux_fix.rs`, `auto_launch.rs`, `tray.rs`
(58 KB), `app_config.rs`, `store.rs`, `error.rs`, `init_status.rs`, `lightweight.rs`, `lib.rs`,
`main.rs`, `config.rs`, `codex_config.rs`, `services/proxy.rs`, and `.github/workflows/{ci,release}.yml`.
Comments in the source are in Chinese; translations below are mine.

cc-switch is roughly 15-20x our scope (it manages Claude/Codex/Gemini/Grok/OpenCode/OpenClaw/Hermes
configs, MCP servers, prompts, a proxy layer with failover, WebDAV/S3 sync, deep links, and a SQLite
schema with versioned migrations). Most of its 58 KB `tray.rs` and 44 KB `app_config.rs` is that
feature breadth, not resilience density — I've filtered for the minority that's actually a crash-
safety or platform-robustness pattern transferable to a single-purpose tray app.

Ranked by (risk avoided) × (likelihood of hitting it). Findings 1-3 are the ones that matter; the
rest are given for completeness.

---

## 1. `panic = "abort"` silently defeats poller.rs's own crash-containment design

**What cc-switch does.** `src-tauri/Cargo.toml`:
```
[profile.release]
panic = "unwind"
```
with the comment (translated): *"use unwind so the panic hook can capture a backtrace — abort
terminates immediately and nothing can catch it."*

**What we do.** `src-tauri/Cargo.toml` line 76: `panic = "abort"`.

**Why this is the top finding.** `poller.rs`'s own module doc comment (lines 69-79) is titled
*"Never panics"* and describes an architecture built entirely on `std::panic::catch_unwind`:
the network fetch runs in its own `tokio::spawn` so a panic surfaces as a `JoinError`; `decide()`,
tray rendering, and history recording are each individually wrapped in `catch_unwind` so "one bad
tick" can't kill the daemon. This is real, careful, well-tested code (`poller.rs:616-627`,
`642-648`, `705-708`).

**The problem: none of it works in a release build.** `catch_unwind` only catches anything when the
crate is compiled with `panic = "unwind"`. Under `panic = "abort"`, the process terminates
immediately when any panic hook returns, regardless of how many `catch_unwind` wrappers sit above it
on the call stack. Every `catch_unwind` in `poller.rs` is currently dead code in the artifact we
actually ship — a panic in, say, `tiny_skia` rendering an edge-case icon value, or a `chrono` parse
on a malformed timestamp from the network, kills the whole tray app instantly instead of being
absorbed as one skipped tick, exactly the outcome the module's doc comment says cannot happen.

The same setting is also *why* `login::sweep_stale_login_dirs` exists at all — its own doc comment
(`login.rs:521-525`) says plainly: *"this crate sets `panic = "abort"` in its release profile, and an
abort runs no destructors at all. A directory that briefly held a real credential would then survive
on disk indefinitely."* The sweep is a good backstop, but it is a symptom of the same root cause.

**Verdict: adopt.** For a credentials-handling background daemon whose entire pitch is uptime and
never-corrupt-state, `panic = "unwind"` is the right call, exactly as cc-switch concluded. It is also
just the Cargo default — `abort` had to be explicitly opted into. The downside (slightly larger
binary from unwind tables, marginally slower panicking path) is irrelevant next to "the tray app
silently dies once and doesn't come back until the user notices and relaunches it."

**Concretely:** change `src-tauri/Cargo.toml`'s `[profile.release]` from `panic = "abort"` to
`panic = "unwind"` (or delete the line — `unwind` is the Cargo default). This one line is what makes
`poller.rs`'s existing `catch_unwind` machinery actually do what its comments already claim. See
finding 3 for the one thing that must be fixed *alongside* this change, not after it.

---

## 2. No panic hook — a crash today leaves zero diagnosable trace

**What cc-switch does.** `panic_hook.rs` installs a hook that writes a timestamped report (OS/arch/
version/cwd/thread, panic message, `file:line:column`, full backtrace via `Backtrace::force_capture`)
to a dedicated `<app_config_dir>/crash.log`, size-rotated at 5 MB keeping 2 archives, guarded by a
`Mutex` so two threads panicking near-simultaneously can't interleave writes or race the rotation.

**What we do.** Nothing. `init_logging()` in `lib.rs` solves this exact problem for `log::warn!`/
`log::error!` (its own doc comment: *"stderr output is simply lost ... how a usage-fetch failure
became invisible"*), but a panic doesn't go through the `log` crate — it goes through `std::panic`,
which `init_logging` never touches. Today, a release-build panic is invisible: no console (GUI
process), no log line, nothing for a user to attach to a bug report.

**Verdict: adopt.** Directly requested; implemented below.

**What I implemented differs from cc-switch on purpose:** one shared `app.log` (via
`crate::log_path()`) rather than a second `crash.log` file, per the task's brief — one file for a
user to find and attach, not two. I did not implement rotation for it: `app.log` is concurrently held
open (in append mode) by `env_logger` for the process's whole lifetime, and renaming/truncating a file
another handle already has open for writes is the kind of platform-dependent behavior (fine on POSIX,
genuinely risky on Windows without `FILE_SHARE_DELETE`) that I'm not willing to bolt on blind, from a
file that isn't allowed to touch `lib.rs`'s `init_logging`. `app.log` growing unbounded over months of
uptime is real but low-severity (disk space, not correctness) — flagged in §9 as a separate,
lower-priority follow-up that should be designed alongside `init_logging`, not smuggled in here.

---

## 3. Lock poisoning becomes a real hazard the moment #1 ships

**What cc-switch does.** Every shared `Mutex`/`RwLock` it touches from a context that might panic
uses `.lock().unwrap_or_else(|poisoned| poisoned.into_inner())` rather than `.lock().unwrap()` — see
`panic_hook.rs`'s own `CRASH_LOG_LOCK`, `tray.rs`'s `LAST_TRAY_USAGE_REFRESH`, and `init_status.rs`.
Under `panic = "unwind"`, a panic while holding a lock poisons it; every subsequent `.lock().unwrap()`
anywhere else in the process then panics too, on a completely unrelated call site, for the rest of
the process's life.

**What we do.** Mostly the same idiom already (`switcher.rs`, `credentials.rs` — I didn't audit every
call site, but the pattern is present), *except* seven sites in `history.rs`
(`self.conn.lock().unwrap()` — lines 346, 405, 465, 523, 565, 806, 1018) and one in `lib.rs`
(`cache.0.lock().unwrap()`, line 53) use the panicking form.

**Why this matters specifically now.** Under today's `panic = "abort"`, this is moot — the process is
already dead before a second `.lock()` call could ever observe the poison. The moment finding #1
ships, though, it stops being moot: `poller.rs:616-627` already wraps `store.record(&snapshot)` in
`catch_unwind` specifically so a panic there doesn't take down the daemon — but if that panic happened
while `self.conn` was locked, the *caught* panic leaves the connection mutex poisoned, and every
subsequent tick's `store.record()` call panics again on the very next `.lock().unwrap()`, this time
for the rest of the session. The daemon itself survives (still inside `catch_unwind`), but history
recording silently and permanently stops working until restart — a regression `catch_unwind` was
supposed to prevent, just moved one layer down.

**Verdict: adopt, as a package with #1, not standalone.** Shipping #1 without this is a net negative
change (trades "the whole app dies" for "history quietly and permanently breaks after one bad tick" —
arguably worse, since it's silent). This is a small, mechanical fix (8 call sites), but it touches
files this task's constraints don't let me edit, so it isn't in `resilience.rs`.

**Concretely:** in `history.rs` and `lib.rs`, replace every `.lock().unwrap()` with
`.lock().unwrap_or_else(|poisoned| poisoned.into_inner())`.

---

## 4. Atomic file replacement and cross-file switching — implemented with recovery

**What cc-switch does.** `config.rs::atomic_write` (line 297): write to `<name>.tmp.<nanos>` in the
same directory, `flush()`, then `rename()` over the target (with an explicit `remove_file` first on
Windows, since `rename` fails there if the destination exists). `codex_config.rs::write_codex_live_atomic`
(line 223) layers a second guarantee on top: write file A, and if writing file B then fails, roll file
A back to its pre-write bytes (or delete it if it didn't exist before) — a two-file transaction that
never leaves the pair in a state neither caller nor a previous run would recognize.

**What we do now.** `durable_fs.rs` provides synced sibling staging, `ReplaceFileW` / write-through
`MoveFileExW` on Windows, and rename plus parent-directory sync on Unix. That gives atomic visibility
for each regular file, but the old review incorrectly treated per-file atomicity as a transaction
across the active credential backend, `~/.claude.json`, and `sequence.json`.

`switch_transaction.rs` now supplies the missing cross-file guarantee. It snapshots exact presence
and bytes for every active credential backend, persists protected before-images and a secret-free
journal, stages regular-file outputs, advances durable phases after each live write, verifies the
committed target, and rolls every noncommitted phase back in reverse order. The outgoing account's
credential/config generation is monotonic: once safely captured it is preserved even when the live
switch later rolls back.

**Verdict: implemented and fault-tested.** Atomic replacement prevents torn individual files;
the journal and protected before-images make the multi-file operation recoverable. These are
different guarantees and neither substitutes for the other.

---

## 5. Linux WebKitGTK workarounds — real, currently absent here

**What cc-switch does.** Three independent, cited-upstream-issue workarounds:

- `main.rs` (before Tauri even starts): sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` (white/black screen on
  some GPU+driver combos, e.g. Nvidia on Debian 13) and `WEBKIT_DISABLE_COMPOSITING_MODE=1` (webview
  crashes on resize, surface-negotiation failures on some Wayland compositors that leave the whole
  window permanently unresponsive to clicks) — both citing `tauri-apps/tauri#9394`. Both are set only
  if the user hasn't already set them (an env-var escape hatch, `CC_SWITCH_GDK_BACKEND`, exists for
  the case where the fix itself causes a regression on a specific compositor).
- `linux_fix.rs`: a `nudge_main_window` sequence (explicit second `set_focus()` after a 200 ms wait for
  webview realization, then a ±1px resize-and-restore with a reconciliation read-back) working around
  `tauri-apps/tauri#10746` / `wry#637` — the webview not receiving keyboard/click focus after
  `show()`, so the *first* click after launch does nothing at all. This is invoked from every
  "make the main window appear" path: normal startup, single-instance re-activation, deep-link
  handling.
- `lib.rs:1258-1271`: explicitly disables WebKitGTK hardware acceleration at startup (same underlying
  EGL-init class of bug).

**What we do.** Nothing — no `#[cfg(target_os = "linux")]` window/webview hardening exists in this
codebase today.

**Why this matters.** These are cited as general Tauri-on-Linux bugs (the GitHub issue numbers are
Tauri's own tracker, not cc-switch-specific), so they are not a "cc-switch problem," they're a
"Tauri + WebKitGTK on Linux problem" that any Tauri 2 app with a real window is exposed to. We have
two real windows (dashboard + popover), and README.md commits to shipping Linux builds. A user whose
first launch produces a dead-to-clicks window, or a white screen on their GPU, has no way to know it's
a known, one-line-env-var-fixable issue rather than a broken install.

**Verdict: adopt, but out of scope for `resilience.rs`** — this needs edits to `main.rs` (setting the
env vars before Tauri initializes) and to wherever the main/popover windows are shown (`lib.rs`'s
`reveal`, the single-instance callback, `toggle_popover`), which the task's constraints put out of
reach for this deliverable. Flagged here as the concretely-actionable next step, in priority order
right after #1-3.

**What I could not verify:** I have no Linux machine in this session, so none of this is confirmed
against our own Tauri/WebKitGTK version pairing — I'm relying on the upstream issue numbers cc-switch
cites and PLAN.md's own prior note ("`linux_fix.rs` exists for a reason") being right that it's worth
budgeting for before it's hit by a real user, not on having reproduced it myself.

---

## 6. Startup crash-recovery / interrupted-mutation detection — implemented

**What cc-switch does.** `services/proxy.rs::recover_from_crash` (line 2182) and
`detect_takeover_in_live_configs` (line 2206): on startup, check whether the shared "live" provider
config files were left in a "taken over" placeholder state by a proxy session that crashed mid-flight,
and if so, restore them from a DB-tracked backup automatically, rather than leaving the user's Claude
Code / Codex config pointed at a placeholder.

**What we do now.** Startup loads `switch-journal.json` before application state or the poller starts.
Noncommitted phases restore the outgoing live credential, global config, and sequence exactly;
`Committed` verifies the target hashes and cleans retained recovery artifacts. Unknown schemas,
malformed journals, missing/tampered before-images, and incomplete rollback all fail closed. Manual
and automatic switching remain disabled and the daemon reports `recoveryRequired` until recovery
succeeds.

The lock protocol is intentionally the same as current cswap/Claude Code. A hard termination leaves
proper-lockfile directories behind; fresh credential locks are not stolen for 60 seconds. Startup
tries once, then bounded background retries cross that staleness window without blocking the UI.

**Verdict: implemented and process-death tested.** Tests terminate a child with `abort()` after the
journal, each live write, and commit, then recover from a fresh process. A second termination during
rollback is also recovered idempotently.

---

## 7. Tray hardening — partially already present, one idiom missing

**What cc-switch does.** `tray.rs` debounces two different things: `schedule_tray_refresh` (line
1012) coalesces rapid successive "rebuild the tray" triggers into one, 50ms-delayed rebuild (a plain
`AtomicBool` guard, not a lock); `refresh_all_usage_in_tray` (line 1027) separately throttles the
*network-fetching* usage refresh to at most once per 10 seconds regardless of how many times the user
hovers the tray icon. Both use the poisoned-lock-tolerant idiom from §3.

**What we do.** The equivalent idea for icon *repainting* is already implemented and, if anything,
cleaner: `TrayCache` (`lib.rs:40-41`) and `update_tray_icon` (`poller.rs:866-879`) both key off
`IconSpec::cache_key()` so the (comparatively expensive) rasterization only runs when the displayed
value actually changed — this is the same debounce goal as cc-switch's `schedule_tray_refresh`,
solved with a cache key instead of a timer, which is arguably more precise (it depends on the value
changing, not on wall-clock coalescing). We have no dynamic tray *menu* content today (the menu is
three static items built once in `setup()`), so cc-switch's "don't rebuild the menu while a user has
it open on macOS" concern (their `TRAY_REBUILD_SCHEDULED` comment) doesn't apply yet — it would if a
future feature added a dynamic per-account submenu.

**Verdict: no action beyond §3's lock-poisoning fix**, which already covers `TrayCache`'s one
`.lock().unwrap()`. Revisit the "soft update vs. full rebuild" distinction only if the tray menu grows
dynamic content.

---

## 8. Autostart — already avoids the bug cc-switch had to work around

**What cc-switch does.** `auto_launch.rs` bypasses `tauri-plugin-autostart` entirely on macOS,
resolving the `.app` bundle path itself (`get_macos_app_bundle_path`) before calling the `auto_launch`
crate directly. The comment explains why: the AppleScript-login-item method needs the bundle path, or
it opens a Terminal window instead of launching the app silently.

**What we do.** `lib.rs:230-233`: `tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None)`
— the first-party plugin, explicitly configured to use the `LaunchAgent` (a `.plist`) mechanism rather
than an AppleScript login item.

**Verdict: reject adopting cc-switch's workaround.** It solves a problem specific to the AppleScript
login-item method, which we don't use — we already sidestep the underlying bug via a different launch
mechanism. Not verified end-to-end on a real Mac in this session (no macOS machine available), but the
configuration is unambiguous in the source.

---

## 9. CI / release matrix — near parity already

**What cc-switch does** (`.github/workflows/release.yml`): `windows-2022`, `windows-11-arm`,
`ubuntu-22.04`, `ubuntu-22.04-arm`, `macos-14` (building both `aarch64-apple-darwin` and
`x86_64-apple-darwin` targets for a universal-ish release), `fail-fast: false`. `ci.yml` runs a
per-OS Rust matrix (`ubuntu-22.04`, `windows-latest`, `macos-latest`) doing `fmt --check`, `clippy -D
warnings`, `cargo test`, plus a separate frontend job.

**What we do** (`.github/workflows/{ci,release}.yml`): release matrix is `windows-latest`, `macos-14`,
`macos-13` (Intel), `ubuntu-22.04`, `fail-fast: false`; CI runs a per-OS Rust matrix
(`ubuntu-latest`/`windows-latest`/`macos-latest`) with `fmt --check` (ubuntu only), `clippy --all-targets
--all-features -D warnings`, `cargo test --all-features`, plus a separate frontend job. Structurally
the same shape as cc-switch's, arrived at independently.

**Verdict: no action needed now.** The only gaps are the ARM Windows/Linux runners, which matter once
there's ARM64 Windows/Linux user demand to justify the extra CI minutes on a pre-release (`0.1.0`)
project — not now.

---

## Deliberately rejected from cc-switch (and why)

- **Separate `crash.log` with size-based rotation.** Rejected in favor of appending to the existing
  `app.log`, per this task's brief — one file, not two, for a user to find. Rotation deferred; see §2.
- **The bulk of `tray.rs` (dynamic per-provider submenus, i18n tier grouping, ~40 KB of menu-building
  logic).** Reflects cc-switch managing a dozen-plus AI CLI providers with per-provider usage tiers; we
  manage one thing (Claude accounts) with a three-item static menu. Nothing here is a resilience
  pattern once the provider-count driving it is factored out.
- **`init_status.rs`'s `OnceLock<RwLock<...>>` startup-error handoff and the DB schema-version guard in
  `main.rs`/`database.rs` (`stored_user_version_exceeds_supported`).** Solves "an old client must not
  let a newer client's DB schema corrupt it" — a real problem once you ship versioned SQLite
  migrations across releases. `history.rs` doesn't have versioned migrations yet; adopting this now
  would be solving a problem this codebase doesn't have. Revisit if/when `history.rs` grows a schema
  version.
- **Deep-link handling (`ccswitch://` URLs), WebDAV/S3 auto-sync workers, the proxy failover/circuit-
  breaker layer.** Out of scope entirely — different product surface, not resilience patterns.
- **cc-switch's own `auto_launch.rs` macOS bundle-path workaround.** See §8 — we don't have the bug it
  fixes.

## What I could not verify

- Nothing on this machine runs Linux or macOS, so §5 and §8 are read from source and upstream issue
  citations, not reproduced here.
- I did not download or read cc-switch's `tests/` directory, so I can't say whether it has anything
  resembling this project's `test_support::guard_real_store` fail-safe (PLAN.md §7a) — the incident
  that guard exists for (a parallel test suite race writing to a real credential store) may or may not
  have a cc-switch analogue; I have no evidence either way.
- `codex_config.rs`, `app_config.rs`, and `services/proxy.rs` were read selectively (grepped for the
  atomic-write and crash-recovery functions specifically), not read cover-to-cover — there may be
  other patterns in the unread ~90% of those files I didn't surface.

---

## What's in `src-tauri/src/resilience.rs`

A panic hook (`resilience::install()`) that appends a bordered crash report — timestamp, thread name,
panic message, `file:line:column`, and a best-effort backtrace — to `crate::log_path()` (the same
`app.log` `init_logging()` writes to). It is written to work correctly regardless of the crate's
`panic` profile setting (the hook itself always fires; see §1 for what depends on the profile
*beyond* the hook). Every internal step that could itself panic (timestamp formatting, file I/O) is
defensive — a panic *inside* the panic hook has no fallback and would abort with nothing written at
all, which is worse than a slightly degraded report.

Four `#[cfg(test)]` tests: report formatting contains every expected field; `append_report` creates
its parent directory and appends rather than overwrites; `append_report` degrades silently (no panic)
on an unwritable path; and an end-to-end test that installs the hook, triggers a real panic under
`catch_unwind`, and confirms the message/location extraction works against a genuine `PanicHookInfo`
(guarded with a unique marker string so it can't be confused with an unrelated panic on another test
thread, and restores the previous hook immediately afterward).

## Wiring needed in `lib.rs`

Not done here — the task's constraints only allow creating the two new files. To activate:

1. Add `pub mod resilience;` to the `pub mod` list near the top of `lib.rs` (alongside `commands`,
   `credentials`, etc.).
2. Call `resilience::install();` as the **first line** of `run()`, before `init_logging()` — so a
   panic during logging setup itself is still captured. (`log_path()` is already `pub`, and
   `resilience::install()` creates its own parent directory on demand, so ordering relative to
   `init_logging()` doesn't matter for correctness — doing it first just maximizes coverage.)
3. **Companion change this hook is designed to complement, not a substitute for it:** switch
   `src-tauri/Cargo.toml`'s `[profile.release]` from `panic = "abort"` to `panic = "unwind"` (§1), and
   fix the eight `.lock().unwrap()` call sites in `history.rs`/`lib.rs` to the poison-tolerant form
   (§3). The hook alone (step 1-2) is a strict improvement with zero downside on its own — it works
   under either `panic` setting — but its value is capped at "you'll at least know what killed the
   process" until §1 ships; after that, it also means most panics stop killing the process at all.
