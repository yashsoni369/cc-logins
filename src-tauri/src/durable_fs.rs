//! Durable same-directory staging and exact file-state restoration.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub existed: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum DurableFsError {
    #[error("could not stage sibling {path}: {source}")]
    Stage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not replace {target} with staged file {staged}: {source}")]
    Replace {
        target: PathBuf,
        staged: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not remove {path} while restoring absence: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not sync parent directory {path}: {source}")]
    SyncParent {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl From<DurableFsError> for io::Error {
    fn from(error: DurableFsError) -> Self {
        io::Error::other(error)
    }
}

pub fn snapshot(path: &Path) -> io::Result<FileState> {
    match fs::read(path) {
        Ok(bytes) => Ok(FileState {
            existed: true,
            bytes,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(FileState {
            existed: false,
            bytes: Vec::new(),
        }),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub struct StagedFile {
    target: PathBuf,
    staged: PathBuf,
}

impl StagedFile {
    pub fn path(&self) -> &Path {
        &self.staged
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Publish the staged file. On failure the stage is deliberately retained
    /// at [`Self::path`] so transaction recovery has durable material to use.
    pub fn commit(self) -> Result<(), DurableFsError> {
        replace(&self.staged, &self.target).map_err(|source| DurableFsError::Replace {
            target: self.target,
            staged: self.staged,
            source,
        })
    }
}

pub fn stage_sibling(
    target: &Path,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<StagedFile, DurableFsError> {
    #[cfg(windows)]
    let _ = unix_mode;
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| DurableFsError::Stage {
        path: target.to_path_buf(),
        source,
    })?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let staged = parent.join(format!(
        ".{name}.{}.{}.stage",
        std::process::id(),
        NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| -> io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(mode) = unix_mode {
                options.mode(mode);
            }
        }
        let mut file = options.open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = unix_mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&staged);
        return Err(DurableFsError::Stage {
            path: staged,
            source,
        });
    }
    Ok(StagedFile {
        target: target.to_path_buf(),
        staged,
    })
}

pub fn restore(
    path: &Path,
    state: &FileState,
    unix_mode: Option<u32>,
) -> Result<(), DurableFsError> {
    if state.existed {
        return stage_sibling(path, &state.bytes, unix_mode)?.commit();
    }
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DurableFsError::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn sync_parent(path: &Path) -> Result<(), DurableFsError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    sync_directory(parent).map_err(|source| DurableFsError::SyncParent {
        path: parent.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn replace(staged: &Path, target: &Path) -> io::Result<()> {
    fs::rename(staged, target)?;
    sync_directory(target.parent().unwrap_or_else(|| Path::new(".")))
}

#[cfg(windows)]
fn replace(staged: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH,
    };

    let staged_wide: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        if target.exists() {
            ReplaceFileW(
                target_wide.as_ptr(),
                staged_wide.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } else {
            MoveFileExW(
                staged_wide.as_ptr(),
                target_wide.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_: &Path) -> io::Result<()> {
    // ReplaceFileW and MoveFileExW(MOVEFILE_WRITE_THROUGH) provide the Windows
    // write-through boundary; Windows does not expose portable directory fsync.
    Ok(())
}

static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_is_non_mutating_until_commit_then_replaces() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("state.json");
        fs::write(&target, b"before").unwrap();
        let stage = stage_sibling(&target, b"after", Some(0o600)).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert_eq!(fs::read(stage.path()).unwrap(), b"after");
        stage.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"after");
    }

    #[test]
    fn commit_creates_an_absent_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("new.json");
        stage_sibling(&target, b"new", Some(0o600))
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(fs::read(target).unwrap(), b"new");
    }

    #[test]
    fn snapshot_and_restore_preserve_absence_empty_and_bytes() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("state.json");
        let absent = snapshot(&target).unwrap();
        fs::write(&target, b"").unwrap();
        let empty = snapshot(&target).unwrap();
        fs::write(&target, b"bytes").unwrap();
        restore(&target, &empty, Some(0o600)).unwrap();
        assert!(target.exists());
        assert_eq!(fs::read(&target).unwrap(), b"");
        restore(&target, &absent, Some(0o600)).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn stage_failure_does_not_mutate_target() {
        let root = tempfile::tempdir().unwrap();
        let parent_file = root.path().join("not-a-directory");
        fs::write(&parent_file, b"block").unwrap();
        let target = parent_file.join("state.json");
        assert!(stage_sibling(&target, b"after", Some(0o600)).is_err());
        assert_eq!(fs::read(parent_file).unwrap(), b"block");
    }

    #[cfg(windows)]
    #[test]
    fn failed_replace_retains_stage_for_recovery() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("state.json");
        fs::write(&target, b"before").unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&target)
            .unwrap();
        let stage = stage_sibling(&target, b"after", None).unwrap();
        let staged = stage.path().to_path_buf();
        assert!(stage.commit().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert_eq!(fs::read(staged).unwrap(), b"after");
    }

    #[cfg(unix)]
    #[test]
    fn requested_permissions_are_applied() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("secret");
        stage_sibling(&target, b"secret", Some(0o600))
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
