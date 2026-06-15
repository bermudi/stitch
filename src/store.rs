use crate::config::{self, Config, Store, StoreMode};
use crate::linker::{self, LinkStatus};
use crate::platform::Platform;
use globset::{GlobBuilder, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyAction {
    Created(PathBuf),
    Replaced(PathBuf),
    Conflict(PathBuf),
    SkippedPlatform,
    AlreadyLinked,
    Error(String),
}

#[derive(Debug)]
pub struct ApplyResult {
    pub store_name: String,
    pub actions: Vec<ApplyAction>,
}

/// Apply all stores in the config.
pub fn apply_all(
    repo_root: &Path,
    config: &Config,
    platform: &Platform,
    dry_run: bool,
) -> Vec<ApplyResult> {
    let sorted: BTreeMap<_, _> = config.stores.iter().collect();
    sorted
        .into_iter()
        .map(|(name, store)| apply_store(repo_root, name, store, platform, dry_run))
        .collect()
}

/// Apply a single store.
pub fn apply_store(
    repo_root: &Path,
    name: &str,
    store: &Store,
    platform: &Platform,
    dry_run: bool,
) -> ApplyResult {
    if !platform.matches_when(&store.when) {
        return ApplyResult {
            store_name: name.to_string(),
            actions: vec![ApplyAction::SkippedPlatform],
        };
    }

    let store_dir = repo_root.join(name);
    if !store_dir.exists() {
        return ApplyResult {
            store_name: name.to_string(),
            actions: vec![ApplyAction::Error(format!(
                "store directory '{}' does not exist",
                name
            ))],
        };
    }

    let mut actions = Vec::new();

    if store.is_multi_target() {
        for target_entry in &store.targets {
            if !platform.matches_when(&target_entry.when) {
                continue;
            }
            let target_path = config::expand_home(&target_entry.target);
            actions.extend(apply_target(
                &store_dir,
                &target_path,
                repo_root,
                &target_entry.files,
                &target_entry.patterns,
                &target_entry.ignore,
                dry_run,
            ));
        }
    } else if let Some(ref target_str) = store.target {
        let target_path = config::expand_home(target_str);
        actions.extend(apply_target(
            &store_dir,
            &target_path,
            repo_root,
            &store.files,
            &store.patterns,
            &store.ignore,
            dry_run,
        ));
    } else {
        actions.push(ApplyAction::Error("no target configured".into()));
    }

    ApplyResult {
        store_name: name.to_string(),
        actions,
    }
}

fn apply_target(
    store_dir: &Path,
    target_path: &Path,
    repo_root: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
    dry_run: bool,
) -> Vec<ApplyAction> {
    let mode = if files.is_empty() && patterns.is_empty() {
        StoreMode::WholeDir
    } else {
        StoreMode::File
    };
    match mode {
        StoreMode::WholeDir => {
            vec![apply_single_link(
                store_dir,
                target_path,
                repo_root,
                dry_run,
            )]
        }
        StoreMode::File => {
            let resolved = resolve_files(store_dir, files, patterns, ignore);
            let mut actions = Vec::new();
            for file_name in &resolved {
                let source = store_dir.join(file_name);
                let target = target_path.join(file_name);
                actions.push(apply_single_link(&source, &target, repo_root, dry_run));
            }
            actions
        }
    }
}

fn apply_single_link(source: &Path, target: &Path, repo_root: &Path, dry_run: bool) -> ApplyAction {
    let status = linker::check_link(target, source);

    match status {
        LinkStatus::Linked => ApplyAction::AlreadyLinked,
        LinkStatus::Missing => {
            if dry_run {
                ApplyAction::Created(target.to_path_buf())
            } else {
                match linker::create_link(target, source) {
                    Ok(()) => ApplyAction::Created(target.to_path_buf()),
                    Err(e) => ApplyAction::Error(e.to_string()),
                }
            }
        }
        LinkStatus::Conflict(p) => ApplyAction::Conflict(p),
        LinkStatus::Broken(_) => {
            // A symlink that isn't ours. Relink only if it points into this
            // repo (stale stitch state — the store moved or a file was
            // renamed); a foreign symlink (stow/chezmoi/Nix/Home-Manager, or
            // a dangling user link) is a conflict, never silently clobbered.
            if !linker::points_into_repo(target, repo_root) {
                return ApplyAction::Conflict(target.to_path_buf());
            }
            if dry_run {
                return ApplyAction::Replaced(target.to_path_buf());
            }
            if let Err(e) = std::fs::remove_file(target) {
                return ApplyAction::Error(e.to_string());
            }
            match linker::create_link(target, source) {
                Ok(()) => ApplyAction::Replaced(target.to_path_buf()),
                Err(e) => ApplyAction::Error(e.to_string()),
            }
        }
    }
}

#[derive(Debug)]
pub struct StatusEntry {
    pub store_name: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub status: LinkStatus,
    pub skipped_platform: bool,
}

/// Get status for all stores.
pub fn status_all(repo_root: &Path, config: &Config, platform: &Platform) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let sorted: BTreeMap<_, _> = config.stores.iter().collect();

    for (name, store) in sorted {
        if !platform.matches_when(&store.when) {
            entries.push(StatusEntry {
                store_name: name.clone(),
                source: PathBuf::new(),
                target: PathBuf::new(),
                status: LinkStatus::Missing,
                skipped_platform: true,
            });
            continue;
        }

        let store_dir = repo_root.join(name);
        if store.is_multi_target() {
            for target_entry in &store.targets {
                if !platform.matches_when(&target_entry.when) {
                    continue;
                }
                let target_path = config::expand_home(&target_entry.target);
                let mode = if target_entry.files.is_empty() && target_entry.patterns.is_empty() {
                    StoreMode::WholeDir
                } else {
                    StoreMode::File
                };
                entries.extend(collect_statuses(
                    &store_dir,
                    &target_path,
                    mode,
                    &target_entry.files,
                    &target_entry.patterns,
                    &target_entry.ignore,
                    name,
                ));
            }
        } else if let Some(ref target_str) = store.target {
            let target_path = config::expand_home(target_str);
            entries.extend(collect_statuses(
                &store_dir,
                &target_path,
                store.mode(),
                &store.files,
                &store.patterns,
                &store.ignore,
                name,
            ));
        }
    }

    entries
}

fn collect_statuses(
    store_dir: &Path,
    target_path: &Path,
    mode: StoreMode,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
    name: &str,
) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    match mode {
        StoreMode::WholeDir => {
            entries.push(StatusEntry {
                store_name: name.to_string(),
                source: store_dir.to_path_buf(),
                target: target_path.to_path_buf(),
                status: linker::check_link(target_path, store_dir),
                skipped_platform: false,
            });
        }
        StoreMode::File => {
            let resolved = resolve_files(store_dir, files, patterns, ignore);
            for file_name in &resolved {
                let source = store_dir.join(file_name);
                let target = target_path.join(file_name);
                entries.push(StatusEntry {
                    store_name: name.to_string(),
                    source: source.clone(),
                    target: target.clone(),
                    status: linker::check_link(&target, &source),
                    skipped_platform: false,
                });
            }
        }
    }
    entries
}

#[derive(Debug)]
pub struct DoctorResult {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub info: Vec<String>,
}

/// Run health checks.
pub fn doctor(repo_root: &Path, config: &Config, platform: &Platform) -> DoctorResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut info = Vec::new();

    if config.stores.is_empty() {
        warnings.push("no stores configured".into());
        return DoctorResult {
            errors,
            warnings,
            info,
        };
    }

    info.push(format!("{} stores configured", config.stores.len()));

    let mut seen_targets: BTreeMap<PathBuf, String> = BTreeMap::new();

    // Compute status once, not per store.
    let all_statuses = status_all(repo_root, config, platform);

    for (name, store) in &config.stores {
        let store_dir = repo_root.join(name);

        if !store_dir.exists() {
            errors.push(format!(
                "store '{}': directory '{}' does not exist",
                name,
                store_dir.display()
            ));
            continue;
        }

        if store_dir
            .read_dir()
            .map_or(true, |mut d| d.next().is_none())
        {
            warnings.push(format!("store '{}': directory is empty", name));
        }

        if !platform.matches_when(&store.when) {
            info.push(format!("store '{}': skipped (platform filter)", name));
            continue;
        }

        if let Some(ref target_str) = store.target {
            let target_path = config::expand_home(target_str);
            if let Some(other) = seen_targets.get(&target_path) {
                errors.push(format!(
                    "stores '{}' and '{}' both target '{}'",
                    name,
                    other,
                    target_path.display()
                ));
            } else {
                seen_targets.insert(target_path, name.clone());
            }
        }

        for entry in all_statuses
            .iter()
            .filter(|e| e.store_name == *name && !e.skipped_platform)
        {
            if let LinkStatus::Broken(ref resolved) = entry.status {
                errors.push(format!(
                    "store '{}': broken symlink at {} -> {}",
                    name,
                    entry.target.display(),
                    resolved.display()
                ));
            }
        }
    }

    DoctorResult {
        errors,
        warnings,
        info,
    }
}

/// Resolve the complete file list for a file-mode store.
///
/// Combines explicit `files` with glob `patterns` matched against the store directory,
/// then removes anything matched by `ignore` patterns. Returns deduplicated, sorted paths
/// relative to `store_dir`.
fn resolve_files(
    store_dir: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // Explicit files always included.
    for f in files {
        seen.insert(f.clone());
    }

    // Build the include globset from patterns.
    if !patterns.is_empty() {
        let mut builder = GlobSetBuilder::new();
        let mut valid = true;
        for pat in patterns {
            match GlobBuilder::new(pat).literal_separator(false).build() {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(e) => {
                    eprintln!("warning: invalid glob pattern '{}': {}", pat, e);
                    valid = false;
                }
            }
        }

        if valid && let Ok(globset) = builder.build() {
            // Walk the store directory and match against patterns.
            if let Ok(entries) = std::fs::read_dir(store_dir) {
                for entry in entries.flatten() {
                    let file_name = entry.file_name();
                    let name_str = file_name.to_string_lossy();
                    if globset.is_match(name_str.as_ref()) {
                        seen.insert(name_str.into_owned());
                    }
                }
            }
        }
    }

    // Build the ignore globset and filter.
    if !ignore.is_empty() {
        let mut builder = GlobSetBuilder::new();
        let mut valid = true;
        for pat in ignore {
            match GlobBuilder::new(pat).literal_separator(false).build() {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(e) => {
                    eprintln!("warning: invalid ignore pattern '{}': {}", pat, e);
                    valid = false;
                }
            }
        }

        if valid && let Ok(globset) = builder.build() {
            seen.retain(|name| !globset.is_match(name.as_str()));
        }
    }

    seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_files_explicit_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        // Create some files in the store dir.
        std::fs::write(store_dir.join(".bashrc"), "...").unwrap();
        std::fs::write(store_dir.join(".zshrc"), "...").unwrap();
        std::fs::write(store_dir.join(".profile"), "...").unwrap();

        let resolved = resolve_files(&store_dir, &[".bashrc".into()], &[], &[]);
        assert_eq!(resolved, vec![".bashrc"]);
    }

    #[test]
    fn test_resolve_files_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        std::fs::write(store_dir.join(".bashrc"), "...").unwrap();
        std::fs::write(store_dir.join(".zshrc"), "...").unwrap();
        std::fs::write(store_dir.join(".profile"), "...").unwrap();
        std::fs::write(store_dir.join("config.toml"), "...").unwrap();

        let resolved = resolve_files(
            &store_dir,
            &[],
            &[".*".into()], // match dotfiles
            &[],
        );
        assert_eq!(resolved, vec![".bashrc", ".profile", ".zshrc"]);
    }

    #[test]
    fn test_resolve_files_patterns_with_ignore() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        std::fs::write(store_dir.join(".bashrc"), "...").unwrap();
        std::fs::write(store_dir.join(".zshrc"), "...").unwrap();
        std::fs::write(store_dir.join(".profile"), "...").unwrap();

        let resolved = resolve_files(
            &store_dir,
            &[],
            &[".*".into()],
            &[".profile".into()], // ignore .profile
        );
        assert_eq!(resolved, vec![".bashrc", ".zshrc"]);
    }

    #[test]
    fn test_resolve_files_explicit_and_patterns_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        std::fs::write(store_dir.join(".bashrc"), "...").unwrap();
        std::fs::write(store_dir.join(".zshrc"), "...").unwrap();

        // .bashrc appears in both explicit files and pattern match — should dedup.
        let resolved = resolve_files(&store_dir, &[".bashrc".into()], &[".*".into()], &[]);
        assert_eq!(resolved, vec![".bashrc", ".zshrc"]);
    }

    #[test]
    fn test_resolve_files_ignore_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        std::fs::write(store_dir.join("app.conf"), "...").unwrap();
        std::fs::write(store_dir.join("app.local.conf"), "...").unwrap();
        std::fs::write(store_dir.join("app.prod.conf"), "...").unwrap();

        let resolved = resolve_files(
            &store_dir,
            &[],
            &["*.conf".into()],
            &["*.local.conf".into()],
        );
        assert_eq!(resolved, vec!["app.conf", "app.prod.conf"]);
    }
}
