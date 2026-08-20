# Changelog

All notable changes to CC Logins are recorded here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

At `0.x` the public contract is the on-disk shape — the account vault layout, the
settings file, and the usage-history database schema — plus the supported OS matrix.
A breaking change to any of those bumps the minor version, since major is pinned at 0.

## [Unreleased]

Nothing yet.

## [0.2.2]

### Fixed

- Automatic update checks no longer slip by an extra day when a scheduled check runs slightly early or while offline.
- The updater now shows concise changelog highlights instead of the full GitHub release-page instructions.

## [0.2.1] - 2026-07-31

### Fixed

- A console window flashed open and shut on Windows roughly once a minute, for as long as
  the app was running. The tray icon asks the OS for the taskbar theme as it repaints, and
  refreshing that answer shells out to `reg query` — which, spawned from a windowed
  process, gets a console of its own unless told not to. Nothing was wrong with the
  reading; it just announced itself every time. The sign-in terminal is unaffected, since
  that one is meant to be seen.
- A user with a Claude subscription, already signed into Claude Code, opened the app and
  was told "No accounts yet". The heading is about this app's own vault, but it reads as a
  claim about the user — and the one-click button that would have fixed it sat underneath
  the full sign-in flow. First run now detects an existing login and promotes "Add my
  current login" to the first action.

## [0.2.0] - 2026-07-29

### Added

- A Dashboard home screen, and the app now opens on it. It answers the question no
  per-account view can: where the fleet stands as a whole. A pooled runway estimate says
  how long every account together lasts at the current burn rate, one row per account
  carries a sparkline and its 7-day figure, and a reset stagger plots every account's
  5-hour window on one clock — a vertical slice with no headroom in it is the stretch that
  strands you, and nothing else surfaces it.
- Per-account detail opens **in place** beneath its own row, so the rest of the fleet stays
  on screen to compare against: the 5-hour and 7-day windows plotted apart, per-model
  weekly limits, and a heatmap of when that account actually runs hot. One range control
  scopes every panel on the screen.
- A load-balance grid showing each account's daily peak, which answers whether the rotation
  is spreading work or quietly draining one account — a question no per-account chart can.
- A findings section that says in words what the charts imply: which account is carrying
  the fleet, how often one reached its limit, whether the resets cluster.
- A clock-format setting. Times were following the OS in some views and not others;
  12-hour, 24-hour and system are now one choice that every view obeys.
- Support for enterprise accounts, which are limited by a monthly spend cap instead of
  5-hour and 7-day windows. These report no rate-limit windows at all, so until now they
  showed no usage anywhere and could never be switched away from however much of the cap
  was gone. The cap now counts as a real limit — auto-switch, exhaustion and recovery all
  see it — and appears on every screen that shows usage, with an `[E]` badge marking the
  account. It stays out of the pooled runway, which projects hours and would be swamped by
  a monthly figure.
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
  clipped below the axis. Charts no longer stretch their own type when the window widens,
  and every screen now uses the whole window rather than a fixed column.

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

[Unreleased]: https://github.com/yashsoni369/cc-logins/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/yashsoni369/cc-logins/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/yashsoni369/cc-logins/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/yashsoni369/cc-logins/releases/tag/v0.1.0
