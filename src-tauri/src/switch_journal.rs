//! Secret-free durable metadata for account-switch recovery.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::recovery_store::{ProtectedArtifactRef, RecoveryStore};

const SCHEMA_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "switch-journal.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JournalPhase {
    Prepared,
    ActiveCredentialInstalled,
    GlobalConfigInstalled,
    SequenceInstalled,
    Committed,
}

impl JournalPhase {
    fn next(self) -> Option<Self> {
        Some(match self {
            Self::Prepared => Self::ActiveCredentialInstalled,
            Self::ActiveCredentialInstalled => Self::GlobalConfigInstalled,
            Self::GlobalConfigInstalled => Self::SequenceInstalled,
            Self::SequenceInstalled => Self::Committed,
            Self::Committed => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalTarget {
    pub number: String,
    pub email: String,
    pub stable_key: String,
    pub credential_generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRecord {
    pub before: Option<ProtectedArtifactRef>,
    pub staged_relative_path: Option<PathBuf>,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub existed_before: bool,
}

impl ArtifactRecord {
    pub fn load_verified_before(
        &self,
        recovery: &mut RecoveryStore,
    ) -> Result<Option<Vec<u8>>, JournalError> {
        let Some(reference) = &self.before else {
            return if self.existed_before {
                Err(JournalError::Invalid(
                    "artifact existed but has no protected before-image".to_string(),
                ))
            } else {
                Ok(None)
            };
        };
        let bytes = recovery.get(reference)?;
        let expected = self.before_sha256.as_deref().ok_or_else(|| {
            JournalError::Invalid("protected before-image has no hash".to_string())
        })?;
        let actual = sha256(&bytes);
        if actual != expected {
            return Err(JournalError::BeforeImageTampered);
        }
        Ok(Some(bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingGeneration {
    pub account_number: Option<String>,
    pub credential: Option<ProtectedArtifactRef>,
    pub config: Option<ProtectedArtifactRef>,
    pub credential_sha256: Option<String>,
    pub config_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchJournal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub target: JournalTarget,
    pub phase: JournalPhase,
    pub active_credential: ArtifactRecord,
    pub global_config: ArtifactRecord,
    pub sequence: ArtifactRecord,
    pub outgoing_generation: Option<OutgoingGeneration>,
}

impl SwitchJournal {
    pub fn prepared(
        transaction_id: String,
        target: JournalTarget,
        active_credential: ArtifactRecord,
        global_config: ArtifactRecord,
        sequence: ArtifactRecord,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            transaction_id,
            target,
            phase: JournalPhase::Prepared,
            active_credential,
            global_config,
            sequence,
            outgoing_generation: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("switch journal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("switch journal JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported switch journal schema {0}")]
    UnsupportedSchema(u32),
    #[error("a switch transaction is already pending")]
    AlreadyPending,
    #[error("switch journal transaction does not match")]
    TransactionMismatch,
    #[error("invalid switch journal phase transition {from:?} -> {to:?}")]
    InvalidTransition {
        from: JournalPhase,
        to: JournalPhase,
    },
    #[error("invalid switch journal: {0}")]
    Invalid(String),
    #[error("switch recovery before-image failed its integrity check")]
    BeforeImageTampered,
    #[error(transparent)]
    Recovery(#[from] crate::recovery_store::RecoveryStoreError),
}

impl From<crate::durable_fs::DurableFsError> for JournalError {
    fn from(error: crate::durable_fs::DurableFsError) -> Self {
        Self::Io(std::io::Error::from(error))
    }
}

#[derive(Debug, Clone)]
pub struct JournalStore {
    path: PathBuf,
}

impl JournalStore {
    pub fn new(backup_root: impl AsRef<Path>) -> Self {
        Self {
            path: backup_root.as_ref().join(JOURNAL_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<SwitchJournal>, JournalError> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let journal: SwitchJournal = serde_json::from_slice(&raw)?;
        if journal.schema_version != SCHEMA_VERSION {
            return Err(JournalError::UnsupportedSchema(journal.schema_version));
        }
        validate(&journal)?;
        Ok(Some(journal))
    }

    pub fn prepare(&self, journal: &SwitchJournal) -> Result<(), JournalError> {
        if self.path.exists() {
            return Err(JournalError::AlreadyPending);
        }
        if journal.phase != JournalPhase::Prepared || journal.schema_version != SCHEMA_VERSION {
            return Err(JournalError::Invalid(
                "new journal must use the current schema and Prepared phase".to_string(),
            ));
        }
        validate(journal)?;
        self.write(journal)
    }

    pub fn advance(
        &self,
        journal: &mut SwitchJournal,
        next: JournalPhase,
    ) -> Result<(), JournalError> {
        let current = self.load()?.ok_or(JournalError::TransactionMismatch)?;
        if current.transaction_id != journal.transaction_id || current.phase != journal.phase {
            return Err(JournalError::TransactionMismatch);
        }
        if journal.phase.next() != Some(next) {
            return Err(JournalError::InvalidTransition {
                from: journal.phase,
                to: next,
            });
        }
        let mut successor = journal.clone();
        successor.phase = next;
        self.write(&successor)?;
        *journal = successor;
        Ok(())
    }

    pub fn remove(&self, transaction_id: &str) -> Result<(), JournalError> {
        if let Some(current) = self.load()? {
            if current.transaction_id != transaction_id {
                return Err(JournalError::TransactionMismatch);
            }
        } else {
            return Ok(());
        }
        fs::remove_file(&self.path)?;
        crate::durable_fs::sync_parent(&self.path)?;
        Ok(())
    }

    fn write(&self, journal: &SwitchJournal) -> Result<(), JournalError> {
        let body = serde_json::to_vec_pretty(journal)?;
        crate::durable_fs::stage_sibling(&self.path, &body, Some(0o600))?.commit()?;
        Ok(())
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn validate(journal: &SwitchJournal) -> Result<(), JournalError> {
    if journal.transaction_id.is_empty()
        || journal.target.number.is_empty()
        || journal.target.stable_key.is_empty()
        || journal.target.credential_generation.is_empty()
    {
        return Err(JournalError::Invalid(
            "transaction and target identity fields must be non-empty".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(before: ProtectedArtifactRef, bytes: &[u8]) -> ArtifactRecord {
        ArtifactRecord {
            before: Some(before),
            staged_relative_path: Some(PathBuf::from("switch-stages/tx/file.stage")),
            before_sha256: Some(sha256(bytes)),
            after_sha256: Some(sha256(b"after")),
            existed_before: true,
        }
    }

    fn fixture(root: &Path) -> (SwitchJournal, RecoveryStore) {
        let mut recovery = RecoveryStore::new(root);
        let credential = recovery
            .put("tx-1", "credential", b"before-credential")
            .unwrap();
        let config = recovery.put("tx-1", "config", b"before-config").unwrap();
        let sequence = recovery
            .put("tx-1", "sequence", b"before-sequence")
            .unwrap();
        let journal = SwitchJournal::prepared(
            "tx-1".to_string(),
            JournalTarget {
                number: "2".to_string(),
                email: "two@example.com".to_string(),
                stable_key: "org:two".to_string(),
                credential_generation: "sha256-full:generation".to_string(),
            },
            artifact(credential, b"before-credential"),
            artifact(config, b"before-config"),
            artifact(sequence, b"before-sequence"),
        );
        (journal, recovery)
    }

    #[test]
    fn secret_free_round_trip_and_forward_only_phases() {
        let root = tempfile::tempdir().unwrap();
        let (mut journal, _) = fixture(root.path());
        let store = JournalStore::new(root.path());
        store.prepare(&journal).unwrap();
        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("before-credential"));
        assert!(!raw.contains("before-config"));
        assert_eq!(store.load().unwrap().unwrap(), journal);

        assert!(matches!(
            store.advance(&mut journal, JournalPhase::GlobalConfigInstalled),
            Err(JournalError::InvalidTransition { .. })
        ));
        store
            .advance(&mut journal, JournalPhase::ActiveCredentialInstalled)
            .unwrap();
        assert_eq!(journal.phase, JournalPhase::ActiveCredentialInstalled);
    }

    #[test]
    fn malformed_and_unknown_schema_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let store = JournalStore::new(root.path());
        fs::write(store.path(), b"{not-json").unwrap();
        assert!(matches!(store.load(), Err(JournalError::Json(_))));

        let (mut journal, _) = fixture(root.path());
        journal.schema_version = 999;
        fs::write(store.path(), serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(matches!(
            store.load(),
            Err(JournalError::UnsupportedSchema(999))
        ));
    }

    #[test]
    fn before_image_hash_detects_tampering() {
        let root = tempfile::tempdir().unwrap();
        let (journal, mut recovery) = fixture(root.path());
        let reference = journal.active_credential.before.as_ref().unwrap();
        let ProtectedArtifactRef::File { relative_path, .. } = reference else {
            panic!("file backend expected");
        };
        let replacement = crate::credentials::protect_bytes(b"tampered-but-valid");
        fs::write(root.path().join(relative_path), replacement).unwrap();
        assert!(matches!(
            journal
                .active_credential
                .load_verified_before(&mut recovery),
            Err(JournalError::BeforeImageTampered)
        ));
    }

    #[test]
    fn cleanup_removes_journal_only_after_requested_transaction() {
        let root = tempfile::tempdir().unwrap();
        let (journal, _) = fixture(root.path());
        let store = JournalStore::new(root.path());
        store.prepare(&journal).unwrap();
        assert!(store.remove("other").is_err());
        assert!(store.path().exists());
        store.remove("tx-1").unwrap();
        assert!(!store.path().exists());
    }

    #[cfg(windows)]
    #[test]
    fn failed_phase_replace_leaves_previous_phase_visible() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let root = tempfile::tempdir().unwrap();
        let (mut journal, _) = fixture(root.path());
        let store = JournalStore::new(root.path());
        store.prepare(&journal).unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(store.path())
            .unwrap();
        assert!(store
            .advance(&mut journal, JournalPhase::ActiveCredentialInstalled)
            .is_err());
        assert_eq!(journal.phase, JournalPhase::Prepared);
        assert_eq!(store.load().unwrap().unwrap().phase, JournalPhase::Prepared);
    }
}
