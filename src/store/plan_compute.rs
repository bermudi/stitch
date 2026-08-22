//! Plan conversion: translate `apply`'s `ApplyResult`/`ApplyAction` into the
//! M3 `Plan` (`PlanOp`) with source paths and preconditions.
//!
//! One-directional: imports types from `super::apply` and helpers from
//! `super::resolve`, but does not call back into `apply`.

use super::apply::{ApplyAction, ApplyOpts, ApplyResult};
use super::resolve::{resolve_link_sources, resolve_remove_source, whole_dir_link_target};
use crate::config::Store;
use crate::error::FailureClass;
use crate::linker;
use crate::plan::{LinkRequires, Plan, PlanOp, PlanStore, TargetState, path_to_string};
use std::collections::BTreeMap;
use std::path::Path;

/// Convert execution results into a `Plan` with source paths and preconditions.
/// This is where the M3 upgrade happens: every `ApplyAction` gets the source
/// and `requires` state it needs for the M4 executor and for JSON/text render.
pub(super) fn to_plan(
    repo_root: &Path,
    stores: &BTreeMap<String, Store>,
    results: &[ApplyResult],
    _opts: ApplyOpts,
) -> Plan {
    let mut stores_vec = Vec::new();
    for result in results {
        let store = stores.get(&result.store_name);
        let store_dir = repo_root.join(&result.store_name);
        let link_sources = resolve_link_sources(repo_root, &store_dir, store, &result.store_name);
        let mut ops = Vec::new();
        for action in &result.actions {
            ops.push(action_to_plan_op(
                repo_root,
                &store_dir,
                store,
                &result.store_name,
                &link_sources,
                action,
            ));
        }
        stores_vec.push(PlanStore {
            store_name: result.store_name.clone(),
            ops,
        });
    }
    Plan::from_stores(stores_vec)
}

fn action_to_plan_op(
    repo_root: &Path,
    store_dir: &Path,
    store: Option<&Store>,
    store_name: &str,
    link_sources: &BTreeMap<std::path::PathBuf, String>,
    action: &ApplyAction,
) -> PlanOp {
    match action {
        ApplyAction::SkippedPlatform => PlanOp::SkippedPlatform,
        ApplyAction::Error(e) => PlanOp::Error {
            message: e.to_string(),
            class: e.class().id().to_string(),
            hook_name: match e {
                crate::error::StitchError::Hook { name, .. } => Some(name.clone()),
                _ => None,
            },
        },
        ApplyAction::Conflict {
            target,
            resolves_to,
        } => PlanOp::Conflict {
            target: path_to_string(target),
            resolves_to: resolves_to.as_ref().map(|p| path_to_string(p)),
        },
        ApplyAction::Removed(target) => {
            let target_str = path_to_string(target);
            let source = resolve_remove_source(repo_root, store_dir, store, store_name, target);
            let requires = remove_requires(repo_root, store_dir, store, target, &source);
            PlanOp::RemoveLink {
                store: store_name.into(),
                target: target_str,
                source,
                requires,
            }
        }
        ApplyAction::StagedRemoved(path) => PlanOp::RemoveStaged {
            path: path_to_string(path),
        },
        ApplyAction::AlreadyLinked(target) => {
            let target_str = path_to_string(target);
            let Some(source) = link_sources.get(target).cloned() else {
                return unresolved_source_op(&target_str);
            };
            PlanOp::AlreadyLinked {
                target: target_str,
                source: source.clone(),
                requires: LinkRequires::new(TargetState::SymlinkTo(source)),
            }
        }
        ApplyAction::Created(target) => {
            let target_str = path_to_string(target);
            let Some(source) = link_sources.get(target).cloned() else {
                return unresolved_source_op(&target_str);
            };
            PlanOp::CreateLink {
                target: target_str,
                source,
                requires: LinkRequires::new(TargetState::Absent),
            }
        }
        ApplyAction::Replaced {
            target,
            old_resolves_to,
        } => {
            let target_str = path_to_string(target);
            let Some(source) = link_sources.get(target).cloned() else {
                return unresolved_source_op(&target_str);
            };
            let requires_target = match old_resolves_to {
                Some(old) => TargetState::SymlinkTo(path_to_string(old)),
                None => TargetState::RealEntry,
            };
            PlanOp::ReplaceLink {
                target: target_str,
                source,
                old_resolves_to: old_resolves_to.as_ref().map(|p| path_to_string(p)),
                requires: LinkRequires::new(requires_target),
            }
        }
        ApplyAction::ContentChanged(target) => {
            let target_str = path_to_string(target);
            let Some(source) = link_sources.get(target).cloned() else {
                return unresolved_source_op(&target_str);
            };
            PlanOp::ContentChanged {
                target: target_str,
                source: source.clone(),
                requires: LinkRequires::new(TargetState::SymlinkTo(source)),
            }
        }
        ApplyAction::BackedUp { target, backup } => {
            let target_str = path_to_string(target);
            let backup_str = path_to_string(backup);
            let Some(source) = link_sources.get(target).cloned() else {
                return unresolved_source_op(&target_str);
            };
            PlanOp::BackupAndLink {
                target: target_str,
                source,
                backup: backup_str,
                requires: LinkRequires::with_backup(TargetState::RealEntry, TargetState::Absent),
            }
        }
    }
}

fn unresolved_source_op(target: &str) -> PlanOp {
    PlanOp::Error {
        message: format!("could not resolve source for target {target}"),
        class: FailureClass::Internal.id().to_string(),
        hook_name: None,
    }
}

fn remove_requires(
    repo_root: &Path,
    store_dir: &Path,
    _store: Option<&Store>,
    target: &Path,
    source: &Option<String>,
) -> LinkRequires {
    // The source recorded for a whole-directory link uses the configured repo
    // path, while the symlink contains the canonical path. Pin the raw link
    // target in `requires` so plan preflight and execution compare the same
    // bytes that read_link returns.
    if whole_dir_link_target(target, store_dir).is_some()
        && let Ok(resolved) = std::fs::read_link(target)
    {
        return LinkRequires::new(TargetState::SymlinkTo(path_to_string(&resolved)));
    }
    if let Some(src) = source
        && let Ok(resolved) = std::fs::read_link(target)
        && resolved == Path::new(src)
    {
        return LinkRequires::new(TargetState::SymlinkTo(src.clone()));
    }
    if target.is_symlink()
        && linker::points_into_repo(target, repo_root)
        && let Ok(resolved) = std::fs::read_link(target)
    {
        return LinkRequires::new(TargetState::SymlinkTo(path_to_string(&resolved)));
    }
    LinkRequires::new(TargetState::SymlinkIntoRepo)
}
