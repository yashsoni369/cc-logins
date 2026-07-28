# Transactional Switching and Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make account activation cooperate with Claude Code, recover coherently after errors or process death, and block further mutation whenever recovery cannot be verified.

**Architecture:** Focused modules implement Claude `proper-lockfile` directory locks, durable per-file replacement, protected before-images, a secret-free journal, and one transaction coordinator. OAuth freshening happens before locks; the validated generation is rechecked under the complete lock set before the journal is prepared.

**Tech Stack:** Rust 1.81, serde/serde_json, sha2, existing `FileLock`, `windows-sys` (`ReplaceFileW`, `MoveFileExW`), libc/POSIX directory sync, macOS security-framework; no new crate.

## Global Constraints

- Lock order is `cswap` compatibility, Claude primary OAuth, Claude legacy credentials, Claude global config, GUI vault.
- Claude credential locks use 60-second staleness; config lock uses 10 seconds; touch every 3 seconds; wait at most 9 seconds.
- Never hold the full live-state lock set across network I/O.
- Validate target artifacts before the first mutation.
- Journal metadata contains no credential/config plaintext.
- Preserve exact file/Keychain presence, not just equivalent parsed credentials.
- The newest outgoing credential/config backup generation survives rollback.
- Recovery is idempotent; unverifiable recovery retains artifacts and blocks switching.
- Local NTFS/APFS/ext4-class filesystems are the supported durability boundary.
- Do not copy upstream's in-memory-only `SwitchTransaction` literally.

---

### Task 1: Claude-compatible directory locks

**Files:**
- Create: `src-tauri/src/claude_locks.rs`
- Modify: `src-tauri/src/paths.rs`, `src-tauri/src/lib.rs`
- Test: inline `claude_locks::tests`

**Interfaces:**
- Produces: `DirectoryLock`, `ClaudeCredentialLocks`, three path helpers.

- [ ] **Step 1: Write failing lock protocol tests**

Cover create/touch/remove, fresh timeout, 30-second credential non-stale, >60 credential takeover, >10 config takeover, primary contention not creating legacy, legacy contention releasing primary, config-dir path override, and toucher shutdown before removal.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml claude_locks::tests -- --nocapture
```

- [ ] **Step 3: Implement exact constants and guards**

```rust
pub const CREDENTIAL_STALENESS: Duration = Duration::from_secs(60);
pub const CONFIG_STALENESS: Duration = Duration::from_secs(10);
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(3);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(9);
pub fn acquire_credential_locks(timeout: Duration) -> Result<ClaudeCredentialLocks, ClaudeLockError>;
pub fn acquire_config_lock(timeout: Duration) -> Result<DirectoryLock, ClaudeLockError>;
```

Acquire by atomic `create_dir`; stale takeover removes and retries; a background toucher updates directory mtime; Drop stops/joins it before removing the directory.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml claude_locks::tests -- --nocapture
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
git add src-tauri/src/claude_locks.rs src-tauri/src/paths.rs src-tauri/src/lib.rs
git commit -m "feat: add Claude Code compatible directory locks"
```

### Task 2: Cross-process lock interoperability

**Files:**
- Create: `src-tauri/tests/claude_lock_interop.rs`
- Create: `src-tauri/tests/fixtures/upstream_lock_holder.py`

**Interfaces:**
- Consumes: Task 1 lock APIs.
- Produces: Rust/Rust and pinned Python-protocol/Rust evidence.

- [ ] **Step 1: Write failing subprocess tests**

Add Rust holder/contender, Python holder/Rust contender, Rust holder/Python contender, config lock, and stale lock cases. Synchronize with readiness stdout/files; do not use sleep as the correctness signal.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test claude_lock_interop -- --nocapture
```

- [ ] **Step 3: Implement pinned fixture protocol**

The fixture implements only mkdir, 60/10-second stale checks, 3-second touch, and cleanup. It imports no third-party package and performs no network access.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test claude_lock_interop -- --nocapture
git add src-tauri/tests/claude_lock_interop.rs src-tauri/tests/fixtures/upstream_lock_holder.py
git commit -m "test: verify Claude lock interoperability across processes"
```

### Task 3: Durable staging and replacement

**Files:**
- Create: `src-tauri/src/durable_fs.rs`
- Modify: `src-tauri/src/credentials.rs:346-406`
- Modify: `src-tauri/src/switcher.rs:258-315`
- Modify: `src-tauri/src/lib.rs`, `Cargo.toml` windows features if compiler requires them
- Test: inline `durable_fs::tests`

**Interfaces:**
- Produces: `FileState`, `StagedFile`, `snapshot`, `stage_sibling`, `restore`, `sync_parent`.

- [ ] **Step 1: Write failing durable file tests**

Cover non-mutating sibling stage, replace/create, absence restoration, failed stage/replace, POSIX parent sync, Windows open-destination failure, and permissions/ACL preservation.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml durable_fs::tests -- --nocapture
```

- [ ] **Step 3: Implement platform replacement**

```rust
pub struct FileState { pub existed: bool, pub bytes: Vec<u8> }
pub fn snapshot(path: &Path) -> io::Result<FileState>;
pub fn stage_sibling(target: &Path, bytes: &[u8], unix_mode: Option<u32>) -> Result<StagedFile, DurableFsError>;
pub fn restore(path: &Path, state: &FileState, unix_mode: Option<u32>) -> Result<(), DurableFsError>;
```

Unix: sync temp, rename, sync parent directory. Windows: existing destination via `ReplaceFileW`, absent via `MoveFileExW(MOVEFILE_WRITE_THROUGH)`. Retain a failed stage for recovery.

- [ ] **Step 4: Replace duplicate atomic writers and verify**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml durable_fs::tests
cargo test --manifest-path src-tauri/Cargo.toml credentials::tests
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests
```

- [ ] **Step 5: Commit**

```powershell
git add src-tauri/src/durable_fs.rs src-tauri/src/credentials.rs src-tauri/src/switcher.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "refactor: centralize durable file replacement"
```

### Task 4: Protected recovery artifacts

**Files:**
- Create: `src-tauri/src/recovery_store.rs`
- Modify: `src-tauri/src/credentials.rs`, `src-tauri/src/lib.rs`
- Test: inline recovery/credential tests

**Interfaces:**
- Consumes: existing DPAPI/Keychain/envelope protection.
- Produces: `RecoveryStore`, `ProtectedArtifactRef`, protected byte helpers.

- [ ] **Step 1: Write failing protection tests**

Cover no secret in metadata, arbitrary bytes, Unix 0600, Windows DPAPI, macOS Keychain, honest 0600 fallback, transaction cleanup, and cleanup failure retention.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml recovery_store::tests -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml credentials::tests -- --nocapture
```

- [ ] **Step 3: Implement exact API**

```rust
#[serde(tag = "backend", rename_all = "camelCase")]
pub enum ProtectedArtifactRef { File { relative_path: PathBuf }, Keychain { account: String } }
impl RecoveryStore {
    pub fn put(&mut self, transaction_id: &str, name: &str, bytes: &[u8]) -> Result<ProtectedArtifactRef, RecoveryStoreError>;
    pub fn get(&mut self, reference: &ProtectedArtifactRef) -> Result<Vec<u8>, RecoveryStoreError>;
    pub fn remove_transaction(&mut self, transaction_id: &str) -> Result<(), RecoveryStoreError>;
}
```

Recovery persistence failures propagate. A file fallback records its actual `Plain` backend; it never claims encryption.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml recovery_store::tests -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml credentials::tests -- --nocapture
git add src-tauri/src/recovery_store.rs src-tauri/src/credentials.rs src-tauri/src/lib.rs
git commit -m "feat: add protected switch recovery storage"
```

### Task 5: Secret-free durable journal

**Files:**
- Create: `src-tauri/src/switch_journal.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: inline `switch_journal::tests`

**Interfaces:**
- Consumes: `durable_fs`, `ProtectedArtifactRef`.
- Produces: durable journal load/prepare/advance/remove.

- [ ] **Step 1: Write failing journal tests**

Cover secret-free round trip, unknown schema, forward-only phases, failed replace invisibility, before-image tamper hash, cleanup retention, and malformed fail-closed behavior.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_journal::tests -- --nocapture
```

- [ ] **Step 3: Implement phases and records**

```rust
pub enum JournalPhase { Prepared, ActiveCredentialInstalled, GlobalConfigInstalled, SequenceInstalled, Committed }
pub struct ArtifactRecord {
    pub before: Option<ProtectedArtifactRef>,
    pub staged_relative_path: Option<PathBuf>,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub existed_before: bool,
}
```

`advance` accepts only the next forward phase and durably replaces/syncs the journal. `OutgoingGeneration` references protected credential/config artifacts and hashes, never plaintext.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_journal::tests -- --nocapture
git add src-tauri/src/switch_journal.rs src-tauri/src/lib.rs
git commit -m "feat: add durable switch journal"
```

### Task 6: Exact active-backend snapshot and restore

**Files:**
- Modify: `src-tauri/src/credentials.rs:945-1695`
- Test: inline `credentials::tests`

**Interfaces:**
- Produces: `EntryState`, `ActiveCredentialState`, snapshot/restore/verify.

- [ ] **Step 1: Write failing backend-axis tests**

Cover absent vs empty file, removal on restore, unknown Keychain presence failure, OAuth/managed Keychain exact restore/removal, stale shadow detection, and OAuth↔API-key round trips.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml credentials::tests::snapshot_ -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml credentials::tests::restore_ -- --nocapture
```

- [ ] **Step 3: Implement exact state API**

```rust
pub(crate) enum EntryState<T> { Absent, Present(T) }
pub(crate) struct ActiveCredentialState {
    pub credentials_file: EntryState<Vec<u8>>,
    pub oauth_keychain: EntryState<String>,
    pub managed_keychain: EntryState<String>,
}
```

Fail closed when Keychain presence cannot be determined. Do not reuse normalized `read_active_credentials` for exact snapshots.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml credentials::tests -- --nocapture
git add src-tauri/src/credentials.rs
git commit -m "feat: snapshot and restore exact Claude credential backends"
```

### Task 7: Full live-state lock coordinator

**Files:**
- Create: `src-tauri/src/switch_transaction.rs`
- Modify: `src-tauri/src/switcher.rs:632-701`, `locking.rs`, `lib.rs`
- Test: inline transaction/switcher tests

**Interfaces:**
- Consumes: Task 1 locks and existing `FileLock`.
- Produces: `LiveStateLocks`, `acquire_live_state_locks`.

- [ ] **Step 1: Write failing order/unwind tests**

Cover every contention point releasing earlier locks, absent cswap store not created, all locks held at writes, simulated Claude refresh exclusion, and config read-modify-write preserving unrelated keys.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::locks -- --nocapture
```

- [ ] **Step 3: Implement one acquisition helper**

```rust
pub struct LiveStateLocks {
    cswap: Option<crate::locking::FileLock>,
    claude_credentials: crate::claude_locks::ClaudeCredentialLocks,
    claude_config: crate::claude_locks::DirectoryLock,
    vault: crate::locking::FileLock,
}
```

Acquire only in the global order. Delete the old two-lock helper after all callers use this coordinator.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::locks switcher::tests -- --nocapture
git add src-tauri/src/switch_transaction.rs src-tauri/src/switcher.rs src-tauri/src/locking.rs src-tauri/src/lib.rs
git commit -m "feat: coordinate every live-state lock in one order"
```

### Task 8: Transactional switch and ordinary rollback

**Files:**
- Modify: `src-tauri/src/switch_transaction.rs`
- Modify: `src-tauri/src/switcher.rs:725-852`
- Test: inline table-driven fault tests

**Interfaces:**
- Consumes: `ValidatedCredential.generation` from OAuth plan; journal, recovery store, exact credential state.
- Produces: `switch`, `switch_with`, `FaultInjector`.

- [ ] **Step 1: Define fault points and failing tests**

Cover target validation/generation mismatch before mutation, failures after credential/config/sequence, rollback continuation, retained journal, switch blocking, monotonic outgoing generation pair, config splice, mixed-identity verification, and cleanup.

```rust
pub trait FaultInjector: Send + Sync { fn hit(&self, point: FaultPoint) -> Result<(), InjectedFault>; }
pub struct NoFaults;
```

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::transaction -- --nocapture
```

- [ ] **Step 3: Implement prepare-to-commit order**

Under all locks: recover/refuse prior journal, re-read generation, validate target, snapshot exact live state, store protected before-images, write `Prepared`, stage files, capture outgoing pair, install credential/config/sequence, mark `Committed`, verify, cleanup.

- [ ] **Step 4: Implement reverse rollback**

Restore sequence, config, and active credential, continue after individual errors, verify exact state, retain journal on any failure, and preserve the newly captured outgoing generation.

- [ ] **Step 5: Drive every precommit fault and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::transaction -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml switcher::tests
cargo test --manifest-path src-tauri/Cargo.toml commands::tests
cargo test --manifest-path src-tauri/Cargo.toml poller::tests
git add src-tauri/src/switch_transaction.rs src-tauri/src/switcher.rs
git commit -m "feat: make account switching recoverable"
```

### Task 9: Idempotent startup recovery and recovery status

**Files:**
- Modify: `src-tauri/src/switch_transaction.rs`, `lib.rs:247-280`, `commands.rs`, `poller.rs`
- Test: inline recovery tests

**Interfaces:**
- Produces: `recover_pending_switch`, `RecoveryDisposition`, `recovery_requirement`.

- [ ] **Step 1: Write failing phase recovery tests**

Cover every noncommitted phase rollback, committed verify/cleanup, run twice, recovery failure retention/required state, blocked manual/automatic switching, sequence alone not proving commit, and tampered before-image failure.

- [ ] **Step 2: Verify red**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::recovery -- --nocapture
```

- [ ] **Step 3: Implement startup ordering**

Call recovery immediately after setting the store root and before `AppState::new`/poller spawn. Keep the UI open on failure but publish `DaemonPhase::RecoveryRequired`; every switch entry returns `TransactionError::RecoveryRequired` before mutation.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::recovery -- --nocapture
git add src-tauri/src/switch_transaction.rs src-tauri/src/lib.rs src-tauri/src/commands.rs src-tauri/src/poller.rs
git commit -m "feat: recover interrupted switches at startup"
```

### Task 10: Real process-death recovery matrix

**Files:**
- Modify: `src-tauri/src/switch_transaction.rs` tests
- Modify: `src-tauri/src/test_support.rs`

**Interfaces:**
- Consumes: fault points and restart recovery.
- Produces: child-process abort evidence.

- [ ] **Step 1: Write failing parent/child tests**

Cover abort after prepared, credential, config, sequence, commit, and a second abort during recovery. Use `std::process::abort`, not panic, because release unwinds panics.

- [ ] **Step 2: Verify red serially**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::crash_restart -- --nocapture --test-threads=1
```

- [ ] **Step 3: Implement hidden child mode**

Spawn `std::env::current_exe()` with an environment-scoped isolated transaction root, write a readiness checkpoint, abort at the selected fault, then run recovery in the parent against the same root.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml switch_transaction::tests::crash_restart -- --nocapture --test-threads=1
git add src-tauri/src/switch_transaction.rs src-tauri/src/test_support.rs
git commit -m "test: cover switch recovery after process termination"
```

### Task 11: Documentation and cross-platform gate

**Files:**
- Modify: `RESILIENCE.md`, `PLAN.md`, `CHANGELOG.md`
- Create: `docs/TRANSACTION_RECOVERY.md`

**Interfaces:**
- Consumes: verified behavior only.
- Produces: accurate operational/recovery documentation.

- [ ] **Step 1: Correct resilience claims**

Document per-file atomic visibility vs cross-file recovery, five-lock order, pinned Claude protocol, journal layout, outgoing-generation exception, local filesystem boundary, and recovery-required diagnostics.

- [ ] **Step 2: Run all automated gates**

```powershell
pnpm test
pnpm build
Push-Location src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --test claude_lock_interop -- --nocapture
Pop-Location
```

- [ ] **Step 3: Run packaged platform smoke matrix**

On Windows/NTFS, macOS/APFS, and Linux/ext4 test normal switch, held Claude locks, termination after each live write, restart recovery, OAuth/API-key transitions, Windows open destination, and macOS Keychain restoration.

- [ ] **Step 4: Commit verified documentation**

```powershell
git add RESILIENCE.md PLAN.md CHANGELOG.md docs/TRANSACTION_RECOVERY.md
git commit -m "docs: document transactional switching guarantees"
```
