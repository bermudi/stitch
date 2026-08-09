use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Component, Path, PathBuf};

/// Linux's pathname symlink-expansion budget.
const MAX_SYMLINK_EXPANSIONS: usize = 40;

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
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(target) {
            Ok(resolved) => {
                let source_meta = std::fs::symlink_metadata(source).ok();
                let linked = if source_meta
                    .as_ref()
                    .is_some_and(|meta| meta.file_type().is_symlink())
                {
                    // A source symlink is an entry we intentionally preserve,
                    // rather than an endpoint to canonicalize. Compare the
                    // actual directory entry identity so alternate spellings
                    // cannot turn `alias/.` or `alias/..` into `alias`.
                    is_direct_entry_path(source)
                        && is_direct_entry_path(&resolved)
                        && link_target_path(target, &resolved)
                            .ok()
                            .and_then(|path| std::fs::symlink_metadata(path).ok())
                            .is_some_and(|meta| same_entry(&meta, source_meta.as_ref().unwrap()))
                } else {
                    let source = source.canonicalize().ok();
                    let resolved = link_target_path(target, &resolved)
                        .ok()
                        .and_then(|path| path.canonicalize().ok());
                    source.is_some() && source == resolved
                };
                if linked {
                    LinkStatus::Linked
                } else {
                    LinkStatus::Broken(resolved)
                }
            }
            Err(_) => LinkStatus::Broken(PathBuf::from("(unreadable)")),
        },
        Ok(_) => LinkStatus::Conflict(target.to_path_buf()),
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

    let source_abs =
        absolute_path(source).map_err(|e| LinkError::Canonicalize(e, source.to_path_buf()))?;
    symlink(&source_abs, target).map_err(|e| LinkError::Create(e, target.to_path_buf()))?;

    Ok(())
}

/// Create a normal link only when its resolved source remains in its
/// configured root.
pub fn create_link_in(target: &Path, source: &Path, source_root: &Path) -> Result<(), LinkError> {
    if !is_real_directory(source_root) || !source_resolves_within(source, source_root) {
        return Err(LinkError::SourceOutsideRoot(
            source.to_path_buf(),
            source_root.to_path_buf(),
        ));
    }
    create_link(target, source)
}

/// Create an entry-preserving link only when every *ancestor* of `source`
/// resolves inside `source_root`. The terminal entry may itself be a symlink
/// to an external target; that is the deliberately narrow source-symlink
/// exception handled by [`points_at_source`].
pub fn create_link_to_entry_in(
    target: &Path,
    source: &Path,
    source_root: &Path,
) -> Result<(), LinkError> {
    if !is_real_directory(source_root) || !source_ancestors_within(source, source_root) {
        return Err(LinkError::SourceOutsideRoot(
            source.to_path_buf(),
            source_root.to_path_buf(),
        ));
    }
    create_link_to_entry(target, source)
}

/// Whether the symlink at `target` resolves inside `root`.
///
/// This is the store-scoped ownership predicate. Like [`points_into_repo`], it
/// follows every component symlink, rather than accepting an immediate-hop
/// spelling beneath `root`: a gateway from one store into a sibling store is
/// not ownership of the first store.
pub fn points_into(target: &Path, root: &Path) -> bool {
    let Some(root) = resolved_directory(root) else {
        return false;
    };
    let Ok(link) = std::fs::read_link(target) else {
        return false;
    };
    let Ok(path) = link_target_path(target, &link) else {
        return false;
    };
    resolve_path(&path)
        .map(|path| path.starts_with(&root))
        .unwrap_or(false)
}

/// Whether the symlink at `target` resolves beneath `repo_root`, following the
/// complete component-by-component symlink chain.
///
/// Unlike a lexical prefix check, this recognizes that `repo/gateway/file`
/// may resolve outside the repo. It deliberately resolves dangling gateway
/// entries too: `gateway -> /external/missing` is enough to make
/// `gateway/file` foreign even though the final file is absent. Any I/O error,
/// loop, malformed traversal after a missing component, or more than Linux's
/// 40 symlink expansions fails closed.
pub fn points_into_repo(target: &Path, repo_root: &Path) -> bool {
    points_into(target, repo_root)
}

/// Whether `source` is lexically beneath `root` and resolving its ancestors
/// never leaves that root. The terminal entry is deliberately not followed.
///
/// Callers use this before creating a link to a configured source symlink: an
/// external terminal source is valid, but an ancestor gateway out of the store
/// or render tree is not.
pub fn source_ancestors_within(source: &Path, root: &Path) -> bool {
    if !is_direct_entry_path(source) {
        return false;
    }
    let Ok(source) = absolute_path(source) else {
        return false;
    };
    let Ok(root_path) = absolute_path(root) else {
        return false;
    };
    let Ok(relative) = source.strip_prefix(&root_path) else {
        return false;
    };
    let Some(root) = resolved_directory(&root_path) else {
        return false;
    };

    let mut components = path_components(relative);
    // `source` is a direct entry path, so the final component is its entry
    // name. Resolve only the parent; following the terminal source symlink is
    // the exception this helper exists to preserve.
    if components.pop_back().is_none() {
        return true;
    }
    resolve_components(root.clone(), components, Some(&root)).is_ok()
}

/// Whether a normal (non-entry-preserving) source fully resolves within
/// `root`. This is the companion to [`source_ancestors_within`] for regular
/// files and directories.
pub fn source_resolves_within(source: &Path, root: &Path) -> bool {
    source_ancestors_within(source, root)
        && resolved_directory(root).is_some_and(|root| {
            resolve_path(source)
                .map(|source| source.starts_with(root))
                .unwrap_or(false)
        })
}

/// Whether the symlink at `target` points at this exact configured source
/// *symlink entry*.
///
/// This is intentionally not a general "does the target resolve to source"
/// predicate. The broad case belongs to [`points_into_repo`]. This narrow
/// exception exists only for a configured source entry that is itself a
/// symlink (including a dangling one) and whose ancestors remain in the repo.
/// The immediate readlink is compared as a directory-entry identity, never by
/// lexical normalization. Therefore `alias/`, `alias/.`, terminal `..`, and
/// a parent path that leaves the repo cannot masquerade as `alias`.
pub fn points_at_source(target: &Path, expected_source: &Path, repo_root: &Path) -> bool {
    if !is_direct_entry_path(expected_source)
        || !source_ancestors_within(expected_source, repo_root)
    {
        return false;
    }
    let Ok(source_meta) = std::fs::symlink_metadata(expected_source) else {
        return false;
    };
    if !source_meta.file_type().is_symlink() {
        return false;
    }

    let Ok(link) = std::fs::read_link(target) else {
        return false;
    };
    if !is_direct_entry_path(&link) {
        return false;
    }
    let Ok(link_entry) = link_target_path(target, &link) else {
        return false;
    };
    if !source_ancestors_within(&link_entry, repo_root) {
        return false;
    }
    std::fs::symlink_metadata(link_entry)
        .map(|meta| same_entry(&meta, &source_meta))
        .unwrap_or(false)
}

#[derive(Debug)]
enum ResolveError {
    Io,
    ExpansionLimit,
    EscapesRoot,
    MissingTraversal,
}

/// Resolve a path component by component, preserving POSIX's ordering: a
/// symlink target is spliced into the pending component stream before later
/// `..` components are applied. `std::fs::canonicalize` cannot provide the
/// needed dangling-path result and the old "longest existing prefix" approach
/// got this ordering wrong for `gateway/..`.
fn resolve_path(path: &Path) -> Result<PathBuf, ResolveError> {
    let path = absolute_path(path).map_err(|_| ResolveError::Io)?;
    resolve_components(PathBuf::from("/"), path_components(&path), None)
}

/// Resolve pending components from `current`. If `bound` is supplied, the
/// path starts at that bound and may never leave it; an absolute symlink target
/// may walk its way back to the bound, but may not visit another location.
fn resolve_components(
    mut current: PathBuf,
    mut pending: VecDeque<OsString>,
    bound: Option<&Path>,
) -> Result<PathBuf, ResolveError> {
    let mut expansions = 0;
    let mut reached_bound = bound.is_none() || bound.is_some_and(|root| current.starts_with(root));

    while let Some(component) = pending.pop_front() {
        if component.as_bytes() == b"." {
            continue;
        }
        if component.as_bytes() == b".." {
            current.pop();
            check_bound(&current, bound, &mut reached_bound)?;
            continue;
        }

        let next = current.join(&component);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if expansions >= MAX_SYMLINK_EXPANSIONS {
                    return Err(ResolveError::ExpansionLimit);
                }
                expansions += 1;
                let link = std::fs::read_link(&next).map_err(|_| ResolveError::Io)?;
                let mut replacement = path_components(&link);
                replacement.append(&mut pending);
                pending = replacement;
                if link.is_absolute() {
                    current = PathBuf::from("/");
                    // The absolute target may be a spelling of `bound` itself.
                    // Permit its prefixes until it reaches the bound again.
                    reached_bound = false;
                    check_bound(&current, bound, &mut reached_bound)?;
                }
            }
            Ok(meta) => {
                current.push(&component);
                check_bound(&current, bound, &mut reached_bound)?;
                // POSIX requires a directory for every non-final component,
                // including a following `.` or `..`.
                if !pending.is_empty() && !meta.is_dir() {
                    return Err(ResolveError::Io);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current.push(&component);
                check_bound(&current, bound, &mut reached_bound)?;
                // Once a component is missing the kernel cannot resolve later
                // components. Do not invent ownership by lexically applying a
                // later `.` or `..`; a plain missing tail is safe to retain for
                // stale links.
                if pending
                    .iter()
                    .any(|part| part.as_bytes() == b"." || part.as_bytes() == b"..")
                {
                    return Err(ResolveError::MissingTraversal);
                }
                for part in pending {
                    current.push(part);
                    check_bound(&current, bound, &mut reached_bound)?;
                }
                return Ok(current);
            }
            Err(_) => return Err(ResolveError::Io),
        }
    }
    Ok(current)
}

fn check_bound(
    current: &Path,
    bound: Option<&Path>,
    reached_bound: &mut bool,
) -> Result<(), ResolveError> {
    let Some(bound) = bound else {
        return Ok(());
    };
    if current.starts_with(bound) {
        *reached_bound = true;
        Ok(())
    } else if !*reached_bound && bound.starts_with(current) {
        // Processing an absolute symlink target on the way back to `bound`.
        Ok(())
    } else {
        Err(ResolveError::EscapesRoot)
    }
}

fn path_components(path: &Path) -> VecDeque<OsString> {
    // Preserve `.` entries: after a missing component, they are errors, not
    // lexical no-ops.
    let bytes = path.as_os_str().as_bytes();
    let mut parts: VecDeque<_> = bytes
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| OsString::from_vec(part.to_vec()))
        .collect();
    if bytes.ends_with(b"/") {
        parts.push_back(OsString::from("."));
    }
    parts
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir().map(|cwd| cwd.join(path))
    }
}

fn link_target_path(target: &Path, link: &Path) -> io::Result<PathBuf> {
    if link.is_absolute() {
        Ok(link.to_path_buf())
    } else {
        absolute_path(target.parent().unwrap_or(Path::new("."))).map(|parent| parent.join(link))
    }
}

fn resolved_directory(path: &Path) -> Option<PathBuf> {
    let resolved = resolve_path(path).ok()?;
    std::fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_dir())
        .map(|_| resolved)
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

fn is_direct_entry_path(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    !bytes.is_empty()
        && !bytes.ends_with(b"/")
        && !bytes.ends_with(b"/.")
        && !path.components().any(|part| part == Component::ParentDir)
        && matches!(path.components().next_back(), Some(Component::Normal(_)))
}

fn same_entry(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Lexically normalize a path by collapsing `.` and `..` components without
/// touching the filesystem. This remains useful for plan-file presentation;
/// ownership decisions use [`resolve_path`] instead.
pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
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
/// A normal source needs both an exact `check_link` match and broad canonical
/// ownership. The only exception is a terminal configured source symlink,
/// which uses [`points_at_source`] to compare the immediate entry identity
/// without following that terminal source symlink.
pub fn remove_link_to(
    target: &Path,
    expected_source: &Path,
    repo_root: &Path,
) -> Result<bool, LinkError> {
    let expected_is_symlink = std::fs::symlink_metadata(expected_source)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    let owned = if expected_is_symlink {
        points_at_source(target, expected_source, repo_root)
    } else {
        points_into_repo(target, repo_root)
            && check_link(target, expected_source) == LinkStatus::Linked
    };
    if !owned {
        return Ok(false);
    }
    // This is intentionally the final operation: revalidate immediately
    // before an ordinary unlink. Same-UID races after this check are outside
    // this tool's threat model.
    std::fs::remove_file(target).map_err(|e| LinkError::Remove(e, target.to_path_buf()))?;
    Ok(true)
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("source does not exist: {0}")]
    SourceMissing(PathBuf),
    #[error("source {0} escapes configured root {1}")]
    SourceOutsideRoot(PathBuf, PathBuf),
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

        // The gateway itself may be dangling too; readlink still gives us its
        // external target, so that state must not fall back to lexical repo
        // ownership.
        let dangling_gateway = repo.join("dangling-gateway");
        std::os::unix::fs::symlink(external.join("missing"), &dangling_gateway).unwrap();
        let dangling_file = home.join("dangling-gateway-file");
        std::os::unix::fs::symlink(dangling_gateway.join("victim"), &dangling_file).unwrap();
        assert!(!points_into_repo(&dangling_file, &repo));
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
    fn test_points_into_repo_applies_dotdot_after_gateway_expansion() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        // POSIX substitutes the gateway target before it applies the later
        // `..`: this is /external/.., not /repo/gateway/.. (= /repo).
        let gateway = repo.join("gateway");
        std::os::unix::fs::symlink(&external, &gateway).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(gateway.join(".."), &link).unwrap();

        assert!(!points_into_repo(&link, &repo));
    }

    #[test]
    fn test_points_into_repo_follows_repeated_symlinks_and_honors_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let inside = repo.join("inside");
        std::fs::create_dir_all(&inside).unwrap();

        // Repeatedly entering a real gateway and leaving it with `..` is a
        // valid in-repo traversal; `..` is applied after each expansion.
        let gateway = repo.join("gateway");
        std::os::unix::fs::symlink(&inside, &gateway).unwrap();
        let repeated = tmp.path().join("repeated");
        std::os::unix::fs::symlink(
            gateway
                .join("..")
                .join("gateway")
                .join("..")
                .join("inside")
                .join("gone"),
            &repeated,
        )
        .unwrap();
        assert!(points_into_repo(&repeated, &repo));

        // The Linux limit permits 40 expansions but rejects the 41st. The
        // final entry is intentionally missing so this also exercises the
        // safe dangling-tail result after a fully resolvable chain.
        for i in 0..41 {
            let next = if i == 40 {
                repo.join("missing")
            } else {
                repo.join(format!("s{}", i + 1))
            };
            std::os::unix::fs::symlink(next, repo.join(format!("s{i}"))).unwrap();
        }
        let forty = tmp.path().join("forty");
        let forty_one = tmp.path().join("forty-one");
        std::os::unix::fs::symlink(repo.join("s1"), &forty).unwrap();
        std::os::unix::fs::symlink(repo.join("s0"), &forty_one).unwrap();

        assert!(points_into_repo(&forty, &repo));
        assert!(
            !points_into_repo(&forty_one, &repo),
            "the 41st expansion must fail closed"
        );
    }

    #[test]
    fn test_store_scope_rejects_gateway_into_sibling_store() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let alpha = repo.join("alpha");
        let beta = repo.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        std::fs::write(beta.join("victim"), "beta").unwrap();
        std::os::unix::fs::symlink(&beta, alpha.join("gateway")).unwrap();

        let target = tmp.path().join("target");
        std::os::unix::fs::symlink(alpha.join("gateway").join("victim"), &target).unwrap();

        assert!(points_into_repo(&target, &repo));
        assert!(!points_into(&target, &alpha));
        assert!(points_into(&target, &beta));
        assert!(
            create_link_in(
                &tmp.path().join("new"),
                &alpha.join("gateway").join("victim"),
                &alpha
            )
            .is_err(),
            "creation must not canonicalize an alpha source through beta"
        );
        let replaced_root = repo.join("replaced-alpha");
        std::os::unix::fs::symlink(&beta, &replaced_root).unwrap();
        assert!(
            create_link_in(
                &tmp.path().join("new-root"),
                &replaced_root.join("victim"),
                &replaced_root
            )
            .is_err(),
            "a configured store root must not be a sibling-store gateway"
        );
    }

    #[test]
    fn test_points_at_source_rejects_terminal_slash_dot_and_escaped_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let store = repo.join("store");
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let source = store.join("alias");
        std::os::unix::fs::symlink(&external, &source).unwrap();

        let slash = tmp.path().join("slash");
        let dot = tmp.path().join("dot");
        let traversal = tmp.path().join("traversal");
        std::fs::create_dir(store.join("child")).unwrap();
        std::os::unix::fs::symlink(PathBuf::from(format!("{}/", source.display())), &slash)
            .unwrap();
        std::os::unix::fs::symlink(source.join("."), &dot).unwrap();
        std::os::unix::fs::symlink(store.join("child").join("..").join("alias"), &traversal)
            .unwrap();
        assert!(!points_at_source(&slash, &source, &repo));
        assert!(!points_at_source(&dot, &source, &repo));
        assert!(!points_at_source(&traversal, &source, &repo));
        // The expected entry itself exists below the repo spelling, but its
        // parent gateway resolves outside the repo. It cannot use the narrow
        // external-source exception.
        let gateway = repo.join("gateway");
        std::os::unix::fs::symlink(&external, &gateway).unwrap();
        let escaped_source = gateway.join("escaped-alias");
        std::os::unix::fs::symlink(&external, &escaped_source).unwrap();
        let escaped_target = tmp.path().join("escaped-target");
        std::os::unix::fs::symlink(&escaped_source, &escaped_target).unwrap();
        assert!(!points_at_source(&escaped_target, &escaped_source, &repo));
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
