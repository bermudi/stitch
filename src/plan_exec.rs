//! Plan executor for `stitch apply --plan`.
//!
//! Owns execution (`execute_plan`), per-op preflight (`preflight_op`), the
//! simulated-state preflight (`PreflightState`), per-store hooks, and the
//! filesystem mutation helpers (`execute_op`, `create_link_for_plan`,
//! `replace_link_real_entry`, `remove_link_for_store`). Plan file format and
//! construction live in `plan_file`; untrusted-input validation lives in
//! `plan_validate`. Both are one-directional dependencies from this module.
//!
//! The public type surface (`PlanFile`/`PlanFileOp`/`PlanExecError`/
//! `build_plan_file`) is re-exported from `plan_file` so existing
//! `plan_exec::` import paths are unaffected by the split.

use crate::ancestor::{TargetAncestorRedirect, TargetAncestorSnapshot, has_parent_dir};
use crate::config::{self, Config, Loaded, is_safe_fragment};
use crate::error::{FailureClass, StitchError};
use crate::fsutil::{directory_identity, require_directory_identity};
use crate::hooks::{self, HookEnv};
use crate::linker::{self, LinkError};
use crate::plan::{TargetState, path_to_string};
use crate::plan_file::{
    self, PLAN_KIND, PLAN_SCHEMA, PlanConflict, PlanExecReport, PlanFileRequires, base_report,
    check_source_exists_for_preflight, compute_config_hash, op_description, plan_link_targets,
    plan_source_root, sync_ops_remaining, target_state_from, target_state_id, verify_stage_render,
};
use crate::plan_validate::{
    RenderPin, ValidationContext, current_removals, validate_cleanup_dependencies, validate_op,
};
use crate::platform::Platform;
use crate::render;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use plan_file::{PlanExecError, PlanFile, PlanFileOp, build_plan_file};

fn redirect_to_plan_message(repo_root: &Path, redirect: &TargetAncestorRedirect) -> String {
    match redirect {
        TargetAncestorRedirect::Symlinked { path, .. } => symlink_ancestor_error(repo_root, path),
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: Some(_),
        } => symlink_ancestor_error(repo_root, path),
        TargetAncestorRedirect::Removed { path } => {
            format!("target ancestor {} was removed by the hook", path.display())
        }
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: None,
        } => {
            format!(
                "target ancestor {} changed identity during the hook",
                path.display()
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Preflight state simulation
// ---------------------------------------------------------------------------

/// Tracks the predicted filesystem state across a plan's ops so that later
/// ops are preflighted against the *simulated* result of earlier ones.
struct PreflightState<'a> {
    repo_root: &'a Path,
    platform: &'a Platform,
    overrides: BTreeMap<PathBuf, TargetState>,
    /// Missing ancestors that an earlier simulated create would make into
    /// directories. `TargetState::RealEntry` alone is not enough: it can also
    /// mean a file moved to a backup path.
    simulated_dirs: BTreeSet<PathBuf>,
}

impl<'a> PreflightState<'a> {
    fn new(repo_root: &'a Path, platform: &'a Platform) -> Self {
        Self {
            repo_root,
            platform,
            overrides: BTreeMap::new(),
            simulated_dirs: BTreeSet::new(),
        }
    }

    fn actual_target_state(&self, path: &Path) -> TargetState {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(path) {
                Ok(resolved) => TargetState::SymlinkTo(path_to_string(&resolved)),
                Err(_) => TargetState::SymlinkIntoRepo,
            },
            Ok(_) => TargetState::RealEntry,
            Err(_) => TargetState::Absent,
        }
    }

    fn get_effective_state(&self, path: &Path) -> TargetState {
        if let Some(state) = self.overrides.get(path) {
            return state.clone();
        }
        for ancestor in path.ancestors().skip(1) {
            if self.overrides.contains_key(ancestor) {
                // Any path inside an overridden directory is determined by that
                // ancestor: removed dirs are absent, created dirs have no children
                // until an op creates them, and symlinks cannot have children.
                return TargetState::Absent;
            }
        }
        self.actual_target_state(path)
    }

    fn parent_is_writable_dir(&self, path: &Path) -> Result<(), String> {
        // `create_dir_all` follows any symlink in this chain. A captured plan
        // must not authorize that traversal, including through a link another
        // operation in the same plan would create.
        for ancestor in path.ancestors().skip(1) {
            // A verified earlier remove can intentionally clear a whole-dir
            // target before child links are created during mode promotion.
            if self.overrides.get(ancestor) == Some(&TargetState::Absent)
                || (self.overrides.get(ancestor) == Some(&TargetState::RealEntry)
                    && self.simulated_dirs.contains(ancestor))
            {
                continue;
            }
            // Otherwise the simulator must never hide a symlink that exists
            // on disk before hooks run.
            check_physical_ancestor(self.repo_root, ancestor)?;
            if let Some(state) = self.overrides.get(ancestor) {
                match state {
                    TargetState::Absent => continue,
                    TargetState::RealEntry if self.simulated_dirs.contains(ancestor) => continue,
                    TargetState::RealEntry => {
                        return Err(format!(
                            "parent {} is not a simulated directory",
                            ancestor.display()
                        ));
                    }
                    TargetState::SymlinkTo(_) | TargetState::SymlinkIntoRepo => {
                        return Err(symlink_ancestor_error(self.repo_root, ancestor));
                    }
                }
            }
        }
        Ok(())
    }

    fn state_matches(
        &self,
        path: &Path,
        expected: &TargetState,
        actual: &TargetState,
    ) -> Result<(), String> {
        match (expected, actual) {
            (TargetState::Absent, TargetState::Absent)
            | (TargetState::RealEntry, TargetState::RealEntry) => Ok(()),
            (TargetState::SymlinkTo(exp), TargetState::SymlinkTo(act)) => {
                if act == exp {
                    Ok(())
                } else {
                    Err(format!(
                        "{} points to {act} (expected {exp})",
                        path.display()
                    ))
                }
            }
            (TargetState::SymlinkTo(_), TargetState::SymlinkIntoRepo) => Err(format!(
                "{} is a repo-owned symlink but its target cannot be read",
                path.display()
            )),
            (TargetState::SymlinkIntoRepo, TargetState::SymlinkIntoRepo) => Ok(()),
            (TargetState::SymlinkIntoRepo, TargetState::SymlinkTo(_)) => {
                if linker::points_into_repo(path, self.repo_root) {
                    Ok(())
                } else {
                    Err(format!("{} does not point into repo", path.display()))
                }
            }
            _ => Err(format!(
                "{} state {:?} does not match expected {:?}",
                path.display(),
                target_state_id(actual),
                target_state_id(expected)
            )),
        }
    }

    fn set_ancestors_to_real(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.set_ancestors_to_real(parent);
            if self.get_effective_state(parent) == TargetState::Absent {
                self.overrides
                    .insert(parent.to_path_buf(), TargetState::RealEntry);
                self.simulated_dirs.insert(parent.to_path_buf());
            }
        }
    }

    fn apply_op(&mut self, loaded: &Loaded, op: &PlanFileOp) -> Result<(), String> {
        match op {
            PlanFileOp::StageRender {
                store,
                source_rel,
                staged,
                sha256,
            } => {
                let _ = verify_stage_render(
                    self.repo_root,
                    loaded,
                    self.platform,
                    store,
                    source_rel,
                    staged,
                    sha256,
                )?;
                render::preflight_staged_path(
                    self.repo_root,
                    store,
                    &staged_link_identity(self.repo_root, store, staged)?,
                )?;
                Ok(())
            }
            PlanFileOp::CreateLink {
                target,
                source,
                requires,
            } => {
                self.apply_link_op(Path::new(target), source, requires, false)?;
                self.set_ancestors_to_real(Path::new(target));
                self.overrides.insert(
                    Path::new(target).to_path_buf(),
                    TargetState::SymlinkTo(source.clone()),
                );
                Ok(())
            }
            PlanFileOp::ReplaceLink {
                target,
                source,
                requires,
            } => {
                self.apply_link_op(Path::new(target), source, requires, false)?;
                self.overrides.insert(
                    Path::new(target).to_path_buf(),
                    TargetState::SymlinkTo(source.clone()),
                );
                Ok(())
            }
            PlanFileOp::BackupAndLink {
                target,
                source,
                backup,
                requires,
            } => {
                self.apply_link_op(Path::new(target), source, requires, true)?;
                let backup_state = if let Some(backup_req) = &requires.backup {
                    target_state_from(backup_req, &requires.backup_value)
                        .map_err(|e| format!("invalid backup requires: {e}"))?
                } else {
                    TargetState::Absent
                };
                if !matches!(backup_state, TargetState::Absent) {
                    return Err("backup_and_link requires backup=absent".into());
                }
                self.state_matches(
                    Path::new(backup),
                    &backup_state,
                    &self.get_effective_state(Path::new(backup)),
                )?;
                self.overrides.insert(
                    Path::new(target).to_path_buf(),
                    TargetState::SymlinkTo(source.clone()),
                );
                self.overrides
                    .insert(Path::new(backup).to_path_buf(), TargetState::RealEntry);
                Ok(())
            }
            PlanFileOp::RemoveLink {
                store,
                target,
                source,
                requires,
            } => {
                let target_path = Path::new(target);
                let target_state = target_state_from(&requires.target, &requires.value)
                    .map_err(|e| format!("invalid requires: {e}"))?;
                self.parent_is_writable_dir(target_path)?;
                self.state_matches(
                    target_path,
                    &target_state,
                    &self.get_effective_state(target_path),
                )?;
                // This is an ownership check, not merely path validation. Do
                // it during whole-plan preflight so no hook runs for a plan
                // that cannot safely remove its claimed link.
                check_remove_link_ownership(self.repo_root, store, target_path, source.as_deref())?;

                self.overrides
                    .insert(target_path.to_path_buf(), TargetState::Absent);
                Ok(())
            }
            PlanFileOp::RemoveStaged { store, rel } => {
                // A stale render may already be gone; missing is not a failure.
                render::preflight_staged_path(self.repo_root, store, rel)
            }
        }
    }

    fn apply_link_op(
        &mut self,
        target_path: &Path,
        source: &str,
        requires: &PlanFileRequires,
        has_backup: bool,
    ) -> Result<(), String> {
        self.parent_is_writable_dir(target_path)?;
        check_source_exists_for_preflight(self.repo_root, source)?;
        let target_state = target_state_from(&requires.target, &requires.value)
            .map_err(|e| format!("invalid requires: {e}"))?;
        self.state_matches(
            target_path,
            &target_state,
            &self.get_effective_state(target_path),
        )?;
        if has_backup && !matches!(target_state, TargetState::RealEntry) {
            return Err("backup_and_link requires target=real_entry".into());
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PAUSE_AFTER_GLOBAL_HASH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only seam: deterministically simulate a config change between the
/// global hash check and the per-store pre-hook. This is a thread-local
/// callback so unit tests can reproduce the TOCTOU without a flaky race.
#[cfg(test)]
pub fn set_test_pause_after_global_hash(f: Option<Box<dyn FnOnce()>>) {
    TEST_PAUSE_AFTER_GLOBAL_HASH.with(|p| *p.borrow_mut() = f);
}

#[cfg(test)]
fn test_pause_after_global_hash() {
    TEST_PAUSE_AFTER_GLOBAL_HASH.with(|p| {
        if let Some(f) = p.borrow_mut().take() {
            f();
        }
    });
}

/// Execute a plan file. With `dry_run: true` this is a preflight: every
/// precondition and fingerprint is validated and no filesystem mutation occurs.
///
/// The config snapshot that drives every mutation is reloaded under the
/// per-store state lock and bound to the pinned config hash. The `loaded`
/// argument is kept for caller convenience but is not used for execution; it
/// may be stale by the time the lock is acquired.
pub fn execute_plan(
    repo_root: &Path,
    _loaded: &Loaded,
    plan: &PlanFile,
    dry_run: bool,
    force: bool,
    json: bool,
) -> Result<PlanExecReport, PlanExecError> {
    if plan.schema != PLAN_SCHEMA {
        return Err(PlanExecError::new(
            base_report(plan),
            StitchError::plan_stale(format!(
                "unsupported plan schema: {} (expected {})",
                plan.schema, PLAN_SCHEMA
            )),
        ));
    }
    if plan.kind != PLAN_KIND {
        return Err(PlanExecError::new(
            base_report(plan),
            StitchError::plan_stale(format!("unsupported plan kind: {}", plan.kind)),
        ));
    }

    let actual_repo = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let planned_repo = Path::new(&plan.repo)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&plan.repo));
    if planned_repo != actual_repo {
        return Err(PlanExecError::new(
            base_report(plan),
            StitchError::plan_stale("repository mismatch — re-run `stitch plan`"),
        ));
    }

    let platform = Platform::detect();
    if !plan.platform.matches(&platform) {
        return Err(PlanExecError::new(
            base_report(plan),
            StitchError::plan_stale("platform fingerprint mismatch — re-run `stitch plan`"),
        ));
    }

    let actual_hash =
        compute_config_hash(repo_root).map_err(|e| PlanExecError::new(base_report(plan), e))?;
    if actual_hash != plan.config_sha256 {
        return Err(PlanExecError::new(
            base_report(plan),
            StitchError::plan_stale("config hash mismatch — re-run `stitch plan`"),
        ));
    }

    if plan
        .ops
        .iter()
        .any(|op| matches!(op, PlanFileOp::BackupAndLink { .. }))
        && !force
    {
        return Err(PlanExecError::new(
            base_report(plan),
            StitchError::plan_stale(
                "plan contains backup_and_link operations; re-run with `apply --plan … --force`",
            ),
        ));
    }

    let mut report = base_report(plan);
    let mut remaining: BTreeSet<usize> = (0..plan.ops.len()).collect();

    // Load the authoritative snapshot under the state lock, verify its hash,
    // and perform all untrusted-input validation against it. The snapshot
    // authorizes link removals and staged-render cleanups, so validation must
    // run before the plan is grouped or executed.
    let initial_state_lock = match config::StateLock::exclusive_if_present(repo_root) {
        Ok(lock) => lock,
        Err(e) => {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, StitchError::from(e)));
        }
    };
    let initial_loaded = Config::load(repo_root)
        .map_err(|e| PlanExecError::new(report.clone(), StitchError::from(e)))?;
    let initial_hash =
        compute_config_hash(repo_root).map_err(|e| PlanExecError::new(report.clone(), e))?;
    if initial_hash != plan.config_sha256 {
        return Err(PlanExecError::new(
            report,
            StitchError::plan_stale("config hash mismatch — re-run `stitch plan`"),
        ));
    }

    let validation_context = ValidationContext::new(repo_root, &initial_loaded.config);
    let current_removals =
        current_removals(repo_root, &initial_loaded, &platform, force).map_err(|e| {
            PlanExecError::new(
                report.clone(),
                StitchError::plan_stale(format!("could not resolve current plan: {e}")),
            )
        })?;
    let mut rendered: BTreeMap<(String, String), RenderPin> = BTreeMap::new();
    for (idx, op) in plan.ops.iter().enumerate() {
        validate_op(
            &validation_context,
            &current_removals,
            idx,
            op,
            &mut rendered,
        )
        .map_err(|e| {
            PlanExecError::new(
                report.clone(),
                StitchError::plan_stale(format!("plan validation failed: {e}")),
            )
        })?;
    }

    // Group ops by store, preserving each store's plan order while retaining
    // the original operation indices for accurate remainder reporting. By now
    // every op has passed structural validation, so grouping only fails when
    // a hand-edited store list is inconsistent.
    let mut ops_by_store: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, op) in plan.ops.iter().enumerate() {
        let Some(op_store) = op.op_store(repo_root) else {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!("op {idx}: cannot derive store for execution")),
            ));
        };
        ops_by_store.entry(op_store).or_default().push(idx);
    }

    let selected_stores = plan.stores.clone();
    let selected_set: BTreeSet<String> = selected_stores.iter().cloned().collect();
    let op_store_set: BTreeSet<String> = ops_by_store.keys().cloned().collect();
    if selected_set.len() != selected_stores.len() || selected_set != op_store_set {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::plan_stale(
                "plan store selection does not exactly match its executable operations",
            ),
        ));
    }

    // A captured plan must not silently drop an operation because its store
    // was omitted, or because the store is no longer active on this platform.
    // Keep those operations in `remaining` and reject the plan before hooks or
    // mutations, so a successful result can never imply that they ran.
    let mut skipped_ops = Vec::new();
    for (store_name, indices) in &ops_by_store {
        if !selected_set.contains(store_name) {
            skipped_ops.extend(indices.iter().map(|&idx| {
                format!(
                    "{} (store '{store_name}' omitted from selected stores)",
                    op_description(&plan.ops[idx])
                )
            }));
        }
    }
    // Defense-in-depth: `validate_op` already rejects any op for a
    // platform-skipped store (`compute_plan` emits no such ops, so
    // `current_removals` will not contain it). This scan is a second line of
    // defense; if `validate_op` is ever relaxed, it preserves the "abort before
    // side effects" guarantee for platform-skipped stores.
    for store_name in &selected_stores {
        if let Some(store) = initial_loaded.config.stores.get(store_name)
            && !platform.matches_when(&store.when)
            && let Some(indices) = ops_by_store.get(store_name)
        {
            skipped_ops.extend(indices.iter().map(|&idx| {
                format!(
                    "{} (store '{store_name}' skipped by platform conditions)",
                    op_description(&plan.ops[idx])
                )
            }));
        }
    }
    if !skipped_ops.is_empty() {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::plan_stale(format!(
                "plan contains operations that cannot execute: {}",
                skipped_ops.join("; ")
            )),
        ));
    }

    // Flatten the store groups into the exact order the executor uses below.
    let mut exec_order: Vec<usize> = Vec::with_capacity(plan.ops.len());
    for store_name in &selected_stores {
        if let Some(indices) = ops_by_store.get(store_name) {
            exec_order.extend(indices);
        }
    }

    // Staged output must remain readable until every live stale link that
    // depends on it has been removed. Edited plans may omit unrelated work,
    // but they cannot omit or reorder this safety-critical dependency.
    validate_cleanup_dependencies(plan, &current_removals, &exec_order).map_err(|e| {
        PlanExecError::new(
            report.clone(),
            StitchError::plan_stale(format!("plan validation failed: {e}")),
        )
    })?;

    // Preflight the execution sequence against the same locked-and-verified
    // snapshot so cross-store ordering and path interactions are checked before
    // any filesystem mutation.
    let mut state = PreflightState::new(repo_root, &platform);
    for &idx in &exec_order {
        let op = &plan.ops[idx];
        state.apply_op(&initial_loaded, op).map_err(|e| {
            PlanExecError::new(
                report.clone(),
                StitchError::plan_stale(format!("preflight failed for op {idx}: {e}")),
            )
        })?;
    }

    // A plan that captured conflicts or errors is not executable. Reject it
    // before any hook can create side effects or a safe-looking prefix can
    // mutate the filesystem.
    if !plan.conflicts.is_empty() || !plan.errors.is_empty() {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(report, plan_exec_error(plan)));
    }
    if dry_run {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Ok(report);
    }

    drop(initial_state_lock);

    // Pin every target ancestor's identity across the global pre-apply hook.
    // A hook that replaces `~/.config` with a symlink (or a different real
    // directory) must be caught before any store runs.
    let home =
        config::expand_home("~").map_err(|e| PlanExecError::new(report.clone(), e.into()))?;
    let (global_targets, global_removed) = plan_link_targets(&plan.ops);
    let global_ancestors =
        TargetAncestorSnapshot::capture(repo_root, global_targets, &global_removed, &home)
            .map_err(|e| {
                PlanExecError::new(
                    report.clone(),
                    StitchError::plan_stale(redirect_to_plan_message(repo_root, &e)),
                )
            })?;

    // Pin $HOME identity (including the resolved directory behind a symlinked
    // $HOME) across the global pre-apply hook.
    let home_identity = crate::safety::HomeIdentity::capture()
        .map_err(|e| PlanExecError::new(report.clone(), StitchError::plan_stale(e.to_string())))?;

    // Global pre-apply hook (side effect, only on real execution). Pin the
    // repository identity across it so a hook cannot redirect every source by
    // replacing the repository root.
    let repo_identity = directory_identity(repo_root)
        .map_err(|e| PlanExecError::new(base_report(plan), StitchError::plan_stale(e)))?;
    let env = HookEnv {
        root: repo_root,
        store: None,
        target: None,
        action: "apply",
    };
    if let Err(e) = hooks::run_global_hook(repo_root, "pre-apply", &env, &platform, json) {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::hook("pre-apply", e),
        ));
    }
    if let Err(e) = global_ancestors.revalidate() {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::plan_stale(redirect_to_plan_message(repo_root, &e)),
        ));
    }
    if let Err(e) = home_identity.revalidate() {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::plan_stale(e.to_string()),
        ));
    }
    if let Err(e) = require_directory_identity(
        repo_root,
        repo_identity,
        "repository changed during pre-apply hook",
    ) {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(report, StitchError::plan_stale(e)));
    }
    let post_hook_hash =
        compute_config_hash(repo_root).map_err(|e| PlanExecError::new(report.clone(), e))?;
    if post_hook_hash != plan.config_sha256 {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::plan_stale("config changed during pre-apply hook"),
        ));
    }

    // Test-only hook: simulate a concurrent same-UID config change in the
    // window between the global hash check and the per-store Config::load.
    #[cfg(test)]
    test_pause_after_global_hash();

    for store_name in &selected_stores {
        if let Err(e) = require_directory_identity(
            repo_root,
            repo_identity,
            "repository changed before store execution",
        ) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, StitchError::plan_stale(e)));
        }

        // Load the snapshot as it exists immediately before the store's pre-hook.
        // The pre-hook may mutate state, so this is advisory; the authoritative
        // snapshot for execution is reloaded under the state lock below. Before
        // using this snapshot to resolve the pre-hook, verify it matches the
        // plan's pinned hash so a concurrent change cannot install a different
        // hook between the global hash check and this store's pre-hook.
        let pre_hook_loaded = Config::load(repo_root).map_err(|e| {
            sync_ops_remaining(&mut report, plan, &remaining);
            PlanExecError::new(report.clone(), StitchError::from(e))
        })?;
        let pre_hook_hash =
            compute_config_hash(repo_root).map_err(|e| PlanExecError::new(report.clone(), e))?;
        if pre_hook_hash != plan.config_sha256 {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!(
                    "config changed before pre-hook for store '{store_name}'"
                )),
            ));
        }

        let store_dir = repo_root.join(store_name);
        if !std::fs::symlink_metadata(&store_dir)
            .is_ok_and(|meta| meta.file_type().is_dir() && !meta.file_type().is_symlink())
        {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!(
                    "store directory {} is missing, symlinked, or not a directory",
                    store_dir.display()
                )),
            ));
        }
        let store_identity = match directory_identity(&store_dir) {
            Ok(identity) => identity,
            Err(e) => {
                sync_ops_remaining(&mut report, plan, &remaining);
                return Err(PlanExecError::new(report, StitchError::plan_stale(e)));
            }
        };

        // Pin the target ancestors for this store across its pre-hook, with
        // the same identity semantics as the global pre-apply hook.
        let store_ops: Vec<PlanFileOp> = ops_by_store
            .get(store_name)
            .iter()
            .flat_map(|indices| indices.iter().map(|&i| plan.ops[i].clone()))
            .collect();
        let (store_targets, store_removed) = plan_link_targets(&store_ops);
        let store_ancestors =
            TargetAncestorSnapshot::capture(repo_root, store_targets, &store_removed, &home)
                .map_err(|e| {
                    PlanExecError::new(
                        report.clone(),
                        StitchError::plan_stale(redirect_to_plan_message(repo_root, &e)),
                    )
                })?;

        // Revalidate $HOME identity across the per-store pre-hook, using the
        // command-level identity captured before the store loop.
        if let Err(e) = home_identity.revalidate() {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(e.to_string()),
            ));
        }

        if let Err(e) = run_store_pre_hook(
            repo_root,
            store_name,
            &pre_hook_loaded.config,
            &platform,
            json,
        ) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, e));
        }
        if let Err(e) = store_ancestors.revalidate() {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(redirect_to_plan_message(repo_root, &e)),
            ));
        }
        if let Err(e) = home_identity.revalidate() {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(e.to_string()),
            ));
        }
        if let Err(e) = require_directory_identity(
            &store_dir,
            store_identity,
            "store directory changed during pre-hook",
        ) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, StitchError::plan_stale(e)));
        }
        if let Err(e) = require_directory_identity(
            repo_root,
            repo_identity,
            "repository changed during store pre-hook",
        ) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, StitchError::plan_stale(e)));
        }
        let hook_hash =
            compute_config_hash(repo_root).map_err(|e| PlanExecError::new(report.clone(), e))?;
        if hook_hash != plan.config_sha256 {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!(
                    "config changed during pre-hook for store '{store_name}'"
                )),
            ));
        }

        // Serialize this store's mutations with other mutating commands: the
        // plan's pinned config hash was verified above, but a concurrent
        // add/remove/migrate could still interleave between that check and the
        // op loop. Hold the state lock from here through op execution and
        // re-verify the hash under it; release before the post-hook, which may
        // itself invoke a mutating stitch command.
        let _state_lock = match config::StateLock::exclusive_if_present(repo_root) {
            Ok(lock) => lock,
            Err(e) => {
                sync_ops_remaining(&mut report, plan, &remaining);
                return Err(PlanExecError::new(
                    report,
                    StitchError::plan_stale(format!("could not lock state: {e}")),
                ));
            }
        };
        let locked_loaded = Config::load(repo_root).map_err(|e| {
            sync_ops_remaining(&mut report, plan, &remaining);
            PlanExecError::new(report.clone(), StitchError::from(e))
        })?;
        let locked_hash =
            compute_config_hash(repo_root).map_err(|e| PlanExecError::new(report.clone(), e))?;
        if locked_hash != plan.config_sha256 {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!(
                    "config changed before executing store '{store_name}'"
                )),
            ));
        }

        // Re-simulate this store's complete remaining sequence after its hook.
        // Dependent operations such as whole-dir promotion must fail before
        // the root unlink if the hook invalidated a later child source. The
        // simulation uses the locked-and-verified snapshot so a pre-hook or
        // concurrent change cannot authorize a stale operation.
        if let Some(indices) = ops_by_store.get(store_name) {
            let mut hook_state = PreflightState::new(repo_root, &platform);
            for &idx in indices {
                if let Err(error) = hook_state.apply_op(&locked_loaded, &plan.ops[idx]) {
                    sync_ops_remaining(&mut report, plan, &remaining);
                    return Err(PlanExecError::new(
                        report,
                        StitchError::plan_stale(format!(
                            "post-hook preflight failed for op {idx}: {error}"
                        )),
                    ));
                }
            }

            for &idx in indices {
                let op = &plan.ops[idx];

                // Re-check the precondition immediately before acting.
                if let Err(e) = preflight_op(repo_root, &locked_loaded, &platform, op) {
                    sync_ops_remaining(&mut report, plan, &remaining);
                    return Err(PlanExecError::new(
                        report,
                        StitchError::plan_stale(format!(
                            "op {idx} ({}) precondition changed: {e}",
                            op_description(op)
                        )),
                    ));
                }

                match execute_op(repo_root, &locked_loaded, &platform, op, idx, &mut report) {
                    Ok(()) => {
                        report.ops_executed.push(op_description(op));
                        remaining.remove(&idx);
                        sync_ops_remaining(&mut report, plan, &remaining);
                    }
                    Err(e) => {
                        sync_ops_remaining(&mut report, plan, &remaining);
                        return Err(PlanExecError::new(
                            report,
                            StitchError::plan_stale(format!(
                                "op {idx} ({}): {e}",
                                op_description(op)
                            )),
                        ));
                    }
                }
            }
        }
        drop(_state_lock);

        if let Some(warning) = run_store_post_hook(
            repo_root,
            store_name,
            &locked_loaded.config,
            &platform,
            json,
        ) {
            report.warnings.push(warning);
        }
        // Revalidate $HOME identity after the post-hook, using the
        // command-level identity. A post-hook that replaces the directory
        // behind a symlinked $HOME must be caught before the next store.
        if let Err(e) = home_identity.revalidate() {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(e.to_string()),
            ));
        }
        if let Err(error) = require_directory_identity(
            repo_root,
            repo_identity,
            "repository changed during store post-hook",
        ) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, StitchError::plan_stale(error)));
        }
        let hook_hash =
            compute_config_hash(repo_root).map_err(|e| PlanExecError::new(report.clone(), e))?;
        if hook_hash != plan.config_sha256 {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!(
                    "config changed during post-hook for store '{store_name}'"
                )),
            ));
        }
    }

    sync_ops_remaining(&mut report, plan, &remaining);

    // Global post-apply hook (warn on failure, never clobber the apply result).
    let env = HookEnv {
        root: repo_root,
        store: None,
        target: None,
        action: "apply",
    };
    if let Err(e) = hooks::run_global_hook(repo_root, "post-apply", &env, &platform, json) {
        report.warnings.push(format!("post-apply hook: {e}"));
    }

    if !plan.conflicts.is_empty() || !plan.errors.is_empty() {
        Err(PlanExecError::new(report.clone(), plan_exec_error(plan)))
    } else {
        Ok(report)
    }
}

fn run_store_pre_hook(
    repo_root: &Path,
    store_name: &str,
    config: &Config,
    platform: &Platform,
    json: bool,
) -> Result<(), StitchError> {
    let Some(store) = config.stores.get(store_name) else {
        return Ok(());
    };
    if let Some(pre) = &store.hooks.pre {
        let env = HookEnv {
            root: repo_root,
            store: Some(store_name),
            target: store.target.as_deref(),
            action: "apply",
        };
        hooks::run_store_hook(pre, &env, platform, json)
            .map_err(|e| StitchError::hook_store("pre", e, store_name))?;
    }
    Ok(())
}

fn run_store_post_hook(
    repo_root: &Path,
    store_name: &str,
    config: &Config,
    platform: &Platform,
    json: bool,
) -> Option<String> {
    let store = config.stores.get(store_name)?;
    if let Some(post) = &store.hooks.post {
        let env = HookEnv {
            root: repo_root,
            store: Some(store_name),
            target: store.target.as_deref(),
            action: "apply",
        };
        if let Err(e) = hooks::run_store_hook(post, &env, platform, json) {
            return Some(format!("store '{store_name}' post-hook: {e}"));
        }
    }
    None
}

pub fn plan_exec_error(plan: &PlanFile) -> StitchError {
    let mut classes = BTreeSet::new();
    for conflict in &plan.conflicts {
        classes.insert(conflict_class(conflict));
    }
    for error in &plan.errors {
        if let Some(c) = FailureClass::from_id(&error.class) {
            classes.insert(c);
        }
    }
    if classes.is_empty() {
        return StitchError::plan_stale("plan reported conflicts or errors");
    }
    let message = format!(
        "apply --plan reported {} conflict(s), {} error(s)",
        plan.conflicts.len(),
        plan.errors.len()
    );
    StitchError::apply(classes.into_iter().collect(), message)
}

pub fn conflict_class(conflict: &PlanConflict) -> FailureClass {
    if conflict.kind == "foreign_symlink" || conflict.resolves_to.is_some() {
        FailureClass::ConflictForeign
    } else {
        FailureClass::ConflictReal
    }
}

/// Explain a rejected ancestor without accepting an external symlink as safe.
fn symlink_ancestor_error(repo_root: &Path, ancestor: &Path) -> String {
    if linker::points_into_repo(ancestor, repo_root) {
        format!(
            "parent {} is a symlink into the repository; refusing to traverse it",
            ancestor.display()
        )
    } else {
        format!(
            "parent {} is a symlinked ancestor; refusing to traverse it",
            ancestor.display()
        )
    }
}

/// Inspect one actual ancestor without following it.
fn check_physical_ancestor(repo_root: &Path, ancestor: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(ancestor) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(symlink_ancestor_error(repo_root, ancestor))
        }
        Ok(meta) if !meta.is_dir() => {
            Err(format!("parent {} is not a directory", ancestor.display()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "could not inspect target ancestor {}: {e}",
            ancestor.display()
        )),
    }
}

/// The live re-check deliberately reads the filesystem rather than inheriting
/// simulated state: prior plan operations may themselves have made a symlink.
fn check_ancestors_writable(repo_root: &Path, target: &Path) -> Result<(), String> {
    for ancestor in target.ancestors().skip(1) {
        check_physical_ancestor(repo_root, ancestor)?;
    }
    Ok(())
}

/// Check that a target state matches the filesystem reality.
fn check_target_state(path: &Path, expected: &TargetState) -> Result<(), String> {
    match expected {
        TargetState::Absent => {
            if path.symlink_metadata().is_ok() {
                return Err(format!("{} exists", path.display()));
            }
        }
        TargetState::RealEntry => match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.file_type().is_symlink() => {}
            Ok(_) => return Err(format!("{} is a symlink", path.display())),
            Err(_) => return Err(format!("{} does not exist", path.display())),
        },
        TargetState::SymlinkTo(expected_target) => {
            if !path.is_symlink() {
                return Err(format!("{} is not a symlink", path.display()));
            }
            let resolved = std::fs::read_link(path)
                .map_err(|e| format!("could not read link {}: {e}", path.display()))?;
            if resolved != Path::new(expected_target) {
                return Err(format!(
                    "{} points to {} (expected {})",
                    path.display(),
                    resolved.display(),
                    expected_target
                ));
            }
        }
        TargetState::SymlinkIntoRepo => {
            if !path.is_symlink() {
                return Err(format!("{} is not a symlink", path.display()));
            }
        }
    }
    Ok(())
}

/// Verify that a removal still belongs to the named store. Source-less stale
/// removals are scoped to that store's source or render tree; they must not
/// become an ambiguous "any repo link" deletion when targets overlap.
fn check_remove_link_ownership(
    repo_root: &Path,
    store: &str,
    target: &Path,
    source: Option<&str>,
) -> Result<(), String> {
    let owned = if let Some(source) = source {
        linker::points_into_repo(target, repo_root)
            || linker::points_at_source(target, Path::new(source), repo_root)
    } else {
        let store_dir = repo_root.join(store);
        let staged_dir = render::store_render_dir(repo_root, store);
        linker::points_into_repo(target, repo_root)
            && (linker::points_into(target, &store_dir) || linker::points_into(target, &staged_dir))
    };
    if owned {
        Ok(())
    } else if source.is_some() {
        Err(format!("{} does not point into repo", target.display()))
    } else {
        Err(format!(
            "target {} does not point into store '{store}'",
            target.display()
        ))
    }
}

/// Final source-less removal scope check. The threat model deliberately does
/// not attempt to defeat a same-UID swap after this immediate check.
fn remove_link_for_store(repo_root: &Path, store: &str, target: &Path) -> Result<bool, String> {
    check_remove_link_ownership(repo_root, store, target, None)?;
    linker::remove_link(target, repo_root).map_err(link_error)
}

fn preflight_op(
    repo_root: &Path,
    loaded: &Loaded,
    platform: &Platform,
    op: &PlanFileOp,
) -> Result<(), String> {
    match op {
        PlanFileOp::StageRender {
            store,
            source_rel,
            staged,
            sha256,
        } => {
            let _ = verify_stage_render(
                repo_root, loaded, platform, store, source_rel, staged, sha256,
            )?;
            render::preflight_staged_path(
                repo_root,
                store,
                &staged_link_identity(repo_root, store, staged)?,
            )
        }
        PlanFileOp::CreateLink {
            target,
            source,
            requires,
        } => {
            check_ancestors_writable(repo_root, Path::new(target))?;
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;
            check_source_exists_for_preflight(repo_root, source)?;
            check_target_state(Path::new(target), &target_state)?;
            Ok(())
        }
        PlanFileOp::ReplaceLink {
            target,
            source,
            requires,
        } => {
            check_ancestors_writable(repo_root, Path::new(target))?;
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;
            check_source_exists_for_preflight(repo_root, source)?;
            check_target_state(Path::new(target), &target_state)?;
            if matches!(target_state, TargetState::RealEntry) && !Path::new(target).is_dir() {
                return Err("replace_link may only replace an empty directory".into());
            }
            Ok(())
        }
        PlanFileOp::BackupAndLink {
            target,
            backup,
            source,
            requires,
        } => {
            check_ancestors_writable(repo_root, Path::new(target))?;
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;
            if !matches!(target_state, TargetState::RealEntry) {
                return Err("backup_and_link requires target=real_entry".into());
            }
            let backup_state = target_state_from(
                requires.backup.as_deref().unwrap_or("absent"),
                &requires.backup_value,
            )
            .map_err(|e| format!("invalid backup requires: {e}"))?;
            if !matches!(backup_state, TargetState::Absent) {
                return Err("backup_and_link requires backup=absent".into());
            }
            check_source_exists_for_preflight(repo_root, source)?;
            check_target_state(Path::new(target), &target_state)?;
            if Path::new(backup).symlink_metadata().is_ok() {
                return Err(format!("backup {} already exists", backup));
            }
            Ok(())
        }
        PlanFileOp::RemoveLink {
            store,
            target,
            source,
            requires,
        } => {
            check_ancestors_writable(repo_root, Path::new(target))?;
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;
            let target_path = Path::new(target);
            if !matches!(
                target_state,
                TargetState::SymlinkTo(_) | TargetState::SymlinkIntoRepo
            ) {
                return Err("remove_link requires symlink_to or symlink_into_repo".into());
            }
            check_target_state(target_path, &target_state)?;
            check_remove_link_ownership(repo_root, store, target_path, source.as_deref())?;
            Ok(())
        }
        PlanFileOp::RemoveStaged { store, rel } => {
            if !loaded.config.stores.contains_key(store) {
                return Err(format!("unknown store '{store}'"));
            }
            let rel_path = Path::new(rel);
            if rel_path.is_absolute() || has_parent_dir(rel_path) || !is_safe_fragment(rel) {
                return Err(format!("invalid staged rel '{rel}'"));
            }
            // Stale renders may already have been cleaned up by hand; a missing
            // file is not a preflight failure.
            render::preflight_staged_path(repo_root, store, rel)
        }
    }
}

fn is_dir_empty(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut iter) => iter.next().is_none(),
        Err(_) => false,
    }
}

fn replace_link_real_entry(
    repo_root: &Path,
    target_path: &Path,
    source_path: &Path,
    idx: usize,
) -> Result<(), String> {
    if target_path.is_dir() && !is_dir_empty(target_path) {
        return Err(format!(
            "{} is not empty — cannot replace",
            target_path.display()
        ));
    }

    let Some(parent) = target_path.parent() else {
        return Err(format!("{} has no parent directory", target_path.display()));
    };
    let Some(name) = target_path.file_name() else {
        return Err(format!("{} has no file name", target_path.display()));
    };
    let name_str = name.to_string_lossy();
    let pid = std::process::id();
    let tmp_link = parent.join(format!(".{name_str}.stitch-link-{idx}-{pid}"));
    let tmp_orig = parent.join(format!(".{name_str}.stitch-orig-{idx}-{pid}"));

    if tmp_link.symlink_metadata().is_ok() || tmp_orig.symlink_metadata().is_ok() {
        return Err(format!(
            "temporary replacement path for {} already exists",
            target_path.display()
        ));
    }

    // Create the new symlink at a temporary path first so the original is not
    // removed until the link is known to work.
    create_link_for_plan(repo_root, &tmp_link, source_path)?;

    // Move the existing entry aside.
    if let Err(e) = std::fs::rename(target_path, &tmp_orig) {
        let _ = std::fs::remove_file(&tmp_link);
        return Err(format!(
            "could not move {} aside: {e}",
            target_path.display()
        ));
    }

    // Move the new link into place.
    if let Err(e) = std::fs::rename(&tmp_link, target_path) {
        // Roll back on failure.
        let rollback = std::fs::rename(&tmp_orig, target_path);
        let _ = std::fs::remove_file(&tmp_link);
        if let Err(re) = rollback {
            return Err(format!(
                "could not place symlink at {}: {e}; rollback also failed ({re}); the original entry is at {}",
                target_path.display(),
                tmp_orig.display()
            ));
        }
        return Err(format!(
            "could not place symlink at {}: {e}",
            target_path.display()
        ));
    }

    // Remove the original (now at tmp_orig). It was a file or empty directory.
    if tmp_orig.is_dir() {
        if let Err(e) = std::fs::remove_dir(&tmp_orig) {
            return Err(format!(
                "replaced {} but could not remove original: {e}",
                target_path.display()
            ));
        }
    } else if let Err(e) = std::fs::remove_file(&tmp_orig) {
        return Err(format!(
            "replaced {} but could not remove original: {e}",
            target_path.display()
        ));
    }

    Ok(())
}

fn is_symlink_source(source: &Path) -> bool {
    std::fs::symlink_metadata(source)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// The staging identity (link name under `.stitch/render/<store>/`) of a
/// staged path. v0.14: this is *not* derivable from the template's source
/// name — a `sources` template stages under its declared key.
fn staged_link_identity(repo_root: &Path, store: &str, staged: &str) -> Result<String, String> {
    Path::new(staged)
        .strip_prefix(&render::store_render_dir(repo_root, store))
        .map_err(|_| format!("staged path outside render tree: {staged}"))?
        .to_str()
        .map(str::to_owned)
        .filter(|rel| !rel.is_empty())
        .ok_or_else(|| format!("staged path has no link identity: {staged}"))
}

fn create_link_for_plan(repo_root: &Path, target: &Path, source: &Path) -> Result<(), String> {
    // Re-derive and validate the configured source root at the mutation
    // boundary so a hook cannot install a gateway after plan preflight.
    let source_root = plan_source_root(repo_root, source)?;
    linker::validate_source_in(source, &source_root).map_err(|e| e.to_string())?;
    if is_symlink_source(source) {
        linker::create_link_to_entry_in(target, source, &source_root).map_err(|e| e.to_string())
    } else {
        linker::create_link_in(target, source, &source_root).map_err(|e| e.to_string())
    }
}

fn execute_op(
    repo_root: &Path,
    loaded: &Loaded,
    platform: &Platform,
    op: &PlanFileOp,
    idx: usize,
    report: &mut PlanExecReport,
) -> Result<(), String> {
    match op {
        PlanFileOp::StageRender {
            store,
            source_rel,
            staged,
            sha256,
        } => {
            let source_path = verify_stage_render(
                repo_root, loaded, platform, store, source_rel, staged, sha256,
            )?;
            let link_identity = staged_link_identity(repo_root, store, staged)?;
            render::stage_template(
                repo_root,
                store,
                source_rel,
                &source_path,
                &link_identity,
                platform,
                &loaded.config.vars,
            )
            .map_err(|e| format!("stage failed: {e}"))?;
            report.staged.push(staged.clone());
            Ok(())
        }
        PlanFileOp::CreateLink { target, source, .. } => {
            let target_path = Path::new(target);
            let source_path = Path::new(source);
            create_link_for_plan(repo_root, target_path, source_path)?;
            Ok(())
        }
        PlanFileOp::ReplaceLink {
            target,
            source,
            requires,
        } => {
            let target_path = Path::new(target);
            let source_path = Path::new(source);
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;

            match target_state {
                TargetState::SymlinkTo(expected) => {
                    let expected_path = Path::new(&expected);
                    if !linker::remove_link_to(target_path, expected_path, repo_root)
                        .map_err(link_error)?
                    {
                        return Err(format!("{} was repointed", target_path.display()));
                    }
                    create_link_for_plan(repo_root, target_path, source_path)?;
                }
                TargetState::RealEntry => {
                    replace_link_real_entry(repo_root, target_path, source_path, idx)?;
                }
                _ => return Err("replace_link requires symlink_to or real_entry".into()),
            }

            Ok(())
        }
        PlanFileOp::BackupAndLink {
            target,
            backup,
            source,
            ..
        } => {
            let target_path = Path::new(target);
            let backup_path = Path::new(backup);
            let source_path = Path::new(source);

            // Re-check the backup path at exec time (TOCTOU guard).
            if backup_path.symlink_metadata().is_ok() {
                return Err(format!(
                    "backup path {} already exists",
                    backup_path.display()
                ));
            }
            if backup_path.parent() != target_path.parent() {
                return Err(format!(
                    "backup path {} is not under the same directory as target {}",
                    backup_path.display(),
                    target_path.display()
                ));
            }

            std::fs::rename(target_path, backup_path).map_err(|e| {
                format!(
                    "could not back up {} to {}: {e}",
                    target_path.display(),
                    backup_path.display()
                )
            })?;
            if let Err(e) = create_link_for_plan(repo_root, target_path, source_path) {
                // Restore the original on failure.
                let _ = std::fs::rename(backup_path, target_path);
                return Err(e);
            }
            Ok(())
        }
        PlanFileOp::RemoveLink {
            store,
            target,
            source,
            ..
        } => {
            let target_path = Path::new(target);
            let removed = if let Some(src) = source {
                let expected = Path::new(src);
                linker::remove_link_to(target_path, expected, repo_root).map_err(link_error)?
            } else {
                remove_link_for_store(repo_root, store, target_path)?
            };
            if !removed {
                return Err(format!("{} was not repo-owned", target_path.display()));
            }
            Ok(())
        }
        PlanFileOp::RemoveStaged { store, rel } => render::remove_staged(repo_root, store, rel)
            .map_err(|e| format!("could not remove staged render {store}/{rel}: {e}")),
    }
}

fn link_error(e: LinkError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_file::PlatformFingerprint;
    use crate::store::{self, ApplyOpts};
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn whole_dir_promotion_removes_root_before_creating_children() {
        let tmp = tempfile::tempdir().unwrap();
        let _home_guard = config::test_home_guard(tmp.path().to_path_buf());
        let real_root = tmp.path().join("repo");
        let stitch_dir = real_root.join(".stitch");
        let store_dir = real_root.join("shells");
        let target = tmp.path().join("home").join(".shells");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(real_root.join("stitch.toml"), "").unwrap();
        fs::write(store_dir.join("profile"), "profile\n").unwrap();
        fs::write(
            stitch_dir.join("state.toml"),
            format!(
                "[stores.shells]\ntarget = \"{}\"\nfiles = [\"profile\"]\n",
                target.display()
            ),
        )
        .unwrap();

        let repo_alias = tmp.path().join("repo-alias");
        symlink(&real_root, &repo_alias).unwrap();
        linker::create_link(&target, &store_dir).unwrap();

        let loaded = Config::load(&repo_alias).unwrap();
        let platform = Platform::detect();
        let computed = store::compute_plan(
            &repo_alias,
            &loaded.config,
            &platform,
            ApplyOpts {
                dry_run: true,
                force: false,
                json: false,
            },
        );
        let plan = build_plan_file(&repo_alias, &loaded, &computed, &platform).unwrap();
        assert!(plan.ops.iter().any(|op| {
            matches!(op, PlanFileOp::RemoveLink { target: path, .. } if path == &target.display().to_string())
        }));

        assert!(plan.conflicts.is_empty());
        execute_plan(&repo_alias, &loaded, &plan, false, false, false).unwrap();
        assert!(target.is_dir());
        assert!(target.join("profile").is_symlink());
    }

    #[test]
    fn execute_plan_rejects_stale_loaded_and_does_not_create_orphan() {
        // Capture a loaded config snapshot, then simulate a concurrent `remove`
        // that empties `.stitch/state.toml`. A stale `Loaded` passed to
        // execute_plan must not be used to authorize creation: the executor
        // reloads the authoritative empty state under the state lock and rejects
        // the now-unauthorized create_link before the filesystem is touched.
        let tmp = tempfile::tempdir().unwrap();
        let _home_guard = config::test_home_guard(tmp.path().to_path_buf());
        let repo_root = tmp.path().join("repo");
        let stitch_dir = repo_root.join(".stitch");
        let store_dir = repo_root.join("shells");
        let target_dir = tmp.path().join("home").join(".shells");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::create_dir_all(&store_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(repo_root.join("stitch.toml"), "").unwrap();
        fs::write(store_dir.join("profile"), "profile\n").unwrap();
        fs::write(
            stitch_dir.join("state.toml"),
            format!(
                "[stores.shells]\ntarget = \"{}\"\nfiles = [\"profile\"]\n",
                target_dir.display()
            ),
        )
        .unwrap();

        let stale_loaded = Config::load(&repo_root).unwrap();

        // Concurrent `remove` empties the authoritative state without changing
        // the source file, leaving a `Loaded` that still believes the store is
        // desired.
        fs::write(stitch_dir.join("state.toml"), "").unwrap();

        let platform = Platform::detect();
        let fingerprint = PlatformFingerprint::from(&platform);
        let target = target_dir.join("profile");
        let source = store_dir.join("profile");
        let plan = PlanFile {
            schema: PLAN_SCHEMA,
            kind: PLAN_KIND.into(),
            repo: path_to_string(
                &repo_root
                    .canonicalize()
                    .unwrap_or_else(|_| repo_root.clone()),
            ),
            config_sha256: compute_config_hash(&repo_root).unwrap(),
            platform: fingerprint,
            stores: vec!["shells".into()],
            ops: vec![PlanFileOp::CreateLink {
                target: path_to_string(&target),
                source: path_to_string(&source),
                requires: PlanFileRequires {
                    target: "absent".into(),
                    value: None,
                    backup: None,
                    backup_value: None,
                },
            }],
            conflicts: vec![],
            errors: vec![],
        };

        let result = execute_plan(&repo_root, &stale_loaded, &plan, false, false, false);
        assert!(
            result.is_err(),
            "stale loaded must not authorize a create_link for a removed store: {result:?}"
        );
        assert!(
            !target.is_symlink(),
            "no orphan link may be created for a store no longer in state"
        );
        assert!(
            !target.exists(),
            "target must not be created by a rejected plan"
        );
    }

    #[test]
    fn execute_plan_rejects_config_change_before_per_store_pre_hook() {
        // Simulate the TOCTOU: a same-UID process rewrites stitch.toml to
        // install a malicious pre-hook after the global hash check has passed
        // but before the per-store Config::load. The new pre-hook hash check
        // must detect the change before the (untrusted) hook resolves and runs.
        let tmp = tempfile::tempdir().unwrap();
        let _home_guard = config::test_home_guard(tmp.path().to_path_buf());
        let repo_root = tmp.path().join("repo");
        let stitch_dir = repo_root.join(".stitch");
        let store_dir = repo_root.join("s");
        let target_dir = tmp.path().join("home").join("s");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::create_dir_all(&store_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(repo_root.join("stitch.toml"), "").unwrap();
        fs::write(store_dir.join("f"), "f\n").unwrap();
        fs::write(
            stitch_dir.join("state.toml"),
            format!(
                "[stores.s]\ntarget = \"{}\"\nfiles = [\"f\"]\n",
                target_dir.display()
            ),
        )
        .unwrap();

        let loaded = Config::load(&repo_root).unwrap();
        let platform = Platform::detect();
        let computed = store::compute_plan(
            &repo_root,
            &loaded.config,
            &platform,
            ApplyOpts {
                dry_run: true,
                force: false,
                json: false,
            },
        );
        let plan = build_plan_file(&repo_root, &loaded, &computed, &platform).unwrap();

        let marker = repo_root.join("marker");
        let malicious = format!(
            "[stores.s]\nhooks = {{ pre = \"touch {}\" }}\n",
            marker.display()
        );
        let repo_arc = std::sync::Arc::new(repo_root.clone());
        set_test_pause_after_global_hash(Some(Box::new(move || {
            fs::write(repo_arc.join("stitch.toml"), malicious).unwrap();
        })));

        let result = execute_plan(&repo_root, &loaded, &plan, false, false, false);
        set_test_pause_after_global_hash(None);

        let err = result.expect_err("config change must abort before pre-hook");
        let msg = err.error.to_string();
        assert!(
            msg.contains("config changed before pre-hook"),
            "expected pre-hook hash-check failure, got: {msg}"
        );
        assert!(
            !marker.exists(),
            "malicious pre-hook must not run before the hash check"
        );
    }
}
