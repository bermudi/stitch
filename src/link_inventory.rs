//! Link inventory journal — the record apply uses to avoid rescanning targets.
//!
//! File-mode stale cleanup needs to know which links disappeared from the
//! desired inventory. Discovering that by recursively walking every target on
//! every apply makes `target = "~"` scan all of `$HOME` (including unrelated
//! machine-local trees) for a handful of dotfiles. The journal instead records
//! the last desired link inventory apply saw; disappeared entries become the
//! only stale-link candidates on the next run.
//!
//! The file is machine-local bookkeeping, not authored config. It lives at
//! `.stitch/links.toml` and is updated only while the state lock is held by a
//! real (non-dry-run) apply. Missing entries mean "nothing recorded"; callers
//! may use a legacy scan for pre-journal repos. Corrupt or unsafe files are
//! errors rather than silently resetting cleanup state.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config;
use crate::linker;
use crate::render;

pub(crate) const LINKS_GITIGNORE_ENTRY: &str = ".stitch/links.toml";

type StoreInventory = BTreeMap<String, BTreeSet<String>>;
type InventoryMap = BTreeMap<String, StoreInventory>;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InventoryFile {
    #[serde(default)]
    stores: InventoryMap,
}

pub(crate) fn inventory_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".stitch").join("links.toml")
}

/// Load the inventory. `Ok(None)` means this repo predates the journal (or a
/// test fixture bypassed `init`); callers decide whether to use the legacy
/// recursive sweep once. A corrupt, symlinked, or non-regular file is an
/// error: silently resetting it would make cleanup behavior impossible to
/// reason about.
pub(crate) fn load(repo_root: &Path) -> Result<Option<InventoryFile>, String> {
    let path = inventory_path(repo_root);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "could not inspect link inventory {}: {e}",
            path.display()
        )),
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "refusing symlinked link inventory {}",
            path.display()
        )),
        Ok(meta) if !meta.file_type().is_file() => Err(format!(
            "link inventory {} is not a regular file",
            path.display()
        )),
        Ok(_) => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read link inventory {}: {e}", path.display()))?;
            toml::from_str(&raw).map(Some).map_err(|e| {
                format!(
                    "link inventory {} is unreadable ({e}) — delete the file to rebuild it",
                    path.display()
                )
            })
        }
    }
}

fn save(repo_root: &Path, inventory: &InventoryFile) -> Result<(), String> {
    let contents = toml::to_string_pretty(inventory)
        .map_err(|e| format!("could not serialize link inventory: {e}"))?;
    config::atomic_write(&inventory_path(repo_root), &contents)
        .map_err(|e| format!("could not write link inventory: {e}"))
}

/// Whether the journal file exists, independent of its parsed contents.
pub(crate) fn exists(repo_root: &Path) -> bool {
    std::fs::symlink_metadata(inventory_path(repo_root))
        .is_ok_and(|meta| meta.file_type().is_file())
}

/// Initialize an empty journal. New repos start with one so they never use
/// the pre-journal recursive sweep: a link stitch never managed is not its
/// cleanup responsibility.
pub(crate) fn initialize(repo_root: &Path) -> Result<(), String> {
    if exists(repo_root) {
        return Ok(());
    }
    save(repo_root, &InventoryFile::default())
}

/// The recorded links for one canonical target directory.
pub(crate) fn recorded_links(
    inventory: &InventoryFile,
    store_name: &str,
    canonical_target: &Path,
) -> BTreeSet<String> {
    inventory
        .stores
        .get(store_name)
        .and_then(|targets| targets.get(&canonical_target.to_string_lossy().into_owned()))
        .cloned()
        .unwrap_or_default()
}

/// Replace one store's active-target inventory. Keys absent from
/// `active_targets` are left unchanged so platform-skipped targets keep their
/// last seen inventory instead of looking newly deleted on this machine.
pub(crate) fn update_store(
    repo_root: &Path,
    store_name: &str,
    active_targets: &BTreeMap<PathBuf, BTreeSet<String>>,
) -> Result<(), String> {
    ensure_links_gitignore(repo_root)
        .map_err(|e| format!("could not ignore link inventory in .gitignore: {e}"))?;
    let mut inventory = load(repo_root)?.unwrap_or_default();
    let store = inventory.stores.entry(store_name.to_string()).or_default();
    let mut changed = false;
    for (target, links) in active_targets {
        let key = target.to_string_lossy().into_owned();
        if store.get(&key) != Some(links) {
            store.insert(key, links.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    save(repo_root, &inventory)
}

/// Forget a removed store entirely. Keeping its entries would be harmless for
/// ordinary applies (removed stores are not reconciled), but a future store that
/// reuses the same name must not inherit cleanup candidates it never created.
pub(crate) fn forget_store(repo_root: &Path, store_name: &str) -> Result<(), String> {
    let mut inventory = load(repo_root)?.unwrap_or_default();
    if inventory.stores.remove(store_name).is_none() {
        return Ok(());
    }
    save(repo_root, &inventory)
}

/// Remove the recorded candidates that disappeared from desired state.
///
/// This is deliberately exact-path cleanup, not a filesystem walk. Every
/// candidate still has to exist as a symlink at the recorded path and pass the
/// same store/staging/declared-source ownership classification as the legacy
/// sweep. Missing, replaced, foreign, or other-store entries are left alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn remove_recorded_stale_links(
    target_path: &Path,
    repo_root: &Path,
    store_dir: &Path,
    store_name: &str,
    declared_sources: &[PathBuf],
    stale_link_rels: &BTreeSet<String>,
    dry_run: bool,
) -> Result<Vec<PathBuf>, String> {
    if stale_link_rels.is_empty() {
        return Ok(Vec::new());
    }

    // Match the legacy sweep's root handling: never follow a whole-directory
    // root symlink, and a missing/non-directory root has no children to clean.
    match std::fs::symlink_metadata(target_path) {
        Ok(meta) if meta.file_type().is_symlink() => return Ok(Vec::new()),
        Ok(meta) if !meta.is_dir() => return Ok(Vec::new()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "could not inspect target {} for stale links: {e}",
                target_path.display()
            ));
        }
    }

    let staging_dir = render::store_render_dir(repo_root, store_name);
    let mut removed = Vec::new();
    'candidates: for rel in stale_link_rels {
        if !config::is_safe_fragment(rel) {
            return Err(format!(
                "link inventory for store '{store_name}' contains unsafe link path '{rel}'"
            ));
        }
        let path = target_path.join(rel);

        // Never unlink through a symlinked directory below the target root.
        // The legacy walker never descended through one; exact-path cleanup
        // must preserve that boundary or a replacement directory could turn a
        // recorded child path into a foreign path outside the target.
        if let Some(mut ancestor) = path.parent() {
            while ancestor != target_path && ancestor.starts_with(target_path) {
                match std::fs::symlink_metadata(ancestor) {
                    Ok(meta) if meta.file_type().is_symlink() => continue 'candidates,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(format!(
                            "could not inspect stale-link ancestor {}: {e}",
                            ancestor.display()
                        ));
                    }
                }
                let Some(parent) = ancestor.parent() else {
                    break;
                };
                ancestor = parent;
            }
        }

        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        if !linker::points_into_repo(&path, repo_root)
            || (!linker::points_into(&path, store_dir)
                && !linker::points_into(&path, &staging_dir)
                && !declared_sources
                    .iter()
                    .any(|source| linker::points_to_source(&path, source, repo_root)))
        {
            continue;
        }

        if dry_run {
            removed.push(path);
            continue;
        }
        match linker::remove_link(&path, repo_root) {
            Ok(true) => removed.push(path),
            Ok(false) => {}
            Err(e) => {
                return Err(format!(
                    "could not remove stale link {}: {e}",
                    path.display()
                ));
            }
        }
    }
    Ok(removed)
}

/// Append `.stitch/links.toml` to the repo `.gitignore`, creating the file if
/// needed. Idempotent: a no-op when the inventory is already covered.
pub(crate) fn ensure_links_gitignore(repo_root: &Path) -> Result<(), std::io::Error> {
    render::append_gitignore_entry(repo_root, gitignore_has_links_entry, LINKS_GITIGNORE_ENTRY)
}

/// Whether `.gitignore` text ignores the machine-local link inventory.
/// Conservative in the same way as the render/lock checks: any negation
/// revokes coverage, and a broad `.stitch/` rule covers the file.
pub(crate) fn gitignore_has_links_entry(contents: &str) -> bool {
    let mut covered = false;
    for line in contents.lines() {
        let rule = line.trim_end_matches('\r');
        if rule.is_empty() || rule.starts_with('#') {
            continue;
        }
        if rule.starts_with('!') {
            covered = false;
            continue;
        }
        if rule == LINKS_GITIGNORE_ENTRY
            || rule == ".stitch/"
            || rule == ".stitch"
            || rule == "**/.stitch/links.toml"
        {
            covered = true;
        }
    }
    covered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_roundtrip_preserves_nested_targets_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".stitch")).unwrap();
        let target = PathBuf::from("/home/user/.config");
        let mut active = BTreeMap::new();
        active.insert(
            target.clone(),
            BTreeSet::from(["AGENTS.md".to_string(), "devin/AGENTS.md".to_string()]),
        );
        update_store(root, "agentsconfig", &active).unwrap();
        let inventory = load(root).unwrap().unwrap();
        assert_eq!(
            recorded_links(&inventory, "agentsconfig", &target),
            BTreeSet::from(["AGENTS.md".to_string(), "devin/AGENTS.md".to_string()])
        );
    }

    #[test]
    fn update_is_stable_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".stitch")).unwrap();
        let target = PathBuf::from("/home/user");
        let active = BTreeMap::from([(target, BTreeSet::from([".bashrc".to_string()]))]);
        update_store(root, "home", &active).unwrap();
        let before = std::fs::metadata(inventory_path(root))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        update_store(root, "home", &active).unwrap();
        let after = std::fs::metadata(inventory_path(root))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
    }
}
