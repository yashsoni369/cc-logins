# Claude Account Switcher

A tray application for developers who hold more than one Claude subscription and run Claude Code
all day. It shows which account is active and how much 5-hour and 7-day quota each account has
left, and switches between them — one click, or automatically before a limit lands. Windows,
macOS, and Linux.

The package and repository are named `claude-swap-gui`, a working title from before the project
had a real name. The shipped app identifies itself as **Claude Account Switcher**.

This project is young (`0.1.0`) and pre-release. Expect rough edges.

## Before you install this: what it does with your tokens

This app reads `~/.claude/.credentials.json` — the same file Claude Code itself uses — refreshes
OAuth tokens when they're near expiry, polls Anthropic's usage endpoint to compute quota, and
writes that same credentials file when you switch accounts, so the *official* Claude Code binary
picks up the new identity on its next request.

What it does **not** do:

- It does not proxy, relay, or intercept model traffic. There is no server. Every inference
  request still goes straight from Claude Code to Anthropic, unmodified.
- It does not export tokens to a UI, a file you can copy elsewhere, or anywhere off your machine.
  There is no cloud sync.
- It never shows or sends token material anywhere other than the Anthropic OAuth endpoints and
  your own disk.

This distinction matters because Anthropic's terms treat it as the line between what's allowed and
what isn't. They accept per-profile switching between real, individually-authenticated accounts
using the official client — refresh a token, hand it to the real binary, done. They ban relay
servers that pool tokens and impersonate the official client to multiple users or sessions at
once. Running several accounts you legitimately hold is not itself a violation; evading rate
limits through token sharing or relaying is. This app only ever does the former: it moves
credentials on your own machine, into the same binary you'd otherwise run by hand.

That's a description of what the app does, not a legal opinion — read Anthropic's terms yourself
if you're deciding whether multi-account use fits your situation. This project takes no position
beyond staying on the side of it described above.

## What makes this different from another account switcher

- **Local usage history and burn-rate charts.** Every reading is written to a local SQLite
  database, so you can see how fast an account is climbing toward its limit over days and weeks,
  not just its current percentage. Nothing else in this space tracks history at all — it's the
  main reason to run this instead of a one-shot CLI command.
- **WSL-aware.** On Windows, Claude Code running natively and Claude Code running inside a WSL
  distro keep completely separate logins. This app detects both and presents them as separate
  environments to switch between, rather than silently only seeing one.

## Safe defaults

- **Auto-switch is off by default.** The app will show you how close an account is to its limit,
  but it will not move your credentials until you turn auto-switch on yourself. Software that
  starts relocating auth tokens before being asked isn't trustworthy, so the default is to do
  nothing.
- **A 60-second grace period before an armed automatic switch fires.** When auto-switch is on and
  a threshold is crossed, you get a visible countdown — and a chance to cancel — before anything
  actually happens. Silently swapping credentials out from under a task in progress is worse than
  the rate limit it's trying to avoid.
- **A stopped WSL distro is never woken by a background poll.** Touching a WSL distro's files over
  the `\\wsl$` path silently boots its VM, even for something as innocuous as checking whether a
  file exists. This app only ever calls `wsl.exe`'s list commands from its polling loop, which are
  confirmed not to start anything; when a distro is asleep, its last-known numbers are shown with a
  staleness label instead, and waking it up to get a fresh reading is an explicit, user-initiated
  action.

## Relationship to the `cswap` CLI

[claude-swap](https://github.com/realiti4/claude-swap) (`cswap`) is an existing, more mature
Python CLI and macOS menu bar tool that solves the same problem. This app is **not** a wrapper
around it — it never shells out to `cswap`, and it doesn't require it to be installed. It's an
independent Rust implementation: the credential handling, path resolution, OAuth refresh, and
usage-tracking logic are ported from `claude-swap` (MIT-licensed, ported with attribution — see
[LICENSE](LICENSE)), rewritten in Rust rather than wrapped.

It keeps its own credential vault in its own app-data directory — never inside `cswap`'s directory
— so a bug in this project can't corrupt `cswap`'s accounts, or vice versa. The two tools still
interoperate where it counts: this app can read `cswap`'s store to import accounts, and it locks
the shared credentials file using the same cross-process file-locking protocol `cswap` does
(`flock` on Linux/macOS, `LockFileEx` on Windows), so a `cswap` process running in a terminal and
this app sitting in the tray can touch the same files without corrupting each other. That lock
compatibility is checked by an automated test that runs the real, installed `claude_swap` Python
package on one side and this app's Rust lock implementation on the other, contending for the same
lock file — not two separate reimplementations that happen to agree.

### How credentials are stored

Accounts live in this app's own directory, not in the CLI's. Each stored credential carries a
self-describing envelope recording what protection was actually applied:

```json
{"v":1,"scheme":"dpapi"|"keychain"|"plain","data":"..."}
```

| Platform | Scheme | What it means |
|---|---|---|
| Windows | `dpapi` | Encrypted with DPAPI, per-user. Copying the file to another machine or user account yields nothing useful. |
| macOS | `keychain` | Stored in the system Keychain under this app's own service name. |
| Linux | `plain` | **Not encrypted.** A `0600` file, readable by your user account. |

Linux is a genuine gap and the envelope says so rather than implying otherwise — reading a stored
credential tells you exactly how well it was protected. A decryption failure never deletes or
rewrites the stored file; it reports an error and leaves the bytes alone, because re-logging in is
recoverable and a wiped vault is not.

## Building from source

Prerequisites:

- [Rust](https://rustup.rs/), a recent stable toolchain (1.77 or newer)
- [Node.js](https://nodejs.org/), a recent LTS (Vite 7 needs 20.19+ or 22.12+)
- [pnpm](https://pnpm.io/) (this repo pins `pnpm@10.14.0` via `packageManager`)
- Platform build tools for Tauri:
  - **Windows**: [MSVC Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
  - **macOS**: Xcode Command Line Tools (`xcode-select --install`)
  - **Linux**: `webkit2gtk`, plus the other packages listed in
    [Tauri's Linux prerequisites](https://v2.tauri.app/start/prerequisites/#linux)

Then:

```sh
pnpm install
pnpm tauri dev      # run it locally
pnpm tauri build    # produce an installer for your platform
```

## Signing

Builds are currently **unsigned**. On Windows, expect a SmartScreen warning ("Windows protected
your PC"); on macOS, expect Gatekeeper to refuse to open it until you right-click → Open, or clear
the quarantine attribute yourself. This is not a bug report waiting to happen — it's the current
state, and it will stay true until this project has a code-signing certificate, which costs money
(Apple) or requires an established open-source track record (Windows, via programs like the
SignPath Foundation). Verify what you're running before you trust it with your tokens.

## License

MIT — see [LICENSE](LICENSE). Portions of the credential, path-resolution, locking, and
usage-tracking logic are ported from [claude-swap](https://github.com/realiti4/claude-swap) by
Onur Cetinkol (`realiti4`), also MIT-licensed; that attribution is reproduced in full in
[LICENSE](LICENSE) as the license requires.
