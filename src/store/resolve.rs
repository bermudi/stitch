//! Shared resolution layer for stores: glob/ignore handling, source-name
//! resolution, link-source resolution, and reconciliation keep-set collection.
//!
//! Leaf module — no imports from other `store` submodules. Used by `apply`,
//! `plan_compute`, `status`, `doctor`, and (out of `store`) `safety`,
//! `plan_exec`, and `main` (prune).

use crate::config::{self, Store};
use crate::linker::{self, LinkStatus};
use crate::plan::path_to_string;
use crate::platform::Platform;
use crate::render;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Glob patterns always active for every store, regardless of config. Protects
/// against symlinking repo metadata (`.git`, `.stitch`, `.gitignore`,
/// `.DS_Store`) into a target. Per SPEC "Ignore patterns (v0.2)".
const GLOBAL_IGNORES: &[&str] = &[
    ".stitch",
    ".stitch/**",
    ".git",
    ".git/**",
    ".gitignore",
    ".DS_Store",
];

/// What a single store/target should link: one whole-directory symlink, or a
/// list of individual entries (file mode, or a whole-dir store promoted to
/// file mode because it contains ignored content).
pub(crate) enum LinkTargets {
    WholeDir,
    Files(Vec<String>),
}

/// Merge global ignores (always active) with a store's own `ignore` patterns.
fn merge_ignores(store_ignore: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = GLOBAL_IGNORES.iter().map(|s| (*s).to_string()).collect();
    merged.extend(store_ignore.iter().cloned());
    merged
}

/// Build a [`GlobSet`] from patterns. Invalid patterns are warned about on
/// stderr and skipped — one bad pattern does not disable the rest. Returns
/// `None` if `patterns` is empty or every pattern failed to compile.
fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut added = 0;
    for pat in patterns {
        match GlobBuilder::new(pat).literal_separator(false).build() {
            Ok(glob) => {
                builder.add(glob);
                added += 1;
            }
            Err(e) => eprintln!("warning: invalid glob pattern '{}': {}", pat, e),
        }
    }
    if added == 0 {
        return None;
    }
    builder.build().ok()
}

/// Populate the link and staging keep-sets from actual desired sources.
///
/// This intentionally bypasses collision validation: both sides of a
/// `foo`/`foo.tmpl` collision must keep their currently live output safe while
/// apply reports the resolution error. It also runs before `when` filtering so
/// a skipped target cannot make a shared target directory or staged render
/// look stale.
pub(crate) fn collect_reconciliation_keeps(
    store_dir: &Path,
    target_path: &Path,
    files: &[String],
    patterns: &[String],
    store_ignore: &[String],
    staging_keep_links: &mut BTreeSet<String>,
    target_keep_links: &mut BTreeMap<PathBuf, BTreeSet<String>>,
) {
    let LinkTargets::Files(names) = resolve_target_names(store_dir, files, patterns, store_ignore)
    else {
        return;
    };

    let target_links = target_keep_links
        .entry(target_path.to_path_buf())
        .or_default();
    for source_name in names {
        let entry = render::resolve_entry(&source_name);
        let source_path = store_dir.join(&entry.source_rel);
        // A vanished configured source is stale by definition and therefore
        // must not retain its old target link/render. Existing templates stay
        // live even when their render or resolution subsequently fails.
        // Use `symlink_metadata` (which does not follow symlinks) so a dangling
        // symlink source is still considered present.
        if let Ok(meta) = std::fs::symlink_metadata(&source_path)
            && !meta.is_dir()
        {
            target_links.insert(entry.link_rel.clone());
            if entry.is_template {
                staging_keep_links.insert(entry.link_rel);
            }
        }
    }
}

/// Resolve what to link for one store/target, rejecting source names that
/// collapse to the same link name (`foo` + `foo.tmpl`).
pub(super) fn resolve_targets(
    store_dir: &Path,
    files: &[String],
    patterns: &[String],
    store_ignore: &[String],
) -> Result<LinkTargets, String> {
    if let Some(path) = render::unsupported_template_source(store_dir)? {
        return Err(format!(
            "template source {} must be a direct regular file",
            path.display()
        ));
    }
    let targets = resolve_target_names(store_dir, files, patterns, store_ignore);
    if let LinkTargets::Files(ref names) = targets {
        render::check_name_collisions(names)?;
    }
    Ok(targets)
}

/// Compile trailing-slash patterns as directory roots. A root match applies to
/// every descendant, including when the root itself contains wildcards.
fn build_directory_globset(patterns: &[String]) -> Option<GlobSet> {
    let directories: Vec<String> = patterns
        .iter()
        .filter_map(|p| p.strip_suffix('/').map(str::to_owned))
        .collect();
    build_globset(&directories)
}

/// Whether `rel` is a directory-pattern root or a descendant of one.
fn is_under_directory_pattern(rel: &str, is_dir: bool, globset: Option<&GlobSet>) -> bool {
    let Some(globset) = globset else {
        return false;
    };
    let mut components: Vec<&str> = rel.split('/').collect();
    if !is_dir {
        components.pop();
    }
    while !components.is_empty() {
        if globset.is_match(components.join("/")) {
            return true;
        }
        components.pop();
    }
    false
}

fn is_ignored_path(
    name: &str,
    rel: &str,
    is_dir: bool,
    globset: Option<&GlobSet>,
    directory_globset: Option<&GlobSet>,
) -> bool {
    globset.is_some_and(|g| {
        g.is_match(name) || g.is_match(rel) || (is_dir && g.is_match(format!("{rel}/")))
    }) || is_under_directory_pattern(rel, is_dir, directory_globset)
}

fn has_ignored_entry(store_dir: &Path, ignore: &[String]) -> bool {
    let ignore_glob = build_globset(ignore);
    let ignore_dirs = build_directory_globset(ignore);

    for entry in walkdir::WalkDir::new(store_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.depth() == 0 {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(store_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        let name = entry.file_name().to_string_lossy();
        if is_ignored_path(
            &name,
            &rel_str,
            entry.file_type().is_dir(),
            ignore_glob.as_ref(),
            ignore_dirs.as_ref(),
        ) {
            return true;
        }
    }

    false
}

/// Resolve the desired mode and source names without collision validation.
/// Kept separate so reconciliation can conservatively preserve a live template
/// when normal resolution reports an error.
pub(crate) fn resolve_target_names(
    store_dir: &Path,
    files: &[String],
    patterns: &[String],
    store_ignore: &[String],
) -> LinkTargets {
    let ignore = merge_ignores(store_ignore);
    let explicit = !files.is_empty() || !patterns.is_empty();
    let names = if !explicit {
        let has_templates = render::store_has_templates(store_dir);
        if has_templates || has_ignored_entry(store_dir, &ignore) {
            // Promote: expand to every non-ignored file (full tree), so nested
            // `.tmpl` files become individual links rather than riding inside a
            // whole-dir symlink as literal `.tmpl` sources.
            resolve_files(store_dir, &[], &["**/*".into(), "*".into()], &ignore)
        } else {
            return LinkTargets::WholeDir;
        }
    } else {
        resolve_files(store_dir, files, patterns, &ignore)
    };

    LinkTargets::Files(names)
}

/// Collect the link target paths a store will write through, plus the set of
/// target roots that will be explicitly removed before any child links are
/// created (whole-directory → file-mode promotion roots).
pub(crate) fn collect_store_link_targets(
    repo_root: &Path,
    store_name: &str,
    store: &Store,
    platform: &Platform,
) -> Result<(Vec<PathBuf>, BTreeSet<PathBuf>), String> {
    let mut targets = Vec::new();
    let mut removed = BTreeSet::new();

    if !platform.matches_when(&store.when) {
        return Ok((targets, removed));
    }

    let store_dir = repo_root.join(store_name);
    if store.is_multi_target() {
        for target_entry in store.targets.values() {
            if !platform.matches_when(&target_entry.when) {
                continue;
            }
            let target_path = config::expand_home(&target_entry.target)
                .map_err(|e| format!("could not expand target: {e}"))?;
            collect_link_targets_for_target(
                &store_dir,
                &target_path,
                repo_root,
                &target_entry.files,
                &target_entry.patterns,
                &target_entry.ignore,
                &mut targets,
                &mut removed,
            )?;
        }
    } else if let Some(target) = &store.target {
        let target_path =
            config::expand_home(target).map_err(|e| format!("could not expand target: {e}"))?;
        collect_link_targets_for_target(
            &store_dir,
            &target_path,
            repo_root,
            &store.files,
            &store.patterns,
            &store.ignore,
            &mut targets,
            &mut removed,
        )?;
    }

    Ok((targets, removed))
}

#[allow(clippy::too_many_arguments)]
fn collect_link_targets_for_target(
    store_dir: &Path,
    target_path: &Path,
    repo_root: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
    targets: &mut Vec<PathBuf>,
    removed: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    match resolve_target_names(store_dir, files, patterns, ignore) {
        LinkTargets::WholeDir => {
            targets.push(target_path.to_path_buf());
        }
        LinkTargets::Files(names) => {
            // Whole-directory → file-mode promotion removes the whole-dir link
            // at `target_path` before creating child links.
            if std::fs::symlink_metadata(target_path).is_ok_and(|m| m.file_type().is_symlink())
                && linker::check_link(target_path, store_dir, repo_root) == LinkStatus::Linked
            {
                removed.insert(target_path.to_path_buf());
            }
            for name in names {
                let entry = render::resolve_entry(&name);
                targets.push(target_path.join(&entry.link_rel));
            }
        }
    }
    Ok(())
}

/// Resolve the complete file list for a file-mode store.
///
/// Combines explicit `files` with glob `patterns` matched recursively against
/// the store directory via `walkdir`, then removes anything matched by
/// `ignore` patterns (the caller passes the merged global + per-store set).
/// Returns deduplicated, sorted paths relative to `store_dir`.
///
/// Globs are matched against both the file name and the full relative path,
/// so a bare `*.conf` matches at any depth while `subdir/*.conf` scopes the
/// match. Ignore patterns ending in `/` exclude entire subdirectory trees.
fn resolve_files(
    store_dir: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // Explicit files always included (validated elsewhere — safe fragments).
    for f in files {
        seen.insert(f.clone());
    }

    // Walk the store directory recursively.
    let include_glob = build_globset(patterns);
    let include_dirs = build_directory_globset(patterns);
    let ignore_glob = build_globset(ignore);
    let ignore_dirs = build_directory_globset(ignore);

    let filter = |entry: &walkdir::DirEntry| -> bool {
        if entry.depth() == 0 {
            return true;
        }
        let Ok(rel) = entry.path().strip_prefix(store_dir) else {
            return true;
        };
        let rel_str = rel.to_string_lossy();
        let name = entry.file_name().to_string_lossy();
        !is_ignored_path(
            &name,
            &rel_str,
            entry.file_type().is_dir(),
            ignore_glob.as_ref(),
            ignore_dirs.as_ref(),
        )
    };

    for entry in walkdir::WalkDir::new(store_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(filter)
        .filter_map(|e| e.ok())
    {
        // Include regular files and symlinks (both file and directory symlinks).
        // Plain directories are represented by their children, so they are still
        // skipped here.
        if !entry.file_type().is_file() && !entry.file_type().is_symlink() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(store_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        let file_name = entry.file_name().to_string_lossy();

        // Include if the pattern matches file name or relative path.
        if include_glob
            .as_ref()
            .is_some_and(|g| g.is_match(rel_str.as_ref()) || g.is_match(file_name.as_ref()))
            || is_under_directory_pattern(rel_str.as_ref(), false, include_dirs.as_ref())
        {
            seen.insert(rel_str.into_owned());
        }
    }

    seen.into_iter().collect()
}

/// Whether `target` is a whole-directory link pointing exactly at the
/// canonical `store_dir`. Returns the resolved link target when it matches.
pub(super) fn whole_dir_link_target(target: &Path, store_dir: &Path) -> Option<PathBuf> {
    let resolved = std::fs::read_link(target).ok()?;
    let canonical_store = store_dir.canonicalize().ok()?;
    (resolved == canonical_store).then_some(resolved)
}

pub(crate) fn resolve_link_source(
    repo_root: &Path,
    store_dir: &Path,
    store: Option<&Store>,
    store_name: &str,
    target: &Path,
) -> Option<String> {
    let store_config = store?;

    // A single-target store carries its inventory on Store itself.
    if !store_config.is_multi_target()
        && let Some(target_str) = &store_config.target
        && let Some(source) = resolve_link_source_for_target(
            repo_root,
            store_dir,
            store_name,
            target,
            &config::expand_home(target_str).ok()?,
            &store_config.files,
            &store_config.patterns,
            &store_config.ignore,
        )
    {
        return Some(source);
    }

    // Multi-target stores carry independent inventories and ignore rules on
    // each TargetEntry. A target path must be resolved with the entry that
    // owns it, not with the store-level (usually empty) lists.
    for target_entry in store_config.targets.values() {
        if let Some(source) = resolve_link_source_for_target(
            repo_root,
            store_dir,
            store_name,
            target,
            &config::expand_home(&target_entry.target).ok()?,
            &target_entry.files,
            &target_entry.patterns,
            &target_entry.ignore,
        ) {
            return Some(source);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn resolve_link_source_for_target(
    repo_root: &Path,
    store_dir: &Path,
    store_name: &str,
    target: &Path,
    target_path: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
) -> Option<String> {
    let resolved = resolve_target_names(store_dir, files, patterns, ignore);

    // A target root is desired only in whole-directory mode. File mode may
    // still need to remove a former whole-directory link at this path.
    if target == target_path {
        return matches!(resolved, LinkTargets::WholeDir).then(|| path_to_string(store_dir));
    }

    let rel = target.strip_prefix(target_path).ok()?;
    let link_rel = rel.to_string_lossy().into_owned();
    let LinkTargets::Files(source_names) = resolved else {
        return None;
    };
    for source_name in source_names {
        let entry = render::resolve_entry(&source_name);
        if entry.link_rel == link_rel {
            if entry.is_template {
                return Some(path_to_string(&render::staging_path(
                    repo_root, store_name, &link_rel,
                )));
            }
            return Some(path_to_string(&store_dir.join(&entry.source_rel)));
        }
    }
    None
}

pub(super) fn resolve_remove_source(
    repo_root: &Path,
    store_dir: &Path,
    store: Option<&Store>,
    store_name: &str,
    target: &Path,
) -> Option<String> {
    if !target.is_symlink() {
        return None;
    }
    // Whole-directory root removal: the link points exactly at the canonical
    // store dir. Keep the configured path as the source in the plan so it
    // remains consistent with normal whole-directory link planning.
    if whole_dir_link_target(target, store_dir).is_some() {
        return Some(path_to_string(store_dir));
    }
    // File-mode stale link: resolve back to the store-relative source, then
    // verify the link actually points at it (exact-entry). A foreign link
    // sitting at a configured target location — or one pointing *through* a
    // repo gateway symlink to outside the repo — fails the exact match and is
    // not planned for removal. This replaces the old broad points_into_repo
    // gate, which was too strict for source-symlink entries (a configured
    // source that is itself a symlink resolving outside the repo) and too lax
    // for gateway links (it classified them as repo-owned by immediate hop).
    let candidate = resolve_link_source(repo_root, store_dir, store, store_name, target)?;
    let expected = Path::new(&candidate);
    if linker::points_at_source(target, expected, repo_root) {
        Some(candidate)
    } else {
        None
    }
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

    #[test]
    fn test_resolve_files_recursive_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(store_dir.join("sub")).unwrap();

        std::fs::write(store_dir.join("top.conf"), "...").unwrap();
        std::fs::write(store_dir.join("sub").join("nested.conf"), "...").unwrap();
        std::fs::write(store_dir.join("sub").join("other.txt"), "...").unwrap();

        // `*.conf` matches at any depth (leaf name match).
        let resolved = resolve_files(&store_dir, &[], &["*.conf".into()], &[]);
        assert_eq!(resolved, vec!["sub/nested.conf", "top.conf"]);
    }

    #[test]
    fn test_resolve_files_recursive_scoped_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(store_dir.join("sub")).unwrap();

        std::fs::write(store_dir.join("top.conf"), "...").unwrap();
        std::fs::write(store_dir.join("sub").join("nested.conf"), "...").unwrap();

        // `sub/*.conf` scopes to the subdirectory.
        let resolved = resolve_files(&store_dir, &[], &["sub/*.conf".into()], &[]);
        assert_eq!(resolved, vec!["sub/nested.conf"]);
    }

    #[test]
    fn test_resolve_files_ignore_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(store_dir.join("scratch")).unwrap();

        std::fs::write(store_dir.join("keep.conf"), "...").unwrap();
        std::fs::write(store_dir.join("scratch").join("junk.conf"), "...").unwrap();
        std::fs::create_dir_all(store_dir.join("scratch").join("deep")).unwrap();
        std::fs::write(
            store_dir.join("scratch").join("deep").join("also.conf"),
            "...",
        )
        .unwrap();

        // `scratch/` ignores the entire subdirectory tree.
        let resolved = resolve_files(&store_dir, &[], &["*.conf".into()], &["scratch/".into()]);
        assert_eq!(resolved, vec!["keep.conf"]);
    }

    #[test]
    fn test_resolve_files_trailing_wildcard_directory_patterns_are_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(store_dir.join("foo123/deep")).unwrap();
        std::fs::create_dir_all(store_dir.join("other")).unwrap();
        std::fs::write(store_dir.join("foo123/one.conf"), "...").unwrap();
        std::fs::write(store_dir.join("foo123/deep/two.conf"), "...").unwrap();
        std::fs::write(store_dir.join("other/keep.conf"), "...").unwrap();

        let included = resolve_files(&store_dir, &[], &["foo*/".into()], &[]);
        assert_eq!(included, vec!["foo123/deep/two.conf", "foo123/one.conf"]);

        let ignored = resolve_files(&store_dir, &[], &["*.conf".into()], &["foo*/".into()]);
        assert_eq!(ignored, vec!["other/keep.conf"]);
    }

    #[test]
    fn test_resolve_files_ignore_recursive_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(store_dir.join("sub")).unwrap();

        std::fs::write(store_dir.join("top.conf"), "...").unwrap();
        std::fs::write(store_dir.join("top.bak"), "...").unwrap();
        std::fs::write(store_dir.join("sub").join("nested.bak"), "...").unwrap();
        std::fs::write(store_dir.join("sub").join("nested.conf"), "...").unwrap();

        // `*.bak` matches at any depth.
        let resolved = resolve_files(
            &store_dir,
            &[],
            &["*.conf".into(), "*.bak".into()],
            &["*.bak".into()],
        );
        assert_eq!(resolved, vec!["sub/nested.conf", "top.conf"]);
    }

    #[test]
    fn test_resolve_files_empty_dirs_yield_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(store_dir.join("empty")).unwrap();
        std::fs::write(store_dir.join("real.conf"), "...").unwrap();

        // Empty directories produce no matches, no errors.
        let resolved = resolve_files(&store_dir, &[], &["*.conf".into()], &[]);
        assert_eq!(resolved, vec!["real.conf"]);
    }

    #[test]
    fn test_resolve_files_includes_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        std::os::unix::fs::symlink("init.lua", store_dir.join("init.vim")).unwrap();

        let resolved = resolve_files(&store_dir, &[], &["**/*".into(), "*".into()], &[]);
        assert_eq!(resolved, vec!["init.lua", "init.vim"]);
    }

    #[test]
    fn test_resolve_files_includes_dangling_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("mystore");
        std::fs::create_dir_all(&store_dir).unwrap();

        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        std::os::unix::fs::symlink("missing", store_dir.join("dangling")).unwrap();

        let resolved = resolve_files(&store_dir, &[], &["**/*".into(), "*".into()], &[]);
        assert_eq!(resolved, vec!["dangling", "init.lua"]);
    }
}
