# Release Blockers Rollout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the three approved blocker fixes as independently reviewable tracks, then enable automatic switching only after their shared safety gates pass.

**Architecture:** Runtime policy/UI truth, OAuth generation safety, and transactional switching are separate plans with explicit integration contracts. Runtime work and the primitive portions of OAuth/transaction work can proceed in parallel; activation integration is ordered so fresh target validation feeds the transaction coordinator and daemon status reflects recovery state.

**Tech Stack:** Tauri 2, Rust 1.81+, Tokio 1.53.1, React 19, TypeScript 5.8, Vite 7, Vitest 4, Testing Library, platform-native DPAPI/Keychain/file APIs.

## Global Constraints

- Keep the blocker boundary strict: live daemon/UI truth, OAuth refresh safety, and crash-safe switching only.
- `Hold 1h` is a persisted global pause; polling/history continue and `Resume now` clears it.
- Every decision-input change cancels pending grace and requires a fresh successful snapshot.
- Only fresh accounts with known positive headroom may be selected automatically.
- Exact current-generation `invalid_grant` becomes `Re-login required`; ambiguous failures remain retryable.
- No network I/O may occur while Claude Code's credential/config locks or the full live-state lock set are held.
- The transaction journal metadata contains no credential plaintext.
- A newer outgoing credential/config generation survives failed activation.
- Unverified recovery blocks manual and automatic switching.
- Supported durability target is local NTFS, APFS, ext4, and equivalent local filesystems.
- Do not implement WSL mutation, account removal, autostart, notifications, or unrelated UI polish in these plans.
- Preserve Rust `rust-version = "1.81"`, `panic = "unwind"`, and all-platform compilation.
- Use TDD: observe every focused test fail for the intended reason before implementation.
- Commit only the task's named files; do not sweep unrelated user changes into checkpoints.

---

## Linked plans

1. [Runtime Settings and Truthful Daemon UI](./2026-07-28-runtime-daemon-ui.md)
2. [OAuth Generation Safety](./2026-07-28-oauth-generation-safety.md)
3. [Transactional Switching and Recovery](./2026-07-28-transactional-switching.md)

## Dependency graph

```text
Runtime tasks 1-8 ───────────────────────────────┐
                                                │
OAuth tasks 1-6 ──► OAuth target validation ────┼──► integration gate
                                                │
Transaction locks/fs/journal tasks 1-7 ─────────┘
                        │
                        └──► transaction wiring/recovery tasks 8-11
```

- Runtime tasks 1-8 can run independently of the two credential tracks.
- OAuth tasks 1-6 can run in parallel with transaction tasks 1-7.
- OAuth task 7 produces `ValidatedCredential.generation`.
- Transaction task 8 consumes that generation and rechecks it under the full lock set.
- Runtime task 4 produces `DaemonPhase::RecoveryRequired`.
- Transaction task 9 feeds recovery failures into that phase.
- Runtime popover work should rebase after OAuth task 9 if both touch `PopoverPanel.tsx` or `src/types.ts`.
- Automatic switching remains disabled in release builds until the final integration gate passes on all three OS families.

### Task 1: Establish the execution baseline

**Files:**
- Read: `docs/superpowers/specs/2026-07-28-release-blockers-design.md`
- Read: the three linked plans above
- Verify: `package.json`, `src-tauri/Cargo.toml`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: approved design commit `9e163e3`.
- Produces: a clean isolated worktree and recorded baseline command output.

- [ ] **Step 1: Create an isolated worktree**

Use the required `superpowers:using-git-worktrees` skill, create a feature branch from the approved design commit, and verify the new worktree path before editing.

- [ ] **Step 2: Verify the baseline frontend**

Run:

```powershell
pnpm install --frozen-lockfile
pnpm exec tsc --noEmit
pnpm build
```

Expected: all commands exit `0`.

- [ ] **Step 3: Verify the baseline backend**

Run:

```powershell
Push-Location src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
Pop-Location
```

Expected: formatting, Clippy, unit, and integration tests pass; differential/interop tests may print their documented dependency skip.

- [ ] **Step 4: Record the baseline without changing product files**

Add baseline results to the implementation session notes or PR description. Do not commit generated `dist/` or `target/` output.

### Task 2: Execute the independent foundations in parallel

**Files:**
- Follow: runtime plan Tasks 1-8
- Follow: OAuth plan Tasks 1-6
- Follow: transaction plan Tasks 1-7

**Interfaces:**
- Consumes: clean verified baseline.
- Produces: revisioned runtime contracts, coordinated OAuth snapshots, and transaction primitives without wiring automatic activation.

- [ ] **Step 1: Dispatch one fresh worker per track**

Give each worker only its linked plan plus the approved design. Require the named focused test and commit checkpoint before returning.

- [ ] **Step 2: Review runtime foundation**

Verify `RuntimePolicy`, `SettingsStore`, `DaemonStatusStore`, and frontend subscribe-then-hydrate hooks match the exact signatures in the runtime plan.

- [ ] **Step 3: Review OAuth foundation**

Verify structural error parsing, quarantine, compare-and-store, and fail-closed selection match the OAuth plan and contain no response substring quarantine.

- [ ] **Step 4: Review transaction foundation**

Verify Claude lock timings/paths, durable file replacement, recovery protection, and journal schemas match the transaction plan and contain no secret-bearing journal fields.

- [ ] **Step 5: Run combined foundation tests**

```powershell
pnpm test
cargo test --manifest-path src-tauri/Cargo.toml runtime::tests
cargo test --manifest-path src-tauri/Cargo.toml oauth_refresh::tests
cargo test --manifest-path src-tauri/Cargo.toml switch_journal::tests
cargo test --manifest-path src-tauri/Cargo.toml claude_locks::tests
```

Expected: all focused suites pass.

### Task 3: Integrate validated OAuth with transactional switching

**Files:**
- Follow: OAuth plan Tasks 7-8
- Follow: transaction plan Tasks 8-10
- Modify through those plans: `src-tauri/src/switcher.rs`, `commands.rs`, `poller.rs`, `lib.rs`

**Interfaces:**
- Consumes: `ValidatedCredential { identity, credentials, generation }`, `switch_transaction::switch_with`.
- Produces: async validation orchestration with synchronous, no-network locked mutation and restart recovery.

- [ ] **Step 1: Land switch-boundary freshening first**

Run the OAuth plan's Task 7 failing tests, implement `switch_to_validated_with_timeout`, and prove generation mismatch aborts before outgoing backup.

- [ ] **Step 2: Wire the transaction coordinator**

Pass `validated.generation` into `switch_transaction::switch`; re-read and compare it after acquiring the full lock set and before `Prepared` is written.

- [ ] **Step 3: Make only orchestration asynchronous**

`commands::switch_account` awaits public `switch_to`; the locked mutation and its fault tests remain synchronous. Poller panic isolation uses a spawned Tokio task and handles `JoinError`.

- [ ] **Step 4: Wire startup recovery before daemon startup**

Call recovery after the store root is established and before `AppState::new` or poller spawn. Map retained recovery failure to `DaemonPhase::RecoveryRequired`.

- [ ] **Step 5: Run the integration-focused suites**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests::switch_to_
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::transaction
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::recovery
cargo test --manifest-path src-tauri/Cargo.toml poller::tests::perform_switch_
```

Expected: dead/superseded targets cause no live mutation; injected failures yield coherent recovery or a retained blocking journal.

### Task 4: Resolve shared frontend contracts

**Files:**
- Modify: `src/types.ts`
- Modify: `src/components/PopoverPanel.tsx`
- Modify: `src/components/AccountsScreen.tsx`
- Test: corresponding `*.test.tsx` files

**Interfaces:**
- Consumes: `DaemonStatus`, `UsageStatus::ReloginRequired`, revisioned settings API.
- Produces: one truthful rendering contract with no local auto-switch policy.

- [ ] **Step 1: Rebase shared frontend changes**

Resolve both tracks by retaining the runtime plan's daemon-phase rendering and the OAuth plan's re-login label/disabled action.

- [ ] **Step 2: Run the combined failing cases first**

```powershell
pnpm test -- src/components/PopoverPanel.test.tsx src/components/AccountsScreen.test.tsx
```

Expected before reconciliation: at least the new combined recovery/re-login case fails for a missing rendering branch.

- [ ] **Step 3: Implement the combined state matrix**

Ensure `recoveryRequired` disables all switch controls globally; `reloginrequired` disables only that account; high quota alone never produces warning/exhausted copy.

- [ ] **Step 4: Re-run renderer verification**

```powershell
pnpm test
pnpm exec tsc --noEmit
pnpm build
```

Expected: all renderer tests, typecheck, and build pass.

### Task 5: Run the complete blocker gate

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`, `RESILIENCE.md`, `PLAN.md`, `CHANGELOG.md`
- Create: `docs/TRANSACTION_RECOVERY.md`

**Interfaces:**
- Consumes: all three completed subsystem plans.
- Produces: verified release candidate and documentation matching actual guarantees.

- [ ] **Step 1: Run local frontend gates**

```powershell
pnpm install --frozen-lockfile
pnpm test
pnpm exec tsc --noEmit
pnpm build
```

- [ ] **Step 2: Run local Rust gates**

```powershell
Push-Location src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
Pop-Location
```

- [ ] **Step 3: Run interoperability and crash suites explicitly**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test lock_interop -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --test claude_lock_interop -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::crash_restart -- --nocapture --test-threads=1
```

- [ ] **Step 4: Run packaged OS smoke tests**

On Windows/NTFS, macOS/APFS, and Linux/ext4 verify: enable, warning, disable during grace, persisted pause/restart/resume, normal switch, held Claude locks, process termination after each live write, restart recovery, and OAuth/API-key transitions.

- [ ] **Step 5: Update documentation from observed behavior**

Remove the incorrect atomic-parity claim, document the five-lock order, recovery phases, local-filesystem boundary, fail-closed OAuth eligibility, and exact pause/grace behavior.

- [ ] **Step 6: Add CI gates and commit**

Add `pnpm test` to the frontend job and keep the all-platform Rust matrix. Commit only after the full local gate passes:

```powershell
git add .github/workflows/ci.yml README.md RESILIENCE.md PLAN.md CHANGELOG.md docs/TRANSACTION_RECOVERY.md
git commit -m "docs: record verified blocker safety guarantees"
```

## Completion definition

The blocker program is complete only when:

- saved policy changes affect the running daemon without restart;
- the popover renders only authoritative backend state;
- disabling or pausing during grace prevents credential mutation;
- rotated inactive credentials are durably persisted;
- dead or untrusted accounts are never selected automatically;
- the target generation is revalidated under switch locks;
- Claude Code refresh/config writers cannot interleave with switching;
- every injected error or process-death point recovers to a coherent state or retains a journal that blocks further mutation;
- all local and packaged cross-platform gates pass;
- repository documentation states only verified guarantees.
