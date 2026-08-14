//! Filesystem identity primitives.
//!
//! Unified home for "how we identify filesystem objects" — inode/directory
//! identity helpers used by `main.rs` (add rollback) and `plan_exec.rs`
//! (hook boundary revalidation). Named `fsutil` rather than `fs` to avoid
//! shadowing `std::fs` in the ~15 files that import it.
//!
//! `StateLock`/`atomic_write` are *not* here — they return `ConfigError` and
//! encode `.stitch/`-specific semantics. They stay in `config/`.

use crate::error::StitchError;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// ---------------------------------------------------------------------------
// Inode identity (used by `add` rollback in the command layer).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InodeIdentity {
    pub dev: u64,
    pub ino: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CreatedDirectory {
    pub path: std::path::PathBuf,
    pub identity: InodeIdentity,
}

pub(crate) fn inode_identity(path: &Path) -> Result<InodeIdentity, StitchError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        StitchError::io_context(format!("inspecting {}", path.display()), error)
    })?;
    Ok(InodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

pub(crate) fn ensure_inode_identity(
    path: &Path,
    expected: InodeIdentity,
    context: &str,
) -> Result<(), StitchError> {
    let actual = inode_identity(path)?;
    if actual != expected {
        return Err(StitchError::internal(format!(
            "{context}: {} changed identity",
            path.display()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory identity (used by `apply`/`plan` boundary revalidation).
// ---------------------------------------------------------------------------

pub(crate) fn filesystem_identity(path: &Path, label: &str) -> Result<(u64, u64), StitchError> {
    // Repository aliases are supported, so follow the root entry and pin the
    // directory it resolves to. Repointing the alias changes this identity.
    let meta = std::fs::metadata(path)
        .map_err(|e| StitchError::io_context(format!("{label} {}: metadata", path.display()), e))?;
    if !meta.file_type().is_dir() {
        return Err(StitchError::internal(format!(
            "{label} {} does not resolve to a directory",
            path.display()
        )));
    }
    Ok((meta.dev(), meta.ino()))
}

pub(crate) fn ensure_filesystem_identity(
    path: &Path,
    expected: (u64, u64),
    context: &str,
    label: &str,
) -> Result<(), StitchError> {
    let actual = filesystem_identity(path, label)?;
    if actual != expected {
        return Err(StitchError::internal(format!(
            "{context}: {}",
            path.display()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory identity with `String` error type (used by `plan_exec`).
// Separate from `filesystem_identity` which returns `StitchError`; unifying
// the error types is a logic change, out of scope for this refactor.
// ---------------------------------------------------------------------------

pub(crate) fn directory_identity(path: &Path) -> Result<(u64, u64), String> {
    // Repository aliases are supported; pin the directory they resolve to.
    // Repointing an alias during a hook changes this device/inode identity.
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.display()))?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "{} does not resolve to a directory",
            path.display()
        ));
    }
    Ok((meta.dev(), meta.ino()))
}

pub(crate) fn require_directory_identity(
    path: &Path,
    expected: (u64, u64),
    context: &str,
) -> Result<(), String> {
    if directory_identity(path)? != expected {
        return Err(format!("{context}: {}", path.display()));
    }
    Ok(())
}
