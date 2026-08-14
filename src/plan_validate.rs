//! Untrusted-input validation for `apply --plan`.
//!
//! Every op in a captured plan file is validated against a freshly loaded,
//! hash-verified config snapshot before any hook runs or filesystem mutation
//! occurs. This module owns that validation: structural checks per op
//! (`validate_op`/`validate_link_op`/`validate_remove_link_op`), the
//! "is this op still present in the freshly computed apply plan?" freshness
//! checks (`validate_fresh_link_write`, `validate_cleanup_dependencies`), and
//! the current-removals recomputation that authorizes stale-link cleanups
//! (`current_removals`).
//!
//! One-directional: `plan_exec` calls into this module; this module does not
//! call back into execution. It depends on `plan_file` for the plan file types
//! and construction helpers, and on `store`/`linker`/`render`/`config` for
//! resolution.

use crate::ancestor::has_parent_dir;
use crate::config::{self, Config, ConfigError, Loaded, Store, is_safe_fragment};
use crate::error::StitchError;
use crate::linker;
use crate::plan::{PlanOp, path_to_string};
use crate::plan_file::{PlanFile, PlanFileOp, build_plan_file, source_store, staged_store};
use crate::platform::Platform;
use crate::render;
use crate::store;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A pinned `StageRender` op tracked across validation so that later link ops
/// can confirm their staged source was produced by a preceding stage op.
#[derive(Debug, Clone)]
pub(crate) struct RenderPin {
    pub(crate) source_rel: String,
    pub(crate) staged: String,
}

// ---------------------------------------------------------------------------
// Untrusted-input validation
// ---------------------------------------------------------------------------

pub(crate) struct ValidationContext<'a> {
    repo_root: &'a Path,
    config: &'a Config,
}

impl<'a> ValidationContext<'a> {
    pub(crate) fn new(repo_root: &'a Path, config: &'a Config) -> Self {
        Self { repo_root, config }
    }
}

type LinkRemovalKey = (String, String, Option<String>);
type StagedRemovalKey = (String, String);

#[derive(Default)]
pub(crate) struct CurrentRemovals {
    links: BTreeSet<LinkRemovalKey>,
    staged: BTreeSet<StagedRemovalKey>,
    staged_dependencies: BTreeMap<StagedRemovalKey, BTreeSet<LinkRemovalKey>>,
    stage_writes: BTreeSet<String>,
    link_writes: BTreeSet<String>,
    sensitive_mutations: BTreeSet<String>,
}

/// Resolve the cleanup operations the current config and filesystem actually
/// call stale. This is intentionally a small, dry-run recomputation rather
/// than a second authorization language for plan-file removals.
pub(crate) fn current_removals(
    repo_root: &Path,
    loaded: &Loaded,
    platform: &Platform,
    force: bool,
) -> Result<CurrentRemovals, StitchError> {
    let computed = store::compute_plan(
        repo_root,
        &loaded.config,
        platform,
        crate::store::ApplyOpts {
            dry_run: true,
            force,
        },
    );
    let mut removals = CurrentRemovals::default();
    for plan_store in &computed.stores {
        for op in &plan_store.ops {
            if let PlanOp::RemoveLink {
                store,
                target,
                source,
                ..
            } = op
            {
                removals
                    .links
                    .insert((store.clone(), target.clone(), source.clone()));
            }
        }
    }

    let current_file = build_plan_file(repo_root, loaded, &computed, platform)?;
    for op in &current_file.ops {
        match op {
            PlanFileOp::RemoveStaged { store, rel } => {
                let staged_key = (store.clone(), rel.clone());
                let staged_path = render::staging_path(repo_root, store, rel);
                let dependencies = current_file
                    .ops
                    .iter()
                    .filter_map(|candidate| match candidate {
                        PlanFileOp::RemoveLink {
                            store,
                            target,
                            source,
                            ..
                        } if linker::points_to_source(
                            Path::new(target),
                            &staged_path,
                            repo_root,
                        ) =>
                        {
                            Some((store.clone(), target.clone(), source.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                removals.staged.insert(staged_key.clone());
                removals
                    .staged_dependencies
                    .insert(staged_key, dependencies);
            }
            PlanFileOp::StageRender { .. } => {
                removals.stage_writes.insert(
                    serde_json::to_string(op)
                        .map_err(|e| StitchError::internal(format!("could not encode op: {e}")))?,
                );
            }
            PlanFileOp::ReplaceLink { requires, .. } if requires.target == "real_entry" => {
                let encoded = serde_json::to_string(op)
                    .map_err(|e| StitchError::internal(format!("could not encode op: {e}")))?;
                // Restoring whole-directory mode replaces the empty directory
                // left by verified stale-link removal. It is both a normal
                // fresh-plan link write and a sensitive real-entry mutation.
                removals.link_writes.insert(encoded.clone());
                removals.sensitive_mutations.insert(encoded);
            }
            PlanFileOp::BackupAndLink { .. } => {
                let encoded = serde_json::to_string(op)
                    .map_err(|e| StitchError::internal(format!("could not encode op: {e}")))?;
                removals.link_writes.insert(encoded.clone());
                removals.sensitive_mutations.insert(encoded);
            }
            PlanFileOp::CreateLink { .. } | PlanFileOp::ReplaceLink { .. } => {
                removals.link_writes.insert(
                    serde_json::to_string(op)
                        .map_err(|e| StitchError::internal(format!("could not encode op: {e}")))?,
                );
            }
            PlanFileOp::RemoveLink { .. } => {}
        }
    }
    Ok(removals)
}

fn target_paths_for_store(store: &Store) -> Result<Vec<PathBuf>, ConfigError> {
    let mut paths = Vec::new();
    if let Some(ref t) = store.target {
        paths.push(config::expand_home(t)?);
    }
    for te in store.targets.values() {
        paths.push(config::expand_home(&te.target)?);
    }
    Ok(paths)
}

fn is_under_any_target(config: &Config, store: &str, target: &Path) -> Result<bool, String> {
    match config.stores.get(store) {
        Some(store) => {
            let paths = target_paths_for_store(store).map_err(|e| e.to_string())?;
            Ok(paths.iter().any(|p| target == p || target.starts_with(p)))
        }
        None => Ok(false),
    }
}

fn validate_fresh_link_write(
    current: &CurrentRemovals,
    idx: usize,
    op: &PlanFileOp,
) -> Result<(), String> {
    let encoded = serde_json::to_string(op)
        .map_err(|e| format!("op {idx}: could not encode operation: {e}"))?;
    if current.link_writes.contains(&encoded) {
        Ok(())
    } else {
        Err(format!(
            "op {idx}: link operation is not present in the freshly computed apply plan"
        ))
    }
}

pub(crate) fn validate_cleanup_dependencies(
    plan: &PlanFile,
    current: &CurrentRemovals,
    exec_order: &[usize],
) -> Result<(), String> {
    let mut preceding_links: BTreeSet<LinkRemovalKey> = BTreeSet::new();
    for &idx in exec_order {
        match &plan.ops[idx] {
            PlanFileOp::RemoveLink {
                store,
                target,
                source,
                ..
            } => {
                preceding_links.insert((store.clone(), target.clone(), source.clone()));
            }
            PlanFileOp::RemoveStaged { store, rel } => {
                let staged_key = (store.clone(), rel.clone());
                let dependencies =
                    current
                        .staged_dependencies
                        .get(&staged_key)
                        .ok_or_else(|| {
                            format!(
                                "op {idx}: no fresh dependency data for remove_staged {store}/{rel}"
                            )
                        })?;
                if let Some((_, target, _)) = dependencies
                    .iter()
                    .find(|dep| !preceding_links.contains(*dep))
                {
                    return Err(format!(
                        "op {idx}: remove_staged {store}/{rel} requires preceding remove_link {target}; the edited plan omitted or reordered that cleanup"
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn validate_op(
    ctx: &ValidationContext,
    current_removals: &CurrentRemovals,
    idx: usize,
    op: &PlanFileOp,
    rendered: &mut BTreeMap<(String, String), RenderPin>,
) -> Result<(), String> {
    match op {
        PlanFileOp::StageRender {
            store,
            source_rel,
            staged,
            sha256: _,
        } => {
            if !ctx.config.stores.contains_key(store) {
                return Err(format!("op {idx}: unknown store '{store}'"));
            }
            let source_rel_path = Path::new(source_rel);
            if source_rel_path.is_absolute()
                || has_parent_dir(source_rel_path)
                || !is_safe_fragment(source_rel)
            {
                return Err(format!("op {idx}: invalid source_rel '{source_rel}'"));
            }
            if !source_rel.ends_with(render::TMPL_SUFFIX) {
                return Err(format!(
                    "op {idx}: source_rel '{source_rel}' is not a template"
                ));
            }
            let source_path = ctx.repo_root.join(store).join(source_rel);
            if !source_path.is_file() {
                return Err(format!("op {idx}: source does not exist: {source_rel}"));
            }
            let expected_staged = render::staging_path(
                ctx.repo_root,
                store,
                &render::resolve_entry(source_rel).link_rel,
            );
            if path_to_string(&expected_staged) != *staged {
                return Err(format!(
                    "op {idx}: staged path mismatch: expected {}",
                    expected_staged.display()
                ));
            }
            let encoded = serde_json::to_string(op)
                .map_err(|e| format!("op {idx}: could not encode operation: {e}"))?;
            if !current_removals.stage_writes.contains(&encoded) {
                return Err(format!(
                    "op {idx}: render operation is not present in the freshly computed apply plan"
                ));
            }
            let link_rel = render::resolve_entry(source_rel).link_rel;
            rendered.insert(
                (store.clone(), link_rel),
                RenderPin {
                    source_rel: source_rel.clone(),
                    staged: staged.clone(),
                },
            );
            Ok(())
        }
        PlanFileOp::CreateLink {
            target,
            source,
            requires,
        } => {
            validate_link_op(ctx, idx, target, source, rendered)?;
            validate_fresh_link_write(current_removals, idx, op)?;
            if requires.target != "absent" || requires.value.is_some() {
                return Err(format!("op {idx}: create_link requires target=absent"));
            }
            Ok(())
        }
        PlanFileOp::ReplaceLink {
            target,
            source,
            requires,
        } => {
            validate_link_op(ctx, idx, target, source, rendered)?;
            validate_fresh_link_write(current_removals, idx, op)?;
            if requires.target == "real_entry" {
                let encoded = serde_json::to_string(op)
                    .map_err(|e| format!("op {idx}: could not encode operation: {e}"))?;
                if !current_removals.sensitive_mutations.contains(&encoded) {
                    return Err(format!(
                        "op {idx}: real-entry replacement is not present in the freshly computed apply plan"
                    ));
                }
            }
            Ok(())
        }
        PlanFileOp::BackupAndLink {
            target,
            source,
            backup,
            ..
        } => {
            validate_link_op(ctx, idx, target, source, rendered)?;
            validate_fresh_link_write(current_removals, idx, op)?;
            validate_backup_path(idx, target, backup)?;
            let encoded = serde_json::to_string(op)
                .map_err(|e| format!("op {idx}: could not encode operation: {e}"))?;
            if !current_removals.sensitive_mutations.contains(&encoded) {
                return Err(format!(
                    "op {idx}: backup operation is not present in the freshly computed force plan"
                ));
            }
            Ok(())
        }
        PlanFileOp::RemoveLink {
            store,
            target,
            source,
            ..
        } => {
            validate_remove_link_op(ctx, current_removals, idx, store, target, source.as_deref())?;
            Ok(())
        }
        PlanFileOp::RemoveStaged { store, rel } => {
            if !ctx.config.stores.contains_key(store) {
                return Err(format!("op {idx}: unknown store '{store}'"));
            }
            let rel_path = Path::new(rel);
            if rel_path.is_absolute() || has_parent_dir(rel_path) || !is_safe_fragment(rel) {
                return Err(format!("op {idx}: invalid staged rel '{rel}'"));
            }
            let staged_dir = render::store_render_dir(ctx.repo_root, store);
            let staged_path = staged_dir.join(rel);
            if !staged_path.starts_with(&staged_dir) {
                return Err(format!("op {idx}: staged path escapes render tree"));
            }
            if !current_removals
                .staged
                .contains(&(store.clone(), rel.clone()))
            {
                return Err(format!(
                    "op {idx}: staged render {store}/{rel} is still desired or not stale"
                ));
            }
            Ok(())
        }
    }
}

fn validate_link_op(
    ctx: &ValidationContext,
    idx: usize,
    target: &str,
    source: &str,
    rendered: &BTreeMap<(String, String), RenderPin>,
) -> Result<(), String> {
    let source_path = Path::new(source);
    if has_parent_dir(source_path) {
        return Err(format!("op {idx}: source '{source}' contains '..'"));
    }
    if !source_path.starts_with(ctx.repo_root) {
        return Err(format!("op {idx}: source {source} is not under the repo"));
    }

    // Source must live under repo_root, either in a store or in staging.
    let Some(source_store) = source_store(source, ctx.repo_root) else {
        return Err(format!("op {idx}: source {source} is not under a store"));
    };

    if !ctx.config.stores.contains_key(&source_store) {
        return Err(format!(
            "op {idx}: source store '{source_store}' not in config"
        ));
    }

    // For staged sources, derive the link name and ensure the template exists
    // and is pinned by a preceding StageRender op.
    if let Some(staged_store) = staged_store(source_path) {
        if staged_store != source_store {
            return Err(format!(
                "op {idx}: staged path store '{staged_store}' does not match source store"
            ));
        }
        let staged_dir = render::store_render_dir(ctx.repo_root, &source_store);
        let rel = source_path
            .strip_prefix(&staged_dir)
            .map_err(|_| format!("op {idx}: staged path is not under render dir"))?;
        let link_rel = rel.to_string_lossy().into_owned();
        let resolved = render::resolve_entry(&(link_rel.clone() + render::TMPL_SUFFIX));
        let source_rel = resolved.source_rel;
        let tmpl = ctx.repo_root.join(&source_store).join(&source_rel);
        if !tmpl.is_file() {
            return Err(format!(
                "op {idx}: template source does not exist: {source_rel}"
            ));
        }
        let pin = rendered
            .get(&(source_store.clone(), link_rel.clone()))
            .ok_or_else(|| {
                format!("op {idx}: no pinned stage_render for staged source '{source}'")
            })?;
        if pin.staged != *source {
            return Err(format!(
                "op {idx}: staged source '{source}' does not match pinned stage_render"
            ));
        }
        if pin.source_rel != source_rel {
            return Err(format!(
                "op {idx}: staged source template mismatch: expected {source_rel}"
            ));
        }
    } else {
        // Plain source under store directory.
        let rel = source_path
            .strip_prefix(ctx.repo_root.join(&source_store))
            .map_err(|_| format!("op {idx}: source is not under store '{source_store}'"))?;
        let rel_str = rel.to_string_lossy().into_owned();
        if rel_str.is_empty() {
            // Whole-directory link: the source must be the store directory itself
            // and the target must be a configured whole-dir target.
            let store_dir = ctx.repo_root.join(&source_store);
            if source_path != store_dir {
                return Err(format!(
                    "op {idx}: whole-dir source must be the store directory"
                ));
            }
            if !source_path.is_dir() {
                return Err(format!("op {idx}: store directory does not exist"));
            }
        } else {
            if !is_safe_fragment(&rel_str) {
                return Err(format!("op {idx}: invalid source fragment '{rel_str}'"));
            }
            if rel_str.ends_with(render::TMPL_SUFFIX) {
                return Err(format!("op {idx}: template source must use staged path"));
            }
            let source = ctx.repo_root.join(&source_store).join(&rel_str);
            if !std::fs::symlink_metadata(&source)
                .map(|m| !m.file_type().is_dir())
                .unwrap_or(false)
            {
                return Err(format!("op {idx}: source file does not exist: {rel_str}"));
            }
        }
    }

    // Target must fall under a configured target path for this store.
    let target_path = Path::new(target);
    if has_parent_dir(target_path) {
        return Err(format!("op {idx}: target '{target}' contains '..'"));
    }
    if !is_under_any_target(ctx.config, &source_store, target_path)? {
        return Err(format!(
            "op {idx}: target {target} is not under a configured target for store '{source_store}'"
        ));
    }

    // Authorize the exact target/source relationship against resolved config.
    let store = ctx.config.stores.get(&source_store).unwrap();
    let store_dir = ctx.repo_root.join(&source_store);
    let expected = store::resolve_link_source(
        ctx.repo_root,
        &store_dir,
        Some(store),
        &source_store,
        target_path,
    )
    .ok_or_else(|| {
        format!("op {idx}: target {target} does not resolve to a configured source in store '{source_store}'")
    })?;
    if expected != *source {
        return Err(format!(
            "op {idx}: source '{source}' is not the expected source for target {target} (expected {expected})"
        ));
    }

    Ok(())
}

fn validate_remove_link_op(
    ctx: &ValidationContext,
    current_removals: &CurrentRemovals,
    idx: usize,
    store: &str,
    target: &str,
    source: Option<&str>,
) -> Result<(), String> {
    let target_path = Path::new(target);
    if has_parent_dir(target_path) {
        return Err(format!("op {idx}: target '{target}' contains '..'"));
    }
    if !ctx.config.stores.contains_key(store) {
        return Err(format!("op {idx}: unknown store '{store}'"));
    }
    if !is_under_any_target(ctx.config, store, target_path)? {
        return Err(format!(
            "op {idx}: target {target} is not under a configured target for store '{store}'"
        ));
    }

    if let Some(src) = source {
        let src_path = Path::new(src);
        if has_parent_dir(src_path) {
            return Err(format!("op {idx}: source '{src}' contains '..'"));
        }
        if !src_path.starts_with(ctx.repo_root) {
            return Err(format!("op {idx}: source {src} is not under the repo"));
        }
        let source_store = source_store(src, ctx.repo_root)
            .ok_or_else(|| format!("op {idx}: cannot derive store from source '{src}'"))?;
        if source_store != store {
            return Err(format!(
                "op {idx}: source belongs to store '{source_store}', not '{store}'"
            ));
        }
    }

    let removal = (
        store.to_owned(),
        target.to_owned(),
        source.map(str::to_owned),
    );
    if !current_removals.links.contains(&removal) {
        return Err(format!(
            "op {idx}: remove_link for {target} is still desired or not a current stale cleanup"
        ));
    }
    Ok(())
}

fn validate_backup_path(idx: usize, target: &str, backup: &str) -> Result<(), String> {
    let target_path = Path::new(target);
    let backup_path = Path::new(backup);
    if has_parent_dir(backup_path) {
        return Err(format!("op {idx}: backup path '{backup}' contains '..'"));
    }
    if target_path == backup_path {
        return Err(format!(
            "op {idx}: backup path '{backup}' must differ from target"
        ));
    }
    let Some(target_parent) = target_path.parent() else {
        return Err(format!(
            "op {idx}: target '{target}' has no parent directory"
        ));
    };
    let Some(backup_parent) = backup_path.parent() else {
        return Err(format!(
            "op {idx}: backup path '{backup}' has no parent directory"
        ));
    };
    if target_parent != backup_parent {
        return Err(format!(
            "op {idx}: backup path '{backup}' is not under the same directory as target '{target}'"
        ));
    }
    let mut expected = target_path.as_os_str().to_os_string();
    expected.push(".bak");
    if backup_path.as_os_str() != expected {
        return Err(format!(
            "op {idx}: backup path must be exactly '{}.bak'",
            target
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_must_be_exact_target_suffix() {
        assert!(validate_backup_path(0, "/home/user/.bashrc", "/home/user/.bashrc.bak").is_ok());
        let error =
            validate_backup_path(0, "/home/user/.bashrc", "/home/user/other.bak").unwrap_err();
        assert!(error.contains("must be exactly"), "got: {error}");
    }
}
