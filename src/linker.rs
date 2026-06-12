use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    /// Symlink exists and points to the expected target.
    Linked,
    /// No symlink exists.
    Missing,
    /// Something else (file, dir, different symlink) occupies the path.
    Conflict(PathBuf),
    /// Symlink exists but points to a missing or wrong target.
    Broken(PathBuf),
}

/// Check the status of a symlink at `target` pointing to `source`.
pub fn check_link(target: &Path, source: &Path) -> LinkStatus {
    match std::fs::symlink_metadata(target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                match std::fs::read_link(target) {
                    Ok(resolved) => {
                        let source_abs = if source.exists() {
                            source.canonicalize().unwrap_or_else(|_| source.to_path_buf())
                        } else {
                            source.to_path_buf()
                        };
                        let resolved_abs = if resolved.exists() {
                            resolved.canonicalize().unwrap_or(resolved.clone())
                        } else {
                            resolved.clone()
                        };

                        if resolved_abs == source_abs {
                            LinkStatus::Linked
                        } else {
                            LinkStatus::Broken(resolved)
                        }
                    }
                    Err(_) => LinkStatus::Broken(PathBuf::from("(unreadable)")),
                }
            } else {
                // Real file or directory — conflict.
                LinkStatus::Conflict(target.to_path_buf())
            }
        }
        Err(_) => LinkStatus::Missing,
    }
}

/// Create a symlink at `target` pointing to `source`.
/// Parent directories are created as needed.
pub fn create_link(target: &Path, source: &Path) -> Result<(), LinkError> {
    // Ensure the source exists.
    if !source.exists() {
        return Err(LinkError::SourceMissing(source.to_path_buf()));
    }

    // Remove existing symlink/file if it's already a symlink.
    if target.is_symlink() {
        std::fs::remove_file(target).map_err(|e| LinkError::Remove(e, target.to_path_buf()))?;
    }

    // Create parent directory for the target.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LinkError::Mkdir(e, parent.to_path_buf()))?;
    }

    let source_abs = source
        .canonicalize()
        .map_err(|e| LinkError::Canonicalize(e, source.to_path_buf()))?;

    symlink(&source_abs, target).map_err(|e| LinkError::Create(e, target.to_path_buf()))?;

    Ok(())
}

/// Remove a symlink at `target` if it points into the given repo root.
/// Returns true if something was removed.
pub fn remove_link(target: &Path, repo_root: &Path) -> Result<bool, LinkError> {
    if !target.is_symlink() {
        return Ok(false);
    }

    let resolved = std::fs::read_link(target)
        .map_err(|e| LinkError::Read(e, target.to_path_buf()))?;

    // Only remove if it points into our repo.
    let resolved_abs = if resolved.is_absolute() {
        resolved.clone()
    } else {
        target
            .parent()
            .unwrap_or(Path::new("."))
            .join(&resolved)
    };

    if resolved_abs.starts_with(repo_root) || resolved.starts_with(repo_root) {
        std::fs::remove_file(target)
            .map_err(|e| LinkError::Remove(e, target.to_path_buf()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("source does not exist: {0}")]
    SourceMissing(PathBuf),
    #[error("could not create parent directory {1}: {0}")]
    Mkdir(std::io::Error, PathBuf),
    #[error("could not canonicalize source {1}: {0}")]
    Canonicalize(std::io::Error, PathBuf),
    #[error("could not create symlink at {1}: {0}")]
    Create(std::io::Error, PathBuf),
    #[error("could not remove existing symlink at {1}: {0}")]
    Remove(std::io::Error, PathBuf),
    #[error("could not read symlink at {1}: {0}")]
    Read(std::io::Error, PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_check_link() {
        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("source");
        let target_dir = tmp.path().join("target");
        std::fs::create_dir_all(&source_dir).unwrap();

        let source_file = source_dir.join("test.txt");
        std::fs::write(&source_file, "hello").unwrap();

        let target_file = target_dir.join("test.txt");

        // Missing before creation.
        assert_eq!(
            check_link(&target_file, &source_file),
            LinkStatus::Missing
        );

        // Create the link.
        create_link(&target_file, &source_file).unwrap();

        // Now linked.
        assert_eq!(check_link(&target_file, &source_file), LinkStatus::Linked);

        // Read through the link.
        let content = std::fs::read_to_string(&target_file).unwrap();
        assert_eq!(content, "hello");

        // Remove the link.
        assert!(remove_link(&target_file, tmp.path()).unwrap());
        assert_eq!(
            check_link(&target_file, &source_file),
            LinkStatus::Missing
        );
    }

    #[test]
    fn test_conflict_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();

        let target = tmp.path().join("target");
        std::fs::write(&target, "I am a real file").unwrap();

        assert!(matches!(
            check_link(&target, &source),
            LinkStatus::Conflict(_)
        ));
    }

    #[test]
    fn test_remove_only_repo_links() {
        let tmp = tempfile::tempdir().unwrap();
        let other_repo = tmp.path().join("other");
        let our_repo = tmp.path().join("our");
        std::fs::create_dir_all(&other_repo).unwrap();
        std::fs::create_dir_all(&our_repo).unwrap();

        let source = other_repo.join("file.txt");
        std::fs::write(&source, "data").unwrap();

        let target = tmp.path().join("link");
        create_link(&target, &source).unwrap();

        // Should NOT remove — points to other_repo, not our_repo.
        assert!(!remove_link(&target, &our_repo).unwrap());
        assert!(target.exists());
    }
}
