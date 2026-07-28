# Changelog

All notable changes to CC Logins are recorded here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

At `0.x` the public contract is the on-disk shape — the account vault layout, the
settings file, and the usage-history database schema — plus the supported OS matrix.
A breaking change to any of those bumps the minor version, since major is pinned at 0.

## [Unreleased]

Nothing released yet. `0.1.0` below describes the state of the first tagged build
when it happens.

## [0.1.0] - Unreleased

### Added

- Tray application showing 5-hour and 7-day quota per Claude Code account, with a
  live tray icon rasterised at runtime rather than a static image.
- Manual and automatic switching between accounts. Auto-switch is off by default and
  gives a 60-second cancellable countdown before it fires.
- Local usage history in SQLite, with burn-rate charts over days and weeks.
- Interactive sign-in that runs `claude auth login` in an isolated
  `CLAUDE_CONFIG_DIR`, so the live login is never disturbed.
- WSL awareness on Windows: native and per-distro logins are detected as separate
  environments, and a stopped distro is never woken by a background poll.
- Refresh control on the Accounts screen and in the tray popover, rate-limited in the
  backend so it cannot outpace the poller.
- Day / night / system theme.
- Cross-process file locking compatible with the `cswap` CLI, verified by Rust/Rust,
  pinned Python-protocol/Rust, and optional installed-`claude_swap` interoperability tests.
- Recoverable account switching with Claude Code-compatible credential/config locks,
  protected before-images, a secret-free durable journal, exact rollback, startup
  recovery, and an explicit `recoveryRequired` UI state.
- Switch-time credential provenance checks matching current cswap: live bytes proven
  to belong to a different or recycled account are preserved in the unclaimed safety
  store and never written over the configured outgoing slot; an unavailable identity
  oracle retains cswap's fail-open rotation behavior.
- Active usage attribution matching current cswap: a credential accepted by the usage
  endpoint is not assigned to the configured slot when the profile oracle proves it
  belongs to another account; definitive lineage verdicts are memoized, while partial
  or unavailable oracle results remain retryable.
- Active OAuth recovery matching current cswap: expired or server-rejected active
  generations refresh under Claude-compatible credential locks, recheck account and
  lineage ownership before consuming a grant, and persist the successor to both the live
  credential and its slot backup. Exact `invalid_grant` results remain generation-bound.

### Security

- Stored credentials carry a self-describing envelope recording the protection
  actually applied: DPAPI on Windows, Keychain on macOS, and an unencrypted `0600`
  file on Linux. Linux is a known gap and the envelope says so rather than implying
  otherwise.
- Builds are unsigned. See [SECURITY.md](SECURITY.md).

[Unreleased]: https://github.com/yashsoni369/cc-logins/compare/main...HEAD
