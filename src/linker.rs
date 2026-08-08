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

/// Whether the symlink at `target` points beneath `root` by its *immediate
/// hop* — the readlink is compared lexically (collapsing `.` and `..`
/// components) without following further symlink chains.
///
/// Relative symlink targets are resolved against the symlink's own parent
/// directory. Returns `false` for non-symlinks or unreadable links. This is
/// the narrow, store-scoped check: "does this link point at an entry inside
/// this directory?" A link that points directly at a `root` entry is accepted
/// even when that entry is itself a symlink to an external path (the
/// indirection is not chased). Use [`points_into_repo`] for the broad,
/// repo-scoped ownership decision that *does* follow the chain.
pub fn points_into(target: &Path, root: &Path) -> bool {
    let Ok(resolved) = std::fs::read_link(target) else {
        return false;
    };
    let resolved_abs = if resolved.is_absolute() {
        resolved
    } else {
        target.parent().unwrap_or(Path::new(".")).join(resolved)
    };

    let normalized = normalize_lexical(&resolved_abs);
    let normalized_root = if root.exists() {
        root.canonicalize()
            .unwrap_or_else(|_| normalize_lexical(root))
    } else {
        normalize_lexical(root)
    };

    normalized.starts_with(&normalized_root)
}

/// Whether the symlink at `target` resolves beneath `repo_root`, following the
/// full symlink chain.
///
/// This is the broad repo-ownership predicate that distinguishes stitch-owned
/// links (safe to replace/remove) from foreign ones
/// (stow/chezmoi/Nix/Home-Manager/hand-managed — must never be silently
/// clobbered). Ownership is *canonical*: a link that points *through* a repo
/// gateway symlink to an external path (e.g. `home/file ->
/// repo/gateway/victim` where `repo/gateway -> /external`) resolves outside the
/// repo and is foreign, not repo-owned.
///
/// Resolvable targets are canonicalized (chasing the whole chain). Dangling
/// targets are resolved as far as the filesystem allows — the longest existing
/// prefix is canonicalized and the non-existent tail is appended and
/// lexically normalized — so a link through a *resolvable* gateway to a
/// non-existent victim is still classified as foreign, while a stale stitch
/// link whose source entry was simply removed remains repo-owned and self-heals.
pub fn points_into_repo(target: &Path, repo_root: &Path) -> bool {
    let Ok(resolved) = std::fs::read_link(target) else {
        return false;
    };
    let resolved_abs = if resolved.is_absolute() {
        resolved
    } else {
        target.parent().unwrap_or(Path::new(".")).join(resolved)
    };

    let normalized_root = if repo_root.exists() {
        repo_root
            .canonicalize()
            .unwrap_or_else(|_| normalize_lexical(repo_root))
    } else {
        normalize_lexical(repo_root)
    };

    resolve_as_far_as_possible(&resolved_abs).starts_with(&normalized_root)
}

/// Whether the symlink at `target` points exactly at the repo entry
/// `expected_source` — the exact-entry companion to the broad
/// [`points_into_repo`].
///
/// A stitch-created link may point directly at a repo source entry that is
/// itself a symlink resolving *outside* the repo (e.g. a file-mode store whose
/// `alias` source is a symlink to `/external/real`). The broad canonical
/// [`points_into_repo`] correctly classifies such a link as foreign, but since
/// it points exactly at a configured repo entry it is stitch-owned and safe to
/// manage. This compares the link's readlink against `expected_source` without
/// following `expected_source` (so a symlink source — including a dangling one
/// — is matched by its entry path) and requires `expected_source` to be inside
/// `repo_root`.
pub fn points_at_source(target: &Path, expected_source: &Path, repo_root: &Path) -> bool {
    // The expected source must be a repo entry. `expected_source` is built by
    // callers as `repo_root.join(<store-relative>)`, so it shares repo_root's
    // path prefix — including when repo_root is itself accessed through a
    // symlink. Compare lexically (collapsing `.`/`..`) rather than
    // canonicalizing repo_root, which would diverge from the configured path
    // in the symlinked-repo-root case. `..` escapes are rejected here by the
    // normalization, and at the plan layer by fragment validation.
    let root_norm = normalize_lexical(repo_root);
    let source_abs = if expected_source.is_absolute() {
        expected_source.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expected_source)
    };
    if !normalize_lexical(&source_abs).starts_with(&root_norm) {
        return false;
    }
    // check_link's source-symlink branch compares the readlink against the
    // source entry lexically (without following it) — exactly the exact-entry
    // semantics we want; for a regular source it canonicalizes both sides.
    check_link(target, expected_source) == LinkStatus::Linked
}

/// Resolve `path` as far as the filesystem allows, following symlinks, then
/// lexically normalize any non-existent trailing tail.
///
/// Unlike [`std::fs::canonicalize`], this never fails for a dangling path: the
/// longest existing prefix is canonicalized (chasing symlinks) and the
/// non-existent tail is appended and normalized. This catches links that point
/// *through* a resolvable repo gateway symlink to an external path even when
/// the final destination does not exist, while still keeping a stale stitch
/// link whose source entry was simply removed beneath the repo.
fn resolve_as_far_as_possible(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<PathBuf> = Vec::new();
    // Walk up to the longest existing ancestor. `exists()` follows symlinks,
    // so a dangling symlink component is treated as non-existent and walked
    // past (a dangling gateway is indistinguishable from a removed entry).
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                tail.push(PathBuf::from(name));
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let base = if existing.exists() {
        existing
            .canonicalize()
            .unwrap_or_else(|_| normalize_lexical(&existing))
    } else {
        normalize_lexical(&existing)
    };
    let mut full = base;
    for component in tail.iter().rev() {
        full.push(component);
    }
    normalize_lexical(&full)
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
/// this repo. Used for whole-directory → file-mode promotion and store removal:
/// a link repointed to another store (or a foreign target) between inspection
/// and removal must remain untouched.
///
/// Uses the exact-entry [`points_at_source`] check rather than the broad
/// [`points_into_repo`], so a link pointing directly at a repo source entry
/// that is itself a symlink resolving outside the repo is still recognized as
/// stitch-owned and removed.
pub fn remove_link_to(
    target: &Path,
    expected_source: &Path,
    repo_root: &Path,
) -> Result<bool, LinkError> {
    if !points_at_source(target, expected_source, repo_root) {
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

    #[test]
    fn test_points_into_repo_rejects_source_symlink_resolving_outside() {
        // Broad ownership is canonical: a link pointing at a repo source entry
        // that is itself a symlink to an external path resolves outside the
        // repo, so points_into_repo (the broad predicate) classifies it as
        // foreign. The exact-entry points_at_source check handles the
        // legitimate stitch-managed case (see test_points_at_source_*).
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let store = repo.join("store");
        std::fs::create_dir_all(&store).unwrap();

        let external = tmp.path().join("external").join("real");
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::write(&external, "outside").unwrap();
        let source = store.join("alias");
        std::os::unix::fs::symlink(&external, &source).unwrap();

        let target = tmp.path().join("target");
        create_link_to_entry(&target, &source).unwrap();

        assert!(
            !points_into_repo(&target, &repo),
            "broad canonical check must follow the source symlink out of the repo"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "outside",
            "following the target still resolves through the source symlink"
        );
    }

    #[test]
    fn test_points_into_repo_rejects_gateway_to_outside() {
        // The reported P0: a hand-managed link that points *through* a repo
        // gateway symlink to an external path must be foreign, not repo-owned.
        //
        //   repo/gateway -> /external
        //   home/file    -> repo/gateway/victim
        //
        // The immediate-hop readlink is beneath the repo, but the chain
        // resolves outside it. Broad canonical ownership must reject this.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let external = tmp.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        let victim = external.join("victim");
        std::fs::write(&victim, "foreign").unwrap();

        let gateway = repo.join("gateway");
        std::os::unix::fs::symlink(&external, &gateway).unwrap();

        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join("file");
        std::os::unix::fs::symlink(gateway.join("victim"), &file).unwrap();

        assert!(
            !points_into_repo(&file, &repo),
            "a link through a repo gateway to an external path is foreign"
        );
        // And the broad remove_link must refuse to clobber it.
        assert!(!remove_link(&file, &repo).unwrap());
        assert!(
            file.is_symlink(),
            "foreign gateway link must not be removed"
        );
    }

    #[test]
    fn test_points_into_repo_rejects_dangling_victim_through_gateway() {
        // Same gateway shape, but the victim does not exist. The gateway
        // itself resolves, so partial resolution still follows it out of the
        // repo — the dangling-through-gateway link is foreign, not a stale
        // stitch link.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let external = tmp.path().join("external");
        std::fs::create_dir_all(&external).unwrap();

        let gateway = repo.join("gateway");
        std::os::unix::fs::symlink(&external, &gateway).unwrap();

        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join("file");
        std::os::unix::fs::symlink(gateway.join("gone"), &file).unwrap();

        assert!(
            !points_into_repo(&file, &repo),
            "a dangling link through a resolvable gateway is still foreign"
        );
    }

    #[test]
    fn test_points_at_source_accepts_source_symlink_resolving_outside() {
        // The exact-entry check recognizes a stitch-created link pointing
        // directly at a repo source entry, even when that entry is a symlink
        // resolving outside the repo.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let store = repo.join("store");
        std::fs::create_dir_all(&store).unwrap();

        let external = tmp.path().join("external").join("real");
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::write(&external, "outside").unwrap();
        let source = store.join("alias");
        std::os::unix::fs::symlink(&external, &source).unwrap();

        let target = tmp.path().join("target");
        create_link_to_entry(&target, &source).unwrap();

        assert!(points_at_source(&target, &source, &repo));
        // A link pointing elsewhere (through the gateway) is not at this source.
        let other = tmp.path().join("home").join("other");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(gateway_victim(&repo), &other).unwrap();
        assert!(!points_at_source(&other, &source, &repo));
    }

    #[test]
    fn test_points_at_source_rejects_foreign_link_at_configured_target() {
        // A foreign link sitting at a configured target location must not be
        // mistaken for stitch-owned just because the expected source is a repo
        // entry: the link does not point at it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let store = repo.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let source = store.join("file");
        std::fs::write(&source, "repo").unwrap();

        let foreign_target = tmp.path().join("external").join("real");
        std::fs::create_dir_all(foreign_target.parent().unwrap()).unwrap();
        std::fs::write(&foreign_target, "foreign").unwrap();

        let target = tmp.path().join("target");
        std::os::unix::fs::symlink(&foreign_target, &target).unwrap();

        assert!(
            !points_at_source(&target, &source, &repo),
            "a foreign link is not at the expected repo source"
        );
    }

    #[test]
    fn test_remove_link_to_removes_target_to_source_symlink_resolving_outside() {
        // remove_link_to uses the exact-entry check, so a stitch-created link
        // pointing at a source symlink (resolving outside the repo) is removed.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let store = repo.join("store");
        std::fs::create_dir_all(&store).unwrap();

        let external = tmp.path().join("external").join("real");
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::write(&external, "outside").unwrap();
        let source = store.join("alias");
        std::os::unix::fs::symlink(&external, &source).unwrap();

        let target = tmp.path().join("target");
        create_link_to_entry(&target, &source).unwrap();
        assert!(target.is_symlink());

        // remove_link (broad) refuses — the link resolves outside the repo.
        assert!(
            !remove_link(&target, &repo).unwrap(),
            "broad remove_link must not touch a link resolving outside the repo"
        );
        assert!(target.is_symlink());

        // remove_link_to (exact-entry) removes it: it points exactly at the
        // configured repo source entry.
        assert!(remove_link_to(&target, &source, &repo).unwrap());
        assert!(target.symlink_metadata().is_err());
    }

    /// Helper: build `repo/gateway -> /external` and return `repo/gateway/x`.
    fn gateway_victim(repo: &Path) -> PathBuf {
        let tmp = repo.parent().unwrap();
        let external = tmp.join("external");
        std::fs::create_dir_all(&external).unwrap();
        let gateway = repo.join("gateway");
        if !gateway.exists() {
            std::os::unix::fs::symlink(&external, &gateway).unwrap();
        }
        gateway.join("victim")
    }
}
