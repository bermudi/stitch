//! Health checks (`stitch doctor`): config-level findings and per-link status
//! diagnostics.

use super::apply::has_active_template_sources;
use super::resolve::{LinkTargets, resolve_target_names};
use super::status::status_all;
use crate::config::{self, Loaded, WhenClause};
use crate::linker::LinkStatus;
use crate::platform::Platform;
use crate::render;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A single health-check finding from `stitch doctor`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DoctorFinding {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub path: Option<PathBuf>,
    pub hint: Option<String>,
}

#[derive(Debug)]
pub struct DoctorResult {
    pub findings: Vec<DoctorFinding>,
}

/// Build a `duplicate-target` message that names both stores, and names the
/// specific target entry when the collision involves a multi-target store.
fn duplicate_target_message(
    a_store: &str,
    a_tname: Option<&str>,
    b_store: &str,
    b_tname: Option<&str>,
    path: &Path,
) -> String {
    match (a_tname, b_tname) {
        (None, None) => {
            format!(
                "stores '{a_store}' and '{b_store}' both target '{}'",
                path.display()
            )
        }
        (None, Some(b)) => format!(
            "store '{a_store}' and target '{b}' of store '{b_store}' both target '{}'",
            path.display()
        ),
        (Some(a), None) => format!(
            "target '{a}' of store '{a_store}' and store '{b_store}' both target '{}'",
            path.display()
        ),
        (Some(a), Some(b)) if a_store == b_store => format!(
            "targets '{a}' and '{b}' of store '{a_store}' both target '{}'",
            path.display()
        ),
        (Some(a), Some(b)) => format!(
            "target '{a}' of store '{a_store}' and target '{b}' of store '{b_store}' both target '{}'",
            path.display()
        ),
    }
}

/// Whether a symlink resolves (even dangling) back into this repo. Used by
/// the `alias-symlink` finding: live links via the canonical ownership
/// predicate, dangling ones via missing-path resolution against the
/// canonicalized repo root.
fn link_resolves_into_repo(link: &Path, repo_root: &Path) -> bool {
    if crate::linker::points_into_repo(link, repo_root) {
        return true;
    }
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    let absolute = if target.is_absolute() {
        target
    } else {
        link.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target)
    };
    let repo_canon = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    crate::linker::resolve_path_with_missing(&absolute)
        .is_some_and(|resolved| resolved.starts_with(&repo_canon))
}

/// Run health checks.
///
/// Takes [`Loaded`] (both halves) rather than just the merged [`Config`] so it
/// can detect orphaned behavior: a store present in `stitch.toml` (authored)
/// but missing from `state.toml` (generated) — e.g. left behind deliberately
/// by `remove`, which never rewrites the authored file.
pub fn doctor(repo_root: &Path, loaded: &Loaded, platform: &Platform) -> DoctorResult {
    let mut findings = Vec::new();
    let config = &loaded.config;

    // Existing pre-template repos do not need a migration merely because
    // `init` now creates a render directory. Once a configured template or
    // staged output exists, though, this is a hard trust boundary.
    let rendering_in_use = has_active_template_sources(repo_root, config, platform)
        || render::has_staged_output(repo_root);
    if rendering_in_use && !render::repo_gitignore_covers_render(repo_root) {
        findings.push(DoctorFinding {
            id: "missing-render-gitignore",
            severity: Severity::Error,
            message: format!(
                "repo .gitignore is missing `{}` — add that entry before rendering templates; \
                 staged output must never be committed",
                render::RENDER_GITIGNORE_ENTRY
            ),
            path: None,
            hint: Some(format!(
                "add `{}` to the repo's .gitignore",
                render::RENDER_GITIGNORE_ENTRY
            )),
        });
    }

    // Permission contract on an existing staging tree.
    let render_root = render::render_root(repo_root);
    if render_root.exists() {
        match render::path_mode(&render_root) {
            Some(mode) if mode != 0o700 => {
                findings.push(DoctorFinding {
                    id: "staging-permission-drift",
                    severity: Severity::Error,
                    message: format!(
                        "{} has mode {:04o}, expected 0700",
                        render_root.display(),
                        mode
                    ),
                    path: Some(render_root.clone()),
                    hint: Some(format!("chmod 0700 {}", render_root.display())),
                });
            }
            _ => {}
        }
    }

    if config.stores.is_empty() {
        findings.push(DoctorFinding {
            id: "empty-config",
            severity: Severity::Warning,
            message: "no stores configured".into(),
            path: None,
            hint: Some("use `stitch add <path>` to create a store".into()),
        });
        return DoctorResult { findings };
    }

    findings.push(DoctorFinding {
        id: "store-count",
        severity: Severity::Info,
        message: format!("{} stores configured", config.stores.len()),
        path: None,
        hint: None,
    });

    // Orphaned behavior: a store declared in stitch.toml with no state.toml
    // inventory. load-OK (it contributes no link), but worth surfacing so the
    // user can prune the authored entry if it's stale.
    for name in loaded.authored.stores.keys() {
        if !loaded.generated.stores.contains_key(name) {
            findings.push(DoctorFinding {
                id: "orphaned-behavior",
                severity: Severity::Warning,
                message: format!(
                    "store '{name}': behavior configured but no links (orphaned after remove?)"
                ),
                path: None,
                hint: Some(format!(
                    "remove the `{name}` entry from stitch.toml if it is stale"
                )),
            });
        }
    }

    struct Claim {
        store: String,
        tname: Option<String>,
        whens: Vec<WhenClause>,
    }

    fn make_claim(
        store_name: &str,
        tname: Option<&str>,
        store_when: &WhenClause,
        target_when: Option<&WhenClause>,
    ) -> Claim {
        let mut whens = Vec::with_capacity(2);
        whens.push(store_when.clone());
        if let Some(w) = target_when {
            whens.push(w.clone());
        }
        Claim {
            store: store_name.to_string(),
            tname: tname.map(|s| s.to_string()),
            whens,
        }
    }

    fn claims_compatible(a: &Claim, b: &Claim) -> bool {
        let mut combined: Vec<&WhenClause> = Vec::with_capacity(a.whens.len() + b.whens.len());
        combined.extend(a.whens.iter());
        combined.extend(b.whens.iter());
        WhenClause::are_compatible(&combined)
    }

    let mut seen_targets: BTreeMap<PathBuf, Vec<Claim>> = BTreeMap::new();

    // Compute status once, not per store.
    let all_statuses = status_all(repo_root, config, platform);

    for (name, store) in &config.stores {
        let store_dir = repo_root.join(name);

        if !store_dir.exists() {
            findings.push(DoctorFinding {
                id: "missing-store-dir",
                severity: Severity::Error,
                message: format!(
                    "store '{}': directory '{}' does not exist",
                    name,
                    store_dir.display()
                ),
                path: Some(store_dir),
                hint: Some("create the store directory or remove the store from config".into()),
            });
            continue;
        }

        if matches!(
            std::fs::read_dir(&store_dir),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
        ) {
            findings.push(DoctorFinding {
                id: "unreadable-source",
                severity: Severity::Error,
                message: format!("store '{}': directory is unreadable", name),
                path: Some(store_dir.clone()),
                hint: Some("check permissions on the store directory; apply will fail".into()),
            });
            continue;
        }

        // Legacy alias-symlink workaround (v0.14 `sources` migration): a
        // repo-internal symlink inside a store dir that resolves back into
        // the repo. It exists only so another store has a same-named file to
        // link, and it survives the stale-link sweep by geometry — its target
        // lives in another store's directory, which the narrow ownership
        // check never matches. That is protection by coincidence, not by
        // design; the `sources` declaration is the designed replacement.
        for walk in walkdir::WalkDir::new(&store_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if walk.depth() == 0 || !walk.file_type().is_symlink() {
                continue;
            }
            let link = walk.path();
            if !link_resolves_into_repo(link, repo_root) {
                continue;
            }
            findings.push(DoctorFinding {
                id: "alias-symlink",
                severity: Severity::Warning,
                message: format!(
                    "store '{name}': repo-internal symlink {} — the pre-sources \
                     alias workaround",
                    link.display()
                ),
                path: Some(link.to_path_buf()),
                hint: Some(format!(
                    "replace it with a `sources` entry (target name → repo-relative \
                     path) in .stitch/state.toml, then remove the symlink"
                )),
            });
        }

        if store_dir
            .read_dir()
            .map_or(true, |mut d| d.next().is_none())
        {
            findings.push(DoctorFinding {
                id: "empty-store",
                severity: Severity::Warning,
                message: format!("store '{}': directory is empty", name),
                path: Some(store_dir.clone()),
                hint: Some("add files or remove the store".into()),
            });
        }

        // Duplicate targets are a config-level problem and must be reported
        // regardless of whether the current platform would apply the store.
        // They are only a conflict if both claimants could be active on the
        // same platform, i.e. their combined `when` constraints are jointly
        // satisfiable.
        if let Some(ref target_str) = store.target {
            let target_path =
                config::expand_home(target_str).expect("HOME was validated by Config::load");
            let new_claim = make_claim(name, None, &store.when, None);
            if let Some(existing) = seen_targets
                .get(&target_path)
                .and_then(|claims| claims.iter().find(|c| claims_compatible(c, &new_claim)))
            {
                findings.push(DoctorFinding {
                    id: "duplicate-target",
                    severity: Severity::Error,
                    message: duplicate_target_message(
                        name,
                        None,
                        &existing.store,
                        existing.tname.as_deref(),
                        &target_path,
                    ),
                    path: Some(target_path.clone()),
                    hint: Some("reconfigure one store to a different target".into()),
                });
            }
            seen_targets.entry(target_path).or_default().push(new_claim);
        }

        for (tname, tentry) in &store.targets {
            let target_path =
                config::expand_home(&tentry.target).expect("HOME was validated by Config::load");
            let new_claim = make_claim(name, Some(tname), &store.when, Some(&tentry.when));
            if let Some(existing) = seen_targets
                .get(&target_path)
                .and_then(|claims| claims.iter().find(|c| claims_compatible(c, &new_claim)))
            {
                findings.push(DoctorFinding {
                    id: "duplicate-target",
                    severity: Severity::Error,
                    message: duplicate_target_message(
                        name,
                        Some(tname),
                        &existing.store,
                        existing.tname.as_deref(),
                        &target_path,
                    ),
                    path: Some(target_path.clone()),
                    hint: Some("reconfigure one store to a different target".into()),
                });
            }
            seen_targets.entry(target_path).or_default().push(new_claim);
        }

        // Source-name resolution errors are config-level problems and must be
        // reported regardless of platform. Check every configured target before
        // the platform-skip so a skipped store's bad inventory still surfaces.
        let mut check_target_resolution =
            |target_path: &Path,
             tname: Option<&str>,
             files: &[String],
             patterns: &[String],
             sources: &BTreeMap<String, String>,
             ignore: &[String]| {
                // Non-regular template sources (symlink/dir named *.tmpl) must be
                // reported separately from source-name collisions.
                match render::unsupported_template_source(&store_dir) {
                    Err(msg) => {
                        // The top-level permission-denied case is already handled
                        // by the `unreadable-source` check above (it `continue`s).
                        // Deeper I/O errors from the walk are reported here.
                        findings.push(DoctorFinding {
                        id: "store-dir-unreadable",
                        severity: Severity::Error,
                        message: match tname {
                            Some(t) => format!("store '{name}' target '{t}': {msg}"),
                            None => format!("store '{name}': {msg}"),
                        },
                        path: Some(target_path.to_path_buf()),
                        hint: Some("check permissions on the store directory and its descendants; apply will fail".into()),
                    });
                        return;
                    }
                    Ok(Some(path)) => {
                        findings.push(DoctorFinding {
                        id: "unsupported-template-source",
                        severity: Severity::Error,
                        message: match tname {
                            Some(t) => format!(
                                "store '{name}' target '{t}': template source {} must be a direct regular file",
                                path.display()
                            ),
                            None => format!(
                                "store '{name}': template source {} must be a direct regular file",
                                path.display()
                            ),
                        },
                        path: Some(target_path.to_path_buf()),
                        hint: Some("ensure the `.tmpl` entry is a regular file, not a symlink or directory".into()),
                    });
                        return;
                    }
                    Ok(None) => {}
                }

                let resolved =
                    resolve_target_names(repo_root, &store_dir, files, patterns, sources, ignore);
                if let LinkTargets::Files(ref links) = resolved
                    && let Err(msg) = super::resolve::check_link_name_collisions(links)
                {
                    findings.push(DoctorFinding {
                        id: "source-name-collision",
                        severity: Severity::Error,
                        message: match tname {
                            Some(t) => format!("store '{name}' target '{t}': {msg}"),
                            None => format!("store '{name}': {msg}"),
                        },
                        path: Some(target_path.to_path_buf()),
                        hint: Some("remove or rename one of the colliding entries".into()),
                    });
                }
            };

        if let Some(ref target_str) = store.target {
            let target_path =
                config::expand_home(target_str).expect("HOME was validated by Config::load");
            check_target_resolution(
                &target_path,
                None,
                &store.files,
                &store.patterns,
                &store.sources,
                &store.ignore,
            );
        }

        for (tname, tentry) in &store.targets {
            let target_path =
                config::expand_home(&tentry.target).expect("HOME was validated by Config::load");
            check_target_resolution(
                &target_path,
                Some(tname),
                &tentry.files,
                &tentry.patterns,
                &tentry.sources,
                &tentry.ignore,
            );
        }

        if !platform.matches_when(&store.when) {
            findings.push(DoctorFinding {
                id: "platform-skipped",
                severity: Severity::Info,
                message: format!("store '{}': skipped (platform filter)", name),
                path: None,
                hint: None,
            });
            continue;
        }

        for entry in all_statuses
            .iter()
            .filter(|e| e.store_name == *name && !e.skipped_platform)
        {
            // Config-level resolution errors are reported above, before the
            // platform-skip, so do not duplicate them here.
            if matches!(entry.status, LinkStatus::ConfigError(_)) {
                continue;
            }

            if let LinkStatus::StoreError(ref store_dir) = entry.status {
                findings.push(DoctorFinding {
                    id: "store-dir-error",
                    severity: Severity::Error,
                    message: format!(
                        "store '{}': store directory '{}' is missing, symlinked, or not a directory",
                        name,
                        store_dir.display()
                    ),
                    path: Some(store_dir.clone()),
                    hint: Some(format!(
                        "create '{}' as a real directory or remove the store from config",
                        store_dir.display()
                    )),
                });
            }

            if let LinkStatus::Broken(ref resolved) = entry.status {
                findings.push(DoctorFinding {
                    id: "broken-link",
                    severity: Severity::Error,
                    message: format!(
                        "store '{}': broken symlink at {} -> {}",
                        name,
                        entry.target.display(),
                        resolved.display()
                    ),
                    path: Some(entry.target.clone()),
                    hint: Some(format!(
                        "remove or repoint the symlink at {}",
                        entry.target.display()
                    )),
                });
            }

            if let LinkStatus::Foreign(ref resolved) = entry.status {
                findings.push(DoctorFinding {
                    id: "foreign-link",
                    severity: Severity::Error,
                    message: format!(
                        "store '{}': foreign symlink at {} -> {}",
                        name,
                        entry.target.display(),
                        resolved.display()
                    ),
                    path: Some(entry.target.clone()),
                    hint: Some(format!(
                        "foreign symlink at {} -> {}; owned by another tool, `apply` will conflict",
                        entry.target.display(),
                        resolved.display()
                    )),
                });
            }

            if let LinkStatus::Conflict(ref path) = entry.status {
                findings.push(DoctorFinding {
                    id: "conflict-real-file",
                    severity: Severity::Error,
                    message: format!(
                        "store '{}': real file/dir blocks link at {}",
                        name,
                        path.display()
                    ),
                    path: Some(entry.target.clone()),
                    hint: Some(format!(
                        "remove or move `{}`, or run `stitch apply --force`",
                        path.display()
                    )),
                });
            }

            if let LinkStatus::Missing = entry.status {
                findings.push(DoctorFinding {
                    id: "missing-link",
                    severity: Severity::Warning,
                    message: format!(
                        "store '{}': missing link at {}",
                        name,
                        entry.target.display()
                    ),
                    path: Some(entry.target.clone()),
                    hint: Some("run `stitch apply` to create the link".into()),
                });
            }

            // Staging drift: tool-owned render differs from a fresh render.
            // Hand-edits (or a vars/env change since last apply) surface here
            // so the next apply's overwrite is never silent.
            if entry.is_template {
                // `source_name` is the template identity (store-relative for
                // `files` entries, repo-relative for `sources`); the staging
                // identity is carried by `link_source`.
                let link_rel = entry
                    .link_source
                    .strip_prefix(render::store_render_dir(repo_root, name))
                    .ok()
                    .and_then(|rel| rel.to_str().map(str::to_owned));
                if let Some(link_rel) = link_rel {
                    match render::staged_differs(
                        repo_root,
                        name,
                        &entry.source_name,
                        &entry.source,
                        &link_rel,
                        platform,
                        &config.vars,
                    ) {
                        Ok(true) => {
                            findings.push(DoctorFinding {
                                id: "staging-drift",
                                severity: Severity::Warning,
                                message: format!(
                                    "store '{}': staged render for {} is stale (run `stitch apply`)",
                                    name,
                                    entry.target.display()
                                ),
                                path: Some(entry.target.clone()),
                                hint: Some("run `stitch apply`".into()),
                            });
                        }
                        Ok(false) => {}
                        Err(e) => {
                            // A template that fails to render is an error — same
                            // class as a broken link.
                            findings.push(DoctorFinding {
                                id: "render-error",
                                severity: Severity::Error,
                                message: format!("store '{name}': {e}"),
                                path: Some(entry.source.clone()),
                                hint: Some("set missing env vars or fix the template".into()),
                            });
                        }
                    }
                }
            }
        }
    }

    // Flag directories in the repo root that are not configured stores.
    if let Ok(entries) = std::fs::read_dir(repo_root) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            if name == ".stitch" || name == ".git" {
                continue;
            }
            if config.stores.contains_key(name.to_string_lossy().as_ref()) {
                continue;
            }
            findings.push(DoctorFinding {
                id: "untracked-store-dir",
                severity: Severity::Info,
                message: format!(
                    "untracked directory '{}' is not a configured store",
                    entry.path().display()
                ),
                path: Some(entry.path()),
                hint: Some("not a configured store; remove it or add it to config".into()),
            });
        }
    }

    DoctorResult { findings }
}
