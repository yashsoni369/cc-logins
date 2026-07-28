//! Secret-free, generation-bound OAuth rejection state.
//!
//! Callers mutate this store only while holding the GUI vault lock. The file
//! records account identity keys and one-way credential fingerprints, never
//! access tokens, refresh tokens, or credential JSON.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "oauth-quarantine.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineEntry {
    credential_fingerprint: String,
    rejected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineFile {
    schema_version: u32,
    entries: BTreeMap<String, QuarantineEntry>,
}

impl Default for QuarantineFile {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QuarantineError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct OAuthQuarantine {
    path: PathBuf,
}

impl OAuthQuarantine {
    pub fn new(backup_root: impl AsRef<Path>) -> Self {
        Self {
            path: backup_root.as_ref().join(FILE_NAME),
        }
    }

    pub fn is_rejected(&self, stable_key: &str, credential_fingerprint: &str) -> bool {
        self.load()
            .entries
            .get(stable_key)
            .is_some_and(|entry| entry.credential_fingerprint == credential_fingerprint)
    }

    pub fn reject(
        &self,
        stable_key: &str,
        credential_fingerprint: &str,
        rejected_at: DateTime<Utc>,
    ) -> Result<(), QuarantineError> {
        let mut file = self.load();
        file.entries.insert(
            stable_key.to_string(),
            QuarantineEntry {
                credential_fingerprint: credential_fingerprint.to_string(),
                rejected_at,
            },
        );
        self.save(&file)
    }

    /// Clear a verdict only when the caller proves the slot now contains a
    /// different credential generation. Returns whether an entry was removed.
    pub fn clear_obsolete(
        &self,
        stable_key: &str,
        current_fingerprint: &str,
    ) -> Result<bool, QuarantineError> {
        let mut file = self.load();
        let obsolete = file
            .entries
            .get(stable_key)
            .is_some_and(|entry| entry.credential_fingerprint != current_fingerprint);
        if !obsolete {
            return Ok(false);
        }
        file.entries.remove(stable_key);
        self.save(&file)?;
        Ok(true)
    }

    fn load(&self) -> QuarantineFile {
        let Ok(bytes) = fs::read(&self.path) else {
            return QuarantineFile::default();
        };
        let Ok(file) = serde_json::from_slice::<QuarantineFile>(&bytes) else {
            log::warn!(
                "OAuth quarantine at {} is malformed; ignoring it",
                self.path.display()
            );
            return QuarantineFile::default();
        };
        if file.schema_version != SCHEMA_VERSION {
            log::warn!(
                "OAuth quarantine at {} uses unsupported schema {}; ignoring it",
                self.path.display(),
                file.schema_version
            );
            return QuarantineFile::default();
        }
        file
    }

    fn save(&self, file: &QuarantineFile) -> Result<(), QuarantineError> {
        self.save_with_commit(file, |temporary, target| fs::rename(temporary, target))
    }

    fn save_with_commit<F>(&self, file: &QuarantineFile, commit: F) -> Result<(), QuarantineError>
    where
        F: FnOnce(&Path, &Path) -> io::Result<()>,
    {
        let bytes = serde_json::to_vec_pretty(file)?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{FILE_NAME}.{}.{}.tmp",
            std::process::id(),
            suffix
        ));

        let write_result = (|| -> io::Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = options.open(&temporary)?;
            output.write_all(&bytes)?;
            output.sync_all()
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        if let Err(error) = commit(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(())
    }
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "org:account-1";
    const FIRST: &str = "sha256:first-generation";
    const SECOND: &str = "sha256:second-generation";

    #[test]
    fn identity_and_generation_must_both_match() {
        let root = tempfile::tempdir().unwrap();
        let store = OAuthQuarantine::new(root.path());
        store.reject(KEY, FIRST, Utc::now()).unwrap();

        assert!(store.is_rejected(KEY, FIRST));
        assert!(!store.is_rejected(KEY, SECOND));
        assert!(!store.is_rejected("org:other", FIRST));
    }

    #[test]
    fn new_generation_clears_but_same_generation_stays_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = OAuthQuarantine::new(root.path());
        store.reject(KEY, FIRST, Utc::now()).unwrap();

        assert!(!store.clear_obsolete(KEY, FIRST).unwrap());
        assert!(store.is_rejected(KEY, FIRST));
        assert!(store.clear_obsolete(KEY, SECOND).unwrap());
        assert!(!store.is_rejected(KEY, FIRST));
    }

    #[test]
    fn persisted_file_contains_no_credential_material() {
        let root = tempfile::tempdir().unwrap();
        let store = OAuthQuarantine::new(root.path());
        store.reject(KEY, FIRST, Utc::now()).unwrap();

        let text = fs::read_to_string(root.path().join(FILE_NAME)).unwrap();
        assert!(text.contains(FIRST));
        assert!(!text.contains("accessToken"));
        assert!(!text.contains("refreshToken"));
        assert!(!text.contains("sk-ant"));
    }

    #[test]
    fn malformed_or_unknown_schema_degrades_to_empty() {
        let root = tempfile::tempdir().unwrap();
        let store = OAuthQuarantine::new(root.path());
        fs::write(root.path().join(FILE_NAME), b"{not-json").unwrap();
        assert!(!store.is_rejected(KEY, FIRST));

        fs::write(
            root.path().join(FILE_NAME),
            br#"{"schemaVersion":999,"entries":{}}"#,
        )
        .unwrap();
        assert!(!store.is_rejected(KEY, FIRST));
    }

    #[test]
    fn failed_atomic_commit_preserves_prior_file() {
        let root = tempfile::tempdir().unwrap();
        let store = OAuthQuarantine::new(root.path());
        store.reject(KEY, FIRST, Utc::now()).unwrap();
        let before = fs::read(root.path().join(FILE_NAME)).unwrap();

        let mut changed = store.load();
        changed.entries.insert(
            KEY.to_string(),
            QuarantineEntry {
                credential_fingerprint: SECOND.to_string(),
                rejected_at: Utc::now(),
            },
        );
        let error = store
            .save_with_commit(&changed, |_, _| Err(io::Error::other("injected failure")))
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(fs::read(root.path().join(FILE_NAME)).unwrap(), before);
        assert!(store.is_rejected(KEY, FIRST));
    }
}
