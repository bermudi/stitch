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
                        let source_is_symlink = std::fs::symlink_metadata(source)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false);

                        if source_is_symlink {
                            // The desired link points directly at the source
                            // symlink entry, not through its target. Preserve
                            // that indirection (including dangling or relative
                            // targets) by comparing the link paths without
                            // following the source symlink.
                            let resolved_abs = if resolved.is_absolute() {
                                resolved.clone()
                            } else {
                                target.parent().unwrap_or(Path::new(".")).join(&resolved)
                            };
                            let source_abs = if source.is_absolute() {
                                source.to_path_buf()
                            } else {
                                std::env::current_dir()
                                    .map(|cwd| cwd.join(source))
                                    .unwrap_or_else(|_| source.to_path_buf())
                            };

                            if normalize_lexical(&resolved_abs) == normalize_lexical(&source_abs) {
                                LinkStatus::Linked
                            } else {
                                LinkStatus::Broken(resolved)
                            }
                        } else {
                            let source_abs = if source.exists() {
                                source
                                    .canonicalize()
                                    .unwrap_or_else(|_| source.to_path_buf())
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

/// Create a symlink at an absent `target` pointing to `source`.
/// Parent directories are created as needed. Never removes an existing target:
/// callers classify known stale links before calling this, and an unexpected
/// link appearing in the gap must remain a conflict rather than be clobbered.
pub fn create_link(target: &Path, source: &Path) -> Result<(), LinkError> {
    // Ensure the source exists.
    if !source.exists() {
        return Err(LinkError::SourceMissing(source.to_path_buf()));
    }

    // Create parent directory for the target.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LinkError::Mkdir(e, parent.to_path_buf()))?;
    }

    let source_abs = source
        .canonicalize()
        .map_err(|e| LinkError::Canonicalize(e, source.to_path_buf()))?;

    symlink(&source_abs, target).map_err(|e| LinkError::Create(e, target.to_path_buf()))?;

    Ok(())
}

/// Create a symlink at an absent `target` pointing directly to the source
/// filesystem entry, without following or canonicalizing `source`.
///
/// Use this when `source` is itself a symlink (or any directory entry whose
/// identity matters). The link target is the absolute source path as given, so
/// relative and dangling symlink targets are preserved faithfully. Callers must
/// classify the target first (as with [`create_link`]).
pub fn create_link_to_entry(target: &Path, source: &Path) -> Result<(), LinkError> {
    // `symlink_metadata` does not follow symlinks, so a dangling source is
    // still a valid entry. Regular `exists()` would reject it.
    if std::fs::symlink_metadata(source).is_err() {
        return Err(LinkError::SourceMissing(source.to_path_buf()));
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LinkError::Mkdir(e, parent.to_path_buf()))?;
    }

    // The source is passed as an absolute path by all callers. Use it directly
    // rather than canonicalizing; for a symlink source, canonicalization would
    // collapse the indirection and fail for dangling targets.
    let source_abs = if source.is_absolute() {
        source.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(source))
            .map_err(|e| LinkError::Canonicalize(e, source.to_path_buf()))?
    };

    symlink(&source_abs, target).map_err(|e| LinkError::Create(e, target.to_path_buf()))?;

    Ok(())
}

/// Whether the symlink at `target` resolves beneath `root`.
///
/// Relative symlink targets are resolved against the symlink's own parent
/// directory. Returns `false` for non-symlinks or unreadable links. Existing
/// paths are canonicalized to resolve symlink chains; dangling paths are
/// compared after lexical normalization so ownership checks remain useful for
/// stale links whose source was removed.
pub fn points_into(target: &Path, root: &Path) -> bool {
    let Ok(resolved) = std::fs::read_link(target) else {
        return false;
    };
    let resolved_abs = if resolved.is_absolute() {
        resolved
    } else {
        target.parent().unwrap_or(Path::new(".")).join(resolved)
    };

    // If the path exists, canonicalize to resolve any symlink chains and `..`
    // components. Fall back to lexical normalization if canonicalize fails
    // (e.g. permission error) — never use an unnormalized path.
    let normalized = if resolved_abs.exists() {
        resolved_abs
            .canonicalize()
            .unwrap_or_else(|_| normalize_lexical(&resolved_abs))
    } else {
        normalize_lexical(&resolved_abs)
    };
    let normalized_root = if root.exists() {
        root.canonicalize()
            .unwrap_or_else(|_| normalize_lexical(root))
    } else {
        normalize_lexical(root)
    };

    normalized.starts_with(&normalized_root)
}

/// Whether the symlink at `target` points into `repo_root`.
///
/// Distinguishes stitch-owned links (safe to replace/remove) from foreign ones
/// (stow/chezmoi/Nix/Home-Manager/hand-managed — must never be silently
/// clobbered). This is the repo-scoped form of [`points_into`].
pub fn points_into_repo(target: &Path, repo_root: &Path) -> bool {
    points_into(target, repo_root)
}

/// Lexically normalize a path by collapsing `.` and `..` components without
/// touching the filesystem. The result may still contain symlinks if the path
/// does not exist — use `canonicalize` for existing paths.
pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Remove a symlink at `target` if it points into the given repo root.
/// Returns true if something was removed.
pub fn remove_link(target: &Path, repo_root: &Path) -> Result<bool, LinkError> {
    if !points_into_repo(target, repo_root) {
        return Ok(false);
    }
    std::fs::remove_file(target).map_err(|e| LinkError::Remove(e, target.to_path_buf()))?;
    Ok(true)
}

/// Remove `target` only when it still points exactly at `expected_source` in
/// this repo. Used for whole-directory → file-mode promotion: a link repointed
/// to another store between inspection and removal must remain untouched.
pub fn remove_link_to(
    target: &Path,
    expected_source: &Path,
    repo_root: &Path,
) -> Result<bool, LinkError> {
    if check_link(target, expected_source) != LinkStatus::Linked
        || !points_into_repo(target, repo_root)
    {
        return Ok(false);
    }
    std::fs::remove_file(target).map_err(|e| LinkError::Remove(e, target.to_path_buf()))?;
    Ok(true)
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
        assert_eq!(check_link(&target_file, &source_file), LinkStatus::Missing);

        // Create the link.
        create_link(&target_file, &source_file).unwrap();

        // Now linked.
        assert_eq!(check_link(&target_file, &source_file), LinkStatus::Linked);

        // Read through the link.
        let content = std::fs::read_to_string(&target_file).unwrap();
        assert_eq!(content, "hello");

        // Remove the link.
        assert!(remove_link(&target_file, tmp.path()).unwrap());
        assert_eq!(check_link(&target_file, &source_file), LinkStatus::Missing);
    }

    #[test]
    fn test_create_link_does_not_replace_existing_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let source_a = tmp.path().join("a");
        let source_b = tmp.path().join("b");
        std::fs::write(&source_a, "a").unwrap();
        std::fs::write(&source_b, "b").unwrap();
        let target = tmp.path().join("link");
        create_link(&target, &source_a).unwrap();

        assert!(create_link(&target, &source_b).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a");
    }

    #[test]
    fn test_remove_link_to_requires_expected_source() {
        let tmp = tempfile::tempdir().unwrap();
        let source_a = tmp.path().join("a");
        let source_b = tmp.path().join("b");
        std::fs::write(&source_a, "a").unwrap();
        std::fs::write(&source_b, "b").unwrap();
        let target = tmp.path().join("link");
        create_link(&target, &source_a).unwrap();

        assert!(!remove_link_to(&target, &source_b, tmp.path()).unwrap());
        assert!(target.is_symlink(), "wrong expected source must not unlink");
        assert!(remove_link_to(&target, &source_a, tmp.path()).unwrap());
        assert!(target.symlink_metadata().is_err());
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

    #[test]
    fn test_normalize_lexical_basic() {
        assert_eq!(normalize_lexical(Path::new("/a/b")), PathBuf::from("/a/b"));
        assert_eq!(
            normalize_lexical(Path::new("/a/./b")),
            PathBuf::from("/a/b")
        );
        assert_eq!(normalize_lexical(Path::new("/a/b/..")), PathBuf::from("/a"));
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        // `..` at root stays at root.
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(
            normalize_lexical(Path::new("/a/../../b")),
            PathBuf::from("/b")
        );
    }

    #[test]
    fn test_points_into_repo_rejects_dotdot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let inside = repo.join("config");
        let outside = tmp.path().join("foreign");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // A symlink whose lexical target escapes the repo via `..` —
        // e.g. pointing at repo/../foreign. Exists on disk, so canonicalize
        // resolves it.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(repo.join("..").join("foreign"), &link).unwrap();

        assert!(!points_into_repo(&link, &repo));
    }

    #[test]
    fn test_points_into_repo_accepts_dotdot_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sub = repo.join("a");
        std::fs::create_dir_all(&sub).unwrap();

        // Lexically: repo/a/../a/b  →  repo/a/b (still inside).
        // Dangling path, so normalize_lexical is used.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(repo.join("a").join("..").join("a").join("b"), &link).unwrap();

        assert!(points_into_repo(&link, &repo));
    }

    #[test]
    fn test_points_into_repo_rejects_dangling_dotdot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // A dangling symlink whose target escapes via `..`.
        // repo/../nonexistent — the path does not exist, so normalize_lexical
        // is used and should detect the escape.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(repo.join("..").join("nonexistent"), &link).unwrap();

        assert!(!points_into_repo(&link, &repo));
    }
}
