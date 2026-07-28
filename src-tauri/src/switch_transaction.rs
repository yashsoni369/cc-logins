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

/// The narrower lock set used only while reconciling an active OAuth
/// generation. The bounded refresh POST is allowed while this guard exists;
/// callers must never acquire the omitted config lock before dropping it.
pub struct ActiveRefreshLocks {
    _cswap: Option<crate::locking::FileLock>,
    _claude_credentials: crate::claude_locks::ClaudeCredentialLocks,
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
    BeforeRollbackSequence,
    BeforeRollbackConfig,
    BeforeRollbackCredential,
    BeforeRollbackVerification,
    BeforeRecoveryCleanup,
    AfterRecoveryJournalRemoval,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDisposition {
    NothingToRecover,
    RolledBack { transaction_id: String },
    VerifiedCommitted { transaction_id: String },
}

#[cfg(not(test))]
fn recovery_state() -> &'static std::sync::Mutex<Option<String>> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
thread_local! {
    static TEST_RECOVERY_STATE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(not(test))]
fn set_recovery_requirement(detail: Option<String>) {
    *recovery_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = detail;
}

#[cfg(test)]
fn set_recovery_requirement(detail: Option<String>) {
    TEST_RECOVERY_STATE.with(|state| *state.borrow_mut() = detail);
}

#[cfg(not(test))]
pub fn recovery_requirement() -> Option<String> {
    recovery_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
pub fn recovery_requirement() -> Option<String> {
    TEST_RECOVERY_STATE.with(|state| state.borrow().clone())
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
        set_recovery_requirement(Some(
            "an interrupted account switch must be recovered before switching again".to_string(),
        ));
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
        let (outgoing_number, outgoing_email) = match &plan.outgoing {
            OutgoingDestination::Managed { number, email, .. } => {
                (Some(number.clone()), Some(email.clone()))
            }
            OutgoingDestination::Unclaimed => (None, None),
        };
        journal.outgoing_generation = Some(OutgoingGeneration {
            account_number: outgoing_number,
            email: outgoing_email,
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
                store.write_unclaimed_credential_named(
                    &format!("recovery-{transaction_id}"),
                    &active_value,
                    context,
                )?;
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
        journal_store.remove(&transaction_id)?;
        if let Err(error) = recovery.remove_transaction(&transaction_id) {
            log::warn!(
                "switch {transaction_id} committed, but orphaned protected recovery artifacts could not be removed: {error}"
            );
        }
        Ok(())
    })();

    if journal.phase == JournalPhase::Committed {
        if let Err(error) = &operation {
            set_recovery_requirement(Some(error.to_string()));
        }
        return operation;
    }
    if operation.is_ok() {
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
        // The journal is the recovery authority. Remove and sync it before
        // deleting dispensable before-images: a crash in the opposite order
        // leaves a live journal whose references can no longer be restored.
        if journal_store.remove(&transaction_id).is_ok() {
            let _ = recovery.remove_transaction(&transaction_id);
        }
        Err(operation_error)
    } else {
        let error = TransactionError::RollbackIncomplete {
            operation: operation_error.to_string(),
            rollback: rollback_errors.join("; "),
        };
        set_recovery_requirement(Some(error.to_string()));
        Err(error)
    }
}

fn recovered_file_state(
    record: &ArtifactRecord,
    recovery: &mut crate::recovery_store::RecoveryStore,
) -> Result<crate::durable_fs::FileState, TransactionError> {
    Ok(crate::durable_fs::FileState {
        existed: record.existed_before,
        bytes: record.load_verified_before(recovery)?.unwrap_or_default(),
    })
}

fn verify_hash(bytes: &[u8], expected: Option<&str>, label: &str) -> Result<(), TransactionError> {
    if expected.is_some_and(|expected| sha256(bytes) == expected) {
        Ok(())
    } else {
        Err(TransactionError::Verification(format!(
            "{label} recovery bytes failed their integrity check"
        )))
    }
}

fn preserve_outgoing_generation(
    store: &mut CredentialStore<crate::switcher::GuiStoreHost>,
    journal: &SwitchJournal,
    recovery: &mut crate::recovery_store::RecoveryStore,
) -> Result<(), TransactionError> {
    let outgoing = journal.outgoing_generation.as_ref().ok_or_else(|| {
        TransactionError::Verification("journal has no outgoing generation".to_string())
    })?;
    let credential_ref = outgoing.credential.as_ref().ok_or_else(|| {
        TransactionError::Verification("outgoing credential reference is missing".to_string())
    })?;
    let credential = recovery.get(credential_ref)?;
    verify_hash(
        &credential,
        outgoing.credential_sha256.as_deref(),
        "outgoing credential",
    )?;
    let credential_text = String::from_utf8(credential).map_err(|error| {
        TransactionError::Verification(format!("outgoing credential is not UTF-8: {error}"))
    })?;

    match (&outgoing.account_number, &outgoing.email) {
        (Some(number), Some(email)) => {
            let config_ref = outgoing.config.as_ref().ok_or_else(|| {
                TransactionError::Verification("outgoing config reference is missing".to_string())
            })?;
            let config = recovery.get(config_ref)?;
            verify_hash(
                &config,
                outgoing.config_sha256.as_deref(),
                "outgoing config",
            )?;
            store.write_account_credentials(number, email, &credential_text)?;
            let config_path = crate::paths::backup_root()
                .join("configs")
                .join(format!(".claude-config-{number}-{email}.json"));
            crate::durable_fs::stage_sibling(&config_path, &config, Some(0o600))?.commit()?;
            Ok(())
        }
        (None, None) => {
            let mut context = Map::new();
            context.insert(
                "reason".to_string(),
                Value::String("displaced-live-login".to_string()),
            );
            store.write_unclaimed_credential_named(
                &format!("recovery-{}", journal.transaction_id),
                &credential_text,
                context,
            )?;
            Ok(())
        }
        _ => Err(TransactionError::Verification(
            "outgoing slot identity is incomplete".to_string(),
        )),
    }
}

fn remove_journal_stages(journal: &SwitchJournal) {
    if let Some(name) = &journal.global_config.staged_relative_path {
        let target = crate::paths::global_config_path();
        remove_stage(
            &target
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(name),
        );
    }
    if let Some(name) = &journal.sequence.staged_relative_path {
        remove_stage(&crate::paths::backup_root().join(name));
    }
}

fn verify_committed(
    store: &mut CredentialStore<crate::switcher::GuiStoreHost>,
    journal: &SwitchJournal,
) -> Result<(), TransactionError> {
    let active = store
        .read_active_credentials()
        .value
        .ok_or(TransactionError::CredentialRead)?;
    verify_hash(
        active.as_bytes(),
        journal.active_credential.after_sha256.as_deref(),
        "committed active credential",
    )?;
    verify_hash(
        &std::fs::read(crate::paths::global_config_path())?,
        journal.global_config.after_sha256.as_deref(),
        "committed global config",
    )?;
    verify_hash(
        &std::fs::read(crate::paths::backup_root().join("sequence.json"))?,
        journal.sequence.after_sha256.as_deref(),
        "committed sequence",
    )
}

pub fn recover_pending_switch() -> Result<RecoveryDisposition, TransactionError> {
    recover_pending_switch_with(crate::claude_locks::DEFAULT_TIMEOUT)
}

pub(crate) fn recover_pending_switch_with(
    timeout: Duration,
) -> Result<RecoveryDisposition, TransactionError> {
    recover_pending_switch_with_faults(timeout, &NoFaults)
}

fn recover_pending_switch_with_faults(
    timeout: Duration,
    faults: &dyn FaultInjector,
) -> Result<RecoveryDisposition, TransactionError> {
    let result = recover_pending_switch_inner(timeout, faults);
    if let Err(error) = &result {
        set_recovery_requirement(Some(error.to_string()));
    }
    result
}

fn recover_pending_switch_inner(
    timeout: Duration,
    faults: &dyn FaultInjector,
) -> Result<RecoveryDisposition, TransactionError> {
    let root = crate::paths::backup_root();
    let journal_store = JournalStore::new(&root);
    if journal_store.load()?.is_none() {
        set_recovery_requirement(None);
        return Ok(RecoveryDisposition::NothingToRecover);
    }
    let _locks = acquire_live_state_locks(timeout).map_err(|error| {
        TransactionError::Verification(format!("could not acquire recovery locks: {error}"))
    })?;
    let Some(journal) = journal_store.load()? else {
        set_recovery_requirement(None);
        return Ok(RecoveryDisposition::NothingToRecover);
    };
    let transaction_id = journal.transaction_id.clone();
    let mut recovery = crate::recovery_store::RecoveryStore::new(&root);
    let mut store = CredentialStore::new(crate::switcher::GuiStoreHost);

    let result = if journal.phase == JournalPhase::Committed {
        verify_committed(&mut store, &journal)?;
        RecoveryDisposition::VerifiedCommitted {
            transaction_id: transaction_id.clone(),
        }
    } else {
        // Verify and deserialize every before-image before mutating anything.
        let active_bytes = journal
            .active_credential
            .load_verified_before(&mut recovery)?
            .ok_or_else(|| {
                TransactionError::Verification(
                    "active credential before-image is missing".to_string(),
                )
            })?;
        let active: ActiveCredentialState = serde_json::from_slice(&active_bytes)?;
        let config = recovered_file_state(&journal.global_config, &mut recovery)?;
        let sequence = recovered_file_state(&journal.sequence, &mut recovery)?;

        let mut errors = Vec::new();
        if journal.outgoing_generation.is_some() {
            if let Err(error) = preserve_outgoing_generation(&mut store, &journal, &mut recovery) {
                errors.push(format!("outgoing generation: {error}"));
            }
        }
        faults.hit(FaultPoint::BeforeRollbackSequence)?;
        if let Err(error) =
            crate::durable_fs::restore(&root.join("sequence.json"), &sequence, Some(0o600))
        {
            errors.push(format!("sequence: {error}"));
        }
        faults.hit(FaultPoint::BeforeRollbackConfig)?;
        if let Err(error) =
            crate::durable_fs::restore(&crate::paths::global_config_path(), &config, Some(0o600))
        {
            errors.push(format!("global config: {error}"));
        }
        faults.hit(FaultPoint::BeforeRollbackCredential)?;
        if let Err(error) = store.restore_active_state(&active) {
            errors.push(format!("active credential: {error}"));
        }
        faults.hit(FaultPoint::BeforeRollbackVerification)?;
        if store.verify_active_state(&active).is_err()
            || crate::durable_fs::snapshot(&crate::paths::global_config_path())
                .ok()
                .as_ref()
                != Some(&config)
            || crate::durable_fs::snapshot(&root.join("sequence.json"))
                .ok()
                .as_ref()
                != Some(&sequence)
        {
            errors.push("restored state verification failed".to_string());
        }
        if !errors.is_empty() {
            return Err(TransactionError::RollbackIncomplete {
                operation: "restart recovery".to_string(),
                rollback: errors.join("; "),
            });
        }
        RecoveryDisposition::RolledBack {
            transaction_id: transaction_id.clone(),
        }
    };

    faults.hit(FaultPoint::BeforeRecoveryCleanup)?;
    remove_journal_stages(&journal);
    journal_store.remove(&transaction_id)?;
    faults.hit(FaultPoint::AfterRecoveryJournalRemoval)?;
    if let Err(error) = recovery.remove_transaction(&transaction_id) {
        log::warn!(
            "switch {transaction_id} was recovered, but orphaned protected recovery artifacts could not be removed: {error}"
        );
    }
    set_recovery_requirement(None);
    Ok(result)
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

/// Acquire active-refresh locks in the canonical order, omitting the config
/// lock because the refresh path writes only OAuth credential storage.
pub fn acquire_active_refresh_locks(
    timeout: Duration,
) -> Result<ActiveRefreshLocks, LiveStateLockError> {
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
    let vault = crate::locking::acquire_or_err(crate::paths::backup_root().join(".lock"), timeout)?;

    Ok(ActiveRefreshLocks {
        _cswap: cswap,
        _claude_credentials: claude_credentials,
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

    struct PanicFault(FaultPoint);

    impl FaultInjector for PanicFault {
        fn hit(&self, point: FaultPoint) -> Result<(), InjectedFault> {
            if self.0 == point {
                panic!("simulated process termination at {point:?}");
            }
            Ok(())
        }
    }

    struct AbortFault(FaultPoint);

    impl FaultInjector for AbortFault {
        fn hit(&self, point: FaultPoint) -> Result<(), InjectedFault> {
            if self.0 == point {
                std::process::abort();
            }
            Ok(())
        }
    }

    fn fault_from_name(name: &str) -> FaultPoint {
        match name {
            "AfterJournalSync" => FaultPoint::AfterJournalSync,
            "AfterActiveCredentialInstall" => FaultPoint::AfterActiveCredentialInstall,
            "AfterGlobalConfigInstall" => FaultPoint::AfterGlobalConfigInstall,
            "AfterSequenceInstall" => FaultPoint::AfterSequenceInstall,
            "AfterCommitSync" => FaultPoint::AfterCommitSync,
            "BeforeRollbackConfig" => FaultPoint::BeforeRollbackConfig,
            "AfterRecoveryJournalRemoval" => FaultPoint::AfterRecoveryJournalRemoval,
            other => panic!("unknown crash fault point {other}"),
        }
    }

    fn age_crashed_claude_locks() {
        for path in [
            crate::paths::oauth_refresh_lock_dir(),
            crate::paths::credentials_lock_dir(),
            crate::paths::global_config_lock_dir(),
        ] {
            if path.exists() {
                crate::claude_locks::age_lock_for_test(&path, Duration::from_secs(61)).unwrap();
            }
        }
    }

    fn transaction_fixture_at(
        vault: &std::path::Path,
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
        let sequence_path = vault.join("sequence.json");
        fs::write(&sequence_path, &sequence).unwrap();
        let Value::Object(sequence_map) = sequence_value else {
            unreachable!()
        };
        let store = CredentialStore::new(TestHost {
            credentials: vault.join("credentials"),
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
                config_backup_path: vault.join("configs/old.json"),
            },
        };
        (store, plan, active, config, sequence)
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
        transaction_fixture_at(env.vault.path())
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
    fn recovery_rolls_back_every_noncommitted_phase_and_is_idempotent() {
        for point in [
            FaultPoint::AfterJournalSync,
            FaultPoint::AfterStageSequence,
            FaultPoint::AfterActiveCredentialInstall,
            FaultPoint::AfterCredentialPhaseSync,
            FaultPoint::AfterGlobalConfigInstall,
            FaultPoint::AfterConfigPhaseSync,
            FaultPoint::AfterSequenceInstall,
            FaultPoint::AfterSequencePhaseSync,
        ] {
            let env = setup();
            let (mut store, plan, active, config, sequence) = transaction_fixture(&env);
            let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = execute_locked(&mut store, plan.clone(), &PanicFault(point));
            }));
            assert!(crashed.is_err(), "{point:?}");
            assert!(
                JournalStore::new(env.vault.path()).path().exists(),
                "{point:?}"
            );

            let recovered = recover_pending_switch_with(Duration::from_secs(1)).unwrap();
            assert!(matches!(recovered, RecoveryDisposition::RolledBack { .. }));
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
            assert_eq!(
                recover_pending_switch_with(Duration::from_secs(1)).unwrap(),
                RecoveryDisposition::NothingToRecover
            );
            assert!(recovery_requirement().is_none());
            drop(env);
        }
    }

    #[test]
    fn recovery_verifies_a_committed_switch_then_cleans_up() {
        let env = setup();
        let (mut store, plan, active, _, _) = transaction_fixture(&env);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = execute_locked(&mut store, plan, &PanicFault(FaultPoint::AfterCommitSync));
        }));
        assert!(crashed.is_err());
        assert_ne!(fs::read(crate::paths::credentials_path()).unwrap(), active);
        assert!(matches!(
            recover_pending_switch_with(Duration::from_secs(1)).unwrap(),
            RecoveryDisposition::VerifiedCommitted { .. }
        ));
        assert!(!JournalStore::new(env.vault.path()).path().exists());
        assert!(recovery_requirement().is_none());
    }

    #[test]
    fn recovery_noop_does_not_create_or_touch_any_lock_path() {
        let env = setup();
        assert_eq!(
            recover_pending_switch_with(Duration::from_millis(50)).unwrap(),
            RecoveryDisposition::NothingToRecover
        );
        assert!(!crate::paths::oauth_refresh_lock_dir().exists());
        assert!(!crate::paths::credentials_lock_dir().exists());
        assert!(!crate::paths::global_config_lock_dir().exists());
        assert!(!env.vault.path().join(".lock").exists());
        assert!(!crate::paths::cswap_store_root().exists());
    }

    #[test]
    fn recovery_tamper_failure_retains_journal_and_blocks_the_next_switch() {
        let env = setup();
        let (mut store, plan, _, _, _) = transaction_fixture(&env);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = execute_locked(
                &mut store,
                plan.clone(),
                &PanicFault(FaultPoint::AfterActiveCredentialInstall),
            );
        }));
        assert!(crashed.is_err());
        let journal_store = JournalStore::new(env.vault.path());
        let journal = journal_store.load().unwrap().unwrap();
        let crate::recovery_store::ProtectedArtifactRef::File { relative_path, .. } =
            journal.active_credential.before.as_ref().unwrap()
        else {
            panic!("test recovery backend must be file based");
        };
        fs::write(
            env.vault.path().join(relative_path),
            crate::credentials::protect_bytes(b"tampered"),
        )
        .unwrap();

        assert!(recover_pending_switch_with(Duration::from_secs(1)).is_err());
        assert!(journal_store.path().exists());
        assert!(recovery_requirement().is_some());
        let before = fs::read(crate::paths::credentials_path()).unwrap();
        assert!(matches!(
            execute_locked(&mut store, plan, &NoFaults),
            Err(TransactionError::RecoveryRequired)
        ));
        assert_eq!(fs::read(crate::paths::credentials_path()).unwrap(), before);
        set_recovery_requirement(None);
    }

    #[test]
    fn process_death_child() {
        let Ok(vault) = std::env::var("CC_LOGINS_CRASH_TEST_VAULT") else {
            return;
        };
        let _store_root = StoreRootGuard::set(PathBuf::from(&vault));
        if let Ok(point) = std::env::var("CC_LOGINS_CRASH_TEST_RECOVERY_POINT") {
            let _ = recover_pending_switch_with_faults(
                Duration::from_secs(2),
                &AbortFault(fault_from_name(&point)),
            );
            panic!("recovery crash fault was not reached");
        }
        let point = fault_from_name(
            &std::env::var("CC_LOGINS_CRASH_TEST_POINT")
                .expect("crash child requires a fault point"),
        );
        let (mut store, plan, _, _, _) = transaction_fixture_at(std::path::Path::new(&vault));
        let _locks = acquire_live_state_locks(Duration::from_secs(2)).unwrap();
        let _ = execute_locked(&mut store, plan, &AbortFault(point));
        panic!("crash fault was not reached");
    }

    #[test]
    fn crash_restart_matrix_recovers_after_real_process_termination() {
        for (point, committed) in [
            ("AfterJournalSync", false),
            ("AfterActiveCredentialInstall", false),
            ("AfterGlobalConfigInstall", false),
            ("AfterSequenceInstall", false),
            ("AfterCommitSync", true),
        ] {
            let env = setup();
            let (_, plan, active, config, sequence) = transaction_fixture(&env);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("switch_transaction::tests::process_death_child")
                .arg("--nocapture")
                .env("CC_LOGINS_CRASH_TEST_VAULT", env.vault.path())
                .env("CC_LOGINS_CRASH_TEST_POINT", point)
                .env("HOME", env._home_dir.path())
                .env("USERPROFILE", env._home_dir.path())
                .env("CLAUDE_CONFIG_DIR", env._config_dir.path())
                .env_remove("XDG_DATA_HOME")
                .env_remove("WSL_DISTRO_NAME")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success(), "child did not terminate at {point}");
            assert!(JournalStore::new(env.vault.path()).path().exists());
            age_crashed_claude_locks();

            let disposition = recover_pending_switch_with(Duration::from_secs(2)).unwrap();
            if committed {
                assert!(matches!(
                    disposition,
                    RecoveryDisposition::VerifiedCommitted { .. }
                ));
                assert_ne!(fs::read(crate::paths::credentials_path()).unwrap(), active);
            } else {
                assert!(matches!(
                    disposition,
                    RecoveryDisposition::RolledBack { .. }
                ));
                assert_eq!(fs::read(crate::paths::credentials_path()).unwrap(), active);
                assert_eq!(
                    fs::read(crate::paths::global_config_path()).unwrap(),
                    config
                );
                assert_eq!(fs::read(&plan.sequence_path).unwrap(), sequence);
            }
            assert!(!JournalStore::new(env.vault.path()).path().exists());
            drop(env);
        }
    }

    #[test]
    fn second_process_death_during_recovery_remains_recoverable() {
        let env = setup();
        let (_, plan, active, config, sequence) = transaction_fixture(&env);
        let executable = std::env::current_exe().unwrap();
        let child_args = [
            "--exact",
            "switch_transaction::tests::process_death_child",
            "--nocapture",
        ];
        let first = std::process::Command::new(&executable)
            .args(child_args)
            .env("CC_LOGINS_CRASH_TEST_VAULT", env.vault.path())
            .env("CC_LOGINS_CRASH_TEST_POINT", "AfterActiveCredentialInstall")
            .env("HOME", env._home_dir.path())
            .env("USERPROFILE", env._home_dir.path())
            .env("CLAUDE_CONFIG_DIR", env._config_dir.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("WSL_DISTRO_NAME")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!first.success());
        age_crashed_claude_locks();

        let second = std::process::Command::new(&executable)
            .args(child_args)
            .env("CC_LOGINS_CRASH_TEST_VAULT", env.vault.path())
            .env(
                "CC_LOGINS_CRASH_TEST_RECOVERY_POINT",
                "BeforeRollbackConfig",
            )
            .env("HOME", env._home_dir.path())
            .env("USERPROFILE", env._home_dir.path())
            .env("CLAUDE_CONFIG_DIR", env._config_dir.path())
            .env_remove("CC_LOGINS_CRASH_TEST_POINT")
            .env_remove("XDG_DATA_HOME")
            .env_remove("WSL_DISTRO_NAME")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!second.success());
        assert!(JournalStore::new(env.vault.path()).path().exists());
        age_crashed_claude_locks();

        assert!(matches!(
            recover_pending_switch_with(Duration::from_secs(2)).unwrap(),
            RecoveryDisposition::RolledBack { .. }
        ));
        assert_eq!(fs::read(crate::paths::credentials_path()).unwrap(), active);
        assert_eq!(
            fs::read(crate::paths::global_config_path()).unwrap(),
            config
        );
        assert_eq!(fs::read(plan.sequence_path).unwrap(), sequence);
        assert!(!JournalStore::new(env.vault.path()).path().exists());
    }

    #[test]
    fn process_death_after_recovery_journal_removal_never_blocks_restart() {
        let env = setup();
        let executable = std::env::current_exe().unwrap();
        let child_args = [
            "--exact",
            "switch_transaction::tests::process_death_child",
            "--nocapture",
        ];
        let first = std::process::Command::new(&executable)
            .args(child_args)
            .env("CC_LOGINS_CRASH_TEST_VAULT", env.vault.path())
            .env("CC_LOGINS_CRASH_TEST_POINT", "AfterActiveCredentialInstall")
            .env("HOME", env._home_dir.path())
            .env("USERPROFILE", env._home_dir.path())
            .env("CLAUDE_CONFIG_DIR", env._config_dir.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("WSL_DISTRO_NAME")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!first.success());
        age_crashed_claude_locks();

        let second = std::process::Command::new(&executable)
            .args(child_args)
            .env("CC_LOGINS_CRASH_TEST_VAULT", env.vault.path())
            .env(
                "CC_LOGINS_CRASH_TEST_RECOVERY_POINT",
                "AfterRecoveryJournalRemoval",
            )
            .env("HOME", env._home_dir.path())
            .env("USERPROFILE", env._home_dir.path())
            .env("CLAUDE_CONFIG_DIR", env._config_dir.path())
            .env_remove("CC_LOGINS_CRASH_TEST_POINT")
            .env_remove("XDG_DATA_HOME")
            .env_remove("WSL_DISTRO_NAME")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!second.success());

        assert!(
            !JournalStore::new(env.vault.path()).path().exists(),
            "the durable journal must disappear before dispensable recovery blobs"
        );
        assert_eq!(
            recover_pending_switch_with(Duration::from_millis(50)).unwrap(),
            RecoveryDisposition::NothingToRecover
        );
        assert!(recovery_requirement().is_none());
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
    fn active_refresh_locks_omit_config_but_hold_credentials_and_vault() {
        let env = setup();
        fs::create_dir(crate::paths::global_config_lock_dir()).unwrap();

        let locks = acquire_active_refresh_locks(Duration::from_millis(100)).unwrap();

        assert!(crate::paths::oauth_refresh_lock_dir().is_dir());
        assert!(crate::paths::credentials_lock_dir().is_dir());
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
