//! State persistence infrastructure: atomic writes, file validation, and
//! the exclusive state lock.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::error::ConfigError;

/// Reject a `.stitch/` directory that exists but is a symlink or otherwise
/// non-directory. Missing directories are legal (empty state).
///
/// This guards every reader of `.stitch/state.toml`: if the parent is a
/// symlink, the state file resolves outside the repo and must not be trusted.
pub(crate) fn validate_stitch_dir(path: &Path) -> Result<(), ConfigError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => Err(ConfigError::Read(
            std::io::Error::other("refusing symlinked or non-directory state directory"),
            path.to_path_buf(),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ConfigError::Read(e, path.to_path_buf())),
    }
}

fn validate_regular_file(path: &Path, kind: &str) -> Result<(), ConfigError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(ConfigError::Read(
            std::io::Error::other(format!("refusing symlinked or non-regular {kind} file")),
            path.to_path_buf(),
        )),
        Ok(meta) if meta.nlink() > 1 => Err(ConfigError::Read(
            std::io::Error::other(format!(
                "refusing hard-linked {kind} file (multiple paths to the same inode)"
            )),
            path.to_path_buf(),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ConfigError::Read(e, path.to_path_buf())),
    }
}

/// Reject a `state.toml` that exists but is a symlink or otherwise
/// non-regular. Missing files are legal (empty state).
///
/// State is the tool's authoritative inventory; we must never read bytes from
/// a path that could be authored outside the repo.
pub(crate) fn validate_state_file(path: &Path) -> Result<(), ConfigError> {
    validate_regular_file(path, "state")
}

/// Reject a `stitch.toml` that exists but is a symlink or otherwise
/// non-regular, or hard-linked to another path. Missing files are legal (empty
/// authored config).
///
/// Authored config is human-written; we must never read bytes from a path that
/// could be authored outside the repo and influence hook or store behavior.
pub(crate) fn validate_authored_file(path: &Path) -> Result<(), ConfigError> {
    validate_regular_file(path, "authored config")
}

/// Validate an existing atomic-write destination without mutating it.
pub fn validate_atomic_write_target(path: &Path) -> Result<(), ConfigError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let meta = std::fs::symlink_metadata(dir).map_err(|e| ConfigError::Write(e, dir.into()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(ConfigError::Write(
            std::io::Error::other("refusing non-directory or symlinked state parent"),
            dir.into(),
        ));
    }
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(ConfigError::Write(
            std::io::Error::other("refusing to replace symlinked state file"),
            path.to_path_buf(),
        ));
    }
    Ok(())
}

/// Atomically write `contents` to `path` via a temp file in the same directory
/// then rename. On Linux `rename(2)` is atomic for same-filesystem paths, so
/// the destination is never truncated or partially written. The file is synced
/// before the rename and its parent directory after it. A parent-directory sync
/// failure is reported as a committed write: callers must not roll back work
/// that the renamed state file already records. The exclusive random temp name
/// avoids collisions between concurrent stitch processes. The temp file is
/// cleaned up on errors before the rename.
pub fn atomic_write(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    // State must never be written through a pre-existing symlinked parent
    // (notably a hostile `.stitch -> elsewhere`). The final-syscall race is
    // outside stitch's same-UID threat model, but all state observed before
    // the operation is rejected rather than followed.
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(ConfigError::Write(
                std::io::Error::other("refusing non-directory or symlinked state parent"),
                dir,
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e, dir.clone()))?;
            let meta =
                std::fs::symlink_metadata(&dir).map_err(|e| ConfigError::Write(e, dir.clone()))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(ConfigError::Write(
                    std::io::Error::other("refusing non-directory or symlinked state parent"),
                    dir,
                ));
            }
        }
        Err(e) => return Err(ConfigError::Write(e, dir)),
    }
    validate_atomic_write_target(path)?;
    let prefix = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stitch".into());
    let mut random = [0_u8; 16];
    let read = unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) };
    if read != random.len() as isize {
        return Err(ConfigError::Write(
            std::io::Error::last_os_error(),
            path.to_path_buf(),
        ));
    }
    let tmp_path = dir.join(format!(
        ".{prefix}.{:032x}.tmp",
        u128::from_le_bytes(random)
    ));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| ConfigError::Write(e, tmp_path.clone()))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| ConfigError::Write(e, tmp_path.clone()))?;
        f.sync_all()
            .map_err(|e| ConfigError::Write(e, tmp_path.clone()))?;
        std::fs::rename(&tmp_path, path).map_err(|e| ConfigError::Write(e, path.to_path_buf()))?;
        let directory = std::fs::File::open(&dir)
            .map_err(|e| ConfigError::CommittedWrite(e, path.to_path_buf()))?;
        directory
            .sync_all()
            .map_err(|e| ConfigError::CommittedWrite(e, path.to_path_buf()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Exclusive lock on `.stitch/state.lock` via `flock(2)`. Held for the
/// duration of a mutating command (load → mutate → save) to serialize
/// concurrent `stitch add` etc. The lock file is created if missing at
/// `0600`; the lock is advisory and blocking (Linux `LOCK_EX`).
///
/// This prevents the orphan/prune data-loss path where two concurrent adds
/// both read an empty state, each insert one store, and the last writer wins,
/// leaving links without a covering state entry.
#[derive(Debug)]
pub struct StateLock {
    _file: std::fs::File,
}

impl StateLock {
    /// Acquire an exclusive lock for `repo_root`. Blocks until available.
    /// Creates `.stitch` (and the lock file) if missing — used by state
    /// writers (`add`, `remove`, `import`, `migrate`), which create state
    /// anyway.
    pub fn exclusive(repo_root: &Path) -> Result<Self, ConfigError> {
        let stitch_dir = repo_root.join(".stitch");
        // Validate or create .stitch as a real directory (not symlink).
        match std::fs::symlink_metadata(&stitch_dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(ConfigError::Write(
                    std::io::Error::other("refusing non-directory or symlinked state parent"),
                    stitch_dir,
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&stitch_dir)
                    .map_err(|e| ConfigError::Write(e, stitch_dir.clone()))?;
            }
            Err(e) => return Err(ConfigError::Write(e, stitch_dir)),
        }
        Self::acquire(repo_root)
    }

    /// Acquire the exclusive lock only when `.stitch` already exists — for
    /// mutators that never write state (`apply`, `prune --yes`). A repo with
    /// no state directory has nothing to serialize, so `Ok(None)`. A
    /// symlinked `.stitch` is refused rather than followed.
    pub fn exclusive_if_present(repo_root: &Path) -> Result<Option<Self>, ConfigError> {
        let stitch_dir = repo_root.join(".stitch");
        match std::fs::symlink_metadata(&stitch_dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => Err(ConfigError::Write(
                std::io::Error::other("refusing non-directory or symlinked state parent"),
                stitch_dir,
            )),
            Ok(_) => Self::acquire(repo_root).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ConfigError::Write(e, stitch_dir)),
        }
    }

    fn acquire(repo_root: &Path) -> Result<Self, ConfigError> {
        let stitch_dir = repo_root.join(".stitch");
        let lock_path = stitch_dir.join("state.lock");
        // Open or create the lock file without following symlinks. The 0600
        // mode applies only at creation (`O_CREAT|O_EXCL`); an existing file
        // is opened as-is and its permissions are NEVER touched — chmodding
        // the inode would also re-permission every hard link to it.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lock_path)
                .map_err(|e| ConfigError::Write(e, lock_path.clone()))?,
            Err(e) => return Err(ConfigError::Write(e, lock_path)),
        };
        // `create_new` never follows a symlink, and the existing-path open now
        // refuses symlinks too (`O_NOFOLLOW`). The metadata check below is
        // defense-in-depth for a symlink installed after the open wins a race.
        if std::fs::symlink_metadata(&lock_path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(ConfigError::Write(
                std::io::Error::other("refusing symlinked state lock file"),
                lock_path,
            ));
        }
        // Blocking exclusive flock.
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            return Err(ConfigError::Write(
                std::io::Error::last_os_error(),
                lock_path,
            ));
        }
        Ok(Self { _file: file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atomic_write_rejects_symlinked_state_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(external.path(), tmp.path().join(".stitch")).unwrap();

        let err = atomic_write(&tmp.path().join(".stitch/state.toml"), "state").unwrap_err();
        assert!(err.to_string().contains("symlinked state parent"));
        assert!(!external.path().join("state.toml").exists());
    }
}
