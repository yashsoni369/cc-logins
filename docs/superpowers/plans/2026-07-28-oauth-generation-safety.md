# OAuth Generation Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist every inactive OAuth rotation safely, quarantine only proven-dead token generations, exclude untrusted automatic targets, and revalidate credentials immediately before activation.

**Architecture:** An injectable OAuth network boundary feeds a per-account `RefreshCoordinator`. The coordinator uses a cross-process lease plus short GUI-vault critical sections for generation compare-and-store; a secret-free quarantine records exact current-generation `invalid_grant` outcomes.

**Tech Stack:** Rust 1.81, Tokio, reqwest 0.12/rustls, serde_json, SHA-256, existing `CredentialStore` and `FileLock`; no new dependency.

## Global Constraints

- Parse OAuth error JSON structurally; never quarantine by response substring.
- Only exact `invalid_grant` against the still-current generation means `Re-login required`.
- Local access-token expiry is refresh-needed, not account death.
- A refresh is successful only after the successor is durably persisted.
- Never hold the GUI vault lock across network I/O.
- Stale callbacks cannot overwrite moved, re-added, or newer account generations.
- Automatic targets require `UsageStatus::Ok` and known positive headroom.
- Manual stale/unknown targets may proceed only after switch-boundary validation.
- Confirmed re-login-required targets cannot be activated.
- The transaction plan consumes `ValidatedCredential.generation` and rechecks it under locks.

---

### Task 1: Structural OAuth failure classification

**Files:**
- Modify: `src-tauri/src/oauth.rs:578-635,985-1018`
- Test: inline `oauth::tests`

**Interfaces:**
- Produces: `RefreshError::{InvalidGrant, Retryable(...)}` and deterministic token-expiry helper.

- [ ] **Step 1: Write failing classification tests**

Add exact JSON, substring, `invalid_client`, ambiguous 400, marker-on-500, malformed JSON, and deterministic-expiry cases.

```rust
assert_eq!(classify_refresh_failure(400, r#"{"error":"invalid_grant"}"#), RefreshError::InvalidGrant);
assert!(matches!(classify_refresh_failure(400, "invalid_grant later"), RefreshError::Retryable(_)));
assert!(matches!(classify_refresh_failure(400, r#"{"error":"invalid_client"}"#), RefreshError::Retryable(_)));
```

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth::tests::classify_refresh_failure --lib
```

- [ ] **Step 3: Implement exact parsing**

Deserialize `{ error: Option<String> }`; return `InvalidGrant` only for status 400 and exact value. Define retryable variants for no token, invalid client, HTTP, network, timeout, and invalid response. Add `is_oauth_token_expired_at(expires_at, now_ms)` and keep the old helper delegating to wall time.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth::tests --lib
git add src-tauri/src/oauth.rs
git commit -m "refactor: classify OAuth refresh failures structurally"
```

### Task 2: Injectable OAuth network boundary

**Files:**
- Modify: `src-tauri/src/oauth.rs:663-1298`
- Test: inline scripted-network tests

**Interfaces:**
- Consumes: structural errors.
- Produces: `OAuthNetwork`, `ReqwestOAuthNetwork`, raw refresh/usage outcomes.

- [ ] **Step 1: Write failing scripted transport tests**

Cover rotated refresh token, omitted successor preserving predecessor, raw usage 401 exposure, and exact call order.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth::tests::scripted_network --lib
```

- [ ] **Step 3: Implement without a new async-trait crate**

```rust
pub type OAuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub trait OAuthNetwork: Send + Sync {
    fn refresh<'a>(&'a self, credentials: &'a str) -> OAuthFuture<'a, RefreshOutcome>;
    fn fetch_usage<'a>(&'a self, access_token: &'a str) -> OAuthFuture<'a, UsageOutcome>;
}
```

Existing public helpers delegate to `ReqwestOAuthNetwork`; keep production behavior unchanged in this task.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth::tests --lib
git add src-tauri/src/oauth.rs
git commit -m "refactor: inject OAuth refresh and usage transport"
```

### Task 3: Re-login status and fail-closed eligibility

**Files:**
- Modify: `src-tauri/src/model.rs:62-212`
- Modify: `src-tauri/src/switcher.rs:1953-2057`
- Modify: `src-tauri/src/poller.rs` tests
- Test: inline model/switcher/poller tests

**Interfaces:**
- Produces: `UsageStatus::ReloginRequired`, `Account::is_automatic_target()`.

- [ ] **Step 1: Write failing eligibility tests**

Cover new/legacy status deserialization, positive-known-headroom requirement, all three strategies excluding stale/unknown/unavailable/re-login, poller never warning for dead accounts, and unknown candidates preventing false exhaustion.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml model::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests::next_available --lib
```

- [ ] **Step 3: Implement explicit automatic eligibility**

```rust
pub fn is_automatic_target(&self) -> bool {
    !self.active
        && self.usage_status == UsageStatus::Ok
        && matches!(self.headroom(), Some(value) if value > 0.0)
}
```

Accept legacy serialized `expired` as `ReloginRequired`. All automatic strategies use this method; manual `is_switchable` remains a separate policy.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml model::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml poller::tests --lib
git add src-tauri/src/model.rs src-tauri/src/switcher.rs src-tauri/src/poller.rs
git commit -m "fix: require fresh trusted automatic targets"
```

### Task 4: Secret-free persisted quarantine

**Files:**
- Create: `src-tauri/src/oauth_quarantine.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: inline `oauth_quarantine::tests`

**Interfaces:**
- Consumes: stable key, credential fingerprint.
- Produces: generation-bound quarantine CRUD requiring caller-held vault lock.

- [ ] **Step 1: Write failing quarantine tests**

Cover no credential material, identity+fingerprint match, new generation clearing, malformed file degrading safely, and atomic-write failure preserving the prior file.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth_quarantine::tests --lib
```

- [ ] **Step 3: Implement schema and atomic store**

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineFile {
    schema_version: u32,
    entries: BTreeMap<String, QuarantineEntry>,
}
struct QuarantineEntry { credential_fingerprint: String, rejected_at: DateTime<Utc> }
```

Store at `<backup_root>/oauth-quarantine.json`. Reject unknown schema versions and never store access/refresh tokens.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth_quarantine::tests --lib
git add src-tauri/src/oauth_quarantine.rs src-tauri/src/lib.rs
git commit -m "feat: persist generation-bound OAuth quarantine"
```

### Task 5: Generation-safe refresh coordinator

**Files:**
- Create: `src-tauri/src/oauth_refresh.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: inline deterministic fakes

**Interfaces:**
- Consumes: `OAuthNetwork`, quarantine, credential store adapter, refresh lease, clock.
- Produces: `RefreshCoordinator::fetch_inactive_usage`, `freshen_for_activation`, `ValidatedCredential`.

- [ ] **Step 1: Write failing coordinator tests**

Cover rotation persistence, 401-refresh-persist-retry, persist failure, one concurrent refresh, stale successor, moved/re-added slot, identical idempotency, current-generation invalid grant, competing rotation, invalid client, lease timeout/release, and local expiry without quarantine.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth_refresh::tests --lib
```

- [ ] **Step 3: Implement exact contracts**

```rust
pub struct ValidatedCredential {
    pub identity: AccountIdentity,
    pub credentials: String,
    pub generation: String,
}
pub enum CompareAndStore { Persisted(StoredGeneration), AlreadyCurrent(StoredGeneration), Superseded(StoredGeneration), Missing }
pub fn credential_generation(credentials: &str) -> String;
```

Use full-credential SHA-256 for compare-and-store, distinct from lineage fingerprint. Real leases use `<backup_root>/oauth-refresh-locks/<sha256(stable-key)>.lock` acquired via `spawn_blocking`. Read and CAS under short vault locks; network between them.

- [ ] **Step 4: Make persist part of success**

Return `PersistenceFailed` and no healthy usage when successor storage fails. On CAS superseded, use the winner; never overwrite it. On invalid grant, reread and quarantine only if generation remains current.

- [ ] **Step 5: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth_refresh::tests --lib
git add src-tauri/src/oauth_refresh.rs src-tauri/src/lib.rs
git commit -m "feat: coordinate and persist inactive OAuth rotation"
```

### Task 6: Snapshot integration and quarantine clearing

**Files:**
- Modify: `src-tauri/src/switcher.rs:519-574` and account-add paths
- Test: inline `switcher::tests`

**Interfaces:**
- Consumes: `RefreshCoordinator`.
- Produces: exact snapshot status mapping and re-add healing.

- [ ] **Step 1: Write failing snapshot tests**

Add cases for persisted rotated generation, same-pass re-login state, retryable unavailable, disabled precedence, persist-failure suppression, new-generation clearing across add paths, and same rejected generation remaining quarantined.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests::snapshot_ --lib
```

- [ ] **Step 3: Replace the `None` persistence path**

Map success to `Ok`, proven invalid grant to `ReloginRequired`, retryable/lock/persist errors to `Unavailable`, preserve `Disabled`, and use `Unknown` only for never-measured/no-credential state.

- [ ] **Step 4: Clear only obsolete quarantine and commit**

After a successful add/re-add, clear quarantine only when the stored fingerprint differs from the rejected fingerprint.

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests --lib
git add src-tauri/src/switcher.rs
git commit -m "fix: persist refreshes and surface dead credentials"
```

### Task 7: Switch-boundary target freshening

**Files:**
- Modify: `src-tauri/src/switcher.rs:696-852`
- Test: inline `switcher::tests`

**Interfaces:**
- Consumes: `RefreshCoordinator::freshen_for_activation`.
- Produces: async public `switch_to`; synchronous `switch_to_validated_with_timeout` for the transaction plan.

- [ ] **Step 1: Write failing boundary tests**

Cover expired refresh before install, invalid grant before live write, quarantined target without network, current generation rather than snapshot bytes, post-freshen generation mismatch before outgoing backup, retry on winner, stale manual success, and re-login manual rejection.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests::switch_to_refreshes --lib
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests::generation_change_after_freshening --lib
```

- [ ] **Step 3: Implement async orchestration and sync seam**

```rust
pub async fn switch_to(target: &Account) -> Result<(), SwitchError>;
fn switch_to_validated_with_timeout(
    target: &Account,
    validated: &ValidatedCredential,
    timeout: Duration,
) -> Result<(), SwitchError>;
```

Freshen before live locks. Under the mutation seam re-resolve the slot and compare full generation before any backup/live write; mismatch returns `TargetGenerationChanged`.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests --lib
git add src-tauri/src/switcher.rs
git commit -m "fix: validate the latest target generation before activation"
```

### Task 8: Async callers and panic isolation

**Files:**
- Modify: `src-tauri/src/commands.rs:326-360`
- Modify: `src-tauri/src/poller.rs:794-908`
- Test: inline commands/poller tests

**Interfaces:**
- Consumes: async `switcher::switch_to`.
- Produces: awaited manual/automatic orchestration with no success event on failure.

- [ ] **Step 1: Write failing caller tests**

Cover success event after validation, freshening failure without state advance, panicking spawned task survival, and structured manual re-login error.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml poller::tests::perform_switch_ --lib
cargo test --manifest-path src-tauri/Cargo.toml commands::tests::manual_command_ --lib
```

- [ ] **Step 3: Await commands and isolate poller panics**

Commands call `switcher::switch_to(target).await?`. Poller spawns the async switch and converts `JoinError` into a failed transition; do not add `futures`.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml commands::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml poller::tests --lib
git add src-tauri/src/commands.rs src-tauri/src/poller.rs
git commit -m "refactor: await target validation safely"
```

### Task 9: IPC status and truthful account rendering

**Files:**
- Modify: `src/types.ts`, `src/components/AccountsScreen.tsx`, `PopoverPanel.tsx`
- Test: corresponding component tests from runtime plan

**Interfaces:**
- Consumes: serialized `reloginrequired`.
- Produces: distinct label and disabled account activation.

- [ ] **Step 1: Write failing rendering tests**

Assert “Re-login required” appears, quota-expired copy is not reused, the account switch button is disabled, and other healthy manual targets remain available.

- [ ] **Step 2: Verify red**

```powershell
pnpm test -- src/components/AccountsScreen.test.tsx src/components/PopoverPanel.test.tsx
```

- [ ] **Step 3: Implement the status branch**

Add `"reloginrequired"` to the TypeScript union and render explicit recovery guidance. Do not infer this state from local expiration.

- [ ] **Step 4: Verify and commit**

```powershell
pnpm test -- src/components/AccountsScreen.test.tsx src/components/PopoverPanel.test.tsx
pnpm exec tsc --noEmit
pnpm build
git add src/types.ts src/components/AccountsScreen.tsx src/components/PopoverPanel.tsx src/components/AccountsScreen.test.tsx src/components/PopoverPanel.test.tsx
git commit -m "feat: surface accounts that require re-login"
```

### Task 10: OAuth subsystem regression gate

**Files:**
- Verify all changed files; no behavior additions.

**Interfaces:**
- Produces: validated input contract for transactional switching.

- [ ] **Step 1: Run focused suites**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml oauth::tests
cargo test --manifest-path src-tauri/Cargo.toml oauth_refresh::tests
cargo test --manifest-path src-tauri/Cargo.toml oauth_quarantine::tests
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests
```

- [ ] **Step 2: Run full gates**

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --all-features
pnpm test
pnpm build
```

- [ ] **Step 3: Verify forbidden patterns**

```powershell
rg -n "try_fetch_usage_for_account\([^\n]*None|contains\([^\n]*invalid_grant|UsageStatus::Stale" src-tauri/src
```

Expected: no inactive refresh with missing persistence, no substring classifier, and no dead-token downgrade to stale.

- [ ] **Step 4: Commit only if a test-only correction was required**

```powershell
git add src-tauri/src src-tauri/tests src
git commit -m "test: close OAuth rotation regressions"
```

Skip this commit when the gate required no changes.
