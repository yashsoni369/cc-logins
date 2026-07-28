# cc-logins — Desktop App Plan

Product name: `cc-logins` (package/crate/bundle-id renamed from the `claude-swap-gui` working
title — see §9.2). Status: pre-scaffold. Last updated 2026-07-28.

---

## 1. The decision

Build a **native cross-platform desktop app that is independent of `claude-swap` at runtime, but
byte-compatible with it on disk.**

- Not a fork.
- Not a subprocess wrapper.
- A Rust port of the credential/usage core (claude-swap is MIT — porting with attribution is legal
  and cheap), reading and writing *the same paths* and honouring *the same lock files*.

The result: users can run `cswap` in a terminal and this app in the tray simultaneously, both seeing
the same accounts, neither corrupting the other. Additive, not competitive.

---

## 2. What the research changed

### 2.1 The "gap" was mis-stated

`cswap` v0.23.0 already ships auto-rotation (`auto`, with threshold / strategy / cooldown /
hysteresis), per-model scoped limits, burn-rate projection, a full TUI, and a macOS menubar. The
missing thing is not features — it is **surface**. There is no always-on GUI outside macOS.

### 2.2 Competitive reality

| Repo | Stars | Lang | Last push | Category |
|---|---:|---|---|---|
| farion1231/cc-switch | 121,700 | Rust / Tauri | 2026-07-27 | providers |
| realiti4/claude-swap | 1,369 | Python | 2026-07-27 | accounts |
| XueshiQiao/CCSwitcher | 160 | Swift (macOS) | 2026-06-10 | accounts |
| Symbioose/claude-account-switcher | 45 | Python | 2026-05-19 | accounts |
| vyshnavsdeepak/ccswitch | 10 | Rust | 2026-03-04 | accounts |
| brenoluizdev/claude-account-switcher | 1 | C# | 2026-07-12 | accounts |

Two conclusions. **Never compete with cc-switch on providers/MCP/skills** — it is 6× larger than
previously believed and shipping daily. And **the account-switcher GUI field outside macOS is
empty** — the best non-macOS entry has 10 stars.

Critically, cc-switch's own `src-tauri/src/usage_events.rs` is **3 KB** against `tray.rs` at 58 KB and
`codex_config.rs` at 139 KB. Usage tracking is a genuine blind spot in the market leader.

### 2.3 Upstream demand is pre-validated

Open issues on claude-swap — #168, #141, #102, #111 — are all the same request: make the menubar a
persistent background service with login startup and desktop notifications. The maintainer is
solving it in Python + launchd, macOS-only. That is the product, already asked for.

### 2.4 The API surface is small

```
GET  https://api.anthropic.com/api/oauth/usage      Authorization: Bearer <tok>
                                                    anthropic-beta: oauth-2025-04-20
POST https://platform.claude.com/v1/oauth/token     grant_type=refresh_token
                                                    client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e
GET  https://api.anthropic.com/api/oauth/profile    Authorization: Bearer <tok>
```

Three endpoints, ~50 lines. The protocol is *not* the expensive part.

### 2.5 What is actually expensive

16,111 lines of Python total. Excluded from any port: `cli.py` (1,211), `menubar.py` (839),
`tui/*` (~1,700), `transfer.py` (527), `migrations.py` (454 — their schema history, we start clean).
Core to port ≈ **8,500 lines**: `switcher` (4,396), `autoswitch` (1,295), `credentials` (1,051),
`session` (944), `usage_store` (587), `oauth` (575), `paths`, `locking`, `pace`, `poll_policy`.

The cost is the hardening, not the protocol:

- 401 → refresh → retry, once, inactive accounts only
- `invalid_grant` quarantine so dead refresh lineages stop retrying forever
- `Retry-After` / HTTP 429 backoff against a **per-access-token usage request budget**
- Never refresh the *active* account — Claude Code owns those bytes
- Schema drift: newer `limits[]` array supersedes legacy `five_hour` / `seven_day`

Every one of those carries a comment referencing a real bug. Port them; do not rediscover them.

---

## 3. Verified platform facts

### 3.1 On-disk layout

| Thing | Location |
|---|---|
| Active credentials | `(CLAUDE_CONFIG_DIR \|\| ~/.claude)/.credentials.json` |
| Global config | `(CLAUDE_CONFIG_DIR \|\| $HOME)/.claude.json` — note: at homedir, **not** inside `.claude/` |
| Backup root (Linux/WSL) | `$XDG_DATA_HOME/claude-swap` |
| Backup root (macOS/Win) | `~/.claude-swap-backup` |
| Backup format (macOS) | Keychain, with file fallback |
| Backup format (Linux/WSL/Win) | base64 `.enc` files |

### 3.2 Locking

Plain advisory OS locks — `msvcrt.locking` on Windows, `fcntl.flock` on POSIX, over a lock file,
0.1 s poll, 10 s default timeout. Rust speaks both natively (`LockFileEx` / `flock`). **This is why
interop works.** Implement the identical protocol and coexistence is free.

Current upstream keeps profile and usage calls outside locks. Its one deliberate exception is an
active-token refresh grant: the bounded POST runs under the account and Claude credential locks so
Claude Code cannot consume the same refresh generation concurrently. The config lock is not held.

### 3.3 base64 `.enc` is not encryption

On Windows and Linux the credential backups are base64 — obfuscation only. For an app whose entire
pitch is "trust me with your auth tokens," this is an open goal. Use **DPAPI** on Windows
(free, native, per-user), Keychain on macOS, Secret Service on Linux with a file fallback for
headless/D-Bus-less systems.

Compatibility note: writing DPAPI-wrapped blobs breaks byte-compat with `cswap`. Resolution — keep
reading/writing the base64 `.enc` for interop, and additionally maintain a DPAPI-protected store as
the app's own source of truth, or make encryption opt-in behind an "interop mode" toggle. **Decide
before v0.1 ships; this is the one place the interop promise and the security promise conflict.**

### 3.4 Tray text is macOS-only — design constraint

macOS `NSStatusItem` supports a text title. **Windows tray supports icon + tooltip only.** Linux
shows a title only alongside an icon, and the platform guidance discourages it.

So "tray shows live usage %" cannot be done with `set_title` on Windows. Options:

1. **Render the number into the icon bitmap at runtime** (`tiny-skia` / `image`, redraw on poll).
   This is what Windows tray utilities do. Recommended.
2. Tooltip only — hover to see usage. Weak; defeats "ambient".
3. Colour-coded icon (green/amber/red by headroom) + tooltip for exact numbers. Good fallback,
   and worth doing *in addition* to (1).

Budget real time for this. It is the single feature the whole product rests on.

### 3.5 WSL — verified behaviour

Confirmed on this machine (Ubuntu + docker-desktop, both `Stopped`):

- `wsl.exe -l -q` lists distros. **Output is UTF-16LE** — decode explicitly or you get spaced
  garbage.
- `wsl.exe -l -q --running` correctly returns empty and **does not start anything**. Safe to poll.
- `Test-Path "\\wsl$\Ubuntu\home"` returned **True while the distro was Stopped** — touching the UNC
  path **silently boots the distro**.

That last point is the trap. A tray app polling WSL every 60 s would spin up a Linux VM in the
background forever. **Design rule: gate all `\\wsl$` access behind `wsl -l -q --running`. Never
auto-start a distro from a background poll.** When a distro is asleep, show last-known values with a
staleness badge and an explicit *Wake & refresh* button.

---

## 4. Scope

### 4.1 v0.1 — confirmed

1. Tray icon with live active-account + headroom, rendered into the bitmap (see 3.4)
2. Click-to-switch from the tray menu
3. All-accounts dashboard: usage bars, reset countdowns, pace indicators
4. **Start at login** — Tauri autostart, all three OSes (answers upstream #168/#141/#102)
5. **Daemon supervision** — own the autoswitch loop in-process, with an on/off toggle
6. **Native notifications** — on switch, threshold crossing, token expiry (upstream #111, unbuilt)
7. **Usage history + burn-rate charts** — persist snapshots locally, chart per-account over days
   and weeks. *The strongest differentiator; nobody has it; zero ToS risk since it is local data.*
8. **WSL / Windows dual-realm** — detect both, present two credential realms, subject to 3.5

Items 4–8 are all confirmed in scope. Item 8 is the highest-risk item in the list; see §7.

### 4.2 Explicit non-goals

| Not building | Why |
|---|---|
| Provider / endpoint switching | cc-switch, 121.7k stars. Unwinnable. |
| MCP / skills management | Same. |
| Relay or proxy support | Architecture A — actively banned in waves. |
| Credential cloud sync | Maximum liability for zero differentiation. |
| GUI token export/import | Lowers friction on exactly the behaviour that gets accounts banned. Keep CLI-only. |
| **Any inference / completion call** | The bright line. See §6. |

---

## 5. Stack

| Layer | Choice | Note |
|---|---|---|
| Shell | Tauri 2 | ~8 MB; first-party tray, autostart, updater, notification, single-instance plugins |
| Frontend | React + TS + Vite + Tailwind | Next.js muscle memory transfers |
| Charts | Recharts or uPlot | uPlot if history grows large |
| Backend | Rust | `reqwest`, `serde`, `tokio`, `fs4` (locking), `windows` (DPAPI), `security-framework` (Keychain), `tiny-skia` (icon rendering) |
| State | Zustand | TanStack Query is overkill — no server, ~10 accounts |
| Storage | SQLite via `rusqlite` | Needed for time-series history; JSON is not enough here |
| Updates | `tauri-plugin-updater` + GitHub Releases | |
| CI | `tauri-apps/tauri-action` matrix | `windows-latest`, `macos-14` (arm), `macos-13` (intel), `ubuntu-22.04` |

Prerequisite not yet met: **Rust is not installed on this machine.** rustup + MSVC Build Tools,
~15 min and several GB. First concrete step.

Patterns worth studying from cc-switch (`src-tauri/src/`): `tray.rs` (58 KB — tray is genuinely
hard), `auto_launch.rs`, `panic_hook.rs` (9 KB — they invest in crash handling), `linux_fix.rs`
(Linux needs workarounds), and `.github/workflows/release.yml`.

---

## 6. Positioning: compliance is the feature

Anthropic accepts **Architecture B** — isolated profiles, official binary, real interactive logins.
Anthropic bans **Architecture A** — relay servers holding many tokens and impersonating the official
client. The Feb 2026 policy explicitly prohibits subscription OAuth tokens in third-party harnesses.
Multiple accounts are *not* a violation; **limit evasion and token sharing are.**

cc-switch ships community-relay presets and a "signature bypass" utility. That is the contrast to
lean on.

**The line, to be stated in the README on day one:** this app refreshes tokens and reads usage
telemetry, and hands credentials to the official Claude Code binary. It never proxies model traffic.
Token refresh + read-only telemetry + official binary = Architecture B. The moment anything makes an
inference call, it is Architecture A.

Ship an honest ToS section. Default the autoswitch threshold conservatively. That is credibility,
not legal cover.

---

## 7. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| WSL polling silently boots VMs | **High** | Gate on `--running`; never auto-start. Verified §3.5. |
| Windows tray can't show text | **High** | Render into icon bitmap; colour-code as fallback. §3.4 |
| Interop vs. real encryption conflict | **High** | Decide before v0.1 — dual store or opt-in interop mode. §3.3 |
| No upstream fixes inherited | Medium | Accepted cost of independence. The `limits[]` drift proves Anthropic changes this; budget maintenance. |
| Upstream ships cross-platform GUI first | Medium | They are macOS-only and Python-bound today. Speed is the mitigation. |
| Code signing | Medium | **Azure Trusted Signing is unavailable in India** (US/CA/EU/UK only). Ship unsigned → apply to SignPath Foundation (free for OSS; needs a released project + MFA on GitHub). Apple Developer $99/yr for macOS. |
| Effort underestimated | Medium | v0.1 ≈ 2 weeks. Parity with autoswitch + session isolation ≈ 4–8 weeks solo. Not a weekend. |
| Scope creep into cc-switch territory | Low | §4.2 is binding. |

---

## 6a. Superseded: the vault is not shared

The original design in §1 was "independent code, interop-compatible storage" — read and write the
*same* paths as the `cswap` CLI so both tools see one set of accounts. **That was wrong, and §7a is
the proof.** Sharing a mutable directory means either tool's bug is both tools' bug; a fault in this
project's test suite destroyed the user's CLI accounts because they were literally the same bytes.

The interop boundary was mis-drawn. The genuinely shared resource is **Claude Code's official
credential file** — the thing both tools install a login into. The *vault* never needed to be shared.

Current architecture:

```
OUR VAULT (ours, unconditionally — per-platform app-data dir)
  accounts/
    sequence.json     the registry
    credentials/      one protected blob per account
    configs/          per-account .claude.json snapshot
        |
        |  switch installs the chosen login into...
        v
CLAUDE CODE'S OFFICIAL LOCATION (the only genuinely shared thing)
  (CLAUDE_CONFIG_DIR || ~/.claude)/.credentials.json
  (CLAUDE_CONFIG_DIR || $HOME)/.claude.json
```

Locks are split to match the resources and acquired in one structural order: the optional
`<cswap-store>/.lock` OS file lock, Claude Code's primary `.oauth_refresh.lock`, its legacy
credential lock, its global-config lock, then this GUI's private vault lock. The primary/legacy pair
and staleness values track Claude Code 2.1.218 through pinned cswap commit
`65d208081a4985b9fd1786bc258d5172d196bee2`. No network call occurs while this **full** set is held;
active refresh uses a narrower set without the config lock and a six-second request deadline.

Switching is a durable transaction, not merely three atomic writes. Protected before-images and a
secret-free phased journal recover the active credential backend, global config, and sequence after
ordinary failure or process death. See `docs/TRANSACTION_RECOVERY.md`.

There is no storage-location setting. The `StorageMode` toggle that briefly existed was a control
offering users a choice they should never have been asked to make, and for a period it also did
nothing at all — it saved a value no code read.

## 7a. Incident: the test suite destroyed real accounts

**What happened.** During development, a run of the Rust test suite rewrote the developer's real
`~/.claude-swap-backup/sequence.json`, deleting two of three registered accounts. Only the
then-active account survived. The Anthropic accounts themselves were unaffected — this was the local
registry — but the stored OAuth credential backups for the other two were gone and required
re-authentication.

**Cause.** Tests redirect path resolution by setting `CLAUDE_CONFIG_DIR`, `HOME`, `USERPROFILE` and
`XDG_DATA_HOME`. Environment variables are **per-process, not per-thread**, and Rust's test harness
runs tests in parallel threads of one process. Each module guarded its own tests with its own mutex,
which is a lock on nothing shared. A test that lost the race ran against the real store.

**Why it wasn't caught.** The race was diagnosed and filed as the highest-priority gap — and the
suite kept being run anyway. Filing a ticket is not a mitigation. The correct response to
"the test suite may write to real credential paths" is to stop running it immediately.

**The fix, and why the obvious one was wrong.** A shared process-wide mutex was necessary but not
sufficient: a lock only protects tests that remember to take it, and "remember to do X" is precisely
the guarantee that had already failed. The load-bearing defence is instead a fail-safe at the point
where paths are produced:

- `test_support::guard_real_store(path)` panics unless the path is inside a temp directory.
- `paths::backup_root()`, `paths::claude_config_home()` and `paths::global_config_path()` all call it
  under `#[cfg(test)]`.

Because the guard sits in path resolution rather than in each test, **no test can opt out**, whether
or not it took the lock. A test that forgets isolation now fails loudly instead of destroying data.

**Rules that follow from this, and must not be traded away:**

1. Never weaken, bypass or `#[ignore]` `guard_real_store`. If it fires, a test was about to touch a
   real store. Fix the test.
2. Never "fix" flakiness with `--test-threads=1` in config. That hides the race rather than removing
   it, and the suite must be correct under the default parallel run that CI and contributors use.
3. Verify data integrity, don't assume it: hash the real `sequence.json` before and after a run and
   compare. Stability was confirmed by 14 consecutive parallel runs with a byte-identical store.

**Current verification.** Cross-process Rust/Python fixtures exercise cswap-compatible file and
Claude directory locks, including contention, stale takeover, and release ordering. Process-death
tests cover every live-write boundary and a second crash during recovery. Packaged smoke tests on
real macOS/APFS and Linux/ext4 hosts remain release-environment gates, not claims made by Windows CI.

## 7c. Incident: duplicate detection defeated by token rotation

**What happened.** The same account was registered twice. Both slots carried
`charlie@example.com` and the same `organizationUuid`; the second was added hours after the first.

**Cause.** `add_current_account` detected duplicates via `oauth::credential_fingerprint`, which
hashes the **refresh token** when one is present. But `try_refresh_oauth_credentials` rotates that
refresh token whenever the server issues a new one. Observed:

```
slot 1  refreshToken sha256: e9938586d217fcad
slot 2  refreshToken sha256: 0b3c888d8bf0b1b9   <- same account
```

So the fingerprint is stable across *access*-token rotation but not *refresh*-token rotation. The
check was identifying the credential, not the account.

**The tell nobody read.** Slot 1 carried a `uuid`; slot 2 had none. Identity was never resolved for
the new entry, so there was nothing stable to compare against next time.

**Fix.** Compare account identity, in priority order: account `uuid` when both sides have one, else
`organizationUuid` + trimmed case-insensitive email, else fall back to the fingerprint. Resolved
identity is now *persisted* onto the record, which is what stops the cycle repeating. Resolution
happens before the lock is taken, preserving the never-hold-a-lock-across-a-network-call rule, and
when identity cannot be resolved at all the add is allowed with a warning — refusing a legitimate
add because the network is down is the worse failure.

**Rule that follows:** identity is a property of the account, never of the credential bytes.
Anything derived from a token is a cache key, not an identity.

## 7b. Untestable platform paths

Recorded rather than left as silent unknowns:

| Path | Why it cannot be verified here | Risk |
|---|---|---|
| macOS Keychain | No Mac available. The port uses `security-framework` (in-process) where the Python shells out to `/usr/bin/security`. Same Keychain items, but a rebuilt binary is a new "creator" and may re-trigger an access prompt after every update. | Medium — annoying, not destructive |
| Linux Secret Service | Not exercised. Needs a running D-Bus; headless and minimal systems have none, and must fall back to file storage. | Medium |
| Windows `LockFileEx` FFI | Written without a compiler available, later compiled clean — but compiling is not interoperating. | High until the interop test lands |
| `security-framework` credential format | Must remain byte-compatible with what `cswap` writes, or the two tools stop seeing each other's accounts on macOS. | High, unverified |

## 8. Milestones

**M0 — Setup.** rustup + MSVC Build Tools. Tauri 2 scaffold. Tray icon appears.

**M1 — Read-only.** Port `paths` + `credentials` (read) + `oauth`. Enumerate accounts, fetch usage,
render the dashboard. No writes. Prove the port against `cswap list --json` output as ground truth.

**M2 — Tray.** Bitmap icon rendering with headroom. Colour states. Tooltip. Autostart.

**M3 — Writes.** Port `switcher` + `locking`. Click-to-switch. Verify coexistence with `cswap`
running concurrently in a terminal — this is the interop acceptance test.

**M4 — Daemon.** Port `autoswitch` + `poll_policy` + `pace`. Supervised in-process. Notifications.

**M5 — History.** SQLite snapshots, burn-rate charts. The differentiator.

**M6 — WSL.** Dual-realm resolver, subject to §3.5 rules.

**M7 — Release.** CI matrix, updater, unsigned release, README with ToS section, SignPath application.

---

## 9. Open questions

1. **Interop vs. encryption** (§3.3) — dual store, or opt-in interop mode? Blocks M3.
2. ~~Product name.~~ Resolved: renamed to `cc-logins`. `claude-swap-gui` was a working title that
   implied a dependency the project does not have; the repository directory keeps that old name for
   historical reasons only — the package, crate, bundle identifier, and product name are all
   `cc-logins`.
3. Does the port keep `cswap`'s slot/alias/mapping semantics exactly, or only the credential and
   usage layers? Exact-match is more interop but more porting.
4. Attribution form — MIT header retention is required; how prominently beyond that?

---

## Sources

- [realiti4/claude-swap](https://github.com/realiti4/claude-swap) (MIT) — source read locally at v0.23.0
- [farion1231/cc-switch](https://github.com/farion1231/cc-switch)
- [Two Multi-Account Claude Code Architectures — one Anthropic accepts, one they ban](https://dev.to/vainamoinen/two-multi-account-claude-code-architectures-one-anthropic-accepts-one-they-ban-2om7)
- [Anthropic bans subscription OAuth for third-party tools](https://alternativeto.net/news/2026/2/anthropic-officially-bans-using-subscription-authentication-for-third-party-claude-use)
- [Tauri v2 system tray](https://v2.tauri.app/learn/system-tray/) · [tray set_title](https://github.com/tauri-apps/tauri/issues/3322)
- [WSL interop](https://learn.microsoft.com/en-us/windows/dev-environment/wsl-interop) · [WSL basic commands](https://learn.microsoft.com/en-us/windows/wsl/basic-commands)
- [Trusted Signing pricing](https://azure.microsoft.com/en-in/pricing/details/trusted-signing/) · [SignPath Foundation terms](https://signpath.org/terms.html)
