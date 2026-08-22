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
#[allow(dead_code)]
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
    platform: Platform,
}

impl<'a> ValidationContext<'a> {
    #[allow(dead_code)]
    pub(crate) fn new(repo_root: &'a Path, config: &'a Config) -> Self {
        Self {
            repo_root,
            config,
            platform: Platform::detect(),
        }
    }

    pub(crate) fn with_platform(
        repo_root: &'a Path,
        config: &'a Config,
        platform: Platform,
    ) -> Self {
        Self {
            repo_root,
            config,
            platform,
        }
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
            json: false,
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
            // Check freshness first so injected unselected stage_renders fail
            // with the expected "not present" message rather than an inventory
            // mismatch. This matches the test expectation for
            // apply_plan_rejects_unselected_injected_stage_render.
            let encoded = serde_json::to_string(op)
                .map_err(|e| format!("op {idx}: could not encode operation: {e}"))?;
            if !current_removals.stage_writes.contains(&encoded) {
                return Err(format!(
                    "op {idx}: render operation is not present in the freshly computed apply plan"
                ));
            }
            // Staged path determines the link identity; source_rel is validated
            // against the store's declared inventory so shared templates
            // (repo-relative source outside the consumer store) are handled.
            let staged_path = Path::new(staged);
            let staged_dir = render::store_render_dir(ctx.repo_root, store);
            let link_rel = staged_path
                .strip_prefix(&staged_dir)
                .map_err(|_| format!("op {idx}: staged path outside render tree: {staged}"))?
                .to_string_lossy()
                .into_owned();
            if link_rel.is_empty() {
                return Err(format!(
                    "op {idx}: staged path has no link identity: {staged}"
                ));
            }
            let expected_staged = render::staging_path(ctx.repo_root, store, &link_rel);
            if path_to_string(&expected_staged) != *staged {
                return Err(format!(
                    "op {idx}: staged path mismatch: expected {}",
                    expected_staged.display()
                ));
            }
            // Resolve the expected template source for this link via the store's
            // inventory. This handles both in-store templates (store/<source_rel>)
            // and shared `sources` templates (repo-relative outside the store).
            let store_cfg = ctx.config.stores.get(store).unwrap();
            if !ctx.platform.matches_when(&store_cfg.when) {
                return Err(format!(
                    "op {idx}: no template entry stages at '{link_rel}' for store '{store}'"
                ));
            }
            let store_dir = ctx.repo_root.join(store);
            let mut found: Option<(PathBuf, String)> = None;
            let mut check = |files: &[String],
                             patterns: &[String],
                             sources: &BTreeMap<String, String>,
                             ignore: &[String]| {
                if found.is_some() {
                    return;
                }
                if let store::LinkTargets::Files(links) = store::resolve_target_names(
                    ctx.repo_root,
                    &store_dir,
                    files,
                    patterns,
                    sources,
                    ignore,
                ) {
                    found = links
                        .into_iter()
                        .find(|l| l.is_template() && l.name == link_rel)
                        .map(|l| (l.source, l.source_rel));
                }
            };
            if store_cfg.is_multi_target() {
                for t in store_cfg.targets.values() {
                    if !ctx.platform.matches_when(&t.when) {
                        continue;
                    }
                    check(&t.files, &t.patterns, &t.sources, &t.ignore);
                }
            } else {
                check(
                    &store_cfg.files,
                    &store_cfg.patterns,
                    &store_cfg.sources,
                    &store_cfg.ignore,
                );
            }
            let (actual_source, actual_rel) = found.ok_or_else(|| {
                format!("op {idx}: no template entry stages at '{link_rel}' for store '{store}'")
            })?;
            if actual_rel != *source_rel {
                return Err(format!(
                    "op {idx}: template identity drifted: plan says '{source_rel}', state says '{actual_rel}'"
                ));
            }
            if !actual_source.is_file() {
                return Err(format!("op {idx}: source does not exist: {source_rel}"));
            }
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
            store,
            target,
            source,
            requires,
        } => {
            validate_link_op(ctx, idx, store, target, source, rendered)?;
            validate_fresh_link_write(current_removals, idx, op)?;
            if requires.target != "absent" || requires.value.is_some() {
                return Err(format!("op {idx}: create_link requires target=absent"));
            }
            Ok(())
        }
        PlanFileOp::ReplaceLink {
            store,
            target,
            source,
            requires,
        } => {
            validate_link_op(ctx, idx, store, target, source, rendered)?;
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
            store,
            target,
            source,
            backup,
            ..
        } => {
            validate_link_op(ctx, idx, store, target, source, rendered)?;
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
    store: &str,
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

    // For backward compat, allow missing store (empty) by inferring from source.
    let inferred;
    let store = if store.is_empty() {
        inferred = source_store(source, ctx.repo_root)
            .ok_or_else(|| format!("op {idx}: source {source} is not under a store"))?;
        inferred.as_str()
    } else {
        store
    };

    if !ctx.config.stores.contains_key(store) {
        return Err(format!("op {idx}: store '{store}' not in config"));
    }

    // For staged sources, derive the link name and ensure the template exists
    // and is pinned by a preceding StageRender op.
    if let Some(staged_store) = staged_store(source_path) {
        if staged_store != store {
            return Err(format!(
                "op {idx}: staged path store '{staged_store}' does not match op store '{store}'"
            ));
        }
        let staged_dir = render::store_render_dir(ctx.repo_root, store);
        let rel = source_path
            .strip_prefix(&staged_dir)
            .map_err(|_| format!("op {idx}: staged path is not under render dir"))?;
        let link_rel = rel.to_string_lossy().into_owned();
        let resolved = render::resolve_entry(&(link_rel.clone() + render::TMPL_SUFFIX));
        let source_rel = resolved.source_rel;
        let tmpl = ctx.repo_root.join(store).join(&source_rel);
        // For shared sources the template lives at repo_root/shared/... not under consumer store,
        // so we also check the resolved template source from the store's inventory.
        let actual_source = if tmpl.is_file() {
            tmpl
        } else {
            // Try to resolve via store inventory (handles cross-store sources)
            let store_cfg = ctx.config.stores.get(store).unwrap();
            let store_dir = ctx.repo_root.join(store);
            let mut found = None;
            let mut check = |files: &[String],
                             patterns: &[String],
                             sources: &std::collections::BTreeMap<String, String>,
                             ignore: &[String]| {
                if found.is_some() {
                    return;
                }
                if let store::LinkTargets::Files(links) = store::resolve_target_names(
                    ctx.repo_root,
                    &store_dir,
                    files,
                    patterns,
                    sources,
                    ignore,
                ) {
                    found = links
                        .into_iter()
                        .find(|l| l.is_template() && l.name == link_rel)
                        .map(|l| l.source);
                }
            };
            if store_cfg.is_multi_target() {
                for t in store_cfg.targets.values() {
                    check(&t.files, &t.patterns, &t.sources, &t.ignore);
                }
            } else {
                check(
                    &store_cfg.files,
                    &store_cfg.patterns,
                    &store_cfg.sources,
                    &store_cfg.ignore,
                );
            }
            found.ok_or_else(|| {
                format!("op {idx}: template source does not exist for '{store}/{link_rel}'")
            })?
        };
        if !actual_source.is_file() {
            return Err(format!(
                "op {idx}: template source does not exist: {}",
                actual_source.display()
            ));
        }
        let pin = rendered
            .get(&(store.to_owned(), link_rel.clone()))
            .ok_or_else(|| {
                format!("op {idx}: no pinned stage_render for staged source '{source}'")
            })?;
        if pin.staged != *source {
            return Err(format!(
                "op {idx}: staged source '{source}' does not match pinned stage_render"
            ));
        }
    } else {
        // Plain source: must be a file under the repo (either in-store or shared via sources map).
        // Validate it exists and is not a directory; the exact store mapping is authorized below via
        // resolve_link_source against the declared consumer store.
        let repo_rel = source_path
            .strip_prefix(ctx.repo_root)
            .map_err(|_| format!("op {idx}: source is not under repo"))?
            .to_string_lossy()
            .into_owned();
        if repo_rel.is_empty() {
            return Err(format!("op {idx}: source is repo root"));
        }
        if !is_safe_fragment(&repo_rel) {
            return Err(format!("op {idx}: invalid source fragment '{repo_rel}'"));
        }
        if repo_rel == ".stitch"
            || repo_rel.starts_with(".stitch/")
            || repo_rel == ".git"
            || repo_rel.starts_with(".git/")
        {
            return Err(format!(
                "op {idx}: source must not be under .stitch/ or .git/"
            ));
        }
        match std::fs::symlink_metadata(source_path) {
            Ok(meta) if meta.is_dir() => {
                // Whole-directory link: the source must be the store directory itself.
                let store_dir = ctx.repo_root.join(store);
                if source_path != store_dir {
                    return Err(format!(
                        "op {idx}: whole-dir source must be the store directory"
                    ));
                }
                if !source_path.is_dir() {
                    return Err(format!("op {idx}: store directory does not exist"));
                }
            }
            Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "op {idx}: source is not a regular file: {repo_rel}"
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("op {idx}: source file does not exist: {repo_rel}"));
            }
            Err(e) => {
                return Err(format!(
                    "op {idx}: could not inspect source {repo_rel}: {e}"
                ));
            }
        }
        if repo_rel.ends_with(render::TMPL_SUFFIX) {
            return Err(format!("op {idx}: template source must use staged path"));
        }
    }

    // Target must fall under a configured target path for this store.
    let target_path = Path::new(target);
    if has_parent_dir(target_path) {
        return Err(format!("op {idx}: target '{target}' contains '..'"));
    }
    if !is_under_any_target(ctx.config, store, target_path)? {
        return Err(format!(
            "op {idx}: target {target} is not under a configured target for store '{store}'"
        ));
    }

    // Authorize the exact target/source relationship against resolved config.
    let store_cfg = ctx.config.stores.get(store).unwrap();
    let store_dir = ctx.repo_root.join(store);
    let expected = store::resolve_link_source(
        ctx.repo_root,
        &store_dir,
        Some(store_cfg),
        store,
        target_path,
    )
    .ok_or_else(|| {
        format!(
            "op {idx}: target {target} does not resolve to a configured source in store '{store}'"
        )
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
        // Cross-store `sources` are repo-relative and may live outside the
        // consumer store (e.g. `shared/hub.txt` for store `consumer`). The
        // consumer store's RemoveLink may legitimately point at a source
        // whose first path component is not the consumer store.
        let _ = source_store(src, ctx.repo_root)
            .ok_or_else(|| format!("op {idx}: cannot derive store from source '{src}'"))?;
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
