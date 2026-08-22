//! Status reporting: enumerate the link state of every configured store/target.

use super::resolve::{FileLink, LinkTargets, resolve_targets};
use crate::config::{self, Config};
use crate::linker::{self, LinkStatus};
use crate::platform::Platform;
use crate::render;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub store_name: String,
    /// Multi-target name, if this entry belongs to a named target. Single-
    /// target stores have `None`.
    pub target_name: Option<String>,
    /// Repo source path (the `.tmpl` for templates, plain file otherwise).
    /// Never the staged render — status compares the *link* against the
    /// effective source (staging for templates).
    pub source: PathBuf,
    /// The effective source the link is compared against (staging path for
    /// templates, the repo source otherwise). Carried so removal can re-check
    /// the exact entry — including source-symlink entries that resolve outside
    /// the repo — via the exact-entry `remove_link_to` rather than the broad
    /// `remove_link`.
    pub link_source: PathBuf,
    /// Template identity (store-relative for `files` entries, repo-relative
    /// for `sources` entries) — the `source_rel` render errors and staging
    /// lookups use.
    pub source_name: String,
    /// Link name under the target directory (e.g. "nested/alias" for a nested
    /// source). Used by `why` consumers to report the exact link name.
    pub link_name: String,
    /// True when this entry is backed by a `.tmpl` (link points at staging).
    pub is_template: bool,
    /// True when the entry comes from a `sources` declaration (v0.14): the
    /// link name is decoupled from a repo path outside the store dir.
    pub from_sources: bool,
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
                target_name: None,
                source: PathBuf::new(),
                link_source: PathBuf::new(),
                source_name: String::new(),
                link_name: String::new(),
                is_template: false,
                from_sources: false,
                target: PathBuf::new(),
                status: LinkStatus::Missing,
                skipped_platform: true,
            });
            continue;
        }

        let store_dir = repo_root.join(name);
        if std::fs::symlink_metadata(&store_dir).is_ok() && !linker::is_real_directory(&store_dir) {
            // The store root is unhealthy (not a real directory). Surface it as
            // `StoreError` for every target so `status`, `doctor`, `why`, and
            // `remove` all agree. Use the home target path (not the repo path) so
            // `stitch why <home-target>` can find the entry.
            if store.is_multi_target() {
                for (target_name, target_entry) in &store.targets {
                    if !platform.matches_when(&target_entry.when) {
                        continue;
                    }
                    let target_path = config::expand_home(&target_entry.target)
                        .expect("HOME was validated by Config::load");
                    entries.push(StatusEntry {
                        store_name: name.clone(),
                        target_name: Some(target_name.clone()),
                        source: store_dir.clone(),
                        link_source: store_dir.clone(),
                        source_name: String::new(),
                        link_name: String::new(),
                        is_template: false,
                        from_sources: false,
                        target: target_path,
                        status: LinkStatus::StoreError(store_dir.clone()),
                        skipped_platform: false,
                    });
                }
            } else if let Some(ref target_str) = store.target {
                let target_path =
                    config::expand_home(target_str).expect("HOME was validated by Config::load");
                entries.push(StatusEntry {
                    store_name: name.clone(),
                    target_name: None,
                    source: store_dir.clone(),
                    link_source: store_dir.clone(),
                    source_name: String::new(),
                    link_name: String::new(),
                    is_template: false,
                    from_sources: false,
                    target: target_path,
                    status: LinkStatus::StoreError(store_dir.clone()),
                    skipped_platform: false,
                });
            }
            continue;
        }
        if store.is_multi_target() {
            for (target_name, target_entry) in &store.targets {
                if !platform.matches_when(&target_entry.when) {
                    continue;
                }
                let target_path = config::expand_home(&target_entry.target)
                    .expect("HOME was validated by Config::load");
                entries.extend(collect_statuses(
                    repo_root,
                    name,
                    Some(target_name),
                    &store_dir,
                    &target_path,
                    &target_entry.files,
                    &target_entry.patterns,
                    &target_entry.sources,
                    &target_entry.ignore,
                ));
            }
        } else if let Some(ref target_str) = store.target {
            let target_path =
                config::expand_home(target_str).expect("HOME was validated by Config::load");
            entries.extend(collect_statuses(
                repo_root,
                name,
                None,
                &store_dir,
                &target_path,
                &store.files,
                &store.patterns,
                &store.sources,
                &store.ignore,
            ));
        }
    }

    entries
}

#[allow(clippy::too_many_arguments)] // internal helper; arity matches the data it carries
fn collect_statuses(
    repo_root: &Path,
    name: &str,
    target_name: Option<&str>,
    store_dir: &Path,
    target_path: &Path,
    files: &[String],
    patterns: &[String],
    sources: &std::collections::BTreeMap<String, String>,
    ignore: &[String],
) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    match resolve_targets(repo_root, store_dir, files, patterns, sources, ignore) {
        Err(msg) => {
            // Config-level resolution error (e.g. source-name collision). Keep
            // the message from resolve_targets so status/doctor/remove agree
            // with apply/diff that this store is misconfigured.
            entries.push(StatusEntry {
                store_name: name.to_string(),
                target_name: target_name.map(str::to_owned),
                source: store_dir.to_path_buf(),
                link_source: store_dir.to_path_buf(),
                source_name: String::new(),
                link_name: String::new(),
                target: target_path.to_path_buf(),
                status: LinkStatus::ConfigError(msg),
                skipped_platform: false,
                is_template: false,
                from_sources: false,
            });
        }
        Ok(LinkTargets::WholeDir) => {
            entries.push(StatusEntry {
                store_name: name.to_string(),
                target_name: target_name.map(str::to_owned),
                source: store_dir.to_path_buf(),
                link_source: store_dir.to_path_buf(),
                source_name: String::new(),
                link_name: String::new(),
                target: target_path.to_path_buf(),
                status: linker::check_link(target_path, store_dir, repo_root),
                skipped_platform: false,
                is_template: false,
                from_sources: false,
            });
        }
        Ok(LinkTargets::Files(links)) => {
            // File-mode target root must be a real directory, not a symlink
            // or real file. Surface a root conflict directly so status/doctor
            // agree with apply's foreign/real-file handling.
            if let Ok(meta) = std::fs::symlink_metadata(target_path) {
                if meta.file_type().is_symlink() {
                    // A symlink at the file-mode root is acceptable only if
                    // it is $HOME itself (home_link -> real_home) or this
                    // store's OWN whole-dir link awaiting promotion. Any
                    // other symlink — foreign, or repo-owned but belonging to
                    // a different store — is a root conflict: apply would
                    // refuse to write through it, so status/doctor must not
                    // report the per-file entries as merely missing.
                    let is_home_itself = crate::config::expand_home("~")
                        .ok()
                        .is_some_and(|home| home == target_path);
                    let owns_root = matches!(
                        linker::check_link(target_path, store_dir, repo_root),
                        linker::LinkStatus::Linked
                    );
                    if !is_home_itself && !owns_root {
                        let resolves_to = std::fs::read_link(target_path)
                            .unwrap_or_else(|_| PathBuf::from("(unreadable)"));
                        entries.push(StatusEntry {
                            store_name: name.to_string(),
                            target_name: target_name.map(str::to_owned),
                            source: store_dir.to_path_buf(),
                            link_source: store_dir.to_path_buf(),
                            source_name: String::new(),
                            link_name: String::new(),
                            target: target_path.to_path_buf(),
                            status: linker::LinkStatus::Foreign(resolves_to),
                            skipped_platform: false,
                            is_template: false,
                            from_sources: false,
                        });
                        return entries;
                    }
                } else if !meta.is_dir() {
                    // Real file blocks the directory target.
                    entries.push(StatusEntry {
                        store_name: name.to_string(),
                        target_name: target_name.map(str::to_owned),
                        source: store_dir.to_path_buf(),
                        link_source: store_dir.to_path_buf(),
                        source_name: String::new(),
                        link_name: String::new(),
                        target: target_path.to_path_buf(),
                        status: linker::LinkStatus::Conflict(target_path.to_path_buf()),
                        skipped_platform: false,
                        is_template: false,
                        from_sources: false,
                    });
                    return entries;
                }
            }
            entries.extend(links.iter().map(|link| {
                status_entry_for_link(repo_root, name, target_name, link, target_path)
            }));
        }
    }
    entries
}

/// Build one [`StatusEntry`] for a resolved file-mode entry. The link source
/// is the consumer's staging path for templates (per-store identity), the
/// repo source otherwise.
fn status_entry_for_link(
    repo_root: &Path,
    name: &str,
    target_name: Option<&str>,
    link: &FileLink,
    target_path: &Path,
) -> StatusEntry {
    let target = target_path.join(&link.name);
    let is_template = link.is_template();
    let link_source = if is_template {
        render::staging_path(repo_root, name, &link.name)
    } else {
        link.source.clone()
    };
    let status = linker::check_link(&target, &link_source, repo_root);
    StatusEntry {
        store_name: name.to_string(),
        target_name: target_name.map(str::to_owned),
        source: link.source.clone(),
        link_source,
        source_name: link.source_rel.clone(),
        link_name: link.name.clone(),
        is_template,
        from_sources: link.from_sources,
        target,
        status,
        skipped_platform: false,
    }
}
