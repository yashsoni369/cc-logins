# Changelog

All notable changes to CC Logins are recorded here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

At `0.x` the public contract is the on-disk shape — the account vault layout, the
settings file, and the usage-history database schema — plus the supported OS matrix.
A breaking change to any of those bumps the minor version, since major is pinned at 0.

## [Unreleased]

### Added

- In-app updates. Settings → About has a **Check for updates** control that fetches the
  latest release, verifies it against a signing key compiled into this build, installs it
  and restarts. The check runs only when you press the button — nothing contacts the
  network on a timer or at startup.

  Because an update arrives through the app rather than a browser download, it carries no
  mark of the web and never reaches Windows SmartScreen. The first install still warns;
  every update after it does not.

  `0.1.0` has no updater, so upgrading from it has to be done by hand once.

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
