# Release Blockers Design

**Date:** 2026-07-28

**Status:** Proposed for implementation planning

**Repository baseline:** `de35cfdcc40159ac9ba928cd4debfb84df1876b4`

## Objective

Make the existing automatic account switching feature safe and truthful enough
to ship by fixing the four confirmed release blockers:

1. Settings changes do not reach the running poller.
2. The popover invents auto-switch state instead of rendering daemon state.
3. Inactive OAuth refresh-token rotations are not persisted, and dead token
   generations remain eligible for automatic selection.
4. Switching is a multi-file, multi-backend mutation without Claude Code lock
   cooperation or crash recovery.

This design treats these as one safety boundary. A correct UI is not useful if
the daemon uses stale policy; a correct decision is not useful if its target
credential is stale; and a valid target is not useful if activation can leave
the machine in a mixed-account state.

## Scope

### Included

- Live, interruptible daemon policy updates.
- Revisioned partial settings updates to prevent lost writes.
- A persisted one-hour global auto-switch pause and `Resume now` action.
- An authoritative daemon status API and event contract.
- Exact grace deadlines owned by the backend.
- Persisted inactive-account OAuth refresh rotations.
- Exact OAuth error classification and a `Re-login required` state.
- Fail-closed automatic target eligibility.
- Target freshening and validation immediately before activation.
- Claude Code-compatible credential and config locks.
- A durable switch journal, idempotent startup recovery, rollback, and
  post-commit verification.
- Deterministic backend and frontend tests for these paths.
- Correction of resilience documentation that currently overstates atomicity.

### Explicitly deferred

- WSL account mutation.
- Account removal.
- Start-at-login behavior.
- OS notification delivery and the popover's reset notification feature.
- Broader history retention and performance changes.
- General UI polish unrelated to truthful auto-switch state.
- CI pinning and other supply-chain improvements.

Deferred controls that currently imply working behavior must be hidden or
clearly marked unavailable in any blocker-touched surface. They must not remain
interactive client-only simulations.

## Considered architectures

### A. Minimal patch

Re-read settings every poll, subscribe the popover to existing decision events,
persist refreshed credentials with the current callback, and add in-memory
reverse rollback.

This has the smallest diff, but it leaves event hydration gaps, lost settings
updates, tick-delayed grace periods, refresh races, and no recovery after a
process crash or power loss. It is rejected because it reduces symptoms without
establishing a coherent safety model.

### B. Canonical runtime state plus durable transactions — selected

Keep the existing modules, but add explicit control planes:

- a Tokio `watch` channel for the latest complete runtime policy;
- a revisioned backend settings store;
- a revisioned authoritative daemon-status snapshot plus update event;
- a generation-aware OAuth refresh coordinator;
- one switch transaction coordinator with Claude-compatible locks and a durable
  journal.

This is a material refactor, but its boundaries align with the current code and
can be delivered in testable slices without rewriting the entire application.

### C. Full poller/switcher actor and generation-addressed vault

Make one actor own settings, polling, OAuth refresh, and switching, and redesign
the GUI vault around immutable generations plus one manifest pointer.

This gives excellent total ordering for GUI-owned state, but it is substantially
larger and still cannot atomically redirect Claude Code's fixed config,
credential, and Keychain locations. It is rejected for the blocker phase and
may be reconsidered as a later architectural project.

## Runtime policy and settings

### Canonical state

The backend owns the canonical persisted settings. Startup creates a
`tokio::sync::watch` channel containing a complete `RuntimePolicy`; the sender
is managed application state and the poller owns a receiver.

`RuntimePolicy` contains only values needed by the running daemon:

```rust
struct RuntimePolicy {
    revision: u64,
    threshold: f64,
    cooldown: Duration,
    hysteresis_pct: f64,
    strategy: Strategy,
    unhealthy_ticks: u32,
    grace: Duration,
    auto_switch_enabled: bool,
    paused_until: Option<DateTime<Utc>>,
}
```

The persisted settings schema gains `autoSwitchPausedUntil`. An expired pause is
treated as absent and is cleared on the next successful settings write. The
pause affects automatic switching only: usage polling, history recording, tray
updates, and manual switching continue.

### Settings writes

Whole-object replacement is replaced by an explicit patch command:

```ts
type SettingsSnapshot = {
  revision: number;
  settings: Settings;
};

updateSettings(input: {
  expectedRevision: number;
  patch: Partial<Settings>;
}): Promise<SettingsSnapshot>;
```

The backend merges only supplied fields into its current canonical value,
sanitizes the result, writes it atomically, increments the revision, updates the
watch channel, and emits `settings://updated`. A stale expected revision returns
a typed conflict and cannot overwrite newer fields.

`snoozeAutoSwitch({ durationSeconds: 3600 })` and `resumeAutoSwitch()` are
specialized commands over the same canonical mutation path.

### Policy-change semantics

Every policy update wakes the daemon immediately.

- Disabling auto-switch or starting a pause cancels any pending countdown.
- Changing decision inputs cancels the pending decision and its elapsed grace.
- Enabling auto-switch, resuming, or changing decision inputs requires a fresh
  successful usage snapshot before a new warning or switch.
- The daemon never acts on a decision produced under an older policy revision.
- Rapid policy updates may coalesce, but the daemon always observes one complete
  policy value, never fields from mixed revisions.

The polling loop uses `tokio::select!` over policy changes and its next exact
deadline. A grace deadline wakes the loop at that deadline rather than waiting
for the next adaptive polling interval.

## Authoritative daemon status

The daemon owns a revisioned status snapshot:

```ts
type DaemonPhase =
  | { kind: "disabled" }
  | { kind: "paused"; until: string }
  | { kind: "monitoring" }
  | { kind: "cooldown"; until: string }
  | { kind: "warning"; from: number; to: number; deadline: string }
  | { kind: "switching"; from: number; to: number }
  | { kind: "exhausted"; earliestReset: string | null }
  | { kind: "degraded"; reason: "usageUnknown" | "fetchFailed" }
  | { kind: "recoveryRequired"; detail: string };

type DaemonStatus = {
  revision: number;
  policyRevision: number;
  phase: DaemonPhase;
  updatedAt: string;
};
```

The frontend subscribes to `daemon://status`, then calls `getDaemonStatus`, and
retains whichever payload has the greatest revision. This closes the race where
an event arrives between initial fetch and listener registration.

The popover removes its local policy engine. It does not infer warning,
exhaustion, target, grace, enabled state, or pause from quota percentages. It
renders the backend phase and derives the visible countdown from the backend's
absolute deadline. `preview_target` is no longer used for automatic-switch UI.

Quota severity colors remain general visual indicators. The configured action
threshold controls daemon decisions; the popover communicates an impending
switch only when the daemon reports `warning`.

The client-only `Notify me at reset` behavior is removed from the blocker-phase
popover. Durable notifications remain a separate feature.

## OAuth refresh safety

### Error model

OAuth error responses are parsed structurally. Only an exact JSON OAuth error
of `invalid_grant`, observed against the still-current stored credential
generation, proves that the account requires re-login.

- `invalid_grant` becomes `UsageStatus::ReloginRequired`.
- `invalid_client`, unrelated 4xx responses, network failures, timeouts, 5xx
  responses, lock timeouts, and persistence failures remain retryable or
  degraded states.
- Local access-token expiry means refresh is required; it does not itself mean
  the account is dead.
- Error classification never depends on an arbitrary response-body substring.

Quarantine is keyed by stable account identity and rejected credential
fingerprint. Re-adding the account with a new generation clears the quarantine.

### Refresh coordination and persistence

Inactive-account refresh runs through a per-account cross-process refresh
coordinator. The flow is:

1. Acquire the account refresh lease.
2. Briefly acquire the GUI vault lock and re-read the current credential.
3. Release the vault lock before network I/O.
4. Refresh with a bounded request.
5. Reacquire the vault lock and compare the stored source generation.
6. Persist the successor only if the source still matches, or accept the
   already-persisted identical successor.
7. Release locks before the usage request when possible.

If the stored generation changed while the request was in flight, the stale
callback cannot overwrite it. The operation reuses or retries from the winning
generation. A refresh is successful only after its successor is durably
persisted; otherwise usage is not published as healthy.

### Selection and activation

Automatic selection requires fresh, decision-trusted usage and positive
headroom. `ReloginRequired`, stale, unknown, and transiently unavailable
accounts are excluded from every automatic strategy.

Manual selection may name a stale or unknown account, but the switch boundary
freshens and validates the latest generation before any live credential write.
A confirmed `ReloginRequired` account cannot be activated until it is re-added.

Immediately before the switch transaction acquires the full live-state lock
set, the target is freshened. After locks are acquired, the target generation is
re-read and compared. Any mismatch aborts and restarts validation rather than
installing the snapshot's stale bytes.

## Transactional switching

### Lock protocol

One coordinator acquires every multi-resource lock in one fixed order:

1. Upstream `cswap` compatibility file lock when that store exists.
2. Claude Code primary OAuth refresh directory lock.
3. Claude Code legacy credential directory lock.
4. Claude Code global-config directory lock.
5. GUI vault lock.

The Claude locks mirror the current `proper-lockfile` protocol, including
staleness, periodic touches, bounded waiting, cleanup, and unwind behavior. The
exact paths and timings are compatibility constants covered by cross-process
tests and documented as a tracked upstream dependency.

No network I/O occurs while this full lock set is held. All callers that need
multiple locks use the coordinator so lock ordering cannot drift.

### Journal structure

The durable transaction consists of:

- a secret-free metadata journal containing transaction ID, target identity,
  artifact references/hashes, phase, and intended commit;
- before-image artifacts stored with the same protection backend as credential
  vault material;
- sibling staged outputs for regular files;
- explicit Keychain presence/value snapshots where applicable.

On Windows, credential before-images use DPAPI. On macOS, secret material uses
Keychain entries when available. On platforms where the existing vault offers
only restrictive file permissions rather than encryption, the recovery store
must report that limitation honestly and use mode `0600`; the metadata journal
still contains no credential plaintext.

Journal and before-image writes are synced before the first live mutation.
Regular-file commits use same-directory replacement. POSIX directory entries
are synced after rename. Windows replacement uses the appropriate native
replacement operation and preserves recoverable before-images. These provide
durability building blocks, not an assertion of cross-file atomicity.

### Switch protocol

1. Perform network target validation before live-state locks.
2. Acquire the complete lock set.
3. Recover or refuse any existing incomplete transaction.
4. Re-read and validate current and target state under lock.
5. Snapshot exact live state, including absence and credential backend.
6. Write and sync the `Prepared` journal and protected before-images.
7. Stage and sync every regular-file output.
8. Capture the outgoing credential and config as one coherent backup
   generation.
9. Install the target active credential.
10. Splice the target identity into the latest live global config.
11. Update the GUI registry/sequence last as the application commit indicator.
12. Mark and sync `Committed`.
13. Verify credential, config, and registry identify the same target.
14. Clean up journal artifacts only after successful verification.
15. Release locks and publish success/status events.

All target artifacts are validated before the first mutation. Unrelated global
config fields are preserved by a locked read-modify-write.

### Failure and recovery semantics

For an ordinary error, rollback begins immediately and continues through every
restore step even if one restore fails. On startup and at every switch entry,
the same recovery routine runs under the full lock set.

- A non-committed journal restores the live pre-switch state.
- A committed journal verifies the target state and completes cleanup.
- Recovery is idempotent.
- A journal is retained until verification succeeds.
- If recovery cannot prove coherence, automatic and manual switching are
  blocked and daemon status becomes `recoveryRequired` with a safe diagnostic.
- The application never treats `sequence.json` alone as proof of the active
  account.

The outgoing account's newest captured credential/config backup is monotonic
preservation and survives a failed switch. Rollback does not restore an older
refresh token that may already be consumed. The journal records this exception
to byte-for-byte reversal and verifies the outgoing backup as one generation.

The durability target is coherent recovery on supported local filesystems
(NTFS, APFS, ext4 and equivalent local filesystems). Network/removable
filesystem guarantees are not strengthened beyond what the host filesystem
provides; unsupported or unsafe replacement behavior fails closed.

## Testing strategy

Implementation follows test-driven slices. Network, clock, filesystem,
credential backend, and lock acquisition receive injectable boundaries.

### Runtime and UI

- Settings patches modify only named fields and reject stale revisions.
- Rapid policy updates yield the newest complete policy.
- Disable/pause during warning cancels the switch immediately.
- Enable/resume/policy changes require a fresh snapshot.
- Grace zero switches without a phantom countdown.
- Exact grace deadlines wake independently of the poll interval.
- Popover phases, targets, deadlines, pause, and exhaustion come only from
  daemon status.
- Subscribe-then-hydrate revision ordering cannot regress UI state.
- Listener cleanup and cross-window settings updates are covered.

### OAuth

- Expired inactive credentials persist rotated access and refresh tokens.
- A usage 401 refreshes once, persists, then retries usage.
- Persistence failure cannot return healthy usage.
- Exact `invalid_grant` on the current generation marks re-login required.
- `invalid_client`, ambiguous 400s, network errors, and 5xx stay retryable.
- Concurrent refreshes consume one generation.
- A stale callback cannot overwrite a moved, re-added, or newer slot.
- Automatic strategies never select non-fresh or quarantined accounts.
- Switch-boundary validation aborts before live mutation on a dead target.

### Transactions and locks

- Failure injection covers every journal, stage, live write, phase update,
  rollback, verification, and cleanup boundary.
- Returned errors restore coherent pre-switch live state.
- A failed rollback retains the journal and blocks later switches.
- Child-process termination at every checkpoint is recovered on restart.
- OAuth/API-key combinations and file/Keychain backends are covered.
- File absence is restored as absence, not an empty file.
- Cross-process Rust/Rust and Python-upstream/Rust lock interoperability is
  tested.
- Simulated Claude refresh and config writers cannot interleave with switching.
- Windows replacement failure caused by an open destination fails coherently.

### Verification gates

Each implementation slice must pass its focused tests before integration. The
final blocker gate includes:

- frontend production build;
- Rust formatting and Clippy with warnings denied;
- all Rust tests on all features;
- frontend component tests;
- cross-process lock interoperability tests;
- packaged smoke tests on Windows, macOS, and Linux for enable/disable/pause,
  warning cancellation, switching, and restart recovery.

## Delivery sequence

1. Establish revisioned settings/runtime policy and daemon status contracts.
2. Make the poller interruptible and the popover a pure daemon-state renderer.
3. Add persisted pause semantics and exact grace deadlines.
4. Introduce OAuth transport/persistence seams and exact error classification.
5. Add generation-safe inactive refresh and fail-closed eligibility.
6. Add switch-boundary target validation.
7. Implement Claude-compatible locks and interoperability tests.
8. Implement journal primitives and protected before-image storage.
9. Move `switch_to` behind the transaction coordinator and add recovery.
10. Run fault-injection, crash-restart, cross-platform, and packaged gates.
11. Correct resilience and architecture documentation to match verified
    guarantees.

The detailed implementation plan will split these into small TDD tasks with
exact file paths, test names, verification commands, and commit checkpoints.

## Sources

- [Tauri commands](https://v2.tauri.app/develop/calling-rust/)
- [Tauri frontend events](https://v2.tauri.app/develop/calling-frontend/)
- [Tauri state management](https://v2.tauri.app/develop/state-management/)
- [Tokio watch receiver](https://docs.rs/tokio/latest/tokio/sync/watch/struct.Receiver.html)
- [Tokio select](https://docs.rs/tokio/latest/tokio/macro.select.html)
- [OAuth 2.0 RFC 6749](https://www.rfc-editor.org/rfc/rfc6749.html)
- [OAuth 2.0 Security Best Current Practice RFC 9700](https://www.rfc-editor.org/rfc/rfc9700.html#section-4.14)
- [Current upstream claude-swap](https://github.com/realiti4/claude-swap/tree/3be1cb16095a3769926e6255c4cfcc39f501f08b)
- [Upstream Claude lock implementation](https://github.com/realiti4/claude-swap/blob/3be1cb16095a3769926e6255c4cfcc39f501f08b/src/claude_swap/claude_locks.py)
- [Rust `std::fs::rename`](https://doc.rust-lang.org/std/fs/fn.rename.html)
- [POSIX rename](https://pubs.opengroup.org/onlinepubs/9799919799/functions/rename.html)
- [POSIX fsync](https://pubs.opengroup.org/onlinepubs/9799919799/functions/fsync.html)
- [Windows `MoveFileExW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)
- [Windows `ReplaceFileW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)
