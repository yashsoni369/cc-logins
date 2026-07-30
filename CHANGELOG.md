# Changelog

All notable changes to CC Logins are recorded here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

At `0.x` the public contract is the on-disk shape — the account vault layout, the
settings file, and the usage-history database schema — plus the supported OS matrix.
A breaking change to any of those bumps the minor version, since major is pinned at 0.

## [Unreleased]

### Added

- Support for enterprise accounts, which are limited by a monthly spend cap instead of
  5-hour and 7-day windows. These report no rate-limit windows at all, so until now they
  showed no usage anywhere and could never be switched away from however much of the cap
  was gone. The cap now counts as a real limit — auto-switch, exhaustion and recovery all
  see it — and appears on every screen that shows usage, with an `[E]` badge marking the
  account. It stays out of the pooled runway, which projects hours and would be swamped by
  a monthly figure.

- A reworked dashboard. Accounts open in place rather than replacing the screen, one range
  control scopes every panel, and a findings section says in words what the charts imply.
  Pooled headroom is drawn against the fleet's real capacity, so the bar's length finally
  means something.

- Last-known usage survives a failed fetch. Readings are cached, so opening the app during
  a cold start, a network blip or a rate-limit backoff shows what was true a moment ago
  with its age, instead of a screen of blanks. How long a reading stays trusted depends on
  why the fetch failed: a rate-limit response says nothing about your quota, so the figures
  hold until the window they describe resets; any other failure gets an hour. Past that the
  reading is dropped rather than shown, because a stale number that looks live is worse
  than an honest gap.

- In-app updates. The app checks GitHub for a newer release shortly after launch and then
  once a day, notifies you once per version, and shows a dot on Settings while one is
  waiting. Settings → About installs it: the download is verified against a signing key
  compiled into this build, then the app restarts.

  Automatic checking is a setting and can be turned off, leaving the manual check working.
  Installing is refused while a switch is running, is about to run, or while credential
  recovery is outstanding — an update restarts the app, and that must not interrupt a
  credential rotation.

  Because an update arrives through the app rather than a browser download, it carries no
  mark of the web and never reaches Windows SmartScreen. The first install still warns;
  every update after it does not.

  `0.1.0` has no updater, so upgrading from it has to be done by hand once.

### Fixed

- Usage readings for an account with no rate-limit windows were recorded as a measured
  zero. Every reading of an enterprise account was stored as "idle" and drawn on the charts
  as a flat, fictional line. Unknown is now a gap in the record rather than a zero in it.

- The rate-limit backoff reacted to the wrong events. It fired whenever any account failed
  to read this tick — a dropped connection counted — and cleared one tick after a genuine
  refusal. It now reacts to real rate-limit responses over the full window, and honours the
  server's own retry hint with a margin rather than retrying at the exact instant it names.

- Relaunching no longer assumes a rested token. The endpoint budgets requests over a
  trailing hour that capacity ages out of, and every launch used to start as though none
  had been spent, so a machine that restarted often spent the hour on launches alone.

- The pooled-capacity bar reported "0%" when no usage could be read at all, which asserts
  no capacity left where the truth is that nothing is known yet.

- Long account names wrapped a row to five lines, and end labels on the fleet chart were
  clipped below the axis. Charts no longer stretch their own type when the window widens.

## [0.1.0] - 2026-07-29

### Added

- Tray application showing 5-hour and 7-day quota per Claude Code account, with a
  live tray icon rasterised at runtime rather than a static image.
- Manual and automatic switching between accounts. Auto-switch is off by default and
  gives a 60-second cancellable countdown before it fires.
- Local usage history in SQLite, with burn-rate charts over days and weeks.
- Interactive sign-in that runs `claude auth login` in an isolated
  `CLAUDE_CONFIG_DIR`, so the live login is never disturbed.
- In-place re-login for rejected OAuth accounts. The selected slot, alias, and
  account metadata are preserved, and the replacement is accepted only when
  the isolated login resolves to the same account identity.
- WSL awareness on Windows: native and per-distro logins are detected as separate
  environments, and a stopped distro is never woken by a background poll.
- Refresh control on the Accounts screen and in the tray popover, rate-limited in the
  backend so it cannot outpace the poller.
- Day / night / system theme.
- Cross-process file locking for account, credential, and configuration stores, verified by
  Rust/Rust and protocol-level interoperability tests.
- Recoverable account switching with Claude Code-compatible credential/config locks,
  protected before-images, a secret-free durable journal, exact rollback, startup
  recovery, an explicit `recoveryRequired` UI state, and backend-enforced blocking
  of every credential or account-registry mutation until recovery succeeds.
- Switch-time credential provenance checks: live bytes proven
  to belong to a different or recycled account are preserved in the unclaimed safety
  store and never written over the configured outgoing slot; an unavailable identity
  oracle retains fail-open rotation behavior.
- Active usage attribution: a credential accepted by the usage
  endpoint is not assigned to the configured slot when the profile oracle proves it
  belongs to another account; definitive lineage verdicts are memoized, while partial
  or unavailable oracle results remain retryable.
- Active OAuth recovery: expired or server-rejected active
  generations refresh under Claude-compatible credential locks, recheck account and
  lineage ownership before consuming a grant, and persist the successor to both the live
  credential and its slot backup. Exact `invalid_grant` results remain generation-bound.
- Active recovery now restores a definitively wiped live OAuth store from the slot backup, keeps
  read failures distinct from genuine absence, and shares the inactive path's per-account lease so
  a concurrent snapshot cannot consume the same rotating grant twice.

### Security

- Stored credentials carry a self-describing envelope recording the protection
  actually applied: DPAPI on Windows, Keychain on macOS, and an unencrypted `0600`
  file on Linux. Linux is a known gap and the envelope says so rather than implying
  otherwise.
- Builds are unsigned. See [SECURITY.md](SECURITY.md).

[Unreleased]: https://github.com/yashsoni369/cc-logins/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/yashsoni369/cc-logins/releases/tag/v0.1.0
