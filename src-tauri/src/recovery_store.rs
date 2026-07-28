//! Protected before-image storage for durable switch recovery.
//!
//! Journal metadata contains only these opaque references. Secret bytes use
//! DPAPI on Windows, Keychain in macOS production, and an honestly-labelled
//! 0600 `plain` envelope where no native protection is available.

use std::fs;
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "macos")]
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
#[cfg(target_os = "macos")]
use base64::Engine;
use serde::{Deserialize, Serialize};

const RECOVERY_DIR: &str = "switch-recovery";
#[cfg(target_os = "macos")]
const KEYCHAIN_MANIFEST: &str = "keychain-accounts.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "camelCase")]
pub enum ProtectedArtifactRef {
    File {
        relative_path: PathBuf,
        protection: String,
    },
    Keychain {
        account: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryStoreError {
    #[error("invalid recovery identifier {0:?}")]
    InvalidIdentifier(String),
    #[error("invalid recovery artifact reference")]
    InvalidReference,
    #[error("recovery I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("recovery metadata error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not protect or recover switch state: {0}")]
    Protection(String),
}

#[derive(Debug, Clone)]
pub struct RecoveryStore {
    backup_root: PathBuf,
    root: PathBuf,
}

impl RecoveryStore {
    pub fn new(backup_root: impl AsRef<Path>) -> Self {
        let backup_root = backup_root.as_ref().to_path_buf();
        let root = backup_root.join(RECOVERY_DIR);
        Self { backup_root, root }
    }

    pub fn put(
        &mut self,
        transaction_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<ProtectedArtifactRef, RecoveryStoreError> {
        validate_identifier(transaction_id)?;
        validate_identifier(name)?;

        #[cfg(all(target_os = "macos", not(test)))]
        {
            let account = format!("{transaction_id}:{name}");
            let encoded = BASE64_STANDARD.encode(bytes);
            match crate::credentials::recovery_keychain_set(&account, &encoded) {
                Ok(()) => {
                    self.record_keychain_account(transaction_id, &account)?;
                    return Ok(ProtectedArtifactRef::Keychain { account });
                }
                Err(error) => log::warn!(
                    "Keychain unavailable for switch recovery; using 0600 plain fallback: {error}"
                ),
            }
        }

        let relative_path = PathBuf::from(RECOVERY_DIR)
            .join(transaction_id)
            .join(format!("{name}.protected"));
        let target = self.backup_root.join(&relative_path);
        let protected = crate::credentials::protect_bytes(bytes);
        let protection = serde_json::from_slice::<serde_json::Value>(&protected)
            .ok()
            .and_then(|value| value.get("scheme")?.as_str().map(str::to_string))
            .unwrap_or_else(|| "plain".to_string());
        crate::durable_fs::stage_sibling(&target, &protected, Some(0o600))?.commit()?;
        Ok(ProtectedArtifactRef::File {
            relative_path,
            protection,
        })
    }

    pub fn get(&mut self, reference: &ProtectedArtifactRef) -> Result<Vec<u8>, RecoveryStoreError> {
        match reference {
            ProtectedArtifactRef::File { relative_path, .. } => {
                validate_relative_reference(relative_path)?;
                let raw = fs::read(self.backup_root.join(relative_path))?;
                crate::credentials::unprotect_bytes(&raw).map_err(RecoveryStoreError::Protection)
            }
            ProtectedArtifactRef::Keychain { account } => {
                #[cfg(target_os = "macos")]
                {
                    let encoded = crate::credentials::recovery_keychain_get(account)
                        .map_err(RecoveryStoreError::Protection)?
                        .ok_or_else(|| {
                            RecoveryStoreError::Protection(
                                "recovery Keychain item is missing".to_string(),
                            )
                        })?;
                    BASE64_STANDARD.decode(encoded).map_err(|error| {
                        RecoveryStoreError::Protection(format!(
                            "recovery Keychain value is invalid base64: {error}"
                        ))
                    })
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = account;
                    Err(RecoveryStoreError::Protection(
                        "Keychain recovery reference cannot be read on this platform".to_string(),
                    ))
                }
            }
        }
    }

    pub fn remove_transaction(&mut self, transaction_id: &str) -> Result<(), RecoveryStoreError> {
        validate_identifier(transaction_id)?;
        let transaction = self.root.join(transaction_id);

        #[cfg(target_os = "macos")]
        if let Ok(raw) = fs::read(transaction.join(KEYCHAIN_MANIFEST)) {
            for account in serde_json::from_slice::<Vec<String>>(&raw)? {
                crate::credentials::recovery_keychain_delete(&account)
                    .map_err(RecoveryStoreError::Protection)?;
            }
        }

        match fs::remove_dir_all(&transaction) {
            Ok(()) => crate::durable_fs::sync_parent(&transaction)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    #[cfg(all(target_os = "macos", not(test)))]
    fn record_keychain_account(
        &self,
        transaction_id: &str,
        account: &str,
    ) -> Result<(), RecoveryStoreError> {
        let manifest = self.root.join(transaction_id).join(KEYCHAIN_MANIFEST);
        let mut accounts = fs::read(&manifest)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        if !accounts.iter().any(|existing| existing == account) {
            accounts.push(account.to_string());
        }
        let body = serde_json::to_vec(&accounts)?;
        crate::durable_fs::stage_sibling(&manifest, &body, Some(0o600))?.commit()?;
        Ok(())
    }
}

fn validate_identifier(value: &str) -> Result<(), RecoveryStoreError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RecoveryStoreError::InvalidIdentifier(value.to_string()));
    }
    Ok(())
}

fn validate_relative_reference(path: &Path) -> Result<(), RecoveryStoreError> {
    let mut components = path.components();
    if components.next() != Some(Component::Normal(RECOVERY_DIR.as_ref()))
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RecoveryStoreError::InvalidReference);
    }
    Ok(())
}

impl From<crate::durable_fs::DurableFsError> for RecoveryStoreError {
    fn from(error: crate::durable_fs::DurableFsError) -> Self {
        Self::Io(std::io::Error::from(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_bytes_round_trip_without_secret_in_reference_metadata() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RecoveryStore::new(root.path());
        let secret = b"\0credential\xffconfig\nbytes";
        let reference = store.put("tx-1", "active", secret).unwrap();
        assert_eq!(store.get(&reference).unwrap(), secret);
        let metadata = serde_json::to_string(&reference).unwrap();
        assert!(!metadata.contains("credential"));
        assert!(!metadata.contains("config"));
        assert!(!metadata.contains("bytes"));
    }

    #[test]
    fn file_reference_reports_the_actual_protection_backend() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RecoveryStore::new(root.path());
        let reference = store.put("tx-1", "active", b"secret").unwrap();
        let ProtectedArtifactRef::File { protection, .. } = reference else {
            panic!("test file backend expected");
        };
        #[cfg(windows)]
        assert_eq!(protection, "dpapi");
        #[cfg(not(windows))]
        assert_eq!(protection, "plain");
    }

    #[test]
    fn cleanup_removes_only_the_named_transaction() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RecoveryStore::new(root.path());
        let first = store.put("tx-1", "active", b"first").unwrap();
        let second = store.put("tx-2", "active", b"second").unwrap();
        store.remove_transaction("tx-1").unwrap();
        assert!(store.get(&first).is_err());
        assert_eq!(store.get(&second).unwrap(), b"second");
    }

    #[test]
    fn traversal_identifiers_and_references_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RecoveryStore::new(root.path());
        assert!(store.put("../escape", "active", b"secret").is_err());
        assert!(store
            .get(&ProtectedArtifactRef::File {
                relative_path: PathBuf::from("switch-recovery/../escape"),
                protection: "plain".to_string(),
            })
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn plain_fallback_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut store = RecoveryStore::new(root.path());
        let ProtectedArtifactRef::File { relative_path, .. } =
            store.put("tx-1", "active", b"secret").unwrap()
        else {
            panic!("file backend expected");
        };
        assert_eq!(
            fs::metadata(root.path().join(relative_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
