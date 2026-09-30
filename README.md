<div align="center">

<img src="src-tauri/icons/128x128.png" alt="" width="96" height="96" />

# CC Logins

**Quota visibility and account switching for Claude Code**

by [Yash Soni](https://github.com/yashsoni369), founder of
[Apex36 Technologies](https://apex36tech.com/?utm_source=cc-logins)

<sub>Independent project. Not affiliated with or endorsed by Anthropic.</sub>

[![Release](https://img.shields.io/github/v/release/yashsoni369/cc-logins?style=flat-square&label=release)](https://github.com/yashsoni369/cc-logins/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/yashsoni369/cc-logins/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/yashsoni369/cc-logins/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/yashsoni369/cc-logins?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey?style=flat-square)](#install)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24c8db?style=flat-square)](https://v2.tauri.app/)

</div>

![The Dashboard: pooled runway across every account, a utilisation line per account over the last
seven days, and a timeline of when each account's 5-hour window
resets.](docs/screenshots/dashboard.png)

A tray application for developers who hold more than one Claude subscription and run Claude Code
all day. It shows how much 5-hour and 7-day quota each account has left, and picks which account
new Claude Code sessions start with — one click, or automatically before a limit lands. Windows,
macOS, and Linux.

This project is young; expect rough edges, and see
[Before you install this](#before-you-install-this-what-it-does-with-your-login) for what it does
with your login.

## Before you install this: what it does with your login

Since v0.4 the app never stores, copies or refreshes a Claude login.

- **Each account has its own Claude Code folder.** Adding an account opens the official
  `claude auth login` in a terminal, with `CLAUDE_CONFIG_DIR` pointed at a new folder under
  `~/.cc-logins/profiles/`. The sign-in completes in Anthropic's own flow and stays in that folder,
  exactly where Claude Code puts it. The account already signed in to your normal `~/.claude` is
  used as it is.
- **Picking an account changes which folder new sessions use.** The optional `claude` command
  (see [The claude command](#the-claude-command)) starts Claude Code with the selected account's
  folder. Sessions already running keep their account.
- **Usage is read with the token Claude Code keeps fresh, in place.** To show quota, the app reads
  an account's current access token from its folder (the `.credentials.json` file, or Claude
  Code's own Keychain item on macOS), uses it for one request to Anthropic's usage endpoint, and
  drops it. It is never written, cached, logged or refreshed. When a token has expired because
  nobody has used that account for a while, the app does not renew it; it shows the last reading
  instead (see [Idle accounts](#idle-accounts)).

What it does **not** do:

- It does not proxy, relay, or intercept model traffic. There is no server. Every inference
  request still goes straight from Claude Code to Anthropic, unmodified.
- It does not keep its own copy of any login, and never writes Claude Code's credentials.
- It sends no telemetry and reports no usage. The only other host it can contact is GitHub, to
  ask whether a newer release exists. That request carries nothing but the current version.
  It runs once about half a minute after launch and then daily, and **Settings → Check for
  updates automatically** turns it off — the manual check in About keeps working either way.
  Nothing installs without you asking.

### Is this allowed?

What Anthropic has said, in its own words:

- Holding more than one account is fine. In February 2026 Anthropic's Thariq Shihipar wrote that
  "it's not against terms of service to have multiple MAX accounts", and that enforcement targets
  token resale ([post](https://x.com/trq212/status/2024230184287949207)).
- Anthropic documents running accounts side by side: `CLAUDE_CONFIG_DIR` is "useful for running
  multiple accounts side by side" ([env vars](https://code.claude.com/docs/en/env-vars),
  [authentication](https://code.claude.com/docs/en/authentication)).
- The [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance) also says
  that "developers may not collect, store, or intermediate Claude.ai credentials or session
  tokens — sign-in to a Claude account must complete through Anthropic's own flow."

v0.4 is built around those three lines. Every sign-in completes in the unmodified
`claude auth login`. Each account lives in its own `CLAUDE_CONFIG_DIR` folder, the setup
Anthropic's docs describe. The app keeps no copy of any login and never refreshes one. Versions
before v0.4 kept an encrypted local copy of each account to switch between them; that is gone, and
upgrading removes those copies (see [Moving from v0.3](#moving-from-v03)).

This is a tool for people who legitimately hold separate subscriptions (work and personal, say)
and want to see where each one stands. It is not a way to get more usage than your plans allow.
Automatic switching is off until you turn it on. None of this is legal advice; read Anthropic's
terms yourself if you're deciding whether multi-account use fits your situation.

### The one request it makes with a token

In **February 2026** Anthropic updated its documentation to say that OAuth tokens from Free, Pro
and Max plans are for use with Claude Code and claude.ai only, and deployed server-side blocking
against third-party clients that route model requests through them. That enforcement targets tools
which *spend your subscription* — external harnesses that send prompts using your login.

This app sends no prompts. It does make one API call with an account's token: a read of
Anthropic's usage endpoint, to know how much quota the account has left. That is the entire network
surface, and it is the reason the app can tell you anything at all.

Two things follow, and both are deliberate:

- It identifies itself honestly. The `User-Agent` is `cc-logins/<version>` — never Claude Code's.
  The usage endpoint is more generous to the first-party client, and presenting as one would be
  circumventing the blocking above, with the consequences landing on your account rather than on
  this project. The app lives inside the limit it is given instead.
- It stays well under that limit. Requests are paced to roughly twenty an hour against a measured
  cap of about thirty, the budget is remembered across restarts so relaunching cannot spend it
  twice, and a rate-limit response backs the app off rather than retrying into it.

That's a description of what the app does, not a legal opinion — read Anthropic's terms yourself
if you're deciding whether multi-account use fits your situation. This project takes no position
beyond staying on the side of it described above.

## Install

Download from the [latest release](https://github.com/yashsoni369/cc-logins/releases/latest). These
links always point at the newest build:

| Platform | Download |
| --- | --- |
| Windows | [`CC-Logins-windows-x64-setup.exe`](https://github.com/yashsoni369/cc-logins/releases/latest/download/CC-Logins-windows-x64-setup.exe) |
| macOS (Apple Silicon and Intel) | [`CC-Logins-darwin-universal.dmg`](https://github.com/yashsoni369/cc-logins/releases/latest/download/CC-Logins-darwin-universal.dmg) — one universal build covers both |
| Linux | [`CC-Logins-linux-amd64.AppImage`](https://github.com/yashsoni369/cc-logins/releases/latest/download/CC-Logins-linux-amd64.AppImage) |
| Debian / Ubuntu | [`CC-Logins-linux-amd64.deb`](https://github.com/yashsoni369/cc-logins/releases/latest/download/CC-Logins-linux-amd64.deb) |

Windows installs per-user, so it never asks for administrator rights. On Linux the `.deb` pulls in
`libayatana-appindicator3-1` for the tray; the `.AppImage` needs `chmod +x` and nothing else.

Prefer to build it yourself? See [Building from source](#building-from-source).

### These builds are unsigned

They are **unsigned**, and you should know what that looks like before you download one:

- **Windows** shows "Windows protected your PC". Click **More info**, then **Run anyway**.
- **macOS** refuses to open it. Right-click the app → **Open**, or clear the quarantine flag:
  ```sh
  xattr -d com.apple.quarantine /Applications/CC\ Logins.app
  ```
  Prefer that over `xattr -cr`, which strips *every* extended attribute rather than just the
  download flag.

The reason is cost and eligibility, not evasion. Apple's certificate is an annual fee; Microsoft's
own [Azure Artifact Signing](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options)
is unavailable to individual developers outside the USA and Canada. The remaining route is the
[SignPath Foundation](https://signpath.org/), which signs open-source projects for free but requires
a project to have already shipped a release — so it became an option only with `0.1.0`.

Worth knowing either way: **signing would not make the warning disappear.** Since 2024 no
certificate grants immediate SmartScreen trust — [Microsoft removed that behaviour from EV
certificates](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation),
and reputation now accrues per file and per certificate through download volume. What signing buys
is that reputation carries across releases instead of resetting with every unsigned build.

### Verify what you downloaded

Every release ships `SHA256SUMS.txt`. With no signature on the binaries, this is the check that
tells you a download is the file the build actually produced:

```sh
# macOS / Linux — run in the directory you downloaded into
curl -LO https://github.com/yashsoni369/cc-logins/releases/latest/download/SHA256SUMS.txt
sha256sum --check --ignore-missing SHA256SUMS.txt      # shasum -a 256 -c on macOS
```

```powershell
# Windows — compare against the matching line in SHA256SUMS.txt
Get-FileHash .\CC-Logins-windows-x64-setup.exe -Algorithm SHA256
```

A checksum proves the file matches this release. It does not prove the release is trustworthy —
that rests on the source in this repository and on the
[build log](https://github.com/yashsoni369/cc-logins/actions/workflows/release.yml) for the tag,
which is public and shows exactly what produced each artifact. **You should verify what you're
running before trusting it with your tokens.** Building from source remains the strongest check.

## How it works

### Accounts and folders

Each account signs in once, through `claude auth login`, into its own folder under
`~/.cc-logins/profiles/`. The account signed in to your normal `~/.claude` needs no new sign-in.
Folders are named once and never renamed: on macOS Claude Code names its Keychain item after the
exact folder path, so a rename would lose the login.

Shared between every account, so each one feels like your usual Claude Code: `settings.json`,
`keybindings.json`, `CLAUDE.md`, and your `agents`, `commands`, `skills`, `output-styles`, `hooks`
and `ide` folders. Directories are linked (a directory junction on Windows); files are copied and
kept in step with `~/.claude`. If a file changes in both places, `~/.claude`'s version wins and the
account's copy is saved beside it.

Per account: session history (`projects/`), plugins, and the login itself. Claude Code breaks when
`projects/` or `plugins/` are linked, so they never are.

### The claude command

**Settings → Claude command → Install** copies a small launcher into `~/.cc-logins/bin/` and puts
that folder first on your `PATH` (the per-user `Path` on Windows, a marked block at the end of your
shell startup files elsewhere). From then on, a new terminal's `claude` starts Claude Code on the
account selected in the app. Uninstall removes both.

- Picking another account, by hand or by auto-switch, changes what the **next** `claude` starts
  with. A session that is already running keeps its account.
- Every account also gets a `claude-<name>` command that always starts that account, whatever is
  selected. Handy for running two accounts side by side.
- If you already set `CLAUDE_CONFIG_DIR` yourself (an alias, say), plain `claude` leaves it alone.
- VS Code's Claude Code panel starts its own `claude`. Settings shows a line to add to your VS
  Code settings, `"claudeCode.claudeProcessWrapper": "<path to the launcher>"`, so the panel
  follows the selection too. The app never edits your editor settings.

Without the command installed, plain `claude` keeps using your default `~/.claude` account, and
the Settings screen shows the full launcher paths to run instead.

### Idle accounts

An account is measured only while Claude Code holds a current token for it, which is while it is
in use. An account nobody has used for a few hours has an expired token that only Claude Code will
renew, so the app shows its last reading, marked `idle · 3h old`. That reading stays accurate:
an idle account spends no quota. A window whose reset time has passed since is shown at 0%, marked
`reset since`.

### What changed from v0.3

- **Switching applies to new sessions.** A session that hits its limit keeps its account: `/exit`
  and start `claude` again to continue on the selected account.
- **History is per account.** `claude --continue` only finds conversations from the same account.
- **WSL environments are display-only.**

### Moving from v0.3

On first start, the account signed in to `~/.claude` becomes the default account as soon as
`claude auth status` confirms it. Every other account shows **Move**: sign in to it once, the app
checks it is the same account, and only then deletes the old stored copy. A deletion that fails is
retried at the next start. Nothing is deleted for an account that has not been signed in again.

### Also

- **Local usage history and burn-rate charts.** Every reading is written to a local SQLite
  database, so you can see how fast an account is climbing toward its limit over days and weeks,
  not just its current percentage.
- **WSL-aware.** On Windows, Claude Code running inside a WSL distro keeps a separate login. The
  app shows it as a separate environment.

## Screenshots

Account names and organisations are masked in these — the app itself masks addresses on screen for
the same reason, since people screenshot it.

![The tray popover: the selected account's 5-hour and 7-day bars, every other account with its
utilisation and reset countdown, and another account one click
away.](docs/screenshots/tray.png)

The popover is the surface most of the time — one glance from the corner of the eye, one click to
move. The window is for when you want detail.

<details>
<summary><b>More screenshots</b> — accounts, per-account history, settings</summary>

### Accounts

Every account in one table: 5-hour and 7-day utilisation, when each window resets, and a **Use**
button on the ones that are not selected. The `E` badge marks an enterprise account, which is
limited by a monthly spend cap instead of rate-limit windows.

![The Accounts screen listing four logins with utilisation bars, reset countdowns and Use
buttons.](docs/screenshots/accounts.png)

Any row expands in place for the exact reset instants, the organisation it belongs to, and
per-model weekly windows.

![An expanded account row showing organisation, 5-hour and 7-day reset times, when it was last
measured, and per-model weekly windows.](docs/screenshots/accounts-expanded.png)

### Per-account history

Opening an account on the Dashboard plots its 5-hour and 7-day windows apart, rather than
collapsing both into one daily average — quota is spent in 5-hour windows, so an average hides
every spike.

![An account opened on the Dashboard, showing the 5-hour and 7-day windows charted separately and
a daily min/max range.](docs/screenshots/account-detail.png)

### Settings

Everything auto-switch does, adjustable — threshold, strategy, and how long a warning runs before
it fires. Auto-switch is off until you turn it on.

![The Settings screen with theme, time format, usage-check cadence, auto-switch threshold,
strategy and grace period.](docs/screenshots/settings.png)

</details>

## Safe defaults

- **Auto-switch is off by default.** The app will show you how close an account is to its limit,
  but it will not change the selected account until you turn auto-switch on yourself.
- **A 60-second grace period before an armed automatic switch fires.** When auto-switch is on and
  a threshold is crossed, the backend publishes the chosen target and exact deadline. The popover
  renders that authoritative countdown rather than trying to recreate the decision from quota
  percentages. **Hold 1h** is persisted, so it remains paused across popover closes and app
  restarts. An automatic switch only changes what new sessions start with; it never touches a
  session that is running.
- **A stopped WSL distro is never woken by a background poll.** Touching a WSL distro's files over
  the `\\wsl$` path silently boots its VM, even for something as innocuous as checking whether a
  file exists. This app only ever calls `wsl.exe`'s list commands from its polling loop, which are
  confirmed not to start anything; when a distro is asleep, its last-known numbers are shown with a
  staleness label instead, and waking it up to get a fresh reading is an explicit, user-initiated
  action.

## How often it checks usage

Anthropic's usage endpoint allows only about 30 reads per hour per account, so the polling cadence
is fixed rather than configurable: roughly every 5 minutes, tightening automatically as an account
approaches its limit and backing off after a rate limit. Idle accounts are not measured at all;
see [Idle accounts](#idle-accounts). A **Refresh** button on Home and in
the tray popover fetches on demand, itself rate-limited so it can't outpace the
background poller.

## Troubleshooting

### "Claude Code isn't installed" even though it is

When you launch the app from the Dock or Finder on macOS, it inherits a minimal `PATH`
(`/usr/bin:/bin:/usr/sbin:/sbin`) that does not include directories added by your shell's startup
files. The official Claude Code installer places the `claude` binary at `~/.local/bin/claude`, which
is only on `PATH` because your shell rc adds it — the app cannot see it when started graphically.

The app now falls back to the documented install locations when `PATH` comes up empty —
`~/.local/bin` (the native installer), `~/.claude/local` (the legacy local install), the common
npm/pnpm/bun/volta global bins, and `/opt/homebrew/bin` or `/usr/local/bin` for a Homebrew cask.

If your `claude` lives somewhere else entirely, set its full path under **Settings → Claude
binary** — the field validates the path as soon as you commit it and shows which binary the app
resolved. (`~` is expanded, so `~/.local/bin/claude` works as typed.)

The **`CC_LOGINS_CLAUDE_BIN`** environment variable does the same thing per launch and takes
precedence over the setting — useful for scripts and debugging. For an app launched from a
terminal, an ordinary `export CC_LOGINS_CLAUDE_BIN=...` is enough; for Dock/Finder launches the
variable is only visible via `launchctl setenv`, which lasts until logout — which is exactly why
the Settings field exists.

The same gap affects Linux, where a `.desktop` launch does not source your shell startup files
either; the fallback, the Settings field, and the environment variable work identically there.

## Where your data lives

| What | Location |
|---|---|
| Account folders (logins, history) | `~/.cc-logins/profiles/` — outside the app's own folder, so uninstalling the app never deletes them |
| The `claude` command and its settings | `~/.cc-logins/bin/`, `~/.cc-logins/shim.json` |
| Account list, settings, usage history | `%APPDATA%\cc-logins` (Windows) · `~/Library/Application Support/cc-logins` (macOS) · `~/.local/share/cc-logins` (Linux) |
| Log file | `app.log` in that same `cc-logins` folder |

Removing an account in the app leaves its folder, and the history in it, on disk.

Versions before v0.4 kept a stored copy of each login in `cc-logins/accounts` and switched by
rewriting Claude Code's credentials under a journal. v0.4 only finishes a switch such a version
left interrupted, then deletes each stored copy once its account has moved. See
[transaction recovery](docs/TRANSACTION_RECOVERY.md).

## Building from source

Prerequisites:

- [Rust](https://rustup.rs/), a recent stable toolchain (1.85 or newer)
- [Node.js](https://nodejs.org/), a recent LTS (Vite 7 needs 20.19+ or 22.12+)
- [pnpm](https://pnpm.io/) (this repo pins `pnpm@10.14.0` via `packageManager`)
- Platform build tools for Tauri:
  - **Windows**: [MSVC Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
  - **macOS**: Xcode Command Line Tools (`xcode-select --install`)
  - **Linux**: `webkit2gtk` and friends — see
    [Tauri's Linux prerequisites](https://v2.tauri.app/start/prerequisites/)

Then:

```sh
pnpm install
pnpm tauri dev      # run it locally
pnpm tauri build    # produce an installer for your platform
```

## Contributing

Bug reports and PRs are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md), and please open an issue
before starting anything large. Security issues go through a
[private advisory](https://github.com/yashsoni369/cc-logins/security/advisories/new), never a
public issue; see [SECURITY.md](SECURITY.md).

## Made by

Built and maintained by [Yash Soni](https://github.com/yashsoni369), founder
of [Apex36 Technologies](https://apex36tech.com/?utm_source=cc-logins). This is an independent
project and is not affiliated with, endorsed by, or supported by Anthropic.

## License

MIT — see [LICENSE](LICENSE). Portions are adapted from third-party code by Onur Cetinkol under
the MIT License; the required notice is included in [LICENSE](LICENSE).
