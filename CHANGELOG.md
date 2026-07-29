# Changelog

All notable changes to CC Logins are recorded here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

At `0.x` the public contract is the on-disk shape — the account vault layout, the
settings file, and the usage-history database schema — plus the supported OS matrix.
A breaking change to any of those bumps the minor version, since major is pinned at 0.

## [Unreleased]

Nothing yet.

## [0.2.0] - 2026-07-29

### Added

- A Dashboard home screen, and the app now opens on it. It answers the question no
  per-account view can: where the fleet stands as a whole. A pooled runway estimate says
  how long every account together lasts at the current burn rate, one row per account
  carries a 24-hour sparkline and its 7-day figure, and a reset stagger plots every
  account's 5-hour window on one clock — a vertical slice with no headroom in it is the
  stretch that strands you, and nothing else surfaces it.
- Per-account analytics behind any row: the 5-hour and 7-day windows plotted apart, a
  weekly heatmap of when that account actually runs hot, and a 30-day range.
- A clock-format setting. Times were following the OS in some views and not others;
  12-hour, 24-hour and system are now one choice that every view obeys.

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

### Changed

- Installers carry the app's own identity. The Windows installer has a branded header and
  welcome panel and its icon on both the installer and the uninstaller; the macOS disk
  image opens on a laid-out window rather than a bare volume. The Windows installer still
  installs per-user, so it never asks for administrator rights.
- The rotation control on each account row is a labelled Enable/Disable button rather than
  a bare switch. It sits beside Switch and Re-login, and a lone toggle among buttons read
  as an odd one out.
- This project is presented on its own terms. The upstream tool it was ported from is no
  longer named in the README, the settings copy or log messages, and the app no longer
  reads or writes that tool's store. Attribution is unchanged: the MIT notice and its
  copyright line are intact in LICENSE.

### Fixed

- The dashboard could not be read. Measured against the running app rather than judged by
  eye: the reset stagger drew its replenished stretch at 1.19:1 against the background
  where meaningful graphics need 3:1, and no opacity reached it — so a band now carries a
  neutral outline for its extent and fills only the consumed part. The "no samples" cells
  in the heatmap sat at 1.22:1, making the one distinction that grid exists to draw
  invisible, and every in-chart label was below the 4.5:1 text minimum.
- Heatmap rows read `Night`/`Morning`/`Afternoon`/`Evening` instead of `Nig`/`Mor`/`Aft`/`Eve`.
- Sample data ages with the clock. Every instant in the demo fixture was hardcoded, so
  once those dates passed a first-run user saw every countdown collapsed to "now" and every
  account drawn as fully replenished.

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

[Unreleased]: https://github.com/yashsoni369/cc-logins/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/yashsoni369/cc-logins/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/yashsoni369/cc-logins/releases/tag/v0.1.0
