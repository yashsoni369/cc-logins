//! One-time migration of this app's OWN data directory across bundle
//! identifier renames, currently `dev.apex36.cc-logins` -> `cc-logins`.
//!
//! Tauri derives `app_data_dir()` from the bundle identifier
//! (`tauri.conf.json`'s `identifier`), so renaming that identifier moves
//! where the whole app-data tree lives — `accounts/` (the credential vault:
//! `sequence.json`, `credentials/`, `configs/`, `.lock`), `history.sqlite3`
//! (+ `-shm`/`-wal`), and `settings.json`. Without this module the renamed
//! app would look in the new, empty directory, show first-run, and silently
//! orphan a user's real accounts sitting one directory over.
//!
//! Despite the module name, this is not about migrating any other tool's
//! store: it only ever moves bytes this app itself owns, between two
//! directories both named after *this app's own* bundle identifiers (old and
//! new).
//!
//! # Safety model
//!
//! 1. **No-op if `new_dir` already has managed app data.** Diagnostic
//!    `app.log*` files do not count because logging historically used the
//!    target `cc-logins` directory before the bundle identifier did. Never
//!    overwrite a newer store with an older one — this also makes the migration attempt
//!    idempotent: once it has succeeded, every later launch (which now finds
//!    `new_dir` populated) skips straight past it.
//! 2. **No-op if `old_dir` does not exist.** A fresh install (or a machine
//!    that never ran the pre-rename build) must not log a scary migration
//!    message about a directory that was never there.
//! 3. **Copy first, verify, then retire the old.** [`migrate_app_data`] never
//!    calls `remove_dir_all` on `old_dir` and never renames it until *after*
//!    a full recursive copy into `new_dir` has both completed and been
//!    verified. A crash (power loss, panic, forced kill) at any point before
//!    that final rename leaves `old_dir` byte-for-byte as it was — the
//!    original is always intact and usable, and the worst a crash can leave
//!    behind at `new_dir` is a partial copy, never data loss.
//! 4. **Retire by rename, not delete.** Once verified, `old_dir` is renamed
//!    to `<old_dir>.migrated-<unix-seconds>` rather than removed. Leaving a
//!    fully-populated, live-looking credential directory sitting at its old,
//!    now-unused path is itself a hazard (a stale copy of real tokens with no
//!    code left that manages it), which is why it is moved out of the way —
//!    but moved, not destroyed, so it stays recoverable if anything about the
//!    migration turns out to be wrong.
//! 5. **Verification failure aborts, does not clean up.** If the copy cannot
//!    be verified (`accounts/sequence.json` fails to parse, or the copy's
//!    account count disagrees with the original's), both directories are
//!    left exactly as they landed: `old_dir` was never touched, and the
//!    (untrustworthy) partial/failed copy at `new_dir` is left in place
//!    rather than silently deleted, so there is something on disk for a
//!    human to inspect: refuse and leave evidence, don't guess.
//!    An error is logged and a failure outcome returned; nothing is
//!    half-migrated.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Result of attempting [`migrate_app_data`]. Every branch is observable so
/// callers (and tests) can assert on exactly what happened rather than just
/// "it didn't panic".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// `new_dir` already had content; nothing was read from or written to
    /// either directory.
    NewDirAlreadyPopulated,
    /// `old_dir` did not exist; nothing to migrate.
    NoOldData,
    /// The copy completed, verification passed, and `old_dir` was renamed to
    /// the contained path. `accounts_migrated` is the account count found in
    /// `accounts/sequence.json`, or `0` if neither side had that file (e.g. a
    /// data dir that only ever held `settings.json`).
    Migrated {
        accounts_migrated: usize,
        retired_old_dir: PathBuf,
    },
    /// The copy completed and verified, but renaming `old_dir` out of the way
    /// afterwards failed (e.g. a file still open elsewhere). `new_dir` is a
    /// fully verified, usable copy — the app is not orphaned — but `old_dir`
    /// was left in place rather than retired.
    MigratedButOldDirNotRetired {
        accounts_migrated: usize,
        reason: String,
    },
    /// The copy could not be produced or could not be verified. Both
    /// directories were left exactly as they were when this was called
    /// (`old_dir` untouched throughout; `new_dir` left as whatever partial
    /// state the copy reached, for inspection).
    Failed { reason: String },
}

/// Try legacy app-data directories from newest to oldest, stopping as soon as
/// one exists (or a migration attempt produces any result other than
/// [`MigrationOutcome::NoOldData`]). This lets a user upgrade directly from
/// either prior identifier without allowing an older store to overwrite a
/// newer one.
pub fn migrate_app_data_chain(old_dirs: &[PathBuf], new_dir: &Path) -> MigrationOutcome {
    for old_dir in old_dirs {
        let outcome = migrate_app_data(old_dir, new_dir);
        if outcome != MigrationOutcome::NoOldData {
            return outcome;
        }
    }
    MigrationOutcome::NoOldData
}

/// Migrate `old_dir` into `new_dir` if, and only if, `new_dir` looks unused
/// and `old_dir` looks real. See the module docs for the full safety model.
pub fn migrate_app_data(old_dir: &Path, new_dir: &Path) -> MigrationOutcome {
    if dir_has_managed_content(new_dir) {
        log::info!(
            "cc-logins: app data dir {} already has content; skipping migration from {}",
            new_dir.display(),
            old_dir.display()
        );
        return MigrationOutcome::NewDirAlreadyPopulated;
    }

    if !old_dir.exists() {
        // Fresh install, or a machine that never ran the pre-rename build.
        // Deliberately no log line here — see module docs, rule 2.
        return MigrationOutcome::NoOldData;
    }

    log::info!(
        "cc-logins: migrating app data {} -> {}",
        old_dir.display(),
        new_dir.display()
    );

    if let Err(e) = copy_dir_recursive(old_dir, new_dir) {
        let reason = format!(
            "copying {} -> {} failed: {e}",
            old_dir.display(),
            new_dir.display()
        );
        log::error!("cc-logins: migration failed: {reason}");
        return MigrationOutcome::Failed { reason };
    }

    let accounts_migrated = match verify_migration(old_dir, new_dir) {
        Ok(count) => count,
        Err(reason) => {
            log::error!(
                "cc-logins: migration verification failed, leaving {} and the partial copy at \
                 {} untouched for inspection: {reason}",
                old_dir.display(),
                new_dir.display()
            );
            return MigrationOutcome::Failed { reason };
        }
    };

    let retired = retired_dir_path(old_dir);
    match fs::rename(old_dir, &retired) {
        Ok(()) => {
            log::info!(
                "cc-logins: migrated {accounts_migrated} account(s); old data dir moved to {}",
                retired.display()
            );
            MigrationOutcome::Migrated {
                accounts_migrated,
                retired_old_dir: retired,
            }
        }
        Err(e) => {
            // The copy at new_dir is fully verified and usable at this
            // point -- the app is not orphaned -- so this is degraded, not
            // fatal. Leave old_dir in place rather than risk losing it.
            let reason = format!(
                "verified copy at {}, but could not retire {} (rename to {} failed): {e}",
                new_dir.display(),
                old_dir.display(),
                retired.display()
            );
            log::error!("cc-logins: {reason}");
            MigrationOutcome::MigratedButOldDirNotRetired {
                accounts_migrated,
                reason,
            }
        }
    }
}

/// `true` if `dir` exists and contains at least one entry. A missing
/// directory, or one that exists but is empty (Tauri may create
/// `app_data_dir()` eagerly before this runs), both count as "no content" and
/// do not block a migration.
#[cfg(test)]
fn dir_has_content(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

fn is_diagnostic_log(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "app.log" || name.starts_with("app.log."))
}

/// The log path was already `<data-dir>/cc-logins/app.log` before the Tauri
/// identifier changed to `cc-logins`. Those files may therefore pre-exist in
/// an otherwise unused target directory and must not strand the real vault,
/// settings, and history in the legacy identifier directory.
fn dir_has_managed_content(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .any(|entry| !is_diagnostic_log(&entry.path())),
        Err(_) => false,
    }
}

/// `<old_dir>.migrated-<unix-seconds>` — a sibling of `old_dir`, so it stays
/// discoverable next to where the user would look for the original.
fn retired_dir_path(old_dir: &Path) -> PathBuf {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = old_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sibling_name = format!("{name}.migrated-{epoch}");
    match old_dir.parent() {
        Some(parent) => parent.join(sibling_name),
        None => PathBuf::from(sibling_name),
    }
}

/// Number of registered accounts recorded in `<dir>/accounts/sequence.json`,
/// or `None` if that file does not exist (e.g. a data dir that never had any
/// accounts added). Mirrors the `{"accounts": {"1": {...}, "2": {...}}}`
/// shape `crate::switcher` reads and writes.
///
/// # Errors
///
/// Returns `Err` if the file exists but cannot be read or does not parse as
/// JSON — an unreadable/corrupt registry must fail verification loudly
/// rather than silently being counted as zero accounts, which would let a
/// broken copy pass as "verified".
fn count_accounts(dir: &Path) -> Result<Option<usize>, String> {
    let seq_path = dir.join("accounts").join("sequence.json");
    if !seq_path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&seq_path)
        .map_err(|e| format!("failed to read {}: {e}", seq_path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("failed to parse {}: {e}", seq_path.display()))?;
    let count = value
        .get("accounts")
        .and_then(Value::as_object)
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(Some(count))
}

/// Verify that `new_dir` is a trustworthy copy of `old_dir`. At minimum,
/// checks that `accounts/sequence.json` (if present) parses and reports the
/// same account count on both sides.
fn verify_migration(old_dir: &Path, new_dir: &Path) -> Result<usize, String> {
    let old_count = count_accounts(old_dir)?;
    let new_count = count_accounts(new_dir)?;
    match (old_count, new_count) {
        (Some(old), Some(new)) if old == new => Ok(new),
        (Some(old), Some(new)) => Err(format!(
            "account count mismatch after copy: old={old} new={new}"
        )),
        (Some(old), None) => Err(format!(
            "old dir had {old} account(s) in sequence.json, but the copy has none"
        )),
        (None, Some(new)) => {
            // A copy producing a sequence.json where the source had none
            // should not be possible for a straight recursive copy, but it
            // is not itself evidence of corruption -- report what's there.
            Ok(new)
        }
        (None, None) => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// Recursive copy
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = fs::read_link(src)?;
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(windows)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = fs::read_link(src)?;
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    // No portable symlink primitive off unix/windows; copy the link's
    // contents as the closest achievable approximation.
    let target = fs::read_link(src)?;
    fs::copy(target, dst).map(|_| ())
}

/// Recursively copy a directory tree. Preserves structure and file contents;
/// does not attempt to preserve metadata (permissions, timestamps) beyond
/// whatever `fs::copy` already does.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if is_diagnostic_log(&src_path) && dst_path.exists() {
            // Keep the log already written at the canonical path. Diagnostic
            // logs are not part of the state whose integrity is verified.
            continue;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Every test here operates entirely inside `tempfile::tempdir()` trees passed
// directly as `old_dir`/`new_dir` -- this module never calls
// `paths::backup_root()` or any other path-resolution function, so there is
// no environment variable to redirect and no way for a test to reach a real
// app-data directory, on this or any other platform.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_sequence(dir: &Path, account_count: usize) {
        let accounts_dir = dir.join("accounts");
        fs::create_dir_all(&accounts_dir).unwrap();
        let mut accounts = serde_json::Map::new();
        for i in 1..=account_count {
            accounts.insert(
                i.to_string(),
                serde_json::json!({
                    "added": "2026-07-28T00:00:00Z",
                    "email": format!("user{i}@example.com"),
                    "organizationName": "",
                    "organizationUuid": format!("uuid-{i}"),
                }),
            );
        }
        let body = serde_json::json!({
            "accounts": accounts,
            "activeAccountNumber": 1,
            "lastUpdated": "2026-07-28T00:00:00Z",
            "sequence": (1..=account_count).collect::<Vec<_>>(),
        });
        fs::write(
            accounts_dir.join("sequence.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
        // A per-account credential file, to prove the copy isn't just
        // touching sequence.json.
        let creds_dir = accounts_dir.join("credentials");
        fs::create_dir_all(&creds_dir).unwrap();
        for i in 1..=account_count {
            fs::write(creds_dir.join(format!("{i}.enc")), format!("secret-{i}")).unwrap();
        }
    }

    fn populate_old_dir(dir: &Path) {
        write_sequence(dir, 2);
        fs::write(dir.join("settings.json"), r#"{"threshold":90}"#).unwrap();
        fs::write(
            dir.join("history.sqlite3"),
            b"not a real sqlite file, just bytes",
        )
        .unwrap();
    }

    #[test]
    fn happy_path_migrates_and_retires_old_dir() {
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");
        populate_old_dir(&old_dir);

        let outcome = migrate_app_data(&old_dir, &new_dir);

        let (accounts_migrated, retired) = match outcome {
            MigrationOutcome::Migrated {
                accounts_migrated,
                retired_old_dir,
            } => (accounts_migrated, retired_old_dir),
            other => panic!("expected Migrated, got {other:?}"),
        };
        assert_eq!(accounts_migrated, 2);

        // new_dir is a full, usable copy.
        assert!(new_dir.join("settings.json").exists());
        assert!(new_dir.join("history.sqlite3").exists());
        assert!(new_dir.join("accounts").join("sequence.json").exists());
        assert!(new_dir
            .join("accounts")
            .join("credentials")
            .join("1.enc")
            .exists());
        assert!(new_dir
            .join("accounts")
            .join("credentials")
            .join("2.enc")
            .exists());
        assert_eq!(
            fs::read_to_string(new_dir.join("settings.json")).unwrap(),
            r#"{"threshold":90}"#
        );

        // old_dir is gone from its original path...
        assert!(!old_dir.exists());
        // ...but not deleted -- it was renamed, and every byte is still there.
        assert!(retired.exists());
        assert!(retired.join("accounts").join("sequence.json").exists());
        assert!(retired
            .join("accounts")
            .join("credentials")
            .join("1.enc")
            .exists());
        assert_eq!(
            fs::read_to_string(retired.join("settings.json")).unwrap(),
            r#"{"threshold":90}"#
        );
        // The retired name is a sibling of the original, suffixed, not some
        // unrelated location.
        assert_eq!(retired.parent(), old_dir.parent());
        assert!(retired
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("old.migrated-"));
    }

    #[test]
    fn no_op_when_new_dir_already_has_content() {
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");
        populate_old_dir(&old_dir);
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(new_dir.join("settings.json"), r#"{"threshold":50}"#).unwrap();

        let outcome = migrate_app_data(&old_dir, &new_dir);

        assert_eq!(outcome, MigrationOutcome::NewDirAlreadyPopulated);
        // Nothing about either directory was touched.
        assert!(old_dir.exists());
        assert!(old_dir.join("accounts").join("sequence.json").exists());
        assert_eq!(
            fs::read_to_string(new_dir.join("settings.json")).unwrap(),
            r#"{"threshold":50}"#
        );
        // The old data was never copied in on top of the newer store.
        assert!(!new_dir.join("history.sqlite3").exists());
    }

    #[test]
    fn no_op_when_new_dir_exists_but_is_empty() {
        // Tauri may create app_data_dir() as an empty directory before this
        // runs; an empty new_dir must not block migration the way a
        // populated one does.
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");
        populate_old_dir(&old_dir);
        fs::create_dir_all(&new_dir).unwrap();

        let outcome = migrate_app_data(&old_dir, &new_dir);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        assert!(new_dir.join("settings.json").exists());
    }

    #[test]
    fn diagnostic_logs_in_target_do_not_block_or_get_overwritten_by_migration() {
        let root = tempdir().unwrap();
        let old_dir = root.path().join("dev.apex36.cc-logins");
        let new_dir = root.path().join("cc-logins");
        populate_old_dir(&old_dir);
        fs::write(old_dir.join("app.log"), "legacy log").unwrap();
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(new_dir.join("app.log"), "canonical log").unwrap();
        fs::write(new_dir.join("app.log.old"), "canonical old log").unwrap();

        let outcome = migrate_app_data(&old_dir, &new_dir);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        assert_eq!(
            fs::read_to_string(new_dir.join("app.log")).unwrap(),
            "canonical log"
        );
        assert!(new_dir.join("accounts").join("sequence.json").exists());
        assert!(new_dir.join("history.sqlite3").exists());
    }

    #[test]
    fn no_op_when_old_dir_absent() {
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old"); // never created
        let new_dir = root.path().join("new");

        let outcome = migrate_app_data(&old_dir, &new_dir);

        assert_eq!(outcome, MigrationOutcome::NoOldData);
        assert!(!new_dir.exists());
        assert!(!old_dir.exists());
    }

    #[test]
    fn migration_chain_prefers_the_most_recent_populated_identifier() {
        let root = tempdir().unwrap();
        let recent = root.path().join("dev.apex36.cc-logins");
        let ancient = root.path().join("dev.apex36.claude-account-switcher");
        let new_dir = root.path().join("cc-logins");
        populate_old_dir(&recent);
        fs::create_dir_all(&ancient).unwrap();
        fs::write(ancient.join("settings.json"), r#"{"threshold":50}"#).unwrap();

        let outcome = migrate_app_data_chain(&[recent.clone(), ancient.clone()], &new_dir);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        assert_eq!(
            fs::read_to_string(new_dir.join("settings.json")).unwrap(),
            r#"{"threshold":90}"#
        );
        assert!(
            ancient.exists(),
            "the older candidate must remain untouched"
        );
    }

    #[test]
    fn migration_chain_falls_back_to_the_older_identifier() {
        let root = tempdir().unwrap();
        let missing_recent = root.path().join("dev.apex36.cc-logins");
        let ancient = root.path().join("dev.apex36.claude-account-switcher");
        let new_dir = root.path().join("cc-logins");
        populate_old_dir(&ancient);

        let outcome = migrate_app_data_chain(&[missing_recent, ancient], &new_dir);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        assert!(new_dir.join("accounts").join("sequence.json").exists());
    }

    #[test]
    fn verification_failure_leaves_old_dir_untouched_and_returns_failure() {
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");

        // A sequence.json that exists but does not parse -- verification
        // must catch this rather than silently treating it as zero accounts.
        let accounts_dir = old_dir.join("accounts");
        fs::create_dir_all(&accounts_dir).unwrap();
        fs::write(accounts_dir.join("sequence.json"), "{ not valid json").unwrap();
        fs::write(old_dir.join("settings.json"), r#"{"threshold":90}"#).unwrap();

        let outcome = migrate_app_data(&old_dir, &new_dir);

        let reason = match outcome {
            MigrationOutcome::Failed { reason } => reason,
            other => panic!("expected Failed, got {other:?}"),
        };
        assert!(reason.contains("parse"), "unexpected reason: {reason}");

        // The original is completely intact -- this is the load-bearing
        // guarantee. It was never renamed, never partially modified.
        assert!(old_dir.exists());
        assert_eq!(
            fs::read_to_string(old_dir.join("settings.json")).unwrap(),
            r#"{"threshold":90}"#
        );
        assert_eq!(
            fs::read_to_string(old_dir.join("accounts").join("sequence.json")).unwrap(),
            "{ not valid json"
        );
        // No "<old>.migrated-*" sibling was created -- nothing was retired.
        let siblings: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !siblings.iter().any(|n| n.contains(".migrated-")),
            "unexpected retired directory among: {siblings:?}"
        );
    }

    #[test]
    fn migrated_accounts_count_matches_registered_accounts() {
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");
        write_sequence(&old_dir, 5);

        let outcome = migrate_app_data(&old_dir, &new_dir);

        match outcome {
            MigrationOutcome::Migrated {
                accounts_migrated, ..
            } => {
                assert_eq!(accounts_migrated, 5);
            }
            other => panic!("expected Migrated, got {other:?}"),
        }
    }

    #[test]
    fn migrated_with_no_accounts_at_all_still_succeeds() {
        // A data dir that only ever held settings.json (no accounts added
        // yet) has no accounts/sequence.json at all -- must not be treated
        // as a verification failure.
        let root = tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("settings.json"), r#"{"threshold":90}"#).unwrap();

        let outcome = migrate_app_data(&old_dir, &new_dir);

        match outcome {
            MigrationOutcome::Migrated {
                accounts_migrated, ..
            } => {
                assert_eq!(accounts_migrated, 0);
            }
            other => panic!("expected Migrated, got {other:?}"),
        }
        assert!(new_dir.join("settings.json").exists());
    }

    #[test]
    fn dir_has_content_treats_missing_and_empty_dirs_as_no_content() {
        let root = tempdir().unwrap();
        let missing = root.path().join("missing");
        let empty = root.path().join("empty");
        let populated = root.path().join("populated");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir_all(&populated).unwrap();
        fs::write(populated.join("file.txt"), b"x").unwrap();

        assert!(!dir_has_content(&missing));
        assert!(!dir_has_content(&empty));
        assert!(dir_has_content(&populated));
    }

    #[test]
    fn managed_content_ignores_only_diagnostic_logs() {
        let root = tempdir().unwrap();
        let dir = root.path().join("cc-logins");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.log"), "current").unwrap();
        fs::write(dir.join("app.log.old"), "previous").unwrap();
        assert!(!dir_has_managed_content(&dir));

        fs::write(dir.join("settings.json"), "{}").unwrap();
        assert!(dir_has_managed_content(&dir));
    }
}
