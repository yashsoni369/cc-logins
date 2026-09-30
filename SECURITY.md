# Security policy

CC Logins reads Claude Code's current access token for each account to measure
quota. It never stores, copies, refreshes or writes one, but a bug in how it
reads them could still expose a token, so security reports are taken seriously
and handled privately.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.** A public report
against a credential-handling app is a working exploit against everyone running
it, published before there is a fix.

Report it through GitHub's private reporting instead:

- **[Open a private security advisory](https://github.com/yashsoni369/cc-logins/security/advisories/new)**

Include what you would put in any bug report — OS, app version, steps — plus
what you believe the impact is. A proof of concept helps, but a clear
description of the mechanism is worth more than a script.

This is a single-maintainer, pre-1.0 project. There is no guaranteed response
time and it would be dishonest to publish one. Reports are read and answered as
soon as they are seen, and you will get an acknowledgement before any fix.

**Never include a real token, credential file, or the contents of
`.credentials.json` in a report.** Redact them. If a bug can only be shown with
real credential material, say so and we will work out how to reproduce it
without sending any.

## Supported versions

Only the latest release. At `0.x` there are no maintenance branches, and
backporting a fix to a version nobody is running would be theatre.

## What this app does with your tokens

Stated here as well as in the README, because it defines what is and is not a
vulnerability in this project (v0.4 and later).

What it reads:

- For each account, the current access token from that account's own Claude
  Code folder: `<folder>/.credentials.json` on Windows and Linux, or on macOS
  Claude Code's Keychain item for that folder (service
  `Claude Code-credentials-<first 8 hex of sha256(folder path)>`, or the plain
  `Claude Code-credentials` for the default `~/.claude`). Read-only.
- The token is used for **one request** to Anthropic's usage endpoint and then
  dropped. It is never logged, cached, written, or kept in memory past that
  request. An expired token is left alone; the app never refreshes one.
- Each folder's `.claude.json` `oauthAccount` block, to know which account is
  signed in there, and the output of `claude auth status`. Neither contains a
  token.

What it writes:

- Its own account list, settings, usage cache and history, in its app-data
  directory.
- `~/.cc-logins/shim.json` (which folder new sessions use) and the folders it
  creates under `~/.cc-logins/profiles/`, seeded with your MCP servers and
  preferences from `~/.claude.json` and linked or copied shared settings. The
  login inside a folder is written by `claude auth login`, never by this app.
- Only when you install the `claude` command: copies of its launcher in
  `~/.cc-logins/bin/`, and either your per-user `Path` registry value (Windows)
  or a marked block in your shell startup files (macOS, Linux; a backup is kept
  the first time). Uninstall removes them.

It never writes Claude Code's credentials. The one exception is inherited from
versions before v0.4: if such a version left an account switch interrupted,
v0.4 finishes or rolls back that one journaled switch before doing anything
else. See [transaction recovery](docs/TRANSACTION_RECOVERY.md).

Versions before v0.4 kept an encrypted copy of each login (DPAPI on Windows,
Keychain on macOS, an unencrypted `0600` file on Linux). v0.4 deletes each copy
once its account has signed in to its own folder and been verified as the same
account.

Nothing is sent anywhere except Anthropic's usage endpoint (and GitHub, to check
for a newer release). There is no server, no telemetry, and no cloud sync. Any
observed network traffic to a third party **is** a vulnerability, and an urgent
one.

## Builds are unsigned

Releases are not code-signed, so Windows SmartScreen and macOS Gatekeeper will
warn. That is expected and documented in the README — it is not a
vulnerability report, but it does mean **you should verify what you are running
before trusting it with your tokens**. Building from source is the strongest
check available today.
