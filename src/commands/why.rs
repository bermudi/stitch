use super::common::print_warnings;
use crate::config::{self, Config};
use crate::error::StitchError;
use crate::linker::LinkStatus;
use crate::platform::Platform;
use crate::report::{self, WhyData, WhyEntry};
use crate::store;

pub(crate) fn cmd_why(root: &std::path::Path, query: &str, json: bool) -> Result<(), StitchError> {
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    let platform = Platform::detect();

    // Expand the query path the same way config targets are expanded, so a
    // user can pass `~/.bashrc` and match a target stored as `~/.bashrc`.
    let query_path = config::expand_home(query)
        .map_err(|e| StitchError::usage(format!("invalid target path: {e}")))?;
    let query_canonical = canonicalize_or_path(&query_path);

    // Run status_all and find the entry whose target matches the query.
    let entries = store::status_all(root, &loaded.config, &platform);

    let mut matched: Option<&store::StatusEntry> = None;
    let mut skipped_platform = false;
    for entry in &entries {
        if entry.skipped_platform {
            // Skipped-platform entries carry an empty target; the second loop
            // below handles skipped-platform matching by store config.
            continue;
        }
        if path_matches(&entry.target, &query_canonical, &query_path) {
            matched = Some(entry);
            break;
        }
    }

    // Whole-dir containment: if no entry's target *is* the query, a path may
    // still live *inside* a whole-dir store's target directory (the directory
    // itself is the stitch-managed symlink, so there are no per-file entries).
    // Pick the deepest matching ancestor so nested whole-dir stores resolve to
    // the closest owner.
    let mut matched_subpath: Option<std::path::PathBuf> = None;
    if matched.is_none() {
        let mut best_len = 0usize;
        for entry in &entries {
            if entry.skipped_platform {
                continue;
            }
            if let Some(rel) = path_inside(&entry.target, &query_canonical, &query_path) {
                let len = entry.target.as_os_str().len();
                if len > best_len {
                    best_len = len;
                    matched = Some(entry);
                    matched_subpath = Some(rel);
                }
            }
        }
    }

    // If no active entry matched but a skipped store/target covers the path,
    // report skipped_platform. Mirror status_all's target enumeration: check
    // both top-level `store.target` and named `targets` entries, including
    // target-level `when` filters when the store itself is active. Coverage
    // includes whole-dir containment (a path inside a skipped whole-dir
    // target), not just exact target equality.
    if matched.is_none() && !skipped_platform {
        for store in loaded.config.stores.values() {
            if !platform.matches_when(&store.when) {
                // Store-level when fails: check all its target paths.
                if store.is_multi_target() {
                    for target_entry in store.targets.values() {
                        if let Ok(target) = config::expand_home(&target_entry.target)
                            && path_covers(&target, &query_canonical, &query_path)
                        {
                            skipped_platform = true;
                            break;
                        }
                    }
                } else if let Some(ref target_str) = store.target
                    && let Ok(target) = config::expand_home(target_str)
                    && path_covers(&target, &query_canonical, &query_path)
                {
                    skipped_platform = true;
                }
            } else {
                // Store is active but a named target's when may fail.
                if store.is_multi_target() {
                    for target_entry in store.targets.values() {
                        if !platform.matches_when(&target_entry.when)
                            && let Ok(target) = config::expand_home(&target_entry.target)
                            && path_covers(&target, &query_canonical, &query_path)
                        {
                            skipped_platform = true;
                            break;
                        }
                    }
                }
            }
            if skipped_platform {
                break;
            }
        }
    }

    let entry = matched.map(|e| build_why_entry(e, matched_subpath.as_deref()));

    let data = WhyData {
        query: query.to_string(),
        entry,
        skipped_platform,
    };

    if json {
        report::write("why", data, loaded.warnings);
        return Ok(());
    }

    print_why(&data);
    Ok(())
}

fn build_why_entry(
    entry: &store::StatusEntry,
    matched_subpath: Option<&std::path::Path>,
) -> WhyEntry {
    let (state, resolves_to) = match &entry.status {
        LinkStatus::Linked => ("linked".to_string(), None),
        LinkStatus::Missing => ("missing".to_string(), None),
        LinkStatus::Conflict(_) => ("conflict".to_string(), None),
        LinkStatus::Broken(p) => ("broken".to_string(), Some(p.to_string_lossy().into_owned())),
        LinkStatus::Foreign(p) => (
            "foreign".to_string(),
            Some(p.to_string_lossy().into_owned()),
        ),
        LinkStatus::StoreError(p) => (
            "store-error".to_string(),
            Some(p.to_string_lossy().into_owned()),
        ),
        LinkStatus::ConfigError(msg) => ("config-error".to_string(), Some(msg.clone())),
    };
    WhyEntry {
        store: entry.store_name.clone(),
        target_name: entry.target_name.clone(),
        target: entry.target.to_string_lossy().into_owned(),
        source: entry.source.to_string_lossy().into_owned(),
        templated: entry.is_template,
        state,
        resolves_to,
        matched_subpath: matched_subpath.map(|p| p.to_string_lossy().into_owned()),
        owning_config: "state.toml".to_string(),
    }
}

/// Check if a status entry's target path matches the query. Compares both
/// canonical and literal paths to handle symlinked home dirs.
///
/// The target is canonicalized via its *parent* directory only, keeping the
/// final component literal. This prevents a stitch-managed symlink at the
/// target from resolving to the repo source — `stitch why /repo/store/file`
/// must not match a target whose symlink points back at that source.
fn path_matches(
    target: &std::path::Path,
    query_canonical: &std::path::Path,
    query_literal: &std::path::Path,
) -> bool {
    if target == query_literal {
        return true;
    }
    let target_canonical = canonicalize_parent_join(target);
    target_canonical == *query_canonical
}

/// True if `target` owns the query either exactly (`path_matches`) or by
/// whole-dir containment (`path_inside`). Used for skipped-platform detection
/// where a path may sit inside a skipped whole-dir store's target directory.
fn path_covers(
    target: &std::path::Path,
    query_canonical: &std::path::Path,
    query_literal: &std::path::Path,
) -> bool {
    path_matches(target, query_canonical, query_literal)
        || path_inside(target, query_canonical, query_literal).is_some()
}

/// If `target` is a proper ancestor directory of the query, return the
/// subpath of the query relative to `target`. Used to detect that a path
/// lives *inside* a whole-dir store's target directory (the directory itself
/// is the stitch-managed symlink, so per-file entries don't exist).
///
/// Mirrors `path_matches`'s canonicalization strategy: literal component
/// prefix first, then canonical (following the target symlink so a query
/// inside a linked whole-dir target resolves through it). The empty relative
/// path (query *is* the target) is rejected — that case is `path_matches`.
fn path_inside(
    target: &std::path::Path,
    query_canonical: &std::path::Path,
    query_literal: &std::path::Path,
) -> Option<std::path::PathBuf> {
    // Literal component-prefix: target must be a directory ancestor.
    if let Ok(rel) = query_literal.strip_prefix(target)
        && !rel.as_os_str().is_empty()
    {
        return Some(rel.to_path_buf());
    }
    // Canonical: follow the target symlink (a whole-dir target is itself the
    // link) so a query inside it resolves through to the repo source dir.
    let target_canonical = std::fs::canonicalize(target).ok()?;
    let rel = query_canonical.strip_prefix(&target_canonical).ok()?;
    if !rel.as_os_str().is_empty() {
        Some(rel.to_path_buf())
    } else {
        None
    }
}

/// Canonicalize a path for comparison, falling back to the literal path if
/// canonicalization fails (e.g. the path doesn't exist yet).
fn canonicalize_or_path(path: &std::path::Path) -> std::path::PathBuf {
    canonicalize_parent_join(path)
}

/// Canonicalize the parent directory of a path and re-join the final
/// component literally. This avoids following a symlink at the path itself
/// (e.g. a stitch-managed target symlink that resolves to the repo source).
fn canonicalize_parent_join(path: &std::path::Path) -> std::path::PathBuf {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        // No parent (e.g. "/"): the path is its own canonical form.
        _ => return path.to_path_buf(),
    };
    let file_name = match path.file_name() {
        Some(f) => f,
        None => return path.to_path_buf(),
    };
    match std::fs::canonicalize(parent) {
        Ok(canon_parent) => canon_parent.join(file_name),
        Err(_) => path.to_path_buf(),
    }
}

fn print_why(data: &WhyData) {
    println!("query: {}", data.query);
    if data.skipped_platform {
        println!("skipped: platform (store's `when` does not match this host)");
    }
    match &data.entry {
        Some(e) => {
            println!("store: {}", e.store);
            if let Some(ref name) = e.target_name {
                println!("target_name: {name}");
            }
            println!("target: {}", e.target);
            println!("source: {}", e.source);
            println!("templated: {}", e.templated);
            println!("state: {}", e.state);
            if let Some(ref r) = e.resolves_to {
                println!("resolves_to: {r}");
            }
            if let Some(ref sub) = e.matched_subpath {
                println!("matched_subpath: {sub}");
            }
            println!("owning_config: {}", e.owning_config);
        }
        None => {
            if !data.skipped_platform {
                println!("no store owns this path");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linker::LinkStatus;
    use crate::report::WhyEntry;
    use crate::store::StatusEntry;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    fn why_entry(status: LinkStatus) -> WhyEntry {
        let entry = StatusEntry {
            store_name: "bash".to_string(),
            target_name: Some("work".to_string()),
            source: PathBuf::from("/repo/bash/.bashrc"),
            link_source: PathBuf::from("/repo/bash/.bashrc"),
            target: PathBuf::from("/home/.bashrc"),
            status,
            skipped_platform: false,
            is_template: true,
        };
        build_why_entry(&entry, None)
    }

    #[test]
    fn path_matches_literal_path() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("a");
        std::fs::write(&target, "").unwrap();
        let query = tmp.path().join("a");
        assert!(path_matches(&target, &canonicalize_or_path(&query), &query));
    }

    #[test]
    fn path_matches_does_not_follow_target_symlink() {
        // A stitch-managed target symlink at `link` points to `real` (the repo
        // source). `stitch why /repo/source/file` (query = `real`) must NOT
        // match the target `link`, because the query is the source, not the
        // managed target. The target is canonicalized via its parent only,
        // keeping the final component literal, so the symlink at the target
        // itself is not followed.
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("a");
        std::fs::write(&real, "").unwrap();
        let link = tmp.path().join("b");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let query_canonical = std::fs::canonicalize(&real).unwrap();
        assert!(!path_matches(&link, &query_canonical, &real));
    }

    #[test]
    fn path_matches_follows_parent_symlink() {
        // When the *parent* of the target is a symlink (e.g. ~ → /home/user),
        // the match should still work: the parent is canonicalized, and the
        // final component is joined literally.
        let tmp = tempdir().unwrap();
        let real_dir = tmp.path().join("real_dir");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link_dir = tmp.path().join("link_dir");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
        // Target is link_dir/.bashrc (doesn't need to exist for parent-canonicalization)
        let target = link_dir.join(".bashrc");
        // Query is real_dir/.bashrc (canonicalized through the parent)
        let query = real_dir.join(".bashrc");
        let query_canonical = std::fs::canonicalize(&real_dir).unwrap().join(".bashrc");
        assert!(path_matches(&target, &query_canonical, &query));
    }

    #[test]
    fn path_matches_mismatch_returns_false() {
        let a = Path::new("/home/a");
        let b = Path::new("/home/b");
        assert!(!path_matches(a, &canonicalize_or_path(b), b));
    }

    #[test]
    fn canonicalize_or_path_existing_returns_canonical() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("sub").join("a");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
        let c = canonicalize_or_path(&p);
        assert!(c.is_absolute());
        assert!(c.ends_with("sub/a"));
    }

    #[test]
    fn canonicalize_or_path_missing_returns_literal() {
        let p = Path::new("/no/such/path/exists");
        let c = canonicalize_or_path(p);
        assert_eq!(c, p);
    }

    #[test]
    fn build_why_entry_linked_and_missing() {
        for status in [LinkStatus::Linked, LinkStatus::Missing] {
            let expected = if matches!(&status, LinkStatus::Linked) {
                "linked"
            } else {
                "missing"
            };
            let e = why_entry(status);
            assert_eq!(e.state, expected);
            assert!(e.resolves_to.is_none());
        }
    }

    #[test]
    fn build_why_entry_conflict() {
        let e = why_entry(LinkStatus::Conflict(PathBuf::from("/x")));
        assert_eq!(e.state, "conflict");
        assert!(e.resolves_to.is_none());
    }

    #[test]
    fn build_why_entry_broken_foreign_store_error() {
        let cases = [
            (
                LinkStatus::Broken(PathBuf::from("/gone")),
                "broken",
                Some("/gone"),
            ),
            (
                LinkStatus::Foreign(PathBuf::from("/other")),
                "foreign",
                Some("/other"),
            ),
            (
                LinkStatus::StoreError(PathBuf::from("/store")),
                "store-error",
                Some("/store"),
            ),
        ];
        for (status, expected_state, expected_resolves) in cases {
            let e = why_entry(status);
            assert_eq!(e.state, expected_state);
            assert_eq!(e.resolves_to.as_deref(), expected_resolves);
        }
    }

    #[test]
    fn build_why_entry_config_error() {
        let e = why_entry(LinkStatus::ConfigError("bad pattern".to_string()));
        assert_eq!(e.state, "config-error");
        assert_eq!(e.resolves_to.as_deref(), Some("bad pattern"));
    }

    #[test]
    fn build_why_entry_preserves_identity() {
        let e = why_entry(LinkStatus::Linked);
        assert_eq!(e.store, "bash");
        assert_eq!(e.target_name.as_deref(), Some("work"));
        assert_eq!(e.target, "/home/.bashrc");
        assert_eq!(e.source, "/repo/bash/.bashrc");
        assert!(e.templated);
        assert_eq!(e.owning_config, "state.toml");
    }

    #[test]
    fn build_why_entry_records_matched_subpath() {
        let entry = StatusEntry {
            store_name: "nvim".to_string(),
            target_name: None,
            source: PathBuf::from("/repo/nvim"),
            link_source: PathBuf::from("/repo/nvim"),
            target: PathBuf::from("/home/.config"),
            status: LinkStatus::Linked,
            skipped_platform: false,
            is_template: false,
        };
        let e = build_why_entry(&entry, Some(Path::new("init.lua")));
        assert_eq!(e.matched_subpath.as_deref(), Some("init.lua"));
        // Exact-match entries (no subpath arg) omit the field.
        let e2 = build_why_entry(&entry, None);
        assert!(e2.matched_subpath.is_none());
    }

    #[test]
    fn path_inside_literal_subpath() {
        let target = Path::new("/home/.config");
        let query = Path::new("/home/.config/init.lua");
        let rel = path_inside(target, &canonicalize_or_path(query), query);
        assert_eq!(rel.as_deref(), Some(Path::new("init.lua")));
    }

    #[test]
    fn path_inside_rejects_exact_match() {
        // Query *is* the target — that's path_matches, not containment.
        let target = Path::new("/home/.config");
        let rel = path_inside(target, &canonicalize_or_path(target), target);
        assert!(rel.is_none());
    }

    #[test]
    fn path_inside_rejects_unrelated() {
        let target = Path::new("/home/.config");
        let query = Path::new("/home/.config-other/x");
        // Component boundary: ".config" != ".config-other".
        let rel = path_inside(target, &canonicalize_or_path(query), query);
        assert!(rel.is_none());
    }

    #[test]
    fn path_inside_follows_target_symlink() {
        // Whole-dir target `link` is a symlink to real_dir; a query inside it
        // resolves through the symlink and matches the canonicalized target.
        let tmp = tempdir().unwrap();
        let real_dir = tmp.path().join("real_dir");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        std::fs::write(real_dir.join("init.lua"), "").unwrap();

        let target = link.clone();
        let query = link.join("init.lua");
        let query_canonical = std::fs::canonicalize(&real_dir).unwrap().join("init.lua");
        let rel = path_inside(&target, &query_canonical, &query);
        assert_eq!(rel.as_deref(), Some(Path::new("init.lua")));
    }

    #[test]
    fn path_inside_nested_subpath() {
        let target = Path::new("/home/.config");
        let query = Path::new("/home/.config/nvim/lua/opts.lua");
        let rel = path_inside(target, &canonicalize_or_path(query), query);
        assert_eq!(rel.as_deref(), Some(Path::new("nvim/lua/opts.lua")));
    }
}
