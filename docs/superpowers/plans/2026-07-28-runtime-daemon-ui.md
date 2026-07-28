# Runtime Settings and Truthful Daemon UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make settings live, persist the global one-hour pause, enforce exact backend grace deadlines, and make renderers display authoritative daemon state.

**Architecture:** A revisioned `SettingsStore` publishes complete `RuntimePolicy` values through Tokio `watch`. A revisioned `DaemonStatusStore` is hydrated by command and updated by event; the popover becomes a pure status renderer.

**Tech Stack:** Rust 1.81, Tauri 2, Tokio 1.53.1, chrono, React 19, TypeScript 5.8, Vitest 4.1.10, jsdom 30.0.0, Testing Library.

## Global Constraints

- `Hold 1h` persists `autoSwitchPausedUntil`; polling/history continue.
- Every decision-input change cancels grace and requires a fresh successful snapshot.
- Grace is an absolute backend deadline, independent of polling cadence.
- Subscribe before hydration and retain the highest revision.
- Only daemon status may claim warning, switching, cooldown, exhaustion, or recovery.
- Hide deferred autostart and notification controls.
- Do not change OAuth refresh or switch transaction behavior in this plan.

---

### Task 1: Persisted pause and runtime policy

**Files:**
- Create: `src-tauri/src/runtime.rs`
- Modify: `src-tauri/src/settings.rs:80-151`
- Modify: `src-tauri/src/lib.rs:188-207`
- Modify: `src-tauri/src/poller.rs:300-347`
- Test: inline `settings::tests`, `runtime::tests`

**Interfaces:**
- Consumes: `settings::Settings`, `switcher::Strategy`.
- Produces: `RuntimePolicy::from_settings(revision, settings, now)`.

- [ ] **Step 1: Write failing tests**

Add `auto_switch_pause_is_absent_by_default`, `auto_switch_pause_round_trips_through_disk`, `an_expired_pause_is_absent_from_runtime_policy`, `an_active_pause_is_preserved_in_runtime_policy`, `runtime_policy_maps_every_daemon_setting`, and `runtime_policy_uses_the_supplied_revision`.

```rust
let now = DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
    .unwrap()
    .with_timezone(&Utc);
let mut settings = Settings::default();
settings.auto_switch_paused_until = Some(now + chrono::Duration::hours(1));
let policy = RuntimePolicy::from_settings(7, &settings, now);
assert_eq!(policy.revision, 7);
assert_eq!(policy.paused_until, settings.auto_switch_paused_until);
```

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml settings::tests::auto_switch_pause --lib
cargo test --manifest-path src-tauri/Cargo.toml runtime::tests --lib
```

- [ ] **Step 3: Implement the contract**

```rust
pub struct RuntimePolicy {
    pub revision: u64,
    pub threshold: f64,
    pub cooldown: Duration,
    pub hysteresis_pct: f64,
    pub strategy: crate::switcher::Strategy,
    pub unhealthy_ticks: u32,
    pub grace: Duration,
    pub auto_switch_enabled: bool,
    pub paused_until: Option<DateTime<Utc>>,
}
```

Add `#[serde(default)] pub auto_switch_paused_until: Option<DateTime<Utc>>` to `Settings`. An expired timestamp maps to `None` at runtime.

- [ ] **Step 4: Verify green and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml settings::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml runtime::tests --lib
git add src-tauri/src/settings.rs src-tauri/src/runtime.rs src-tauri/src/lib.rs src-tauri/src/poller.rs
git commit -m "feat: define persisted pause and runtime policy"
```

### Task 2: Revisioned canonical settings store

**Files:**
- Modify: `src-tauri/src/settings.rs`
- Test: inline `settings::tests`

**Interfaces:**
- Consumes: `RuntimePolicy::from_settings`.
- Produces: `SettingsPatch`, `SettingsSnapshot`, `SettingsStore`, `SettingsUpdateError`.

- [ ] **Step 1: Write failing store tests**

Cover named-field merge, sanitization, stale revision, persist-before-publish, rapid update coalescing, save failure, explicit-null pause clearing, and omitted pause preservation.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml settings::tests::settings_store --lib
```

- [ ] **Step 3: Implement DTOs and store**

```rust
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot { pub revision: u64, pub settings: Settings }

pub fn update(&self, expected: u64, patch: SettingsPatch, now: DateTime<Utc>)
    -> Result<SettingsSnapshot, SettingsUpdateError>;
pub fn snooze(&self, duration: Duration, now: DateTime<Utc>)
    -> Result<SettingsSnapshot, SettingsUpdateError>;
pub fn resume(&self, now: DateTime<Utc>)
    -> Result<SettingsSnapshot, SettingsUpdateError>;
```

`SettingsPatch` uses `deny_unknown_fields`; `auto_switch_paused_until` uses `Option<Option<DateTime<Utc>>>` to distinguish omission from explicit null. Hold one mutex through merge, sanitize, save, memory/revision update, and `send_replace`. A save error changes nothing.

- [ ] **Step 4: Verify green and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml settings::tests --lib
git add src-tauri/src/settings.rs
git commit -m "feat: add revisioned canonical settings store"
```

### Task 3: Settings IPC and global events

**Files:**
- Modify: `src-tauri/src/commands.rs:34-145,482-570,653-670`
- Modify: `src-tauri/src/lib.rs:274-345`
- Test: inline `commands::tests`

**Interfaces:**
- Consumes: `SettingsStore`.
- Produces: `get_settings`, `update_settings`, `snooze_auto_switch`, `resume_auto_switch`, `settings://updated`.

- [ ] **Step 1: Write failing command tests**

Add tests for revisioned hydration, structural conflict, stale overwrite prevention, exact snooze deadline, resume clearing, and event-after-success ordering.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml commands::tests::update_settings --lib
cargo test --manifest-path src-tauri/Cargo.toml commands::tests::snooze --lib
```

- [ ] **Step 3: Implement command inputs**

```rust
pub struct UpdateSettingsInput { pub expected_revision: u64, pub patch: SettingsPatch }
pub struct SnoozeAutoSwitchInput { pub duration_seconds: u64 }
```

Reject zero snooze duration, replace `AppState.settings: Mutex<Settings>` with `SettingsStore`, remove `set_settings` from the handler, and emit only after successful canonical mutation.

- [ ] **Step 4: Verify green and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml commands::tests --lib
git add src-tauri/src/commands.rs src-tauri/src/lib.rs
git commit -m "feat: expose revisioned settings commands"
```

### Task 4: Authoritative daemon status

**Files:**
- Modify: `src-tauri/src/runtime.rs`, `commands.rs`, `poller.rs`, `lib.rs`
- Test: inline `runtime::tests`

**Interfaces:**
- Consumes: `RuntimePolicy`.
- Produces: `DaemonPhase`, `DaemonStatus`, `DaemonStatusStore`, `get_daemon_status`, `daemon://status`.

- [ ] **Step 1: Write failing status tests**

Cover initial disabled/paused/monitoring, revision increments, identical transition suppression, policy revision changes, and exact camelCase tagged serialization.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml runtime::tests::status --lib
```

- [ ] **Step 3: Implement the phase union**

```rust
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum DaemonPhase {
    Disabled,
    Paused { until: DateTime<Utc> },
    Monitoring,
    Cooldown { until: DateTime<Utc> },
    Warning { from: u32, to: u32, deadline: DateTime<Utc> },
    Switching { from: u32, to: u32 },
    Exhausted { earliest_reset: Option<DateTime<Utc>> },
    Degraded { reason: DegradedReason },
    RecoveryRequired { detail: String },
}
```

Store transitions before emitting; emit only if phase or policy revision changed. Hydration reads the store.

- [ ] **Step 4: Verify green and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml runtime::tests --lib
git add src-tauri/src/runtime.rs src-tauri/src/commands.rs src-tauri/src/poller.rs src-tauri/src/lib.rs
git commit -m "feat: add authoritative daemon status"
```

### Task 5: Exact-deadline decision state machine

**Files:**
- Modify: `src-tauri/src/poller.rs:303-626,1152-1665`
- Test: inline `poller::tests`

**Interfaces:**
- Consumes: `RuntimePolicy`, `DaemonPhase`.
- Produces: `PollerLoopState`, absolute `PendingSwitch`, phase-specific `Decision`.

- [ ] **Step 1: Write failing behavior tests**

Cover disabled, active/expired pause, before/at exact deadline, policy cancellation, fresh barrier, strategy restart, exact cooldown, degraded unknown usage, and proven exhaustion.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml poller::tests::grace --lib
cargo test --manifest-path src-tauri/Cargo.toml poller::tests::policy_change --lib
```

- [ ] **Step 3: Implement state seam**

```rust
struct PollerLoopState {
    policy: RuntimePolicy,
    daemon: DaemonState,
    last_trusted_snapshot: Option<Snapshot>,
    requires_fresh_snapshot: bool,
}
```

`apply_policy` clears pending and arms the fresh barrier. Deadline switching requires matching policy revision and a released barrier.

- [ ] **Step 4: Verify green and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml poller::tests --lib
git add src-tauri/src/poller.rs
git commit -m "refactor: make daemon decisions revisioned and deadline based"
```

### Task 6: Interruptible poller loop

**Files:**
- Modify: `src-tauri/Cargo.toml`, `Cargo.lock`, `poller.rs:716-923`, `lib.rs:335-345`
- Test: paused-time `poller::tests`

**Interfaces:**
- Consumes: `watch::Receiver<RuntimePolicy>`, `PollerLoopState`.
- Produces: `poller::run(app, policy_rx)`.

- [ ] **Step 1: Add Tokio `test-util` and failing paused-time tests**

Cover long-sleep wake, rapid updates, disable/pause cancellation, enable/resume barrier, failed fetch barrier retention, grace wake without extra fetch, and old-policy rejection.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml poller::tests::policy_update_wakes --lib
cargo test --manifest-path src-tauri/Cargo.toml poller::tests::grace_deadline_wakes --lib
```

- [ ] **Step 3: Implement `tokio::select!`**

```rust
tokio::select! {
    changed = policy_rx.changed() => {
        changed?;
        let next = policy_rx.borrow_and_update().clone();
        state.apply_policy(next, Utc::now());
    }
    _ = tokio::time::sleep_until(next_deadline) => run_due_action().await,
}
```

Drop the watch borrow before awaiting and select the earlier poll/grace deadline.

- [ ] **Step 4: Verify green and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml poller::tests --lib
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/poller.rs src-tauri/src/lib.rs
git commit -m "feat: make the poller interruptible"
```

### Task 7: Frontend test harness and IPC contracts

**Files:**
- Create: `vitest.config.ts`, `src/test/setup.ts`, `src/lib/api.test.ts`
- Modify: `package.json`, `pnpm-lock.yaml`, `src/types.ts`, `src/lib/api.ts`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: backend settings/status shapes.
- Produces: renderer test harness and exact API functions.

- [ ] **Step 1: Install current compatible test packages**

```powershell
pnpm add -D vitest@^4.1.10 jsdom@^30.0.0 @testing-library/react@^16.3.2 @testing-library/dom@^10.4.1 @testing-library/user-event@^14.6.1 @testing-library/jest-dom@^7.0.0
```

Add `test`/`test:watch` scripts and pin CI Node to `22.22.2`, jsdom 30's minimum Node 22 release.

- [ ] **Step 2: Write failing mocked IPC/event tests**

Use `mockIPC(..., { shouldMockEvents: true })` and `clearMocks()`; cover command arguments, conflict detail, and listener cleanup.

- [ ] **Step 3: Verify red**

```powershell
pnpm test -- src/lib/api.test.ts
```

- [ ] **Step 4: Implement TS DTOs/API and verify**

Add `SettingsSnapshot`, `SettingsPatch`, tagged `DaemonPhase`, `DaemonStatus`, update/snooze/resume/get-status functions, and listeners.

```powershell
pnpm test -- src/lib/api.test.ts
pnpm exec tsc --noEmit
git add package.json pnpm-lock.yaml vitest.config.ts src/test/setup.ts src/types.ts src/lib/api.ts src/lib/api.test.ts .github/workflows/ci.yml
git commit -m "test: add frontend runtime contract coverage"
```

### Task 8: Revision-safe frontend hooks

**Files:**
- Create: `src/lib/useSettings.ts`, `useSettings.test.tsx`, `useDaemonStatus.ts`, `useDaemonStatus.test.tsx`

**Interfaces:**
- Consumes: runtime API/event functions.
- Produces: `UseSettingsResult`, `UseDaemonStatusResult`.

- [ ] **Step 1: Write failing subscribe-then-hydrate tests**

Prove event-during-hydration wins, old revisions cannot regress, unmount unlistens, writes use latest revision, and conflict rehydrates.

- [ ] **Step 2: Verify red**

```powershell
pnpm test -- src/lib/useSettings.test.tsx src/lib/useDaemonStatus.test.tsx
```

- [ ] **Step 3: Implement highest-revision acceptance**

```ts
const acceptNewest = <T extends { revision: number }>(old: T | null, next: T) =>
  old === null || next.revision > old.revision ? next : old;
```

Subscribe first, hydrate second, and pass both through this function.

- [ ] **Step 4: Verify green and commit**

```powershell
pnpm test -- src/lib/useSettings.test.tsx src/lib/useDaemonStatus.test.tsx
pnpm exec tsc --noEmit
git add src/lib/useSettings.ts src/lib/useSettings.test.tsx src/lib/useDaemonStatus.ts src/lib/useDaemonStatus.test.tsx
git commit -m "feat: hydrate revisioned runtime state in the frontend"
```

### Task 9: Convert all settings consumers to patches

**Files:**
- Modify: `src/App.tsx`, `src/lib/useTheme.ts`, `SettingsScreen.tsx`, `HistoryScreen.tsx`
- Test: `useTheme.test.tsx`, `SettingsScreen.test.tsx`

**Interfaces:**
- Consumes: `UseSettingsResult.update(patch)`.
- Produces: no full-object settings writers.

- [ ] **Step 1: Write failing lost-update tests**

Prove theme/threshold send single-field patches, simultaneous edits preserve both, conflict restores confirmed state, and deferred controls are absent.

- [ ] **Step 2: Verify red**

```powershell
pnpm test -- src/lib/useTheme.test.tsx src/components/SettingsScreen.test.tsx
```

- [ ] **Step 3: Implement one settings owner per window**

Mount `useSettings` in `App`, pass it to theme/settings consumers, retain slider debounce with `{ threshold }`, and unwrap `SettingsSnapshot.settings` in history.

- [ ] **Step 4: Verify green and commit**

```powershell
rg -n "setSettings\(|set_settings" src src-tauri/src
pnpm test -- src/lib/useTheme.test.tsx src/components/SettingsScreen.test.tsx
pnpm exec tsc --noEmit
git add src/App.tsx src/lib/useTheme.ts src/lib/useTheme.test.tsx src/components/SettingsScreen.tsx src/components/SettingsScreen.test.tsx src/components/HistoryScreen.tsx
git commit -m "fix: replace whole settings writes with revisioned patches"
```

### Task 10: Pure daemon-status popover and subsystem gate

**Files:**
- Modify: `src/components/PopoverPanel.tsx`, `src/styles/app.css`, `README.md`
- Create: `src/components/PopoverPanel.test.tsx`

**Interfaces:**
- Consumes: `useDaemonStatus`, pause actions, usage snapshot for display only.
- Produces: truthful phase rendering.

- [ ] **Step 1: Write the failing phase matrix**

Cover disabled/monitoring at high usage, backend warning target/deadline, zero countdown without client switch, persisted hold, paused/resume, cooldown, backend-only exhausted, degraded, recovery required, absent reset notification, absent `preview_target`, and manual-only direct switch.

- [ ] **Step 2: Verify red**

```powershell
pnpm test -- src/components/PopoverPanel.test.tsx
```

- [ ] **Step 3: Delete local policy state and render phases**

Remove hardcoded grace/hold, preview target, held target, reset notification, and quota-derived action inference. Countdown derives from the backend deadline; zero never invokes switching.

- [ ] **Step 4: Run full gate and commit**

```powershell
pnpm test
pnpm exec tsc --noEmit
pnpm build
Push-Location src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
Pop-Location
git add src/components/PopoverPanel.tsx src/components/PopoverPanel.test.tsx src/styles/app.css README.md
git commit -m "fix: render authoritative daemon state in the popover"
```
