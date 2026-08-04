//! Plan file format, validation, and executor for `stitch plan` and
//! `stitch apply --plan`.
//!
//! The on-disk plan file is a stable, versioned artifact. It carries hashes
//! for staged renders and the config+platform fingerprint so that `apply --plan`
//! can refuse to execute anything that shifted between capture and execution.

use crate::config::{self, Config, Loaded, Store, is_safe_fragment};
use crate::error::{FailureClass, StitchError};
use crate::hooks::{self, HookEnv};
use crate::linker::{self, LinkError};
use crate::plan::{LinkRequires, Plan, PlanOp, TargetState, path_to_string};
use crate::platform::Platform;
use crate::render;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

pub const PLAN_SCHEMA: u32 = 1;
pub const PLAN_KIND: &str = "stitch/plan";

/// True if `p` contains any `..` path component.
fn has_parent_dir(p: &Path) -> bool {
    p.components().any(|c| c == Component::ParentDir)
}

/// The on-disk plan file format. Kept intentionally close to the §2 spec so
/// that hand inspection and external tooling can rely on its shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFile {
    pub schema: u32,
    pub kind: String,
    pub repo: String,
    pub config_sha256: String,
    pub platform: PlatformFingerprint,
    pub ops: Vec<PlanFileOp>,
    pub conflicts: Vec<PlanConflict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<PlanError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformFingerprint {
    pub os: String,
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    pub hostname: String,
    pub shell: String,
}

impl From<&Platform> for PlatformFingerprint {
    fn from(p: &Platform) -> Self {
        Self {
            os: p.os.clone(),
            arch: p.arch.clone(),
            distro: p.distro.clone(),
            hostname: p.hostname.clone(),
            shell: p.shell.clone(),
        }
    }
}

impl PlatformFingerprint {
    pub fn matches(&self, platform: &Platform) -> bool {
        self.os == platform.os
            && self.arch == platform.arch
            && self.distro == platform.distro
            && self.hostname == platform.hostname
            && self.shell == platform.shell
    }
}

/// The `requires` field on a link op in the plan file. The spec uses a flatter
/// shape than the M3 `LinkRequires`/`TargetState` enums, so this is a separate
/// file-oriented representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFileRequires {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_value: Option<String>,
}

/// An executable op in the plan file. The `op` tag matches the §2 spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum PlanFileOp {
    StageRender {
        store: String,
        source_rel: String,
        staged: String,
        sha256: String,
    },

    CreateLink {
        target: String,
        source: String,
        requires: PlanFileRequires,
    },

    ReplaceLink {
        target: String,
        source: String,
        requires: PlanFileRequires,
    },

    BackupAndLink {
        target: String,
        backup: String,
        source: String,
        requires: PlanFileRequires,
    },

    RemoveLink {
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        requires: PlanFileRequires,
    },
}

impl PlanFileOp {
    pub fn op_store(&self, repo_root: &Path, config: &Config) -> Option<String> {
        match self {
            PlanFileOp::StageRender { store, .. } => Some(store.clone()),
            PlanFileOp::CreateLink { source, .. }
            | PlanFileOp::ReplaceLink { source, .. }
            | PlanFileOp::BackupAndLink { source, .. } => source_store(source, repo_root),
            PlanFileOp::RemoveLink { target, source, .. } => {
                if let Some(s) = source {
                    source_store(s, repo_root)
                } else {
                    find_store_for_target(repo_root, config, Path::new(target))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanConflict {
    pub target: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolves_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanError {
    pub target: Option<String>,
    pub message: String,
    pub class: String,
}

/// The result of executing (or preflighting) a plan file.
#[derive(Debug, Clone, Serialize)]
pub struct PlanExecReport {
    pub ops_total: usize,
    pub ops_executed: Vec<String>,
    pub ops_remaining: VecDeque<String>,
    pub conflicts: Vec<PlanConflict>,
    pub staged: Vec<String>,
}

/// An aborted execution: the prefix that ran and the error that stopped it.
#[derive(Debug)]
pub struct PlanExecError {
    pub report: Box<PlanExecReport>,
    pub error: Box<StitchError>,
}

impl PlanExecError {
    pub fn new(report: PlanExecReport, error: StitchError) -> Self {
        Self {
            report: Box::new(report),
            error: Box::new(error),
        }
    }
}

/// Build a plan file from the M3 `Plan` produced by `store::compute_plan`.
///
/// This is where `StageRender` ops are inserted: every template that would be
/// linked (or whose link is already correct but content may drift) is rendered
/// in memory and pinned by SHA-256. The plan file contains no rendered content,
/// only hashes and paths.
pub fn build_plan_file(
    repo_root: &Path,
    loaded: &Loaded,
    plan: &Plan,
    platform: &Platform,
) -> Result<PlanFile, StitchError> {
    let config_sha256 = compute_config_hash(repo_root)?;
    let platform_fp = PlatformFingerprint::from(platform);

    let mut ops = Vec::new();
    let mut conflicts = Vec::new();
    let mut errors = Vec::new();

    for store in &plan.stores {
        let store_dir = repo_root.join(&store.store_name);
        let (renders, mut link_ops) = convert_store_ops(
            repo_root,
            loaded,
            &store.store_name,
            &store_dir,
            &store.ops,
            platform,
        )?;
        ops.extend(renders);
        ops.append(&mut link_ops);

        for op in &store.ops {
            if let PlanOp::Conflict {
                target,
                resolves_to,
            } = op
            {
                conflicts.push(PlanConflict {
                    target: target.clone(),
                    kind: conflict_kind(resolves_to),
                    resolves_to: resolves_to.clone(),
                });
            } else if let PlanOp::Error { message, class } = op {
                errors.push(PlanError {
                    target: PlanOp::target(op).map(str::to_owned),
                    message: message.clone(),
                    class: class.clone(),
                });
            }
        }
    }

    Ok(PlanFile {
        schema: PLAN_SCHEMA,
        kind: PLAN_KIND.into(),
        repo: path_to_string(repo_root),
        config_sha256,
        platform: platform_fp,
        ops,
        conflicts,
        errors,
    })
}

fn conflict_kind(resolves_to: &Option<String>) -> String {
    if resolves_to.is_some() {
        "foreign_symlink".into()
    } else {
        "real_entry".into()
    }
}

/// Convert one store's M3 ops into stage renders (first) and link ops (second).
///
/// All `StageRender` ops for the store come first so that whole-directory
/// promotion and stale-link removal happen *after* templates are pinned and
/// staged, matching the real execution order in `store::apply_store`.
fn convert_store_ops(
    repo_root: &Path,
    loaded: &Loaded,
    store_name: &str,
    store_dir: &Path,
    ops: &[PlanOp],
    platform: &Platform,
) -> Result<(Vec<PlanFileOp>, Vec<PlanFileOp>), StitchError> {
    let mut renders = Vec::new();
    let mut links = Vec::new();

    for op in ops {
        let maybe_render =
            stage_render_for_op(repo_root, loaded, store_name, store_dir, op, platform)?;
        if let Some(render) = maybe_render {
            renders.push(render);
        }

        match op {
            PlanOp::CreateLink {
                target,
                source,
                requires,
            } => {
                links.push(PlanFileOp::CreateLink {
                    target: target.clone(),
                    source: source.clone(),
                    requires: requires.clone().into(),
                });
            }
            PlanOp::ReplaceLink {
                target,
                source,
                requires,
                ..
            } => {
                links.push(PlanFileOp::ReplaceLink {
                    target: target.clone(),
                    source: source.clone(),
                    requires: requires.clone().into(),
                });
            }
            PlanOp::BackupAndLink {
                target,
                source,
                backup,
                requires,
            } => {
                links.push(PlanFileOp::BackupAndLink {
                    target: target.clone(),
                    source: source.clone(),
                    backup: backup.clone(),
                    requires: requires.clone().into(),
                });
            }
            PlanOp::RemoveLink {
                target,
                source,
                requires,
            } => {
                links.push(PlanFileOp::RemoveLink {
                    target: target.clone(),
                    source: source.clone(),
                    requires: requires.clone().into(),
                });
            }
            PlanOp::AlreadyLinked { .. } | PlanOp::ContentChanged { .. } => {
                // For templates these became `StageRender`; for plain files
                // they are no-ops in the executable plan.
            }
            PlanOp::StageRender { .. }
            | PlanOp::SkippedPlatform
            | PlanOp::Conflict { .. }
            | PlanOp::Error { .. } => {}
        }
    }

    Ok((renders, links))
}

/// If `op` represents a template that must be rendered, produce a `StageRender`
/// op pinning the fresh in-memory render hash. Non-template ops return `None`.
fn stage_render_for_op(
    repo_root: &Path,
    loaded: &Loaded,
    store_name: &str,
    store_dir: &Path,
    op: &PlanOp,
    platform: &Platform,
) -> Result<Option<PlanFileOp>, StitchError> {
    let source = match op {
        PlanOp::CreateLink { source, .. }
        | PlanOp::ReplaceLink { source, .. }
        | PlanOp::BackupAndLink { source, .. }
        | PlanOp::AlreadyLinked { source, .. }
        | PlanOp::ContentChanged { source, .. } => source,
        PlanOp::RemoveLink { .. } => {
            // Stale links may point at staged renders, but we never re-stage
            // something that is being removed.
            return Ok(None);
        }
        _ => return Ok(None),
    };

    let source_path = Path::new(source);
    let staged_root = render::render_root(repo_root);
    if !source_path.starts_with(&staged_root) {
        return Ok(None);
    }

    let rel_to_staged = source_path.strip_prefix(&staged_root).map_err(|_| {
        StitchError::plan_stale(format!("staged path outside render tree: {source}"))
    })?;
    let mut components = rel_to_staged.components();
    let store_comp = components
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| {
            StitchError::plan_stale(format!("cannot derive store from staged path {source}"))
        })?;
    if store_comp != store_name {
        return Err(StitchError::plan_stale(format!(
            "staged path store '{store_comp}' does not match expected '{store_name}'"
        )));
    }
    let link_rel: PathBuf = components.collect();
    let link_rel = link_rel.to_string_lossy().into_owned();
    let source_rel = link_rel.clone() + render::TMPL_SUFFIX;
    let tmpl_source = store_dir.join(&source_rel);

    if !tmpl_source.is_file() {
        return Err(StitchError::plan_stale(format!(
            "template source does not exist: {}",
            tmpl_source.display()
        )));
    }

    let content = render::render_file(&tmpl_source, &source_rel, platform, &loaded.config.vars)
        .map_err(|e| StitchError::render(&tmpl_source, e))?;

    let staged_path = render::staging_path(repo_root, store_name, &link_rel);

    Ok(Some(PlanFileOp::StageRender {
        store: store_name.into(),
        source_rel,
        staged: path_to_string(&staged_path),
        sha256: sha256_hex(&content),
    }))
}

impl From<LinkRequires> for PlanFileRequires {
    fn from(req: LinkRequires) -> Self {
        Self {
            target: target_state_id(&req.target),
            value: target_state_value(&req.target),
            backup: req.backup.as_ref().map(target_state_id),
            backup_value: req.backup.as_ref().and_then(target_state_value),
        }
    }
}

fn target_state_id(state: &TargetState) -> String {
    match state {
        TargetState::Absent => "absent".into(),
        TargetState::RealEntry => "real_entry".into(),
        TargetState::SymlinkTo(_) => "symlink_to".into(),
        TargetState::SymlinkIntoRepo => "symlink_into_repo".into(),
    }
}

fn target_state_value(state: &TargetState) -> Option<String> {
    match state {
        TargetState::SymlinkTo(v) => Some(v.clone()),
        _ => None,
    }
}

fn target_state_from(target: &str, value: &Option<String>) -> Result<TargetState, String> {
    match target {
        "absent" => Ok(TargetState::Absent),
        "real_entry" => Ok(TargetState::RealEntry),
        "symlink_to" => match value {
            Some(v) => Ok(TargetState::SymlinkTo(v.clone())),
            None => Err("symlink_to requires a value".into()),
        },
        "symlink_into_repo" => Ok(TargetState::SymlinkIntoRepo),
        other => Err(format!("unknown target state '{other}'")),
    }
}

pub fn compute_config_hash(repo_root: &Path) -> Result<String, StitchError> {
    let mut hasher = Sha256::new();
    let stitch = repo_root.join("stitch.toml");
    let state = repo_root.join(".stitch").join("state.toml");

    for path in [stitch, state] {
        if path.exists() {
            let bytes = std::fs::read(&path)?;
            hasher.update(&bytes);
        }
    }

    Ok(sha256_hex_bytes(&hasher.finalize()))
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    sha256_hex_bytes(&hasher.finalize())
}

fn sha256_hex_bytes(digest: &sha2::digest::Output<Sha256>) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn base_report(plan: &PlanFile) -> PlanExecReport {
    PlanExecReport {
        ops_total: plan.ops.len(),
        ops_executed: Vec::new(),
        ops_remaining: plan.ops.iter().map(op_description).collect(),
        conflicts: plan.conflicts.clone(),
        staged: Vec::new(),
    }
}

/// Execute a plan file. With `dry_run: true` this is a preflight: every
/// precondition and fingerprint is validated and no filesystem mutation occurs.
pub fn execute_plan(
    repo_root: &Path,
    loaded: &Loaded,
    plan: &PlanFile,
    dry_run: bool,
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

    // Untrusted-input validation: every op must be justified by the pinned
    // config. This is the security boundary for hand-edited plan files.
    let validation_context = ValidationContext::new(repo_root, &loaded.config);
    for (idx, op) in plan.ops.iter().enumerate() {
        validate_op(&validation_context, idx, op).map_err(|e| {
            PlanExecError::new(
                base_report(plan),
                StitchError::plan_stale(format!("plan validation failed: {e}")),
            )
        })?;
    }

    // Preflight all ops before mutating anything.
    for (idx, op) in plan.ops.iter().enumerate() {
        preflight_op(repo_root, loaded, &platform, op).map_err(|e| {
            PlanExecError::new(
                base_report(plan),
                StitchError::plan_stale(format!("preflight failed for op {idx}: {e}")),
            )
        })?;
    }

    if dry_run {
        let report = base_report(plan);
        if !plan.conflicts.is_empty() || !plan.errors.is_empty() {
            return Err(PlanExecError::new(report.clone(), plan_exec_error(plan)));
        }
        return Ok(report);
    }

    // Global pre-apply hook (side effect, only on real execution).
    let env = HookEnv {
        root: repo_root,
        store: None,
        target: None,
        action: "apply",
    };
    hooks::run_global_hook(repo_root, "pre-apply", &env, &platform)
        .map_err(|e| PlanExecError::new(base_report(plan), StitchError::hook("pre-apply", e)))?;

    let mut report = base_report(plan);

    let mut last_store: Option<String> = None;
    let mut completed_stores: BTreeSet<String> = BTreeSet::new();

    for (idx, op) in plan.ops.iter().enumerate() {
        let op_store = op.op_store(repo_root, &loaded.config).ok_or_else(|| {
            PlanExecError::new(
                report.clone(),
                StitchError::plan_stale(format!("op {idx}: cannot derive store for execution")),
            )
        })?;

        // Per-store pre-hook: run before the first op of a new store.
        if last_store.as_ref() != Some(&op_store) {
            if let Some(prev) = last_store.take() {
                run_store_post_hook(repo_root, &prev, &loaded.config, &platform)
                    .map_err(|e| PlanExecError::new(report.clone(), e))?;
                completed_stores.insert(prev);
            }
            if !completed_stores.contains(&op_store) {
                run_store_pre_hook(repo_root, &op_store, &loaded.config, &platform)
                    .map_err(|e| PlanExecError::new(report.clone(), e))?;
            }
            last_store = Some(op_store.clone());
        }

        // Re-check the precondition immediately before acting.
        preflight_op(repo_root, loaded, &platform, op).map_err(|e| {
            PlanExecError::new(
                report.clone(),
                StitchError::plan_stale(format!(
                    "op {idx} ({}) precondition changed: {e}",
                    op_description(op)
                )),
            )
        })?;

        match execute_op(repo_root, loaded, &platform, op, idx, &mut report) {
            Ok(()) => {
                report.ops_executed.push(op_description(op));
                report.ops_remaining.pop_front();
            }
            Err(e) => {
                return Err(PlanExecError::new(
                    report,
                    StitchError::plan_stale(format!("op {idx} ({}): {e}", op_description(op))),
                ));
            }
        }
    }

    if let Some(store) = last_store {
        run_store_post_hook(repo_root, &store, &loaded.config, &platform)
            .map_err(|e| PlanExecError::new(report.clone(), e))?;
        completed_stores.insert(store);
    }

    // Global post-apply hook (warn on failure, never clobber the apply result).
    let env = HookEnv {
        root: repo_root,
        store: None,
        target: None,
        action: "apply",
    };
    if let Err(e) = hooks::run_global_hook(repo_root, "post-apply", &env, &platform) {
        eprintln!("warning: post-apply hook: {e}");
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
        hooks::run_store_hook(pre, &env, platform).map_err(|e| StitchError::hook("pre", e))?;
    }
    Ok(())
}

fn run_store_post_hook(
    repo_root: &Path,
    store_name: &str,
    config: &Config,
    platform: &Platform,
) -> Result<(), StitchError> {
    let Some(store) = config.stores.get(store_name) else {
        return Ok(());
    };
    if let Some(post) = &store.hooks.post {
        let env = HookEnv {
            root: repo_root,
            store: Some(store_name),
            target: store.target.as_deref(),
            action: "apply",
        };
        if let Err(e) = hooks::run_store_hook(post, &env, platform) {
            eprintln!("warning: store '{store_name}' post-hook: {e}");
        }
    }
    Ok(())
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
        "{} conflict(s), {} error(s)",
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

pub fn op_description(op: &PlanFileOp) -> String {
    match op {
        PlanFileOp::StageRender {
            store, source_rel, ..
        } => {
            format!("stage_render {store}/{source_rel}")
        }
        PlanFileOp::CreateLink { target, .. } => format!("create_link {target}"),
        PlanFileOp::ReplaceLink { target, .. } => format!("replace_link {target}"),
        PlanFileOp::BackupAndLink { target, .. } => format!("backup_and_link {target}"),
        PlanFileOp::RemoveLink { target, .. } => format!("remove_link {target}"),
    }
}

fn source_store(source: &str, repo_root: &Path) -> Option<String> {
    let path = Path::new(source);
    if let Some(name) = staged_store(path) {
        return Some(name);
    }
    // Plain source: strip repo root and take the first normal component as the
    // store name.
    path.strip_prefix(repo_root)
        .ok()?
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_owned)
}

fn staged_store(path: &Path) -> Option<String> {
    let mut iter = path.components();
    while let Some(c) = iter.next() {
        if c.as_os_str() == std::ffi::OsStr::new(".stitch") {
            let next = iter.next()?;
            if next.as_os_str() == std::ffi::OsStr::new("render") {
                let store = iter.next()?;
                return store.as_os_str().to_str().map(str::to_owned);
            }
        }
    }
    None
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
            let resolved = std::fs::read_link(path).map_err(|e| format!("{e}"))?;
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

fn check_source_exists_for_preflight(repo_root: &Path, source: &str) -> Result<(), String> {
    let source_path = Path::new(source);
    let is_staged = source_path.starts_with(render::render_root(repo_root));
    if !is_staged && !source_path.exists() {
        return Err(format!("source does not exist: {source}"));
    }
    Ok(())
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
            let source_path = repo_root.join(store).join(source_rel);
            let expected_staged = render::staging_path(
                repo_root,
                store,
                &render::resolve_entry(source_rel).link_rel,
            );
            if path_to_string(&expected_staged) != *staged {
                return Err(format!(
                    "staged path mismatch: expected {}",
                    expected_staged.display()
                ));
            }
            let content =
                render::render_file(&source_path, source_rel, platform, &loaded.config.vars)
                    .map_err(|e| format!("render failed: {e}"))?;
            let actual_hash = sha256_hex(&content);
            if actual_hash != *sha256 {
                return Err("render hash mismatch".into());
            }
            Ok(())
        }
        PlanFileOp::CreateLink {
            target,
            source,
            requires,
        } => {
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
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;
            check_source_exists_for_preflight(repo_root, source)?;
            check_target_state(Path::new(target), &target_state)?;
            Ok(())
        }
        PlanFileOp::BackupAndLink {
            target,
            backup,
            source,
            requires,
        } => {
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
            target,
            source,
            requires,
        } => {
            let target_state = target_state_from(&requires.target, &requires.value)
                .map_err(|e| format!("invalid requires: {e}"))?;
            let target_path = Path::new(target);
            match target_state {
                TargetState::SymlinkTo(expected) => {
                    if !target_path.is_symlink() {
                        return Err(format!("{} is not a symlink", target_path.display()));
                    }
                    let resolved = std::fs::read_link(target_path).map_err(|e| format!("{e}"))?;
                    if resolved != Path::new(&expected) {
                        return Err(format!(
                            "{} points to {} (expected {})",
                            target_path.display(),
                            resolved.display(),
                            expected
                        ));
                    }
                    if !linker::points_into_repo(target_path, repo_root) {
                        return Err(format!(
                            "{} does not point into repo",
                            target_path.display()
                        ));
                    }
                }
                TargetState::SymlinkIntoRepo => {
                    if !target_path.is_symlink() {
                        return Err(format!("{} is not a symlink", target_path.display()));
                    }
                    if !linker::points_into_repo(target_path, repo_root) {
                        return Err(format!(
                            "{} does not point into repo",
                            target_path.display()
                        ));
                    }
                }
                _ => return Err("remove_link requires symlink_to or symlink_into_repo".into()),
            }
            // If a `source` is recorded, the link must still point at it or into
            // the repo — both checked above when relevant.
            let _ = source;
            Ok(())
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
    if let Err(e) = linker::create_link(&tmp_link, source_path) {
        return Err(link_error(e));
    }

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
        let _ = std::fs::rename(&tmp_orig, target_path);
        let _ = std::fs::remove_file(&tmp_link);
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
            let source_path = repo_root.join(store).join(source_rel);
            let expected_staged = render::staging_path(
                repo_root,
                store,
                &render::resolve_entry(source_rel).link_rel,
            );
            if path_to_string(&expected_staged) != *staged {
                return Err(format!(
                    "staged path mismatch: expected {}",
                    expected_staged.display()
                ));
            }
            let content =
                render::render_file(&source_path, source_rel, platform, &loaded.config.vars)
                    .map_err(|e| format!("render failed: {e}"))?;
            let actual_hash = sha256_hex(&content);
            if actual_hash != *sha256 {
                return Err("render hash mismatch".into());
            }
            render::stage_template(
                repo_root,
                store,
                source_rel,
                &source_path,
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
            linker::create_link(target_path, source_path).map_err(link_error)?;
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
                    linker::create_link(target_path, source_path).map_err(link_error)?;
                }
                TargetState::RealEntry => {
                    replace_link_real_entry(target_path, source_path, idx)?;
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

            std::fs::rename(target_path, backup_path).map_err(|e| format!("{e}"))?;
            if let Err(e) = linker::create_link(target_path, source_path) {
                // Restore the original on failure.
                let _ = std::fs::rename(backup_path, target_path);
                return Err(link_error(e));
            }
            Ok(())
        }
        PlanFileOp::RemoveLink { target, source, .. } => {
            let target_path = Path::new(target);
            let removed = if let Some(src) = source {
                let expected = Path::new(src);
                linker::remove_link_to(target_path, expected, repo_root).map_err(link_error)?
            } else {
                linker::remove_link(target_path, repo_root).map_err(link_error)?
            };
            if !removed {
                return Err(format!("{} was not repo-owned", target_path.display()));
            }
            Ok(())
        }
    }
}

fn link_error(e: LinkError) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Untrusted-input validation
// ---------------------------------------------------------------------------

struct ValidationContext<'a> {
    repo_root: &'a Path,
    config: &'a Config,
}

impl<'a> ValidationContext<'a> {
    fn new(repo_root: &'a Path, config: &'a Config) -> Self {
        Self { repo_root, config }
    }
}

fn target_paths_for_store(store: &Store) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(ref t) = store.target {
        paths.push(config::expand_home(t));
    }
    for te in store.targets.values() {
        paths.push(config::expand_home(&te.target));
    }
    paths
}

fn find_store_for_target(_repo_root: &Path, config: &Config, target: &Path) -> Option<String> {
    for (name, store) in &config.stores {
        for target_path in target_paths_for_store(store) {
            if target == target_path || target.starts_with(target_path) {
                return Some(name.clone());
            }
        }
    }
    None
}

fn is_under_any_target(config: &Config, store: &str, target: &Path) -> bool {
    config.stores.get(store).is_some_and(|store| {
        target_paths_for_store(store)
            .iter()
            .any(|p| target == p || target.starts_with(p))
    })
}

fn validate_op(ctx: &ValidationContext, idx: usize, op: &PlanFileOp) -> Result<(), String> {
    match op {
        PlanFileOp::StageRender {
            store,
            source_rel,
            staged,
            ..
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
            Ok(())
        }
        PlanFileOp::CreateLink { target, source, .. }
        | PlanFileOp::ReplaceLink { target, source, .. } => {
            validate_link_op(ctx, idx, target, source)?;
            Ok(())
        }
        PlanFileOp::BackupAndLink {
            target,
            source,
            backup,
            ..
        } => {
            validate_link_op(ctx, idx, target, source)?;
            validate_backup_path(idx, target, backup)?;
            Ok(())
        }
        PlanFileOp::RemoveLink { target, source, .. } => {
            if has_parent_dir(Path::new(target)) {
                return Err(format!("op {idx}: target '{target}' contains '..'"));
            }
            let store = if let Some(src) = source {
                if has_parent_dir(Path::new(src)) {
                    return Err(format!("op {idx}: source '{src}' contains '..'"));
                }
                source_store(src, ctx.repo_root)
                    .ok_or_else(|| format!("op {idx}: cannot derive store from source '{src}'"))?
            } else {
                find_store_for_target(ctx.repo_root, ctx.config, Path::new(target)).ok_or_else(
                    || {
                        format!(
                            "op {idx}: target {target} is not under any configured store target"
                        )
                    },
                )?
            };
            if !ctx.config.stores.contains_key(&store) {
                return Err(format!("op {idx}: unknown store '{store}'"));
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

    // For staged sources, derive the link name and ensure the template exists.
    if let Some(staged_store) = staged_store(source_path) {
        if staged_store != source_store {
            return Err(format!(
                "op {idx}: staged path store '{staged_store}' does not match source store"
            ));
        }
        let rel = source_path
            .strip_prefix(render::store_render_dir(ctx.repo_root, &source_store))
            .map_err(|_| format!("op {idx}: staged path is not under render dir"))?;
        let link_rel = rel.to_string_lossy().into_owned();
        let source_rel = link_rel + render::TMPL_SUFFIX;
        let tmpl = ctx.repo_root.join(&source_store).join(&source_rel);
        if !tmpl.is_file() {
            return Err(format!(
                "op {idx}: template source does not exist: {source_rel}"
            ));
        }
    } else {
        // Plain source under store directory.
        let rel = source_path
            .strip_prefix(ctx.repo_root.join(&source_store))
            .map_err(|_| format!("op {idx}: source is not under store '{source_store}'"))?;
        let rel_str = rel.to_string_lossy().into_owned();
        if !is_safe_fragment(&rel_str) {
            return Err(format!("op {idx}: invalid source fragment '{rel_str}'"));
        }
        if rel_str.ends_with(render::TMPL_SUFFIX) {
            return Err(format!("op {idx}: template source must use staged path"));
        }
        if !ctx.repo_root.join(&source_store).join(&rel_str).is_file() {
            return Err(format!("op {idx}: source file does not exist: {rel_str}"));
        }
    }

    // Target must fall under a configured target path for this store.
    let target_path = Path::new(target);
    if has_parent_dir(target_path) {
        return Err(format!("op {idx}: target '{target}' contains '..'"));
    }
    if !is_under_any_target(ctx.config, &source_store, target_path) {
        return Err(format!(
            "op {idx}: target {target} is not under a configured target for store '{source_store}'"
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
    Ok(())
}
