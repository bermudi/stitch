//! Apply execution: reconcile the filesystem to match the config by creating,
//! replacing, or removing symlinks. Owns the `ApplyAction`/`ApplyResult`/
//! `ApplyOpts` types, the per-store and per-target apply pipeline, the
//! `add`-facing preflight helpers, and `compute_plan` (a thin dry-run wrapper
//! over `apply_all`).
//!
//! Imports plan conversion from `super::plan_compute` and shared resolution
//! from `super::resolve`. Does not import `status` or `doctor`.

use super::plan_compute::to_plan;
use super::resolve::{
    LinkTargets, collect_reconciliation_keeps, collect_store_link_targets, resolve_target_names,
    resolve_targets,
};
use crate::ancestor::{TargetAncestorRedirect, TargetAncestorSnapshot};
use crate::config::{self, Config, ConfigSnapshot, Store};
use crate::error::StitchError;
use crate::hooks::{self, HookEnv};
use crate::linker::{self, LinkError, LinkStatus};
use crate::plan::Plan;
use crate::platform::Platform;
use crate::render;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ApplyAction {
    Created(PathBuf),
    /// The old target was replaced by a link to `source`. `old_resolves_to`
    /// is `Some` for a broken repo-owned symlink, `None` for a real file/dir.
    Replaced {
        target: PathBuf,
        old_resolves_to: Option<PathBuf>,
    },
    /// The conflicting real file/dir was renamed to `{target}.bak` and the
    /// link created (`apply --force`). `target` is now symlinked; `backup`
    /// holds the original content.
    BackedUp {
        target: PathBuf,
        backup: PathBuf,
    },
    /// A path is occupied by something other than the desired source.
    /// `resolves_to` is `None` for a real file/dir, `Some` for a symlink that
    /// points elsewhere (foreign or another store).
    Conflict {
        target: PathBuf,
        resolves_to: Option<PathBuf>,
    },
    SkippedPlatform,
    AlreadyLinked(PathBuf),
    /// Templated entry: staged content was (or would be) refreshed; the link
    /// already pointed at the staging path. Surfaced so `diff`/`apply` can
    /// answer "would apply change anything?" for content, not only link state.
    ContentChanged(PathBuf),
    /// A stale stitch-owned file-mode link was removed (or would be removed in
    /// a dry run) because its source is no longer in the resolved entry set.
    Removed(PathBuf),
    /// A stale rendered file was removed, or would be removed in a dry run.
    StagedRemoved(PathBuf),
    Error(StitchError),
}

/// Convenience: wrap a plain-string apply error as an internal failure.
fn internal_error(message: impl Into<String>) -> ApplyAction {
    ApplyAction::Error(StitchError::internal(message))
}

/// Wrap a plain-string apply error as a config failure (exit 3), not an
/// internal one (exit 1). Used for user-facing config problems that surface
/// during apply — e.g. an orphaned store (behavior in `stitch.toml` but no
/// link inventory in `state.toml`) — so the exit code and hint reflect a
/// fixable config issue rather than an unexpected internal failure.
fn config_error(message: impl Into<String>) -> ApplyAction {
    ApplyAction::Error(StitchError::config(config::ConfigError::InvalidPath(
        message.into(),
    )))
}

/// Wrap a config revalidation failure as a config-class error (exit 3), not
/// an internal error (exit 1), preserving the original failed path and
/// adding checkpoint/store context to the message. A config reread failure
/// is a config problem — the caller sees exit 3, not exit 1.
fn config_revalidation_error(
    checkpoint: &str,
    store_name: &str,
    e: config::ConfigError,
) -> ApplyAction {
    let path = match &e {
        config::ConfigError::Read(_, p) => p.clone(),
        _ => PathBuf::new(),
    };
    ApplyAction::Error(StitchError::from(config::ConfigError::Read(
        std::io::Error::other(format!(
            "failed to revalidate config hash {checkpoint} for store '{store_name}': {e}"
        )),
        path,
    )))
}

/// Convert a target-ancestor redirect into the appropriate apply action. A
/// symlinked redirect is a conflict; an identity change on a real directory is
/// an internal error.
fn redirect_to_apply_action(redirect: TargetAncestorRedirect) -> ApplyAction {
    match redirect {
        TargetAncestorRedirect::Symlinked { path, resolves_to } => ApplyAction::Conflict {
            target: path,
            resolves_to,
        },
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: Some(resolves_to),
        } => ApplyAction::Conflict {
            target: path,
            resolves_to: Some(resolves_to),
        },
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: None,
        } => internal_error(format!("target ancestor {} was redirected", path.display())),
        TargetAncestorRedirect::Removed { path } => {
            internal_error(format!("target ancestor {} was removed", path.display()))
        }
    }
}

#[derive(Debug)]
pub struct ApplyResult {
    pub store_name: String,
    pub actions: Vec<ApplyAction>,
}

/// Flags controlling how `apply` reconciles each link.
#[derive(Debug, Clone, Copy)]
pub struct ApplyOpts {
    pub dry_run: bool,
    /// Rename real-file/dir conflicts to `{target}.bak` and link instead of
    /// stopping. Foreign symlinks remain hard conflicts regardless.
    pub force: bool,
}

/// Apply all stores in the config. Returns the executed (or previewed) plan
/// and any warnings generated by side-effecting steps such as post-hooks.
///
/// `pinned_hash` binds the parsed `config` to a specific on-disk state:
/// - `Some(hash)`: direct apply. The hash was computed from the exact bytes
///   that were parsed into `config` (by `ConfigSnapshot`). Every revalidation
///   compares fresh disk bytes to this hash — not a re-read — so a config
///   swapped between parse and revalidation is caught.
/// - `None`: dry-run classification (`compute_plan`). No mutation, no hooks,
///   no hash checks. The hash is irrelevant.
///
/// `only` filters stores by name (empty = all).
pub fn apply_all(
    repo_root: &Path,
    config: &Config,
    pinned_hash: Option<&str>,
    only: &[String],
    platform: &Platform,
    opts: ApplyOpts,
) -> (Plan, Vec<String>) {
    let mut warnings = Vec::new();
    let root_identity = std::fs::metadata(repo_root)
        .ok()
        .filter(|meta| meta.is_dir())
        .map(|meta| (meta.dev(), meta.ino()));
    // Capture $HOME identity once for the entire apply loop. Each store's
    // pre-hook AND post-hook revalidates against this same identity, so a
    // post-hook that replaces the directory behind a symlinked $HOME is
    // detected before the next store's mutations.
    let apply_home_identity = crate::safety::HomeIdentity::capture().ok();
    let sorted: BTreeMap<_, _> = config.stores.iter().collect();
    let mut results = Vec::new();
    let mut locked_stores: BTreeMap<String, Store> = BTreeMap::new();

    for (name, store) in sorted {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        let store_dir = repo_root.join(name);
        if !linker::is_real_directory(&store_dir) {
            results.push(ApplyResult {
                store_name: name.clone(),
                actions: vec![internal_error(format!(
                    "store directory '{}' is missing, symlinked, or not a directory",
                    name
                ))],
            });
            continue;
        }

        let store_identity = std::fs::symlink_metadata(&store_dir)
            .map(|meta| (meta.dev(), meta.ino()))
            .ok();

        // A store excluded by its `when` clause is not applied on this
        // platform, so its hooks must not run either. `when` is the "leave
        // this machine alone" switch; a skipped store firing a hook (e.g.
        // `git config --global`, `systemctl ...`) would execute commands the
        // user deliberately gated off, with no sign in the summary (which
        // reports the store as skipped). `compute_apply_actions` still emits
        // `SkippedPlatform` for reporting; this only suppresses the hooks.
        // Per-target `when` does NOT suppress hooks: if the store is active on
        // this platform at all, its hooks run.
        let skipped_by_platform = !platform.matches_when(&store.when);

        // Per-store pre-hook: aborts the store on failure (SPEC). Runs
        // WITHOUT the state lock — a hook may itself invoke a mutating stitch
        // command, and holding the lock across it would deadlock.
        if !opts.dry_run
            && !skipped_by_platform
            && let Some(pre) = &store.hooks.pre
        {
            // Revalidate $HOME identity (including the resolved directory
            // behind a symlinked $HOME) across the per-store pre-hook, using
            // the command-level identity captured before the store loop.
            if let Some(ref home_id) = apply_home_identity
                && let Err(e) = home_id.revalidate()
            {
                results.push(ApplyResult {
                    store_name: name.clone(),
                    actions: vec![internal_error(e.to_string())],
                });
                continue;
            }

            // Revalidate disk config against the pinned hash BEFORE the hook
            // runs. The parsed config (which selects this hook) was bound to
            // `pinned_hash` at capture time by `ConfigSnapshot`. If the on-disk
            // config no longer matches — e.g. it was swapped to a malicious
            // version for the snapshot capture and restored to benign before
            // this read — the hook selected from the parsed config must not run.
            //
            // The revalidation uses the same no-follow, fd-validated reader as
            // `ConfigSnapshot::load` (`config::revalidate_config_hash`), not
            // the path-based `compute_config_hash`, so a path replacement
            // targeting the file between open and read cannot substitute
            // bytes. A read failure is surfaced as an explicit error action
            // (with path/context), not silently collapsed into "hash
            // mismatch". (Parent-directory replacement is out of scope; see
            // the doc on `config::open_and_read_validated`.)
            if let Some(h) = pinned_hash {
                let pre_hook_hash = match config::revalidate_config_hash(repo_root) {
                    Ok(hash) => hash,
                    Err(e) => {
                        results.push(ApplyResult {
                            store_name: name.clone(),
                            actions: vec![config_revalidation_error("before pre-hook", name, e)],
                        });
                        continue;
                    }
                };
                if pre_hook_hash != h {
                    results.push(ApplyResult {
                        store_name: name.clone(),
                        actions: vec![internal_error(format!(
                            "config hash mismatch before pre-hook for store '{name}': \
                             pinned {h}, found {pre_hook_hash}"
                        ))],
                    });
                    continue;
                }
            }

            // Pin the store's target-ancestor identities across its pre-hook.
            let home = match config::expand_home("~") {
                Ok(h) => h,
                Err(e) => {
                    results.push(ApplyResult {
                        store_name: name.clone(),
                        actions: vec![ApplyAction::Error(StitchError::from(e))],
                    });
                    continue;
                }
            };
            let (store_targets, store_removed) =
                match collect_store_link_targets(repo_root, name, store, platform) {
                    Ok(t) => t,
                    Err(msg) => {
                        results.push(ApplyResult {
                            store_name: name.clone(),
                            actions: vec![internal_error(msg)],
                        });
                        continue;
                    }
                };
            let store_ancestors = match TargetAncestorSnapshot::capture(
                repo_root,
                store_targets,
                &store_removed,
                &home,
            ) {
                Ok(s) => s,
                Err(e) => {
                    results.push(ApplyResult {
                        store_name: name.clone(),
                        actions: vec![redirect_to_apply_action(e)],
                    });
                    continue;
                }
            };

            let env = HookEnv {
                root: repo_root,
                store: Some(name),
                target: store.target.as_deref(),
                action: "apply",
            };
            if let Err(msg) = hooks::run_store_hook(pre, &env, platform) {
                results.push(ApplyResult {
                    store_name: name.clone(),
                    actions: vec![ApplyAction::Error(StitchError::hook("pre", msg))],
                });
                continue;
            }
            if let Err(e) = store_ancestors.revalidate() {
                results.push(ApplyResult {
                    store_name: name.clone(),
                    actions: vec![redirect_to_apply_action(e)],
                });
                continue;
            }
            // Revalidate $HOME identity: detect a hook that replaced the
            // directory behind a symlinked $HOME.
            if let Some(ref home_id) = apply_home_identity
                && let Err(e) = home_id.revalidate()
            {
                results.push(ApplyResult {
                    store_name: name.clone(),
                    actions: vec![internal_error(e.to_string())],
                });
                continue;
            }
            // Hooks may mutate the filesystem. Never let a replaced store
            // root drive source resolution or stale-link reconciliation.
            let current_store_identity = std::fs::symlink_metadata(&store_dir)
                .map(|meta| (meta.dev(), meta.ino()))
                .ok();
            if !linker::is_real_directory(&store_dir) || current_store_identity != store_identity {
                results.push(ApplyResult {
                    store_name: name.clone(),
                    actions: vec![internal_error(format!(
                        "store directory '{}' changed during its pre-hook",
                        name
                    ))],
                });
                continue;
            }
            // Revalidate disk config against the pinned hash AFTER the hook.
            // A hook that changed the config on disk must be caught before
            // mutation uses the parsed config. Same fd-validated reader as
            // the pre-hook check; a read failure is an explicit error action.
            if let Some(h) = pinned_hash {
                let post_hook_hash = match config::revalidate_config_hash(repo_root) {
                    Ok(hash) => hash,
                    Err(e) => {
                        results.push(ApplyResult {
                            store_name: name.clone(),
                            actions: vec![config_revalidation_error("after pre-hook", name, e)],
                        });
                        continue;
                    }
                };
                if post_hook_hash != h {
                    results.push(ApplyResult {
                        store_name: name.clone(),
                        actions: vec![internal_error(format!(
                            "config hash mismatch after pre-hook for store '{name}': \
                             pinned {h}, found {post_hook_hash}"
                        ))],
                    });
                    continue;
                }
            }
        }

        // Lock around the mutation phase. The config-hash pin (which covers
        // state.toml) is rechecked under the lock, so a concurrent add/remove/
        // migrate can never interleave its state change with the links this
        // run creates: the run either applies against the exact state it
        // planned against, or fails honestly instead of orphaning links.
        let _state_lock = if opts.dry_run {
            None
        } else {
            match config::StateLock::exclusive_if_present(repo_root) {
                Ok(lock) => lock,
                Err(e) => {
                    results.push(ApplyResult {
                        store_name: name.clone(),
                        actions: vec![internal_error(e.to_string())],
                    });
                    continue;
                }
            }
        };
        let current_identity = std::fs::metadata(repo_root)
            .ok()
            .filter(|meta| meta.is_dir())
            .map(|meta| (meta.dev(), meta.ino()));
        let (current_hash, hash_ok) = match pinned_hash {
            None => (None, true), // dry-run: no hash binding
            Some(h) => match config::revalidate_config_hash(repo_root) {
                Ok(hash) => {
                    let ok = hash == h;
                    (Some(hash), ok)
                }
                Err(e) => {
                    results.push(ApplyResult {
                        store_name: name.clone(),
                        actions: vec![config_revalidation_error(
                            "under lock before applying",
                            name,
                            e,
                        )],
                    });
                    continue;
                }
            },
        };
        if root_identity.is_none() {
            results.push(ApplyResult {
                store_name: name.clone(),
                actions: vec![internal_error(format!(
                    "repository root disappeared before applying store '{name}'"
                ))],
            });
            continue;
        }
        if current_identity != root_identity {
            results.push(ApplyResult {
                store_name: name.clone(),
                actions: vec![internal_error(format!(
                    "repository root identity changed before applying store '{name}'"
                ))],
            });
            continue;
        }
        if !hash_ok {
            results.push(ApplyResult {
                store_name: name.clone(),
                actions: vec![internal_error(format!(
                    "config hash mismatch under lock before applying store '{name}': \
                     pinned {}, found {}",
                    pinned_hash.unwrap(),
                    current_hash.as_deref().unwrap_or("(none)")
                ))],
            });
            continue;
        }
        let mut result = if opts.dry_run {
            apply_store(
                repo_root,
                name,
                store,
                platform,
                &config.vars,
                opts,
                &mut warnings,
            )
        } else {
            // Reload a fresh snapshot under the lock and require the same hash
            // before using its parsed config for mutation. This binds the
            // mutation phase to the same bytes that were captured at command
            // startup.
            match ConfigSnapshot::load(repo_root) {
                Ok(locked_snap) if locked_snap.hash() == pinned_hash.unwrap() => {
                    match locked_snap.loaded.config.stores.get(name) {
                        Some(store) => {
                            let locked_store = store.clone();
                            let result = apply_store(
                                repo_root,
                                name,
                                &locked_store,
                                platform,
                                &locked_snap.loaded.config.vars,
                                opts,
                                &mut warnings,
                            );
                            locked_stores.insert(name.clone(), locked_store);
                            result
                        }
                        None => ApplyResult {
                            store_name: name.clone(),
                            actions: vec![internal_error(format!(
                                "store '{name}' disappeared from config under lock"
                            ))],
                        },
                    }
                }
                Ok(locked_snap) => ApplyResult {
                    store_name: name.clone(),
                    actions: vec![internal_error(format!(
                        "config hash mismatch under lock for store '{name}': \
                         pinned {}, found {}",
                        pinned_hash.unwrap(),
                        locked_snap.hash()
                    ))],
                },
                Err(e) => ApplyResult {
                    store_name: name.clone(),
                    actions: vec![ApplyAction::Error(StitchError::from(e))],
                },
            }
        };
        drop(_state_lock);

        // Per-store post-hook: warns on failure — the store is already
        // applied, so post-hook failure does not abort (SPEC). Runs without
        // the lock so the hook may invoke a mutating stitch command. Use the
        // locked snapshot for the hook target so a concurrent change cannot
        // redirect the hook.
        if !opts.dry_run
            && !skipped_by_platform
            && let Some(locked_store) = locked_stores.get(name)
            && let Some(post) = &locked_store.hooks.post
        {
            let env = HookEnv {
                root: repo_root,
                store: Some(name),
                target: locked_store.target.as_deref(),
                action: "apply",
            };
            if let Err(msg) = hooks::run_store_hook(post, &env, platform) {
                warnings.push(format!("store '{name}' post-hook: {msg}"));
            }
            // Revalidate $HOME identity after the post-hook, using the
            // command-level identity. A post-hook that replaces the directory
            // behind a symlinked $HOME must be caught before the next store.
            if let Some(ref home_id) = apply_home_identity
                && let Err(e) = home_id.revalidate()
            {
                result.actions.push(internal_error(format!(
                    "$HOME changed during post-hook for store '{name}': {e}"
                )));
            }
        }

        let after_identity = std::fs::metadata(repo_root)
            .ok()
            .filter(|meta| meta.is_dir())
            .map(|meta| (meta.dev(), meta.ino()));
        let (after_hash, after_hash_ok) = match pinned_hash {
            None => (None, true),
            Some(h) => match config::revalidate_config_hash(repo_root) {
                Ok(hash) => {
                    let ok = hash == h;
                    (Some(hash), ok)
                }
                Err(e) => {
                    result
                        .actions
                        .push(config_revalidation_error("after applying", name, e));
                    results.push(result);
                    continue;
                }
            },
        };
        if after_identity != root_identity {
            result.actions.push(internal_error(format!(
                "repository root identity changed while applying store '{name}'"
            )));
        }
        if !after_hash_ok {
            result.actions.push(internal_error(format!(
                "config hash mismatch after applying store '{name}': \
                 pinned {}, found {}",
                pinned_hash.unwrap(),
                after_hash.as_deref().unwrap_or("(none)")
            )));
        }
        results.push(result);
    }
    (
        to_plan(
            repo_root,
            if opts.dry_run {
                &config.stores
            } else {
                &locked_stores
            },
            &results,
            opts,
        ),
        warnings,
    )
}

/// Pure classification: what would `apply` do? No hooks, no staging writes, no
/// link mutations. This is the `Plan` that `diff` and `apply --json` render.
pub fn compute_plan(
    repo_root: &Path,
    config: &Config,
    platform: &Platform,
    opts: ApplyOpts,
) -> Plan {
    // Force dry-run so no side effects run, but preserve the force flag for
    // conflict classification (backed up vs conflict).
    let classify_opts = ApplyOpts {
        dry_run: true,
        force: opts.force,
    };
    apply_all(repo_root, config, None, &[], platform, classify_opts).0
}

/// Whether an apply on this platform could render an on-disk template source.
/// Used to make the `.gitignore` trust boundary conditional for upgraded repos:
/// plain stores need no migration, but rendering refuses to create output until
/// staging is ignored.
pub fn has_active_template_sources(repo_root: &Path, config: &Config, platform: &Platform) -> bool {
    config.stores.iter().any(|(name, store)| {
        if !platform.matches_when(&store.when) {
            return false;
        }
        let store_dir = repo_root.join(name);
        if store.is_multi_target() {
            store.targets.values().any(|target| {
                platform.matches_when(&target.when)
                    && target_has_template_source(
                        &store_dir,
                        &target.files,
                        &target.patterns,
                        &target.ignore,
                    )
            })
        } else {
            target_has_template_source(&store_dir, &store.files, &store.patterns, &store.ignore)
        }
    })
}

fn target_has_template_source(
    store_dir: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
) -> bool {
    let LinkTargets::Files(names) = resolve_target_names(store_dir, files, patterns, ignore) else {
        return false;
    };
    names.into_iter().any(|source_name| {
        let entry = render::resolve_entry(&source_name);
        entry.is_template && is_regular_template_source(&store_dir.join(entry.source_rel))
    })
}

fn is_regular_template_source(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_file() && !meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Apply a single store.
pub fn apply_store(
    repo_root: &Path,
    name: &str,
    store: &Store,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
    opts: ApplyOpts,
    _warnings: &mut Vec<String>,
) -> ApplyResult {
    if !platform.matches_when(&store.when) {
        return ApplyResult {
            store_name: name.to_string(),
            actions: vec![ApplyAction::SkippedPlatform],
        };
    }

    let store_dir = repo_root.join(name);
    if !linker::is_real_directory(&store_dir) {
        return ApplyResult {
            store_name: name.to_string(),
            actions: vec![internal_error(format!(
                "store directory '{}' is missing, symlinked, or not a directory",
                name
            ))],
        };
    }

    // Hooks are not run here: `apply_all` runs pre/post hooks outside the
    // state lock (a hook may invoke a mutating stitch command) and calls this
    // function for the mutation phase only. Direct callers (`add`, `import`)
    // create stores with no hooks.
    let mut actions = Vec::new();

    // Reconciliation is derived from desired sources, not successful entry
    // application. A render/resolution failure and a target skipped by `when`
    // must preserve a live staged render and its owned link; otherwise a
    // harmless config mistake turns a working link dangling.
    let mut keep_links: BTreeSet<String> = BTreeSet::new();
    // File-mode cleanup is target-specific. A store can have multiple targets,
    // and two target entries may even share a target path, so union their
    // configured link names before scanning that path.
    let mut target_keep_links: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut link_reconciliation_failed = false;

    let empty_link_rels = BTreeSet::new();
    if store.is_multi_target() {
        // Build every desired keep-set before applying any target. An active
        // whole-dir target must see a skipped file-mode sibling that shares its
        // path; otherwise it could erase that sibling before its turn.
        for target_entry in store.targets.values() {
            let target_path = config::expand_home(&target_entry.target)
                .expect("HOME was validated by Config::load");
            collect_reconciliation_keeps(
                &store_dir,
                &target_path,
                &target_entry.files,
                &target_entry.patterns,
                &target_entry.ignore,
                &mut keep_links,
                &mut target_keep_links,
            );
            // Keep desired staged renders, but never scan a target whose path
            // reaches back into the repo through an ancestor symlink.
            if target_is_confined(&target_path, repo_root).is_err() {
                target_keep_links.remove(&target_path);
                link_reconciliation_failed = true;
            }
        }
        for target_entry in store.targets.values() {
            if !platform.matches_when(&target_entry.when) {
                continue;
            }
            let target_path = config::expand_home(&target_entry.target)
                .expect("HOME was validated by Config::load");
            match target_is_confined(&target_path, repo_root) {
                Ok(()) => {
                    let target_link_rels = target_keep_links
                        .get(&target_path)
                        .unwrap_or(&empty_link_rels);
                    actions.extend(apply_target(
                        name,
                        &store_dir,
                        &target_path,
                        repo_root,
                        &target_entry.files,
                        &target_entry.patterns,
                        &target_entry.ignore,
                        target_link_rels,
                        platform,
                        vars,
                        opts,
                    ));
                }
                Err(action) => {
                    target_keep_links.remove(&target_path);
                    link_reconciliation_failed = true;
                    actions.push(action);
                }
            }
        }
    } else if let Some(ref target_str) = store.target {
        let target_path =
            config::expand_home(target_str).expect("HOME was validated by Config::load");
        collect_reconciliation_keeps(
            &store_dir,
            &target_path,
            &store.files,
            &store.patterns,
            &store.ignore,
            &mut keep_links,
            &mut target_keep_links,
        );
        if target_is_confined(&target_path, repo_root).is_err() {
            target_keep_links.remove(&target_path);
            link_reconciliation_failed = true;
        }
        match target_is_confined(&target_path, repo_root) {
            Ok(()) => {
                let target_link_rels = target_keep_links
                    .get(&target_path)
                    .unwrap_or(&empty_link_rels);
                actions.extend(apply_target(
                    name,
                    &store_dir,
                    &target_path,
                    repo_root,
                    &store.files,
                    &store.patterns,
                    &store.ignore,
                    target_link_rels,
                    platform,
                    vars,
                    opts,
                ));
            }
            Err(action) => {
                target_keep_links.remove(&target_path);
                link_reconciliation_failed = true;
                actions.push(action);
            }
        }
    } else {
        // No target and no target entries. The common cause is an orphaned
        // store: behavior declared in `stitch.toml` but no link inventory in
        // `state.toml` (e.g. left behind by `remove`, which never rewrites the
        // authored file). This is a user-facing config problem, not an
        // internal failure, so it surfaces as a config error (exit 3) with a
        // fix hint rather than exit 1.
        actions.push(config_error(format!(
            "store '{name}': no target configured — behavior is declared in stitch.toml but \
             state.toml has no link inventory; re-add the store with `stitch add` or remove the \
             entry from stitch.toml"
        )));
    }

    // Reconcile file-mode links before staging: a deleted source must not leave
    // a target symlink pointing at a render that is about to disappear. The
    // helper is dry-run aware so `diff` previews removals without mutating.
    for (target_path, link_rels) in target_keep_links {
        match render::reconcile_store_links(
            &target_path,
            repo_root,
            &store_dir,
            name,
            &link_rels,
            opts.dry_run,
        ) {
            Ok(removed) => actions.extend(removed.into_iter().map(ApplyAction::Removed)),
            Err(e) => {
                link_reconciliation_failed = true;
                actions.push(internal_error(e));
            }
        }
    }

    // Reap staging only after target cleanup fully succeeds. An I/O failure
    // while unlinking a stale target must leave its render readable rather
    // than converting the failure into a dangling link. Dry-run performs the
    // same scan and reports removals without mutating, keeping `diff` exact.
    if !link_reconciliation_failed {
        let stale = if opts.dry_run {
            render::stale_store_staging(repo_root, name, &keep_links)
                .map(|entries| entries.into_iter().map(|(_, path)| path).collect())
        } else {
            render::reconcile_store_staging(repo_root, name, &keep_links)
        };
        match stale {
            Ok(paths) => actions.extend(paths.into_iter().map(ApplyAction::StagedRemoved)),
            Err(e) => actions.push(internal_error(e)),
        }
    }

    ApplyResult {
        store_name: name.to_string(),
        actions,
    }
}

#[allow(clippy::too_many_arguments)] // thin dispatcher over resolve + per-entry apply
fn apply_target(
    store_name: &str,
    store_dir: &Path,
    target_path: &Path,
    repo_root: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
    target_link_rels: &BTreeSet<String>,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
    opts: ApplyOpts,
) -> Vec<ApplyAction> {
    match resolve_targets(store_dir, files, patterns, ignore) {
        Err(msg) => vec![ApplyAction::Error(StitchError::path_validation(msg))],
        Ok(LinkTargets::WholeDir) => apply_whole_dir(
            store_name,
            store_dir,
            target_path,
            repo_root,
            target_link_rels,
            opts,
        ),
        Ok(LinkTargets::Files(names)) => {
            let (replaces_whole_dir, mut actions) = match prepare_file_mode_target(
                store_name,
                store_dir,
                target_path,
                repo_root,
                &names,
                platform,
                vars,
                opts,
            ) {
                Ok(prepared) => prepared,
                Err(action) => return vec![action],
            };
            for source_name in &names {
                if replaces_whole_dir && opts.dry_run {
                    actions.push(preview_file_entry_after_root_removal(
                        target_path,
                        source_name,
                    ));
                } else {
                    actions.push(apply_file_entry(
                        store_name,
                        store_dir,
                        target_path,
                        repo_root,
                        source_name,
                        platform,
                        vars,
                        opts,
                    ));
                }
            }
            actions
        }
    }
}

/// Remove a verified whole-directory link before creating file-mode children.
/// Without this ordering, POSIX follows the root symlink and writes new child
/// links into the store itself.
#[allow(clippy::too_many_arguments)]
fn prepare_file_mode_target(
    store_name: &str,
    store_dir: &Path,
    target_path: &Path,
    repo_root: &Path,
    source_names: &[String],
    platform: &Platform,
    vars: &BTreeMap<String, String>,
    opts: ApplyOpts,
) -> Result<(bool, Vec<ApplyAction>), ApplyAction> {
    let metadata = match std::fs::symlink_metadata(target_path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((false, Vec::new())),
        Err(e) => {
            return Err(internal_error(format!(
                "could not inspect {}: {e}",
                target_path.display()
            )));
        }
    };
    if !metadata.file_type().is_symlink() {
        // A real file (or anything else non-directory) at the file-mode root
        // is a hard conflict — even under `--force`: the whole store's links
        // live inside this directory, and silently renaming the user's file
        // to `.bak` to make room is beyond per-entry force semantics. The
        // conflict (not an internal error) also keeps `diff`/`status`/`apply`
        // in agreement.
        if !metadata.is_dir() {
            return Err(ApplyAction::Conflict {
                target: target_path.to_path_buf(),
                resolves_to: None,
            });
        }
        return Ok((false, Vec::new()));
    }

    // A symlinked target root that does not resolve into this repo is either
    // the special case of $HOME itself being a symlink (issue #3) or a
    // foreign symlink that must be a conflict. Only bypass when the target
    // is lexically $HOME itself — an alias like ~/.alias -> ~ resolves to
    // HOME canonically but must still conflict.
    if !linker::points_into_repo(target_path, repo_root) {
        let is_home_itself = config::expand_home("~")
            .ok()
            .is_some_and(|home| home == target_path);
        if is_home_itself {
            return Ok((false, Vec::new()));
        }
    }

    // Only this store's exact whole-dir link may be promoted. A foreign link,
    // or a link to another store in the same repo, remains a conflict.
    match linker::check_link(target_path, store_dir, repo_root) {
        LinkStatus::Linked => {}
        LinkStatus::StoreError(store_dir) => {
            return Err(ApplyAction::Error(StitchError::internal(format!(
                "store directory '{}' is missing, symlinked, or not a directory",
                store_dir.display()
            ))));
        }
        LinkStatus::Broken(resolved) | LinkStatus::Foreign(resolved) => {
            return Err(ApplyAction::Conflict {
                target: target_path.to_path_buf(),
                resolves_to: Some(resolved),
            });
        }
        LinkStatus::ConfigError(msg) => {
            return Err(ApplyAction::Error(StitchError::path_validation(msg)));
        }
        _ => {
            return Err(ApplyAction::Conflict {
                target: target_path.to_path_buf(),
                resolves_to: None,
            });
        }
    }
    preflight_file_mode_promotion(
        store_name,
        store_dir,
        repo_root,
        source_names,
        platform,
        vars,
        opts,
    )?;
    if opts.dry_run {
        return Ok((true, vec![ApplyAction::Removed(target_path.to_path_buf())]));
    }

    match linker::remove_link_to(target_path, store_dir, repo_root) {
        Ok(true) => Ok((true, vec![ApplyAction::Removed(target_path.to_path_buf())])),
        // The link was repointed between check and removal. Do not risk
        // writing through it; report the now-unmanaged root as a conflict.
        Ok(false) => Err(ApplyAction::Conflict {
            target: target_path.to_path_buf(),
            resolves_to: std::fs::read_link(target_path).ok(),
        }),
        Err(e) => Err(internal_error(format!(
            "could not remove whole-directory link {}: {e}",
            target_path.display()
        ))),
    }
}

/// Verify a whole-directory → file-mode promotion before unlinking its live
/// root. Templates are rendered/staged first; a bad template or missing source
/// leaves the old directory link intact.
#[allow(clippy::too_many_arguments)]
fn preflight_file_mode_promotion(
    store_name: &str,
    store_dir: &Path,
    repo_root: &Path,
    source_names: &[String],
    platform: &Platform,
    vars: &BTreeMap<String, String>,
    opts: ApplyOpts,
) -> Result<(), ApplyAction> {
    let entries: Vec<_> = source_names
        .iter()
        .map(|name| render::resolve_entry(name))
        .collect();
    for entry in &entries {
        let source_path = store_dir.join(&entry.source_rel);
        // Validate every desired source before removing the live whole-dir
        // link. Otherwise a source reached through an escaped gateway could
        // fail only after the old target had already been removed.
        if let Err(error) = linker::validate_source_in(&source_path, store_dir) {
            return Err(ApplyAction::Error(error.into()));
        }
        // `symlink_metadata` does not follow terminal source symlinks, so a
        // dangling non-template source remains a valid entry. Template
        // sources are checked as direct regular files by the render step.
        if std::fs::symlink_metadata(&source_path).is_err() {
            return Err(internal_error(format!(
                "source does not exist: {}",
                source_path.display()
            )));
        }
    }
    for entry in entries.iter().filter(|entry| entry.is_template) {
        let source_path = store_dir.join(&entry.source_rel);
        let result = if opts.dry_run {
            render::staged_differs(
                repo_root,
                store_name,
                &entry.source_rel,
                &source_path,
                platform,
                vars,
            )
            .map(|_| ())
        } else {
            render::stage_template(
                repo_root,
                store_name,
                &entry.source_rel,
                &source_path,
                platform,
                vars,
            )
            .map(|_| ())
        };
        if let Err(e) = result {
            return Err(ApplyAction::Error(StitchError::render(&source_path, e)));
        }
    }
    Ok(())
}

/// Preview an entry after [`prepare_file_mode_target`] would remove the old
/// whole-directory root. The promotion preflight already verified its source
/// and template render, so every child link is known to be absent afterwards.
fn preview_file_entry_after_root_removal(target_path: &Path, source_name: &str) -> ApplyAction {
    let entry = render::resolve_entry(source_name);
    ApplyAction::Created(target_path.join(entry.link_rel))
}

/// Apply a whole-directory store, including a safe transition back from file mode.
///
/// A store with templates is promoted to a real target directory containing
/// individual links. If its last template disappears, its desired state becomes
/// a single directory link again. The real target dir is normally a conflict,
/// but we can prove it came from file mode when it contains stale links owned by
/// this store. Remove only those links, then replace the directory only when it
/// is empty. Foreign content keeps the ordinary conflict behavior.
fn apply_whole_dir(
    store_name: &str,
    store_dir: &Path,
    target_path: &Path,
    repo_root: &Path,
    keep_link_rels: &BTreeSet<String>,
    opts: ApplyOpts,
) -> Vec<ApplyAction> {
    // Do not scan a correct directory symlink: `Path::is_dir` follows it, and
    // treating the root link as a stale child would remove the desired state.
    if target_path.is_symlink() || !target_path.is_dir() {
        return vec![apply_single_link(
            store_dir,
            target_path,
            repo_root,
            store_dir,
            opts,
        )];
    }

    let removed = match render::reconcile_store_links(
        target_path,
        repo_root,
        store_dir,
        store_name,
        keep_link_rels,
        opts.dry_run,
    ) {
        Ok(removed) => removed,
        Err(e) => return vec![internal_error(e)],
    };
    let had_stale_links = !removed.is_empty();
    let mut actions: Vec<ApplyAction> = removed.into_iter().map(ApplyAction::Removed).collect();

    // An empty pre-existing directory is still a user-owned conflict. Only
    // replace it automatically after proving that we removed stale links for
    // this exact store from it.
    if !had_stale_links {
        actions.push(apply_single_link(
            store_dir,
            target_path,
            repo_root,
            store_dir,
            opts,
        ));
        return actions;
    }

    let empty = if opts.dry_run {
        target_would_be_empty_after_removals(target_path, &actions)
    } else {
        remove_empty_target_dir(target_path)
    };
    match empty {
        Ok(true) => {
            if opts.dry_run {
                actions.push(ApplyAction::Replaced {
                    target: target_path.to_path_buf(),
                    old_resolves_to: None,
                });
            } else {
                match linker::create_link_in(target_path, store_dir, store_dir) {
                    Ok(()) => actions.push(ApplyAction::Replaced {
                        target: target_path.to_path_buf(),
                        old_resolves_to: None,
                    }),
                    Err(e) => actions.push(internal_error(e.to_string())),
                }
            }
        }
        Ok(false) => actions.push(apply_single_link(
            store_dir,
            target_path,
            repo_root,
            store_dir,
            opts,
        )),
        Err(e) => actions.push(internal_error(e)),
    }
    actions
}

/// Whether `target_path` would become an empty directory after the removals
/// reported by a dry-run stale-link reconciliation.
fn target_would_be_empty_after_removals(
    target_path: &Path,
    actions: &[ApplyAction],
) -> Result<bool, String> {
    let removed: BTreeSet<&Path> = actions
        .iter()
        .filter_map(|action| match action {
            ApplyAction::Removed(path) => Some(path.as_path()),
            _ => None,
        })
        .collect();

    for entry in walkdir::WalkDir::new(target_path)
        .follow_links(false)
        .into_iter()
    {
        let entry = entry.map_err(|e| {
            format!(
                "could not scan target {} while previewing directory replacement: {e}",
                target_path.display()
            )
        })?;
        if entry.depth() == 0 {
            continue;
        }
        if entry.file_type().is_symlink() && removed.contains(entry.path()) {
            continue;
        }
        // Do not erase even an empty nested directory: it could have been
        // created by the user after the prior file-mode apply. The target root
        // itself is replaced only when it becomes directly empty.
        return Ok(false);
    }
    Ok(true)
}

/// Remove the target root only when it is directly empty. Nested directories
/// are deliberately retained: without a state database we cannot distinguish a
/// directory created for a prior file-mode link from an empty one a user made.
fn remove_empty_target_dir(target_path: &Path) -> Result<bool, String> {
    match std::fs::remove_dir(target_path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(false),
        Err(e) => Err(format!(
            "could not remove empty target directory {}: {e}",
            target_path.display()
        )),
    }
}

/// Reject a target whose ancestors are unsafe, evaluated immediately before
/// the mutation that depends on them (so a pre-apply hook that replaced an
/// ancestor is caught, not written through):
///
/// - a symlink ancestor that resolves back into the repo is a conflict
///   (writing through it could reach another store's sources);
/// - a symlink ancestor that resolves OUTSIDE canonical `$HOME` is a conflict:
///   config validation already rejects such targets at load, and a hook may
///   have introduced one after that check — writing through it would escape
///   `$HOME` entirely.
/// - a symlink *at* the target itself is also rejected for file-mode roots:
///   a file-mode target must be a real directory, not a symlink (even one
///   that resolves to $HOME via an alias like ~/.alias -> ~). Whole-dir
///   targets are allowed to be symlinks pointing at their store dir — that
///   case is handled by the caller after this check.
fn target_is_confined(target: &Path, repo_root: &Path) -> Result<(), ApplyAction> {
    let canonical_home = match crate::config::canonical_home() {
        Ok(home) => home,
        Err(e) => {
            return Err(internal_error(format!(
                "could not resolve $HOME while checking target confinement: {e}"
            )));
        }
    };
    // Check ancestors first (existing behavior).
    let mut ancestor = target.parent();
    while let Some(path) = ancestor {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Repo-pointing ancestors are always conflicts: writing
                // through them reaches other stores' sources.
                if linker::points_into_repo(path, repo_root) {
                    return Err(ApplyAction::Conflict {
                        target: path.to_path_buf(),
                        resolves_to: std::fs::read_link(path).ok(),
                    });
                }
                // A non-repo symlink ancestor must still resolve inside
                // canonical $HOME. External volume/gateway symlinks were
                // rejected by config validation; enforce the same boundary
                // here so a hook that introduced one cannot redirect writes
                // outside $HOME after the load-time check.
                match linker::resolve_path_with_missing(path) {
                    Some(resolved) if resolved.starts_with(&canonical_home) => {}
                    _ => {
                        return Err(ApplyAction::Conflict {
                            target: path.to_path_buf(),
                            resolves_to: std::fs::read_link(path).ok(),
                        });
                    }
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(internal_error(format!(
                    "could not inspect target ancestor {}: {e}",
                    path.display()
                )));
            }
        }
        ancestor = path.parent();
    }
    // For file-mode roots, a symlink at the target itself is a foreign
    // conflict (even if it resolves to $HOME via an alias like ~/.alias -> ~).
    // The exception is $HOME itself being a symlink (home_link -> real_home)
    // — that is the user's actual home and must be allowed for file-mode
    // stores with target "~" (e.g. .bashrc).
    if let Ok(meta) = std::fs::symlink_metadata(target)
        && meta.file_type().is_symlink()
    {
        // $HOME itself may be a symlink; allow it.
        let is_home_itself = crate::config::expand_home("~")
            .ok()
            .is_some_and(|home| home == target);
        if !is_home_itself && !linker::points_into_repo(target, repo_root) {
            return Err(ApplyAction::Conflict {
                target: target.to_path_buf(),
                resolves_to: std::fs::read_link(target).ok(),
            });
        }
    }
    Ok(())
}

/// Walk the ancestor directories of `target` between `target_root` (exclusive)
/// and `target` (exclusive). Return the first that exists as a symlink, along
/// with its read-link target.
///
/// This is the nested-link safety guard: `create_dir_all` follows symlinks, so
/// a symlink at `<target>/lua` would cause `<target>/lua/plugin.lua` to be
/// written through the link. Every intermediate component is checked before
/// linking; any symlink ancestor is a hard conflict — even one that points into
/// the repository — because writing through it can create links inside another
/// store, move repository content to `.bak` under `--force`, or delete repo
/// content through an aliased broken link.
fn symlink_ancestor(target_root: &Path, target: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let mut ancestor = target.parent()?;
    while ancestor != target_root {
        if !ancestor.starts_with(target_root) {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
        {
            let resolves_to = std::fs::read_link(ancestor).ok();
            return Some((ancestor.to_path_buf(), resolves_to));
        }
        ancestor = ancestor.parent()?;
    }
    None
}

/// Apply one resolved file-mode entry. Templates render to staging first;
/// non-templates link the store source directly.
#[allow(clippy::too_many_arguments)] // mirrors apply_target's parameter set
pub(crate) fn store_resolves_source(store_dir: &Path, store: &Store, source_name: &str) -> bool {
    matches!(
        resolve_targets(store_dir, &store.files, &store.patterns, &store.ignore),
        Ok(LinkTargets::Files(names)) if names.iter().any(|name| name == source_name)
    )
}

/// Validate a target path before an add moves user data. This is deliberately
/// separate from linking: add must prove target confinement before it renames
/// the user's source into the repository.
pub(crate) fn preflight_add_target(
    repo_root: &Path,
    target_root: &Path,
    target: &Path,
) -> Result<(), ApplyAction> {
    target_is_confined(target, repo_root)?;
    if let Ok(meta) = std::fs::symlink_metadata(target_root)
        && meta.file_type().is_symlink()
    {
        // The configured target root itself must be stable. The one allowed
        // exception is the user's actual `$HOME` entry, which may legitimately
        // be a symlink (for example, a test or managed home mount).
        let is_home_root = crate::config::expand_home("~")
            .ok()
            .is_some_and(|home| home == target_root);
        if !is_home_root {
            return Err(ApplyAction::Conflict {
                target: target_root.to_path_buf(),
                resolves_to: std::fs::read_link(target_root).ok(),
            });
        }
    }
    if let Some((ancestor, resolves_to)) = symlink_ancestor(target_root, target) {
        return Err(ApplyAction::Conflict {
            target: ancestor,
            resolves_to,
        });
    }
    // `apply` may repair a stale stitch-owned symlink. `add` is different:
    // it must never repoint an existing entry while the user's source is being
    // moved, even when that entry happens to point into this repository.
    if let Ok(meta) = std::fs::symlink_metadata(target)
        && meta.file_type().is_symlink()
    {
        // A symlinked $HOME is the user's actual home directory, not an
        // unsafe target alias. Keep allowing the root itself, while still
        // rejecting symlinked file targets and all other aliases.
        let is_home_root = target == target_root
            && crate::config::expand_home("~")
                .ok()
                .is_some_and(|home| home == target);
        if !is_home_root {
            return Err(ApplyAction::Conflict {
                target: target.to_path_buf(),
                resolves_to: std::fs::read_link(target).ok(),
            });
        }
    }
    Ok(())
}

/// Link exactly one newly adopted plain-file entry without reconciling the
/// rest of the store. `add --to` uses this narrow path so a failed adoption
/// cannot remove unrelated stale links or rendered files.
pub(crate) fn apply_added_plain_file(
    repo_root: &Path,
    store_name: &str,
    store: &Store,
    source_name: &str,
    platform: &Platform,
    opts: ApplyOpts,
) -> ApplyAction {
    if !platform.matches_when(&store.when) {
        return ApplyAction::SkippedPlatform;
    }
    let Some(target) = store.target.as_deref() else {
        return internal_error(format!("store '{store_name}' has no target"));
    };
    let store_dir = repo_root.join(store_name);
    if !linker::is_real_directory(&store_dir) {
        return internal_error(format!(
            "store directory '{}' is missing, symlinked, or not a directory",
            store_dir.display()
        ));
    }
    let target_path = match config::expand_home(target) {
        Ok(path) => path,
        Err(error) => return ApplyAction::Error(error.into()),
    };
    let entry = render::resolve_entry(source_name);
    let target = target_path.join(&entry.link_rel);
    if let Err(action) = preflight_add_target(repo_root, &target_path, &target) {
        return action;
    }
    apply_file_entry(
        store_name,
        &store_dir,
        &target_path,
        repo_root,
        source_name,
        platform,
        &BTreeMap::new(),
        opts,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_file_entry(
    store_name: &str,
    store_dir: &Path,
    target_path: &Path,
    repo_root: &Path,
    source_name: &str,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
    opts: ApplyOpts,
) -> ApplyAction {
    let entry = render::resolve_entry(source_name);
    let source_path = store_dir.join(&entry.source_rel);
    let target = target_path.join(&entry.link_rel);

    // Safety: do not create a nested link through any symlink ancestor, even
    // one that points into the repository. The target itself is handled by
    // `apply_single_link`; this guards every intermediate parent directory.
    if let Some((ancestor, resolves_to)) = symlink_ancestor(target_path, &target) {
        return ApplyAction::Conflict {
            target: ancestor,
            resolves_to,
        };
    }

    if !entry.is_template {
        return apply_single_link(&source_path, &target, repo_root, store_dir, opts);
    }
    if !is_regular_template_source(&source_path) {
        return ApplyAction::Error(StitchError::render(
            &source_path,
            "template source must be a direct regular file",
        ));
    }

    let staged = render::staging_path(repo_root, store_name, &entry.link_rel);

    if opts.dry_run {
        // Content dimension: fresh in-memory render vs staged. Never write.
        let content_differs = match render::staged_differs(
            repo_root,
            store_name,
            &entry.source_rel,
            &source_path,
            platform,
            vars,
        ) {
            Ok(d) => d,
            Err(e) => return ApplyAction::Error(StitchError::render(&source_path, e)),
        };
        let staged_dir = render::store_render_dir(repo_root, store_name);
        let link_action = apply_single_link(&staged, &target, repo_root, &staged_dir, opts);
        if content_differs && matches!(link_action, ApplyAction::AlreadyLinked(_)) {
            ApplyAction::ContentChanged(target)
        } else {
            link_action
        }
    } else {
        // Render-before-link: failure skips the link entirely (no broken link).
        let (link_source, wrote) = match render::stage_template(
            repo_root,
            store_name,
            &entry.source_rel,
            &source_path,
            platform,
            vars,
        ) {
            Ok(render::StageOutcome::Written(p)) => (p, true),
            Ok(render::StageOutcome::Unchanged(p)) => (p, false),
            Err(e) => return ApplyAction::Error(StitchError::render(&source_path, e)),
        };
        let staged_dir = render::store_render_dir(repo_root, store_name);
        let action = apply_single_link(&link_source, &target, repo_root, &staged_dir, opts);
        // Link already correct but staging was refreshed → content changed.
        if wrote && matches!(action, ApplyAction::AlreadyLinked(_)) {
            ApplyAction::ContentChanged(target)
        } else {
            action
        }
    }
}

fn source_is_symlink(source: &Path) -> bool {
    std::fs::symlink_metadata(source)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn create_link_for(target: &Path, source: &Path, source_root: &Path) -> Result<(), LinkError> {
    if source_is_symlink(source) {
        linker::create_link_to_entry_in(target, source, source_root)
    } else {
        linker::create_link_in(target, source, source_root)
    }
}

/// Atomically replace an existing symlink at `target` with a new link to
/// `source`. The new link is created at a sibling temp path, then renamed over
/// the target in a single `rename(2)`. If the link step fails the original
/// target is untouched; if the final rename fails the original is rolled back
/// from a second temp path. This closes the window where a failed `apply`
/// would `remove_file` a stale repo-owned symlink and then fail to create the
/// replacement, leaving the target absent (the v0.11.4 release assessment's
/// "failed apply deletes a dangling link" finding).
///
/// Precondition: `target` is an existing symlink (caller has already
/// classified it via `linker::check_link`). The rename-based swap preserves
/// the symlink-replacement semantics the linker relies on.
fn atomic_replace_link(target: &Path, source: &Path, source_root: &Path) -> Result<(), LinkError> {
    let parent = target.parent().ok_or_else(|| {
        LinkError::Create(
            std::io::Error::other(format!("{} has no parent directory", target.display())),
            target.to_path_buf(),
        )
    })?;
    let name = target.file_name().ok_or_else(|| {
        LinkError::Create(
            std::io::Error::other(format!("{} has no file name", target.display())),
            target.to_path_buf(),
        )
    })?;
    let name_str = name.to_string_lossy();
    let pid = std::process::id();
    let tmp_link = parent.join(format!(".{name_str}.stitch-link-{pid}"));
    let tmp_orig = parent.join(format!(".{name_str}.stitch-orig-{pid}"));

    if tmp_link.symlink_metadata().is_ok() || tmp_orig.symlink_metadata().is_ok() {
        return Err(LinkError::Create(
            std::io::Error::other(format!(
                "temporary replacement path for {} already exists",
                target.display()
            )),
            target.to_path_buf(),
        ));
    }

    // Create the new link at a temp path first. If this fails, the original
    // target is still in place.
    create_link_for(&tmp_link, source, source_root)?;

    // Move the existing symlink aside. `rename` over an existing path is
    // atomic on POSIX.
    if let Err(e) = std::fs::rename(target, &tmp_orig) {
        let _ = std::fs::remove_file(&tmp_link);
        return Err(LinkError::Remove(e, target.to_path_buf()));
    }

    // Move the new link into place.
    if let Err(e) = std::fs::rename(&tmp_link, target) {
        // Roll the original back so the target is not left absent.
        let rollback = std::fs::rename(&tmp_orig, target);
        let _ = std::fs::remove_file(&tmp_link);
        if let Err(re) = rollback {
            return Err(LinkError::Create(
                std::io::Error::other(format!(
                    "could not place symlink at {}: {e}; rollback also failed ({re}); \
                     the original entry is at {}",
                    target.display(),
                    tmp_orig.display()
                )),
                target.to_path_buf(),
            ));
        }
        return Err(LinkError::Create(e, target.to_path_buf()));
    }

    // The original symlink is now at tmp_orig; remove it. A failure here is
    // not data-loss (the new link is in place) but leaves a stray temp file,
    // so report it honestly.
    if let Err(e) = std::fs::remove_file(&tmp_orig) {
        return Err(LinkError::Remove(
            std::io::Error::other(format!(
                "replaced {} but could not remove original: {e}",
                target.display()
            )),
            tmp_orig,
        ));
    }
    Ok(())
}

fn apply_single_link(
    source: &Path,
    target: &Path,
    repo_root: &Path,
    source_root: &Path,
    opts: ApplyOpts,
) -> ApplyAction {
    // Validate both boundaries immediately before classification/mutation. In
    // particular, never remove an old managed link and only then discover that
    // its replacement source escapes through a gateway.
    let staged_dry_run = opts.dry_run && source.starts_with(render::render_root(repo_root));
    if !staged_dry_run && let Err(e) = linker::validate_source_in(source, source_root) {
        return internal_error(e.to_string());
    }
    if let Err(action) = target_is_confined(target, repo_root) {
        return action;
    }

    let status = linker::check_link(target, source, repo_root);

    match status {
        LinkStatus::Linked => ApplyAction::AlreadyLinked(target.to_path_buf()),
        LinkStatus::StoreError(store_dir) => ApplyAction::Error(StitchError::internal(format!(
            "store directory '{}' is missing, symlinked, or not a directory",
            store_dir.display()
        ))),
        LinkStatus::Foreign(resolved) => ApplyAction::Conflict {
            target: target.to_path_buf(),
            resolves_to: Some(resolved),
        },
        LinkStatus::Missing => {
            if opts.dry_run {
                ApplyAction::Created(target.to_path_buf())
            } else {
                match create_link_for(target, source, source_root) {
                    Ok(()) => ApplyAction::Created(target.to_path_buf()),
                    Err(e) => internal_error(e.to_string()),
                }
            }
        }
        // A real file or directory occupies the target. Without --force this
        // is a hard conflict; with --force the target is renamed to `.bak`
        // and the link takes its place. (Foreign symlinks are handled by the
        // dedicated `LinkStatus::Foreign` arm above; they never reach here.)
        LinkStatus::Conflict(_) => {
            if !opts.force {
                ApplyAction::Conflict {
                    target: target.to_path_buf(),
                    resolves_to: None,
                }
            } else {
                force_backup_link(source, target, source_root, opts.dry_run)
            }
        }
        LinkStatus::Broken(resolved) => {
            // A symlink that isn't ours. Relink only if it points into this
            // repo (stale stitch state — the store moved or a file was
            // renamed); a foreign symlink (stow/chezmoi/Nix/Home-Manager, or
            // a dangling user link) is a conflict, never silently clobbered —
            // even under --force.
            if !linker::points_into_repo(target, repo_root) {
                return ApplyAction::Conflict {
                    target: target.to_path_buf(),
                    resolves_to: Some(resolved),
                };
            }
            if opts.dry_run {
                return ApplyAction::Replaced {
                    target: target.to_path_buf(),
                    old_resolves_to: Some(resolved),
                };
            }
            let old_resolves_to = resolved.clone();
            // Atomic swap: create the new link at a temp path, then rename it
            // over the stale one. A failure during link creation leaves the
            // original (stale but present) symlink in place rather than
            // deleting it first and failing to create the replacement — the
            // v0.11.4 release assessment's "failed apply deletes a dangling
            // link" finding.
            match atomic_replace_link(target, source, source_root) {
                Ok(()) => ApplyAction::Replaced {
                    target: target.to_path_buf(),
                    old_resolves_to: Some(old_resolves_to),
                },
                Err(e) => internal_error(e.to_string()),
            }
        }
        LinkStatus::ConfigError(msg) => internal_error(msg),
    }
}

/// Resolve a real-file/dir conflict (`apply --force`) by renaming the target
/// to `{target}.bak` and creating the symlink.
///
/// Fails — leaving the original target in place — if a backup already exists
/// (never silently destroy a prior backup) or the link step fails after the
/// rename (the backup is restored so the user loses nothing).
fn force_backup_link(
    source: &Path,
    target: &Path,
    source_root: &Path,
    dry_run: bool,
) -> ApplyAction {
    let backup = backup_path(target);

    // Catch anything at the backup path in both dry-run and real modes so the
    // rendered plan is honest and `diff --force` matches real execution.
    if backup.symlink_metadata().is_ok() {
        let resolves_to = std::fs::read_link(&backup).ok();
        return ApplyAction::Conflict {
            target: backup,
            resolves_to,
        };
    }

    if dry_run {
        return ApplyAction::BackedUp {
            target: target.to_path_buf(),
            backup,
        };
    }
    if let Err(e) = std::fs::rename(target, &backup) {
        return internal_error(format!(
            "failed to back up {} → {}: {}",
            target.display(),
            backup.display(),
            e
        ));
    }
    if let Err(e) = create_link_for(target, source, source_root) {
        // Restore the original so the user is left with their file, not a
        // missing target. Best-effort: a (near-impossible) restore failure
        // is ignored rather than masking the original link error.
        let _ = std::fs::rename(&backup, target);
        return internal_error(format!(
            "failed to link after backing up {}: {e}",
            target.display()
        ));
    }
    ApplyAction::BackedUp {
        target: target.to_path_buf(),
        backup,
    }
}

/// Backup path for a target: `{target}.bak`.
///
/// Appends rather than `Path::with_extension("bak")` — dotfiles like `.bashrc`
/// have no extension in `Path` semantics, so `with_extension` would yield just
/// `.bak`, dropping the name. Uses `OsString` to stay correct on non-UTF8 paths.
fn backup_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".bak");
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symlink_ancestor_detects_foreign_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target_root = tmp.path().join("target");
        std::fs::create_dir_all(&target_root).unwrap();
        let foreign = tmp.path().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::os::unix::fs::symlink(&foreign, target_root.join("lua")).unwrap();

        let target = target_root.join("lua").join("plugin.lua");
        let (ancestor, resolves_to) =
            symlink_ancestor(&target_root, &target).expect("symlink ancestor found");
        assert_eq!(ancestor, target_root.join("lua"));
        assert_eq!(resolves_to, Some(foreign));
    }

    #[test]
    fn test_symlink_ancestor_detects_repo_owned_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store = repo_root.join("nvim").join("lua");
        std::fs::create_dir_all(&store).unwrap();
        let target_root = tmp.path().join("target");
        std::fs::create_dir_all(&target_root).unwrap();
        std::os::unix::fs::symlink(&store, target_root.join("lua")).unwrap();

        let target = target_root.join("lua").join("plugin.lua");
        let (ancestor, resolves_to) =
            symlink_ancestor(&target_root, &target).expect("symlink ancestor found");
        assert_eq!(ancestor, target_root.join("lua"));
        assert_eq!(resolves_to, Some(store));
    }

    #[test]
    fn test_apply_store_rejects_repo_target_ancestor_without_reconciliation() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("active"), "active").unwrap();
        std::fs::write(store_dir.join("stale"), "stale").unwrap();

        let victim = repo_root.join("victim/.config");
        std::fs::create_dir_all(&victim).unwrap();
        let stale_target = victim.join("stale");
        std::os::unix::fs::symlink(store_dir.join("stale"), &stale_target).unwrap();
        let home = tmp.path().join("home");
        std::os::unix::fs::symlink(repo_root.join("victim"), &home).unwrap();

        let store = crate::config::Store {
            target: Some(home.join(".config").to_string_lossy().into_owned()),
            files: vec!["active".into()],
            patterns: Vec::new(),
            ignore: Vec::new(),
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let result = apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut Vec::new(),
        );

        assert!(matches!(
            result.actions.as_slice(),
            [ApplyAction::Conflict { target, .. }] if target == &home
        ));
        assert!(
            stale_target.is_symlink(),
            "unsafe target must not be scanned"
        );
        assert!(
            !victim.join("active").exists(),
            "must not write through home"
        );
    }

    #[test]
    fn test_apply_store_blocks_external_target_ancestor() {
        // An ancestor symlink that resolves OUTSIDE canonical $HOME is a
        // conflict: config validation rejects such targets at load, and a
        // pre-apply hook could otherwise introduce one after that check and
        // redirect writes out of $HOME. Home itself being a symlink is the
        // allowed exception (it IS the canonical home).
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::create_dir_all(external.join(".config")).unwrap();
        let home = tmp.path().join("home");
        std::os::unix::fs::symlink(&external, &home).unwrap();

        let store = crate::config::Store {
            target: Some(home.join(".config").to_string_lossy().into_owned()),
            files: vec!["init.lua".into()],
            patterns: Vec::new(),
            ignore: Vec::new(),
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let result = apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut Vec::new(),
        );

        // The `home` symlink is the offending ancestor; nothing may be
        // created through it.
        assert!(matches!(
            result.actions.as_slice(),
            [ApplyAction::Conflict { target, .. }] if target == &home
        ));
        assert!(
            !external.join(".config/init.lua").exists(),
            "must not write through the external symlink ancestor"
        );
    }

    #[test]
    fn test_apply_store_blocks_nested_link_through_foreign_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        let lua = store_dir.join("lua");
        std::fs::create_dir_all(&lua).unwrap();
        std::fs::write(lua.join("plugin.lua"), "plugin").unwrap();
        std::fs::write(lua.join("secret.bak"), "secret").unwrap();

        let target = tmp.path().join("home").join(".config").join("nvim");
        std::fs::create_dir_all(&target).unwrap();
        let foreign = tmp.path().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::os::unix::fs::symlink(&foreign, target.join("lua")).unwrap();

        let store = crate::config::Store {
            target: Some(target.to_string_lossy().into_owned()),
            files: Vec::new(),
            patterns: Vec::new(),
            ignore: vec!["*.bak".into()],
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let mut warnings = Vec::new();
        let result = apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut warnings,
        );

        // One nested entry should conflict on the foreign ancestor.
        let conflict = result
            .actions
            .iter()
            .find_map(|a| match a {
                ApplyAction::Conflict {
                    target,
                    resolves_to,
                } => Some((target.clone(), resolves_to.clone())),
                _ => None,
            })
            .expect("expected conflict for foreign ancestor");
        assert_eq!(conflict.0, target.join("lua"));
        assert_eq!(conflict.1, Some(foreign.clone()));

        // The top-level link was created, the foreign directory was not written.
        assert!(target.join("init.lua").is_symlink());
        assert!(target.join("lua").is_symlink());
        assert!(!foreign.join("plugin.lua").exists());
    }

    #[test]
    fn test_apply_store_blocks_nested_link_through_repo_owned_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        let lua = store_dir.join("lua");
        std::fs::create_dir_all(&lua).unwrap();
        std::fs::write(lua.join("plugin.lua"), "plugin").unwrap();

        // Another directory inside the repo that the ancestor symlink will resolve to.
        let other = repo_root.join("other").join("lua");
        std::fs::create_dir_all(&other).unwrap();

        let target = tmp.path().join("home").join(".config").join("nvim");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&other, target.join("lua")).unwrap();

        let store = crate::config::Store {
            target: Some(target.to_string_lossy().into_owned()),
            files: Vec::new(),
            patterns: vec!["**/*".into()],
            ignore: Vec::new(),
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let mut warnings = Vec::new();
        let result = apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut warnings,
        );

        // The nested entry must conflict on the repo-pointing ancestor.
        let conflict = result
            .actions
            .iter()
            .find_map(|a| match a {
                ApplyAction::Conflict {
                    target,
                    resolves_to,
                } => Some((target.clone(), resolves_to.clone())),
                _ => None,
            })
            .expect("expected conflict for repo-pointing ancestor");
        assert_eq!(conflict.0, target.join("lua"));
        assert_eq!(conflict.1, Some(other.clone()));

        // The top-level link was created, but nothing was written through the
        // repo-pointing symlink into the other store.
        assert!(target.join("init.lua").is_symlink());
        assert!(target.join("lua").is_symlink());
        assert!(!other.join("plugin.lua").exists());
        assert!(lua.join("plugin.lua").exists());
    }

    #[test]
    fn test_apply_store_force_does_not_write_through_repo_owned_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        let lua = store_dir.join("lua");
        std::fs::create_dir_all(&lua).unwrap();
        std::fs::write(lua.join("plugin.lua"), "nvim plugin").unwrap();

        // Another directory inside the repo with an existing file at the
        // aliased destination. `--force` must not back it up through the symlink.
        let other = repo_root.join("other").join("lua");
        std::fs::create_dir_all(&other).unwrap();
        let other_plugin = other.join("plugin.lua");
        std::fs::write(&other_plugin, "other plugin").unwrap();

        let target = tmp.path().join("home").join(".config").join("nvim");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&other, target.join("lua")).unwrap();

        let store = crate::config::Store {
            target: Some(target.to_string_lossy().into_owned()),
            files: Vec::new(),
            patterns: vec!["**/*".into()],
            ignore: Vec::new(),
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let mut warnings = Vec::new();
        let result = apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: true,
            },
            &mut warnings,
        );

        let conflict = result
            .actions
            .iter()
            .find_map(|a| match a {
                ApplyAction::Conflict {
                    target,
                    resolves_to,
                } => Some((target.clone(), resolves_to.clone())),
                _ => None,
            })
            .expect("expected conflict for repo-pointing ancestor even with --force");
        assert_eq!(conflict.0, target.join("lua"));
        assert_eq!(conflict.1, Some(other.clone()));

        // No backup was created inside the repo and the existing file is unchanged.
        assert!(!other.join("plugin.lua.bak").exists());
        assert_eq!(
            std::fs::read_to_string(&other_plugin).unwrap(),
            "other plugin"
        );
        assert_eq!(
            std::fs::read_to_string(lua.join("plugin.lua")).unwrap(),
            "nvim plugin"
        );
    }

    #[test]
    fn test_apply_store_preserves_symlink_source_through_promotion() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        std::os::unix::fs::symlink("init.lua", store_dir.join("init.vim")).unwrap();
        std::fs::write(store_dir.join("secret.bak"), "secret").unwrap();

        let target = tmp.path().join("home").join(".config").join("nvim");

        let store = crate::config::Store {
            target: Some(target.to_string_lossy().into_owned()),
            files: Vec::new(),
            patterns: Vec::new(),
            ignore: vec!["*.bak".into()],
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let mut warnings = Vec::new();
        apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut warnings,
        );

        assert!(target.is_dir());
        assert!(!target.is_symlink());
        assert!(target.join("init.lua").is_symlink());
        assert!(target.join("init.vim").is_symlink());
        assert_eq!(
            std::fs::read_link(target.join("init.vim")).unwrap(),
            store_dir.join("init.vim")
        );
        assert!(!target.join("secret.bak").exists());
    }

    #[test]
    fn test_apply_store_preserves_dangling_symlink_source() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        let store_dir = repo_root.join("nvim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("init.lua"), "init").unwrap();
        std::os::unix::fs::symlink("missing", store_dir.join("dangling")).unwrap();
        std::fs::write(store_dir.join("secret.bak"), "secret").unwrap();

        let target = tmp.path().join("home").join(".config").join("nvim");

        let store = crate::config::Store {
            target: Some(target.to_string_lossy().into_owned()),
            files: Vec::new(),
            patterns: Vec::new(),
            ignore: vec!["*.bak".into()],
            when: crate::config::WhenClause::default(),
            hooks: crate::config::Hooks::default(),
            targets: BTreeMap::new(),
        };
        let platform = crate::platform::Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: None,
            hostname: "test".into(),
            shell: "bash".into(),
        };
        let mut warnings = Vec::new();
        apply_store(
            &repo_root,
            "nvim",
            &store,
            &platform,
            &BTreeMap::new(),
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut warnings,
        );

        assert!(target.is_dir());
        assert!(!target.is_symlink());
        assert!(target.join("init.lua").is_symlink());
        assert!(target.join("dangling").is_symlink());
        assert!(!target.join("dangling").exists());
        assert_eq!(
            std::fs::read_link(target.join("dangling")).unwrap(),
            store_dir.join("dangling")
        );
        assert!(!target.join("secret.bak").exists());
    }
}
