//! Scan the filesystem for symlinks pointing into this repo.
//!
//! Shared foundation for `prune` (remove repo-pointing links no store
//! references) and the planned `import` (register existing repo-pointing
//! links in config). The scan is deliberately *not* wired into `doctor`:
//! it walks the user's home directory, which is too slow and surprising for
//! a health check that today is repo-local and instant. See
//! `docs/plans/v0.3-config-state-split.md` (item 8) for the deviation note.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::{self, Config, ConfigError};
use crate::linker;
use crate::platform::Platform;

/// A symlink found on disk whose target points into this repo.
#[derive(Debug, Clone)]
pub struct FoundLink {
    /// Where the symlink lives (absolute path as walked).
    pub link: PathBuf,
    /// What it points at (canonicalized when it exists, else the raw target).
    pub resolves_to: PathBuf,
}

/// A scan root: a directory to walk, with an optional directory-depth cap.
///
/// The home directory is scanned *shallowly* by default (`max_depth = Some(1)`
/// — direct children only) so a bare `stitch prune` catches top-level dotfile
/// links (`~/.bashrc`, `~/.gitconfig`, …) without descending into the slow
/// trees that live under `$HOME` (`~/.cache`, `node_modules`,
/// `~/.local/share/Steam`, …). The XDG-style roots `~/.config` and
/// `~/.local/share`, where nested dotfile links actually live, are walked
/// fully. An explicit `--scan-dir ~` overrides this and walks `$HOME`
/// recursively.
#[derive(Debug, Clone)]
pub struct ScanRoot {
    pub path: PathBuf,
    /// If set, [`walkdir::WalkDir`] is capped at this depth (0 = root only,
    /// 1 = root + direct children).
    pub max_depth: Option<usize>,
}

impl From<PathBuf> for ScanRoot {
    /// Treat a bare path as an unlimited-depth scan root (the behavior of
    /// explicit `--scan-dir` arguments).
    fn from(path: PathBuf) -> Self {
        ScanRoot {
            path,
            max_depth: None,
        }
    }
}

/// Default scan roots used when `prune` is invoked with no `--scan-dir`:
/// `~` (shallow — top-level dotfiles only), then `~/.config` and
/// `~/.local/share` (full depth). See [`ScanRoot`] for the speed rationale.
pub fn default_scan_dirs() -> Result<Vec<ScanRoot>, ConfigError> {
    Ok(vec![
        ScanRoot {
            path: config::expand_home("~")?,
            max_depth: Some(1),
        },
        ScanRoot {
            path: config::expand_home("~/.config")?,
            max_depth: None,
        },
        ScanRoot {
            path: config::expand_home("~/.local/share")?,
            max_depth: None,
        },
    ])
}

/// Walk `roots` and return every symlink whose target points into
/// `repo_root`. Each root's optional `max_depth` caps descent (used to keep
/// the default `~` scan shallow — see [`ScanRoot`]).
///
/// `follow_links(false)` — never recurse through a symlink, which would
/// re-enter the repo via its own links. The repo itself is pruned from the
/// walk (via `filter_entry`) so a repo living under a scan dir (e.g.
/// `~/dotfiles` under `~`) is not classified from the outside. Missing scan
/// dirs are skipped silently — a default root like `~/.local/share` need not
/// exist. Overlapping scan dirs dedup via the canonicalized link location.
pub fn scan_for_repo_links(repo_root: &Path, roots: &[ScanRoot]) -> Vec<FoundLink> {
    let repo_canon = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let mut found = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for root in roots {
        let Ok(dir_canon) = root.path.canonicalize() else {
            continue; // scan dir missing — skip silently
        };
        // Never walk into the repo from the outside. Both roots are canonical,
        // so a component-wise `starts_with` is exact (no prefix false-positives).
        if dir_canon == repo_canon || dir_canon.starts_with(&repo_canon) {
            continue;
        }

        let mut walk = walkdir::WalkDir::new(&dir_canon).follow_links(false);
        if let Some(depth) = root.max_depth {
            walk = walk.max_depth(depth);
        }
        for entry in walk
            .into_iter()
            .filter_entry(|e| !e.path().starts_with(&repo_canon))
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_symlink() {
                continue;
            }
            let link = entry.path();
            if !linker::points_into_repo(link, &repo_canon) {
                continue;
            }
            // Dedup: overlapping scan dirs (e.g. ~ and ~/.config) can yield the
            // same link twice.
            if !seen.insert(canonicalize_link_path(link)) {
                continue;
            }
            found.push(FoundLink {
                link: link.to_path_buf(),
                resolves_to: resolve_target(link),
            });
        }
    }
    found
}

/// From `found`, return only those whose location is not covered by any
/// store's target set — i.e. the symlink inventory `apply` would create,
/// platform-gated.
///
/// A store skipped by the platform filter does not own its targets on this
/// host, so a stray link at such a target is reported as orphaned here (it is
/// genuinely unmanaged on this machine). The owned set reuses `status_all`,
/// the single source of truth for "which (source, target) pairs apply".
pub fn orphan_links<'a>(
    repo_root: &Path,
    found: &'a [FoundLink],
    config: &Config,
    platform: &Platform,
) -> Vec<&'a FoundLink> {
    let owned: HashSet<PathBuf> = crate::store::status_all(repo_root, config, platform)
        .into_iter()
        .filter(|e| !e.skipped_platform)
        .map(|e| canonicalize_link_path(&e.target))
        .collect();

    found
        .iter()
        .filter(|fl| !owned.contains(&canonicalize_link_path(&fl.link)))
        .collect()
}

/// Resolve a symlink's target to an absolute path: relative links are resolved
/// against the symlink's parent; the result is canonicalized when it exists
/// (falling back to the raw absolute target for dangling links).
fn resolve_target(link: &Path) -> PathBuf {
    match std::fs::read_link(link) {
        Ok(r) => {
            let abs = if r.is_absolute() {
                r
            } else {
                link.parent().unwrap_or(Path::new(".")).join(&r)
            };
            abs.canonicalize().unwrap_or(abs)
        }
        Err(_) => link.to_path_buf(),
    }
}

/// Canonicalize a path's *location* without resolving its final component.
///
/// `Path::canonicalize` on a symlink returns the symlink's target, which would
/// lose "where the link lives"; resolving only the parent keeps the leaf intact
/// while still collapsing any symlinked ancestors. Used to compare a link's
/// location against owned target paths robustly (handles e.g. `~/.config` being
/// a symlink to another volume).
fn canonicalize_link_path(path: &Path) -> PathBuf {
    match path.parent().and_then(|p| p.canonicalize().ok()) {
        Some(parent) => parent.join(path.file_name().unwrap_or_default()),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Hooks, Store, WhenClause};
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    /// A minimal merged config with one whole-directory store at `target`.
    fn config_with_store(name: &str, target: &str) -> Config {
        let mut cfg = Config::empty();
        cfg.stores.insert(
            name.to_string(),
            Store {
                target: Some(target.to_string()),
                files: vec![],
                patterns: vec![],
                ignore: vec![],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets: BTreeMap::new(),
            },
        );
        cfg
    }

    #[test]
    fn scan_finds_repo_pointing_symlink() {
        let repo = tempfile::tempdir().unwrap();
        let store_dir = repo.path().join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();

        let home = tempfile::tempdir().unwrap();
        let link = home.path().join(".config").join("nvim");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&store_dir, &link).unwrap();

        let found = scan_for_repo_links(repo.path(), &[home.path().to_path_buf().into()]);
        assert_eq!(found.len(), 1, "exactly one repo-pointing link");
        assert_eq!(found[0].link, link);
        assert_eq!(found[0].resolves_to, store_dir.canonicalize().unwrap());
    }

    #[test]
    fn scan_ignores_foreign_symlink() {
        let repo = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();

        let home = tempfile::tempdir().unwrap();
        let link = home.path().join("stray");
        symlink(foreign.path(), &link).unwrap();

        let found = scan_for_repo_links(repo.path(), &[home.path().to_path_buf().into()]);
        assert!(found.is_empty(), "foreign symlink must not be found");
    }

    #[test]
    fn scan_skips_missing_scan_dir() {
        let repo = tempfile::tempdir().unwrap();
        let found = scan_for_repo_links(
            repo.path(),
            &[PathBuf::from("/nonexistent/stitch/scan/dir").into()],
        );
        assert!(found.is_empty(), "missing scan dir skipped silently");
    }

    #[test]
    fn scan_does_not_walk_into_repo() {
        // A repo nested under the scan dir must be pruned: walking it from the
        // outside would classify its own internal layout as repo-pointing.
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("dotfiles");
        let repo_canon = {
            std::fs::create_dir_all(&repo).unwrap();
            repo.canonicalize().unwrap()
        };
        // A real subdir inside the repo — must NOT show up in results.
        let inner = repo.join("nvim");
        std::fs::create_dir_all(&inner).unwrap();

        let found = scan_for_repo_links(&repo_canon, &[home.path().to_path_buf().into()]);
        assert!(
            found.iter().all(|f| !f.link.starts_with(&repo_canon)),
            "repo internals must not be scanned from the outside"
        );
    }

    #[test]
    fn orphan_links_excludes_covered_targets() {
        let repo = tempfile::tempdir().unwrap();
        let store_dir = repo.path().join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();

        let home = tempfile::tempdir().unwrap();
        // covered: at a path the store owns.
        let covered = home.path().join(".config").join("nvim");
        // orphan: repo-pointing, but at a path no store owns.
        let orphan = home.path().join(".config").join("old");
        std::fs::create_dir_all(covered.parent().unwrap()).unwrap();
        symlink(&store_dir, &covered).unwrap();
        symlink(&store_dir, &orphan).unwrap();

        let found = scan_for_repo_links(repo.path(), &[home.path().to_path_buf().into()]);
        assert_eq!(found.len(), 2, "covered + orphan both repo-pointing");

        let cfg = config_with_store("nvim", &covered.to_string_lossy());
        let platform = Platform::detect();
        let orphans = orphan_links(repo.path(), &found, &cfg, &platform);
        assert_eq!(orphans.len(), 1, "only the uncovered link is orphaned");
        assert_eq!(orphans[0].link, orphan);
    }

    #[test]
    fn orphan_links_reports_stray_link_at_platform_skipped_target() {
        // A store skipped on this host does not own its target here, so a stray
        // link at that target is genuinely unmanaged → orphaned.
        let repo = tempfile::tempdir().unwrap();
        let store_dir = repo.path().join("macos-only");
        std::fs::create_dir_all(&store_dir).unwrap();

        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("karabiner");
        symlink(&store_dir, &target).unwrap();

        let found = scan_for_repo_links(repo.path(), &[home.path().to_path_buf().into()]);
        assert_eq!(found.len(), 1);

        let cfg = {
            let mut c = Config::empty();
            c.stores.insert(
                "macos-only".into(),
                Store {
                    target: Some(target.to_string_lossy().into_owned()),
                    files: vec![],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause {
                        os: Some("macos".to_string()),
                        ..Default::default()
                    },
                    hooks: Hooks::default(),
                    targets: BTreeMap::new(),
                },
            );
            c
        };
        let platform = Platform::detect();
        let orphans = orphan_links(repo.path(), &found, &cfg, &platform);
        // On any non-macOS host (the CI host) this target is unowned.
        if platform.os != "macos" {
            assert_eq!(orphans.len(), 1, "stray link at skipped target is orphaned");
        }
    }

    #[test]
    fn scan_respects_max_depth() {
        // max_depth caps descent: a top-level link (depth 1) is found, a link
        // nested one level deeper is not. This is what keeps the default `~`
        // scan from descending into ~/.cache, node_modules, and friends.
        let repo = tempfile::tempdir().unwrap();
        let store_dir = repo.path().join("s");
        std::fs::create_dir_all(&store_dir).unwrap();

        let home = tempfile::tempdir().unwrap();
        let top = home.path().join(".bashrc");
        symlink(&store_dir, &top).unwrap();
        let nested_parent = home.path().join(".deep");
        std::fs::create_dir_all(&nested_parent).unwrap();
        let nested = nested_parent.join("link");
        symlink(&store_dir, &nested).unwrap();

        let found = scan_for_repo_links(
            repo.path(),
            &[ScanRoot {
                path: home.path().to_path_buf(),
                max_depth: Some(1),
            }],
        );
        assert_eq!(
            found.len(),
            1,
            "max_depth(1) yields the top-level link only"
        );
        assert_eq!(found[0].link, top);
    }

    #[test]
    fn scan_finds_dangling_repo_pointing_link() {
        // A link whose repo target has since been deleted is still
        // repo-pointing: points_into_repo lexical-normalizes the (non-existent)
        // target and resolve_target falls back to the raw absolute path. Such a
        // link is a genuine orphan candidate — the store was removed — and must
        // still be reported.
        let repo = tempfile::tempdir().unwrap();
        let repo_canon = repo.path().canonicalize().unwrap();
        let gone = repo_canon.join("removed-store"); // deliberately not created

        let home = tempfile::tempdir().unwrap();
        let link = home.path().join(".config").join("gone");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&gone, &link).unwrap();

        let found = scan_for_repo_links(repo.path(), &[home.path().to_path_buf().into()]);
        assert_eq!(found.len(), 1, "dangling repo-pointing link is still found");
        // Target is gone, so resolves_to is the raw (uncanonicalized) repo path.
        assert_eq!(found[0].resolves_to, gone);
    }

    #[test]
    fn orphan_links_excludes_file_mode_targets() {
        // A file-mode store owns its individual link locations (target joined
        // with each managed file), not the whole target dir. canonicalize_link_path
        // must make the leaf-level link match, so a covered file link is excluded
        // while a sibling repo-pointing link at an unmanaged name is an orphan.
        let repo = tempfile::tempdir().unwrap();
        let store_dir = repo.path().join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "-- init").unwrap();

        let home = tempfile::tempdir().unwrap();
        let cfg_dir = home.path().join(".config").join("nvim");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let covered = cfg_dir.join("init.lua"); // managed by the store
        let orphan = cfg_dir.join("other.lua"); // repo-pointing, unmanaged name
        symlink(store_dir.join("init.lua"), &covered).unwrap();
        symlink(store_dir.join("init.lua"), &orphan).unwrap();

        let found = scan_for_repo_links(repo.path(), &[home.path().to_path_buf().into()]);
        assert_eq!(found.len(), 2, "both links are repo-pointing");

        let cfg = {
            let mut c = Config::empty();
            c.stores.insert(
                "nvim".into(),
                Store {
                    target: Some(cfg_dir.to_string_lossy().into_owned()),
                    files: vec!["init.lua".into()],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause::default(),
                    hooks: Hooks::default(),
                    targets: BTreeMap::new(),
                },
            );
            c
        };
        let platform = Platform::detect();
        let orphans = orphan_links(repo.path(), &found, &cfg, &platform);
        assert_eq!(orphans.len(), 1, "only the unmanaged file link is orphaned");
        assert_eq!(orphans[0].link, orphan);
    }
}
