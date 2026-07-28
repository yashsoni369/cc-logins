//! Cross-process coordination for mutations of Claude Code's live state.
//!
//! The first four locks mirror current `cswap` exactly: its account-store
//! file lock, Claude Code's primary and legacy credential directory locks,
//! and Claude Code's global-config directory lock. This GUI owns a separate
//! vault, so its private vault lock is acquired last. Keeping the guards in
//! one value makes both acquisition order and lifetime structural.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::credentials::{
    merge_shared_credential_fields, shared_credential_fields, ActiveCredentialState,
    CredentialStore, StoreHost,
};
use crate::switch_journal::{
    sha256, ArtifactRecord, JournalPhase, JournalStore, JournalTarget, OutgoingGeneration,
    SwitchJournal,
};

#[derive(Debug, thiserror::Error)]
pub enum LiveStateLockError {
    #[error(transparent)]
    CswapOrVault(#[from] crate::locking::LockingError),
    #[error(transparent)]
    Claude(#[from] crate::claude_locks::ClaudeLockError),
}

/// Every lock required while mutating the live credential, global config,
/// and this GUI's switch sequence. Fields intentionally remain private so a
/// caller cannot release only part of the lock set before a live write.
pub struct LiveStateLocks {
    _cswap: Option<crate::locking::FileLock>,
    _claude_credentials: crate::claude_locks::ClaudeCredentialLocks,
    _claude_config: crate::claude_locks::DirectoryLock,
    _vault: crate::locking::FileLock,
}

#[derive(Debug, Clone)]
pub(crate) enum OutgoingDestination {
    Managed {
        number: String,
        email: String,
        config_backup_path: std::path::PathBuf,
    },
    Unclaimed,
}

#[derive(Debug, Clone)]
pub(crate) struct SwitchPlan {
    pub target: JournalTarget,
    pub target_credentials: String,
    pub target_oauth: Value,
    pub sequence: Map<String, Value>,
    pub sequence_path: std::path::PathBuf,
    pub global_config_path: std::path::PathBuf,
    pub outgoing: OutgoingDestination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    BeforeJournalWrite,
    AfterJournalSync,
    AfterStageConfig,
    AfterStageSequence,
    AfterOutgoingCapture,
    AfterActiveCredentialInstall,
    AfterCredentialPhaseSync,
    AfterGlobalConfigInstall,
    AfterConfigPhaseSync,
    AfterSequenceInstall,
    AfterSequencePhaseSync,
    AfterCommitSync,
    BeforeVerification,
    BeforeCleanup,
}

pub trait FaultInjector: Send + Sync {
    fn hit(&self, point: FaultPoint) -> Result<(), InjectedFault>;
}

pub struct NoFaults;

impl FaultInjector for NoFaults {
    fn hit(&self, _point: FaultPoint) -> Result<(), InjectedFault> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("injected switch fault at {0:?}")]
pub struct InjectedFault(FaultPoint);

#[derive(Debug, thiserror::Error)]
pub enum TransactionError {
    #[error(transparent)]
    Credential(#[from] crate::credentials::CredentialError),
    #[error(transparent)]
    Journal(#[from] crate::switch_journal::JournalError),
    #[error(transparent)]
    RecoveryStore(#[from] crate::recovery_store::RecoveryStoreError),
    #[error(transparent)]
    Durable(#[from] crate::durable_fs::DurableFsError),
    #[error("switch transaction I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("switch transaction JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not read the active credential")]
    CredentialRead,
    #[error("active credential is empty; refusing to overwrite its backup")]
    EmptyActiveCredential,
    #[error("another switch transaction requires recovery")]
    RecoveryRequired,
    #[error(transparent)]
    Injected(#[from] InjectedFault),
    #[error("switch failed ({operation}); rollback was incomplete: {rollback}")]
    RollbackIncomplete { operation: String, rollback: String },
    #[error("post-switch verification failed: {0}")]
    Verification(String),
    #[error("target account number is not a positive integer: {0}")]
    InvalidTargetNumber(String),
}

fn transaction_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn file_artifact(
    recovery: &mut crate::recovery_store::RecoveryStore,
    transaction_id: &str,
    name: &str,
    state: &crate::durable_fs::FileState,
    after: &[u8],
) -> Result<ArtifactRecord, TransactionError> {
    let before = state
        .existed
        .then(|| recovery.put(transaction_id, name, &state.bytes))
        .transpose()?;
    Ok(ArtifactRecord {
        before,
        staged_relative_path: None,
        before_sha256: state.existed.then(|| sha256(&state.bytes)),
        after_sha256: Some(sha256(after)),
        existed_before: state.existed,
    })
}

fn active_artifact(
    recovery: &mut crate::recovery_store::RecoveryStore,
    transaction_id: &str,
    state: &ActiveCredentialState,
    after: &[u8],
) -> Result<ArtifactRecord, TransactionError> {
    let bytes = serde_json::to_vec(state)?;
    let before = recovery.put(transaction_id, "active-credential", &bytes)?;
    Ok(ArtifactRecord {
        before: Some(before),
        staged_relative_path: None,
        before_sha256: Some(sha256(&bytes)),
        after_sha256: Some(sha256(after)),
        existed_before: true,
    })
}

fn stage_name(stage: &crate::durable_fs::StagedFile) -> Option<std::path::PathBuf> {
    stage.path().file_name().map(std::path::PathBuf::from)
}

fn remove_stage(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => log::warn!("could not remove switch stage {}: {error}", path.display()),
    }
}

/// Execute a prepared, network-free switch while the caller holds
/// [`LiveStateLocks`]. Before-images are protected and journaled before any
/// live or vault mutation. Any pre-commit failure attempts a complete exact
/// rollback and retains the journal if that rollback cannot be verified.
pub(crate) fn execute_locked<H: StoreHost>(
    store: &mut CredentialStore<H>,
    mut plan: SwitchPlan,
    faults: &dyn FaultInjector,
) -> Result<(), TransactionError> {
    let root = crate::paths::backup_root();
    let journal_store = JournalStore::new(&root);
    if journal_store.load()?.is_some() {
        return Err(TransactionError::RecoveryRequired);
    }

    let active_before = store.snapshot_active_state()?;
    let active_value = store
        .read_active_credentials()
        .value
        .ok_or(TransactionError::CredentialRead)?;
    if active_value.is_empty() {
        return Err(TransactionError::EmptyActiveCredential);
    }
    let config_before = crate::durable_fs::snapshot(&plan.global_config_path)?;
    if !config_before.existed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Claude global config does not exist",
        )
        .into());
    }
    let sequence_before = crate::durable_fs::snapshot(&plan.sequence_path)?;

    let mut config: Map<String, Value> = if config_before.existed {
        match serde_json::from_slice::<Value>(&config_before.bytes)? {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    } else {
        Map::new()
    };
    config.insert("oauthAccount".to_string(), plan.target_oauth.clone());
    let config_after = serde_json::to_vec_pretty(&Value::Object(config))?;
    let target_number = plan
        .target
        .number
        .parse::<u64>()
        .map_err(|_| TransactionError::InvalidTargetNumber(plan.target.number.clone()))?;
    plan.sequence.insert(
        "activeAccountNumber".to_string(),
        Value::from(target_number),
    );
    plan.sequence.insert(
        "lastUpdated".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    let sequence_after = serde_json::to_vec_pretty(&Value::Object(plan.sequence.clone()))?;
    let shared = shared_credential_fields(Some(&active_value)).unwrap_or_default();
    let prepared_credentials = merge_shared_credential_fields(&plan.target_credentials, &shared);

    let transaction_id = transaction_id();
    let mut recovery = crate::recovery_store::RecoveryStore::new(&root);
    let journal_result = (|| -> Result<SwitchJournal, TransactionError> {
        Ok(SwitchJournal::prepared(
            transaction_id.clone(),
            plan.target.clone(),
            active_artifact(
                &mut recovery,
                &transaction_id,
                &active_before,
                prepared_credentials.as_bytes(),
            )?,
            file_artifact(
                &mut recovery,
                &transaction_id,
                "global-config",
                &config_before,
                &config_after,
            )?,
            file_artifact(
                &mut recovery,
                &transaction_id,
                "sequence",
                &sequence_before,
                &sequence_after,
            )?,
        ))
    })();
    let mut journal = match journal_result {
        Ok(journal) => journal,
        Err(error) => {
            let _ = recovery.remove_transaction(&transaction_id);
            return Err(error);
        }
    };
    if let Err(error) = faults.hit(FaultPoint::BeforeJournalWrite) {
        let _ = recovery.remove_transaction(&transaction_id);
        return Err(error.into());
    }
    if let Err(error) = journal_store.prepare(&journal) {
        let _ = recovery.remove_transaction(&transaction_id);
        return Err(error.into());
    }
    let operation = (|| -> Result<(), TransactionError> {
        faults.hit(FaultPoint::AfterJournalSync)?;
        let config_stage =
            crate::durable_fs::stage_sibling(&plan.global_config_path, &config_after, Some(0o600))?;
        journal.global_config.staged_relative_path = stage_name(&config_stage);
        journal_store.rewrite_current(&journal)?;
        faults.hit(FaultPoint::AfterStageConfig)?;
        let sequence_stage =
            crate::durable_fs::stage_sibling(&plan.sequence_path, &sequence_after, Some(0o600))?;
        journal.sequence.staged_relative_path = stage_name(&sequence_stage);
        journal_store.rewrite_current(&journal)?;
        faults.hit(FaultPoint::AfterStageSequence)?;

        let outgoing_credential = recovery.put(
            &transaction_id,
            "outgoing-credential",
            active_value.as_bytes(),
        )?;
        let outgoing_config = config_before
            .existed
            .then(|| recovery.put(&transaction_id, "outgoing-config", &config_before.bytes))
            .transpose()?;
        let outgoing_number = match &plan.outgoing {
            OutgoingDestination::Managed { number, .. } => Some(number.clone()),
            OutgoingDestination::Unclaimed => None,
        };
        journal.outgoing_generation = Some(OutgoingGeneration {
            account_number: outgoing_number,
            credential: Some(outgoing_credential),
            config: outgoing_config,
            credential_sha256: Some(sha256(active_value.as_bytes())),
            config_sha256: config_before.existed.then(|| sha256(&config_before.bytes)),
        });
        journal_store.rewrite_current(&journal)?;
        faults.hit(FaultPoint::AfterOutgoingCapture)?;

        match &plan.outgoing {
            OutgoingDestination::Managed {
                number,
                email,
                config_backup_path,
            } => {
                store.write_account_credentials(number, email, &active_value)?;
                crate::durable_fs::stage_sibling(
                    config_backup_path,
                    &config_before.bytes,
                    Some(0o600),
                )?
                .commit()?;
            }
            OutgoingDestination::Unclaimed => {
                let mut context = Map::new();
                context.insert(
                    "reason".to_string(),
                    Value::String("displaced-live-login".to_string()),
                );
                store.write_unclaimed_credential(&active_value, context)?;
            }
        }

        store.write_credentials(&prepared_credentials)?;
        faults.hit(FaultPoint::AfterActiveCredentialInstall)?;
        journal_store.advance(&mut journal, JournalPhase::ActiveCredentialInstalled)?;
        faults.hit(FaultPoint::AfterCredentialPhaseSync)?;

        let config_stage_path = config_stage.path().to_path_buf();
        config_stage.commit()?;
        faults.hit(FaultPoint::AfterGlobalConfigInstall)?;
        journal_store.advance(&mut journal, JournalPhase::GlobalConfigInstalled)?;
        faults.hit(FaultPoint::AfterConfigPhaseSync)?;

        let sequence_stage_path = sequence_stage.path().to_path_buf();
        sequence_stage.commit()?;
        faults.hit(FaultPoint::AfterSequenceInstall)?;
        journal_store.advance(&mut journal, JournalPhase::SequenceInstalled)?;
        faults.hit(FaultPoint::AfterSequencePhaseSync)?;
        journal_store.advance(&mut journal, JournalPhase::Committed)?;
        faults.hit(FaultPoint::AfterCommitSync)?;
        faults.hit(FaultPoint::BeforeVerification)?;

        let active_after = store
            .read_active_credentials()
            .value
            .ok_or(TransactionError::CredentialRead)?;
        if active_after != prepared_credentials
            || sha256(&std::fs::read(&plan.global_config_path)?)
                != journal
                    .global_config
                    .after_sha256
                    .clone()
                    .unwrap_or_default()
            || sha256(&std::fs::read(&plan.sequence_path)?)
                != journal.sequence.after_sha256.clone().unwrap_or_default()
        {
            return Err(TransactionError::Verification(
                "credential, config, or sequence did not match the committed target".to_string(),
            ));
        }
        faults.hit(FaultPoint::BeforeCleanup)?;
        remove_stage(&config_stage_path);
        remove_stage(&sequence_stage_path);
        recovery.remove_transaction(&transaction_id)?;
        journal_store.remove(&transaction_id)?;
        Ok(())
    })();

    if operation.is_ok() || journal.phase == JournalPhase::Committed {
        return operation;
    }

    let mut rollback_errors = Vec::new();
    if let Some(name) = &journal.global_config.staged_relative_path {
        remove_stage(
            &plan
                .global_config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(name),
        );
    }
    if let Some(name) = &journal.sequence.staged_relative_path {
        remove_stage(
            &plan
                .sequence_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(name),
        );
    }
    if let Err(error) =
        crate::durable_fs::restore(&plan.sequence_path, &sequence_before, Some(0o600))
    {
        rollback_errors.push(format!("sequence: {error}"));
    }
    if let Err(error) =
        crate::durable_fs::restore(&plan.global_config_path, &config_before, Some(0o600))
    {
        rollback_errors.push(format!("global config: {error}"));
    }
    if let Err(error) = store.restore_active_state(&active_before) {
        rollback_errors.push(format!("active credential: {error}"));
    }
    if store.verify_active_state(&active_before).is_err()
        || crate::durable_fs::snapshot(&plan.global_config_path)
            .ok()
            .as_ref()
            != Some(&config_before)
        || crate::durable_fs::snapshot(&plan.sequence_path)
            .ok()
            .as_ref()
            != Some(&sequence_before)
    {
        rollback_errors.push("restored state verification failed".to_string());
    }

    let operation_error = operation.unwrap_err();
    if rollback_errors.is_empty() {
        let _ = recovery.remove_transaction(&transaction_id);
        let _ = journal_store.remove(&transaction_id);
        Err(operation_error)
    } else {
        Err(TransactionError::RollbackIncomplete {
            operation: operation_error.to_string(),
            rollback: rollback_errors.join("; "),
        })
    }
}

/// Acquire the complete live-state lock set in the canonical order.
///
/// The cswap lock is optional and is never allowed to create a fake cswap
/// store. All work performed while this guard exists must be local I/O; the
/// caller must complete network refreshes before entering this boundary.
pub fn acquire_live_state_locks(timeout: Duration) -> Result<LiveStateLocks, LiveStateLockError> {
    let cswap_root = crate::paths::cswap_store_root();
    let cswap = if cswap_root.exists() {
        Some(crate::locking::acquire_or_err(
            cswap_root.join(".lock"),
            timeout,
        )?)
    } else {
        None
    };
    let claude_credentials = crate::claude_locks::acquire_credential_locks(timeout)?;
    let claude_config = crate::claude_locks::acquire_config_lock(timeout)?;
    let vault = crate::locking::acquire_or_err(crate::paths::backup_root().join(".lock"), timeout)?;

    Ok(LiveStateLocks {
        _cswap: cswap,
        _claude_credentials: claude_credentials,
        _claude_config: claude_config,
        _vault: vault,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{Platform, StoreHost};
    use crate::test_support::{env_lock, EnvGuard, StoreRootGuard};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct TestEnv {
        _home: EnvGuard,
        _userprofile: EnvGuard,
        _config: EnvGuard,
        _xdg: EnvGuard,
        _wsl: EnvGuard,
        _store: StoreRootGuard,
        _home_dir: TempDir,
        _config_dir: TempDir,
        vault: TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn setup() -> TestEnv {
        let lock = env_lock();
        let home = TempDir::new().unwrap();
        let config = TempDir::new().unwrap();
        let vault = TempDir::new().unwrap();
        let home_text = home.path().to_string_lossy().into_owned();
        let config_text = config.path().to_string_lossy().into_owned();
        let home_guard = EnvGuard::set("HOME", &home_text);
        let userprofile = EnvGuard::set("USERPROFILE", &home_text);
        let config_guard = EnvGuard::set("CLAUDE_CONFIG_DIR", &config_text);
        let xdg = EnvGuard::unset("XDG_DATA_HOME");
        let wsl = EnvGuard::unset("WSL_DISTRO_NAME");
        let store = StoreRootGuard::set(vault.path().to_path_buf());
        TestEnv {
            _home: home_guard,
            _userprofile: userprofile,
            _config: config_guard,
            _xdg: xdg,
            _wsl: wsl,
            _store: store,
            _home_dir: home,
            _config_dir: config,
            vault,
            _lock: lock,
        }
    }

    #[derive(Clone)]
    struct TestHost {
        credentials: PathBuf,
    }

    impl StoreHost for TestHost {
        fn platform(&self) -> Platform {
            Platform::Linux
        }

        fn credentials_dir(&self) -> PathBuf {
            self.credentials.clone()
        }

        fn keychain_service(&self) -> &str {
            crate::credentials::GUI_SECURITY_SERVICE
        }
    }

    struct SingleFault(FaultPoint);

    impl FaultInjector for SingleFault {
        fn hit(&self, point: FaultPoint) -> Result<(), InjectedFault> {
            if self.0 == point {
                Err(InjectedFault(point))
            } else {
                Ok(())
            }
        }
    }

    fn transaction_fixture(
        env: &TestEnv,
    ) -> (
        CredentialStore<TestHost>,
        SwitchPlan,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
    ) {
        fs::create_dir_all(crate::paths::claude_config_home()).unwrap();
        let active =
            br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old-refresh"}}"#.to_vec();
        fs::write(crate::paths::credentials_path(), &active).unwrap();
        let config = serde_json::to_vec_pretty(&json!({
            "oauthAccount": {"emailAddress": "old@example.com"},
            "unrelated": {"preserve": true}
        }))
        .unwrap();
        fs::write(crate::paths::global_config_path(), &config).unwrap();
        let sequence_value = json!({
            "activeAccountNumber": 1,
            "accounts": {
                "1": {"email": "old@example.com"},
                "2": {"email": "target@example.com"}
            }
        });
        let sequence = serde_json::to_vec_pretty(&sequence_value).unwrap();
        let sequence_path = env.vault.path().join("sequence.json");
        fs::write(&sequence_path, &sequence).unwrap();
        let Value::Object(sequence_map) = sequence_value else {
            unreachable!()
        };
        let store = CredentialStore::new(TestHost {
            credentials: env.vault.path().join("credentials"),
        });
        let plan = SwitchPlan {
            target: JournalTarget {
                number: "2".to_string(),
                email: "target@example.com".to_string(),
                stable_key: "email:target@example.com".to_string(),
                credential_generation: "sha256-full:target".to_string(),
            },
            target_credentials:
                r#"{"claudeAiOauth":{"accessToken":"target","refreshToken":"target-refresh"}}"#
                    .to_string(),
            target_oauth: json!({"emailAddress": "target@example.com"}),
            sequence: sequence_map,
            sequence_path,
            global_config_path: crate::paths::global_config_path(),
            outgoing: OutgoingDestination::Managed {
                number: "1".to_string(),
                email: "old@example.com".to_string(),
                config_backup_path: env.vault.path().join("configs/old.json"),
            },
        };
        (store, plan, active, config, sequence)
    }

    #[test]
    fn transaction_every_precommit_fault_restores_exact_live_state() {
        let points = [
            FaultPoint::AfterJournalSync,
            FaultPoint::AfterStageConfig,
            FaultPoint::AfterStageSequence,
            FaultPoint::AfterOutgoingCapture,
            FaultPoint::AfterActiveCredentialInstall,
            FaultPoint::AfterCredentialPhaseSync,
            FaultPoint::AfterGlobalConfigInstall,
            FaultPoint::AfterConfigPhaseSync,
            FaultPoint::AfterSequenceInstall,
            FaultPoint::AfterSequencePhaseSync,
        ];
        for point in points {
            let env = setup();
            let (mut store, plan, active, config, sequence) = transaction_fixture(&env);
            assert!(execute_locked(&mut store, plan.clone(), &SingleFault(point)).is_err());
            assert_eq!(
                fs::read(crate::paths::credentials_path()).unwrap(),
                active,
                "{point:?}"
            );
            assert_eq!(
                fs::read(crate::paths::global_config_path()).unwrap(),
                config,
                "{point:?}"
            );
            assert_eq!(
                fs::read(&plan.sequence_path).unwrap(),
                sequence,
                "{point:?}"
            );
            assert!(
                !JournalStore::new(env.vault.path()).path().exists(),
                "{point:?}"
            );
            assert!(fs::read_dir(crate::paths::claude_config_home())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".stage")));
            drop(env);
        }
    }

    #[test]
    fn transaction_postcommit_fault_retains_committed_journal_and_target_state() {
        for point in [
            FaultPoint::AfterCommitSync,
            FaultPoint::BeforeVerification,
            FaultPoint::BeforeCleanup,
        ] {
            let env = setup();
            let (mut store, plan, active, _, _) = transaction_fixture(&env);
            assert!(execute_locked(&mut store, plan.clone(), &SingleFault(point)).is_err());
            assert_ne!(
                fs::read(crate::paths::credentials_path()).unwrap(),
                active,
                "{point:?}"
            );
            let journal = JournalStore::new(env.vault.path()).load().unwrap().unwrap();
            assert_eq!(journal.phase, JournalPhase::Committed, "{point:?}");
            drop(env);
        }
    }

    #[test]
    fn transaction_success_verifies_and_removes_recovery_artifacts() {
        let env = setup();
        let (mut store, plan, active, _, _) = transaction_fixture(&env);
        execute_locked(&mut store, plan.clone(), &NoFaults).unwrap();
        assert_ne!(fs::read(crate::paths::credentials_path()).unwrap(), active);
        assert!(!JournalStore::new(env.vault.path()).path().exists());
        let recovery_root = env.vault.path().join("switch-recovery");
        assert!(!recovery_root.exists() || fs::read_dir(recovery_root).unwrap().next().is_none());
        let sequence: Value =
            serde_json::from_slice(&fs::read(plan.sequence_path).unwrap()).unwrap();
        assert_eq!(sequence["activeAccountNumber"], 2);
        let config: Value =
            serde_json::from_slice(&fs::read(crate::paths::global_config_path()).unwrap()).unwrap();
        assert_eq!(config["unrelated"]["preserve"], true);
        assert_eq!(config["oauthAccount"]["emailAddress"], "target@example.com");
        let mut outgoing_store = CredentialStore::new(TestHost {
            credentials: env.vault.path().join("credentials"),
        });
        assert_eq!(
            outgoing_store.read_account_credentials("1", "old@example.com"),
            String::from_utf8(active).unwrap()
        );
        assert_eq!(
            fs::read(env.vault.path().join("configs/old.json")).unwrap(),
            serde_json::to_vec_pretty(&json!({
                "oauthAccount": {"emailAddress": "old@example.com"},
                "unrelated": {"preserve": true}
            }))
            .unwrap()
        );
    }

    #[test]
    fn transaction_fault_before_journal_leaves_no_recovery_artifacts() {
        let env = setup();
        let (mut store, plan, active, config, sequence) = transaction_fixture(&env);
        assert!(execute_locked(
            &mut store,
            plan.clone(),
            &SingleFault(FaultPoint::BeforeJournalWrite),
        )
        .is_err());
        assert_eq!(fs::read(crate::paths::credentials_path()).unwrap(), active);
        assert_eq!(
            fs::read(crate::paths::global_config_path()).unwrap(),
            config
        );
        assert_eq!(fs::read(plan.sequence_path).unwrap(), sequence);
        assert!(!JournalStore::new(env.vault.path()).path().exists());
        let recovery_root = env.vault.path().join("switch-recovery");
        assert!(!recovery_root.exists() || fs::read_dir(recovery_root).unwrap().next().is_none());
    }

    #[test]
    fn locks_missing_cswap_store_without_creating_it() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        assert!(!cswap.exists());
        let locks = acquire_live_state_locks(Duration::from_millis(100)).unwrap();
        assert!(!cswap.exists());
        assert!(crate::paths::oauth_refresh_lock_dir().is_dir());
        assert!(crate::paths::credentials_lock_dir().is_dir());
        assert!(crate::paths::global_config_lock_dir().is_dir());
        assert!(env.vault.path().join(".lock").exists());
        drop(locks);
    }

    #[test]
    fn locks_cswap_contention_touches_no_later_lock() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        let _held =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_secs(1)).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        assert!(!env.vault.path().join(".lock").exists());
    }

    #[test]
    fn locks_primary_contention_releases_cswap_and_touches_no_later_lock() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        fs::create_dir(crate::paths::oauth_refresh_lock_dir()).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        assert!(!env.vault.path().join(".lock").exists());
    }

    #[test]
    fn locks_legacy_contention_releases_primary_and_cswap() {
        let _env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        fs::create_dir(crate::paths::credentials_lock_dir()).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
    }

    #[test]
    fn locks_config_contention_releases_credentials_and_cswap() {
        let _env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        fs::create_dir(crate::paths::global_config_lock_dir()).unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
    }

    #[test]
    fn locks_vault_contention_releases_every_external_lock() {
        let env = setup();
        let cswap = crate::paths::cswap_store_root();
        fs::create_dir_all(&cswap).unwrap();
        let _held =
            crate::locking::acquire_or_err(env.vault.path().join(".lock"), Duration::from_secs(1))
                .unwrap();
        assert!(acquire_live_state_locks(Duration::from_millis(30)).is_err());
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        let reacquired =
            crate::locking::acquire_or_err(cswap.join(".lock"), Duration::from_millis(100))
                .unwrap();
        drop(reacquired);
    }
}
