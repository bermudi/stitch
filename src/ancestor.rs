//! Target-ancestor identity pinning across pre-apply and pre-store hooks.
//!
//! A hook must not be able to introduce or repoint a symlinked ancestor that
//! a link operation would traverse, or silently replace a real directory with
//! a different one (bind mount / rename / copy) before that operation runs.
//!
//! This is a safety primitive specific to stitch's apply race model. It was
//! originally parked in `plan_exec.rs` (where it was first needed) but is a
//! top-level module so that both `store` and `plan_exec` can depend on it
//! without a module cycle (`store` ↔ `plan_exec`).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// The pre-hook identity of one filesystem entry that a link operation would
/// traverse.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetAncestorEntry {
    RealDir { dev: u64, ino: u64 },
    Symlink { dev: u64, ino: u64, target: PathBuf },
    Other { dev: u64, ino: u64 },
}

fn target_ancestor_entry(path: &Path) -> Result<Option<TargetAncestorEntry>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => {
            return Err(format!(
                "could not inspect target ancestor {}: {e}",
                path.display()
            ));
        }
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|e| {
            format!(
                "could not read target ancestor symlink {}: {e}",
                path.display()
            )
        })?;
        return Ok(Some(TargetAncestorEntry::Symlink {
            dev: meta.dev(),
            ino: meta.ino(),
            target,
        }));
    }
    if meta.is_dir() {
        return Ok(Some(TargetAncestorEntry::RealDir {
            dev: meta.dev(),
            ino: meta.ino(),
        }));
    }
    Ok(Some(TargetAncestorEntry::Other {
        dev: meta.dev(),
        ino: meta.ino(),
    }))
}

/// A concrete redirect detected by [`TargetAncestorSnapshot::revalidate`].
#[derive(Debug, Clone)]
pub(crate) enum TargetAncestorRedirect {
    /// The path is a symlink (pre-existing or created by the hook) and a link
    /// operation would have to traverse it.
    Symlinked {
        path: PathBuf,
        resolves_to: Option<PathBuf>,
    },
    /// The path was a real directory and is now a different real directory, or
    /// a to-be-removed ancestor was replaced by something other than the same
    /// entry or absence.
    Redirected {
        path: PathBuf,
        resolves_to: Option<PathBuf>,
    },
    /// The path was a real directory and no longer exists.
    Removed { path: PathBuf },
}

impl std::fmt::Display for TargetAncestorRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlinked { path, resolves_to } => {
                write!(
                    f,
                    "target ancestor {} is a symlinked redirect",
                    path.display()
                )?;
                if let Some(target) = resolves_to {
                    write!(f, " -> {}", target.display())?;
                }
                Ok(())
            }
            Self::Redirected { path, resolves_to } => {
                write!(f, "target ancestor {} was redirected", path.display())?;
                if let Some(target) = resolves_to {
                    write!(f, " -> {}", target.display())?;
                }
                Ok(())
            }
            Self::Removed { path } => {
                write!(f, "target ancestor {} was removed", path.display())
            }
        }
    }
}

/// Snapshot of the filesystem identity of target ancestor directories.
///
/// The capture is taken before a hook; [`TargetAncestorSnapshot::revalidate`]
/// is called after the hook and before any link creation through those
/// ancestors.
#[derive(Debug, Clone)]
pub(crate) struct TargetAncestorSnapshot {
    removed: BTreeSet<PathBuf>,
    identities: BTreeMap<PathBuf, Option<TargetAncestorEntry>>,
}

impl TargetAncestorSnapshot {
    /// Capture the pre-hook identity of every target ancestor from the target's
    /// parent up to and including `$HOME`. Ancestors above `$HOME` are not pinned.
    pub fn capture<I: IntoIterator<Item = PathBuf>>(
        _repo_root: &Path,
        targets: I,
        removed_ancestors: &BTreeSet<PathBuf>,
        home: &Path,
    ) -> Result<Self, TargetAncestorRedirect> {
        let mut identities = BTreeMap::new();
        for target in targets {
            for ancestor in target.ancestors().skip(1) {
                if !ancestor.starts_with(home) {
                    break;
                }
                let path = ancestor.to_path_buf();
                if identities.contains_key(&path) {
                    continue;
                }
                match target_ancestor_entry(ancestor) {
                    Ok(id) => {
                        identities.insert(path, id);
                    }
                    Err(_) => {
                        return Err(TargetAncestorRedirect::Redirected {
                            path,
                            resolves_to: None,
                        });
                    }
                }
            }
        }
        Ok(Self {
            removed: removed_ancestors.clone(),
            identities,
        })
    }

    /// Revalidate every captured ancestor after a hook.
    ///
    /// Allowed transitions:
    /// - a real directory stays the same real directory;
    /// - an absent ancestor stays absent or becomes a real directory
    ///   (a hook may `mkdir -p` a missing parent);
    /// - a removed ancestor (e.g., a whole-directory promotion root) stays the
    ///   same symlink or becomes absent; anything else is a redirect.
    pub fn revalidate(&self) -> Result<(), TargetAncestorRedirect> {
        for (path, expected) in &self.identities {
            let actual =
                target_ancestor_entry(path).map_err(|_| TargetAncestorRedirect::Redirected {
                    path: path.clone(),
                    resolves_to: None,
                })?;
            match (expected, actual) {
                (Some(expected_id), Some(actual_id)) if *expected_id == actual_id => {}
                (None, None) => {}
                (None, Some(TargetAncestorEntry::RealDir { .. })) => {
                    // Previously absent: a hook may have `mkdir -p`d a real
                    // directory. This is the benign case.
                }
                (Some(TargetAncestorEntry::Symlink { .. }), None)
                    if self.removed.contains(path) =>
                {
                    // A removed ancestor (e.g., whole-dir promotion root) may
                    // already be gone; the operation will create the
                    // replacement itself.
                }
                (None, Some(TargetAncestorEntry::Symlink { target, .. })) => {
                    return Err(TargetAncestorRedirect::Symlinked {
                        path: path.clone(),
                        resolves_to: Some(target),
                    });
                }
                (None, Some(TargetAncestorEntry::Other { .. })) => {
                    return Err(TargetAncestorRedirect::Redirected {
                        path: path.clone(),
                        resolves_to: None,
                    });
                }
                (Some(_), None) => {
                    return Err(TargetAncestorRedirect::Removed { path: path.clone() });
                }
                (Some(_), Some(TargetAncestorEntry::Symlink { target, .. })) => {
                    return Err(TargetAncestorRedirect::Symlinked {
                        path: path.clone(),
                        resolves_to: Some(target),
                    });
                }
                (Some(_), Some(TargetAncestorEntry::RealDir { .. }))
                | (Some(_), Some(TargetAncestorEntry::Other { .. })) => {
                    return Err(TargetAncestorRedirect::Redirected {
                        path: path.clone(),
                        resolves_to: None,
                    });
                }
            }
        }
        Ok(())
    }
}

/// True if `p` contains any `..` path component.
pub(crate) fn has_parent_dir(p: &Path) -> bool {
    p.components().any(|c| c == Component::ParentDir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn target_ancestor_snapshot_includes_home_and_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cfg = home.join(".config");
        fs::create_dir_all(&cfg).unwrap();

        let targets = vec![cfg.join("a").join("f"), cfg.join("b").join("g")];
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), targets, &BTreeSet::new(), &home).unwrap();

        assert!(snapshot.identities.contains_key(&cfg));
        assert!(snapshot.identities.contains_key(&cfg.join("a")));
        assert!(snapshot.identities.contains_key(&cfg.join("b")));
        assert!(snapshot.identities.contains_key(&home));
    }

    #[test]
    fn target_ancestor_snapshot_allows_absent_to_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let target = home.join(".config").join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::create_dir_all(home.join(".config")).unwrap();
        snapshot.revalidate().unwrap();
    }

    #[test]
    fn target_ancestor_snapshot_rejects_absent_to_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let target = home.join(".config").join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        symlink(&other, home.join(".config")).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Symlinked { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_rejects_real_dir_identity_change() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".config");
        fs::create_dir_all(&config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        // Replace the real directory with a different one (same as a bind
        // mount / rename / copy attack). Use rename to guarantee a new inode.
        let old = tmp.path().join("old_config");
        fs::rename(&config, &old).unwrap();
        fs::create_dir_all(&config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(
                err,
                TargetAncestorRedirect::Redirected {
                    resolves_to: None,
                    ..
                }
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_rejects_real_dir_to_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".config");
        fs::create_dir_all(&config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::remove_dir(&config).unwrap();
        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        symlink(&other, &config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Symlinked { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_allows_symlink_identity_preservation() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        let config = home.join(".config");
        symlink(&other, &config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        // Revalidate sees the same symlink and is fine; the per-link
        // confinement check, not the snapshot, decides whether this is
        // traversable.
        snapshot.revalidate().unwrap();
    }

    #[test]
    fn target_ancestor_snapshot_rejects_symlink_repointing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        let config = home.join(".config");
        symlink(&other, &config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::remove_file(&config).unwrap();
        let third = home.join(".third");
        fs::create_dir_all(&third).unwrap();
        symlink(&third, &config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Symlinked { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_rejects_existing_dir_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".config");
        fs::create_dir_all(&config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::remove_dir(&config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Removed { .. }),
            "got: {err:?}"
        );
    }
}
