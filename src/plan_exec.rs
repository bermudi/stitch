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
use crate::store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

pub const PLAN_SCHEMA: u32 = 1;
pub const PLAN_KIND: &str = "stitch/plan";

/// True if `p` contains any `..` path component.
fn has_parent_dir(p: &Path) -> bool {
    p.components().any(|c| c == Component::ParentDir)
}

/// Whether a resolved symlink target (the destination path, not a path that
/// is itself a symlink) lies inside `repo_root`. Existing paths are canonicalized;
/// dangling or not-yet-created paths are compared after lexical normalization.
fn resolved_target_points_into_repo(resolved: &Path, repo_root: &Path) -> bool {
    let normalized_root = if repo_root.exists() {
        repo_root
            .canonicalize()
            .unwrap_or_else(|_| linker::normalize_lexical(repo_root))
    } else {
        linker::normalize_lexical(repo_root)
    };
    let normalized = if resolved.exists() {
        resolved
            .canonicalize()
            .unwrap_or_else(|_| linker::normalize_lexical(resolved))
    } else {
        linker::normalize_lexical(resolved)
    };
    normalized.starts_with(&normalized_root)
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
    /// Selected store names (the `stitch plan --only` scope, or all stores).
    /// Used to schedule per-store hooks even when a selected store has no
    /// filesystem ops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stores: Vec<String>,
    pub conflicts: Vec<PlanConflict>,
    #[serde(default)]
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

    RemoveStaged {
        store: String,
        rel: String,
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
            PlanFileOp::RemoveStaged { store, .. } => Some(store.clone()),
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
        stores: plan.stores.iter().map(|s| s.store_name.clone()).collect(),
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
    let mut keep_staged: BTreeSet<String> = BTreeSet::new();
    let store_config = loaded.config.stores.get(store_name);

    // Reconciliation is based on configured sources, not only successful plan
    // ops. Per-target `when` skips and render/resolution errors produce no
    // render op, but their existing staged output must remain live. This
    // mirrors `store::apply_store` and prevents preserved links from dangling.
    let mut target_keep_links = BTreeMap::new();
    if let Some(store) = store_config {
        if store.is_multi_target() {
            for target in store.targets.values() {
                store::collect_reconciliation_keeps(
                    store_dir,
                    &config::expand_home(&target.target),
                    &target.files,
                    &target.patterns,
                    &target.ignore,
                    &mut keep_staged,
                    &mut target_keep_links,
                );
            }
        } else if let Some(target) = &store.target {
            store::collect_reconciliation_keeps(
                store_dir,
                &config::expand_home(target),
                &store.files,
                &store.patterns,
                &store.ignore,
                &mut keep_staged,
                &mut target_keep_links,
            );
        }
    }

    for op in ops {
        let maybe_render =
            stage_render_for_op(repo_root, loaded, store_name, store_dir, op, platform)?;
        if let Some(render) = maybe_render {
            if let PlanFileOp::StageRender { source_rel, .. } = &render {
                keep_staged.insert(render::resolve_entry(source_rel).link_rel.clone());
            }
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
            PlanOp::Conflict { target, .. } => {
                // A conflicted but still-configured template keeps its staged
                // render, matching `reconcile_store_staging` in a normal apply.
                if let Some(source) = store::resolve_link_source(
                    repo_root,
                    store_dir,
                    store_config,
                    store_name,
                    &PathBuf::from(target),
                ) {
                    maybe_keep_staged(repo_root, store_name, &source, &mut keep_staged);
                }
            }
            PlanOp::StageRender { .. } | PlanOp::SkippedPlatform | PlanOp::Error { .. } => {}
        }
    }

    // Emit staged-render cleanup for any stale renders in this store.
    // A store that is skipped on this platform is not swept: its ops would be
    // rejected as unexecutable at apply time.
    let store_active = store_config.is_some_and(|s| platform.matches_when(&s.when));
    let staged_dir = render::store_render_dir(repo_root, store_name);
    if store_active && staged_dir.exists() {
        for entry in walkdir::WalkDir::new(&staged_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = match entry.path().strip_prefix(&staged_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().into_owned();
            if !keep_staged.contains(&rel_str) {
                links.push(PlanFileOp::RemoveStaged {
                    store: store_name.into(),
                    rel: rel_str,
                });
            }
        }
    }

    Ok((renders, links))
}

/// If `source` is a staged path for `store_name`, add its link_rel to `keep`.
fn maybe_keep_staged(
    repo_root: &Path,
    store_name: &str,
    source: &str,
    keep: &mut BTreeSet<String>,
) {
    let staged_dir = render::store_render_dir(repo_root, store_name);
    let source_path = Path::new(source);
    if !source_path.starts_with(&staged_dir) {
        return;
    }
    if let Ok(rel) = source_path.strip_prefix(&staged_dir) {
        keep.insert(rel.to_string_lossy().into_owned());
    }
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
    hasher.update(b"stitch/config-hash/v2\0");

    let files = [
        ("stitch.toml", repo_root.join("stitch.toml")),
        (
            ".stitch/state.toml",
            repo_root.join(".stitch").join("state.toml"),
        ),
    ];
    for (label, path) in files {
        // The label and presence marker domain-separate the two config
        // components and distinguish a missing file from an existing empty
        // file. The length also prevents concatenation-boundary collisions.
        hasher.update(label.as_bytes());
        hasher.update([0]);
        match std::fs::read(&path) {
            Ok(bytes) => {
                hasher.update([1]);
                hasher.update((bytes.len() as u64).to_be_bytes());
                hasher.update(bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                hasher.update([0]);
                hasher.update(0u64.to_be_bytes());
            }
            Err(e) => return Err(e.into()),
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
        warnings: Vec::new(),
    }
}

fn sync_ops_remaining(report: &mut PlanExecReport, plan: &PlanFile, remaining: &BTreeSet<usize>) {
    report.ops_remaining = remaining
        .iter()
        .map(|&idx| op_description(&plan.ops[idx]))
        .collect();
}

/// Verify that a `StageRender` op's staged path and pinned hash are consistent
/// with the fresh in-memory render of its template source.
fn verify_stage_render(
    repo_root: &Path,
    loaded: &Loaded,
    platform: &Platform,
    store: &str,
    source_rel: &str,
    staged: &str,
    sha256: &str,
) -> Result<PathBuf, String> {
    let source_path = repo_root.join(store).join(source_rel);
    let expected_staged = render::staging_path(
        repo_root,
        store,
        &render::resolve_entry(source_rel).link_rel,
    );
    if path_to_string(&expected_staged) != staged {
        return Err(format!(
            "staged path mismatch: expected {}",
            expected_staged.display()
        ));
    }
    let content = render::render_file(&source_path, source_rel, platform, &loaded.config.vars)
        .map_err(|e| format!("render failed: {e}"))?;
    if sha256_hex(&content) != sha256 {
        return Err("render hash mismatch".into());
    }
    Ok(source_path)
}

/// Check that a link source exists, unless it is a staged render (which may be
/// created by an earlier `StageRender` op in the same plan).
fn check_source_exists_for_preflight(repo_root: &Path, source: &str) -> Result<(), String> {
    let source_path = Path::new(source);
    if source_path.starts_with(render::render_root(repo_root)) {
        return Ok(());
    }
    if std::fs::symlink_metadata(source_path).is_err() {
        return Err(format!("source does not exist: {source}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Preflight state simulation
// ---------------------------------------------------------------------------

/// Tracks the predicted filesystem state across a plan's ops so that later
/// ops are preflighted against the *simulated* result of earlier ones.
struct PreflightState<'a> {
    repo_root: &'a Path,
    config: &'a Config,
    platform: &'a Platform,
    overrides: BTreeMap<PathBuf, TargetState>,
}

#[derive(Debug, Clone)]
struct RenderPin {
    source_rel: String,
    staged: String,
}

impl<'a> PreflightState<'a> {
    fn new(repo_root: &'a Path, config: &'a Config, platform: &'a Platform) -> Self {
        Self {
            repo_root,
            config,
            platform,
            overrides: BTreeMap::new(),
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
        // `create_dir_all` follows symlinks for any missing ancestor, so every
        // ancestor of the target (not just the immediate parent) must be safe.
        for ancestor in path.ancestors().skip(1) {
            match self.get_effective_state(ancestor) {
                TargetState::Absent => continue,
                TargetState::RealEntry => {
                    // If the plan created this ancestor, it will be a real directory.
                    if self
                        .overrides
                        .get(ancestor)
                        .is_some_and(|s| matches!(s, TargetState::RealEntry))
                        || ancestor.is_dir()
                    {
                        continue;
                    } else {
                        return Err(format!("parent {} is not a directory", ancestor.display()));
                    }
                }
                TargetState::SymlinkTo(value) => {
                    // A symlinked ancestor is only safe when it resolves to a directory
                    // *outside* the repository. A repo-pointing ancestor would cause the
                    // operation to write through the symlink into the repo.
                    let resolved = if self.overrides.contains_key(ancestor) {
                        let target = Path::new(&value);
                        if target.is_absolute() {
                            target.to_path_buf()
                        } else {
                            ancestor.parent().unwrap_or(Path::new(".")).join(target)
                        }
                    } else {
                        ancestor.canonicalize().map_err(|e| {
                            format!(
                                "parent {} is a symlink that cannot be resolved: {e}",
                                ancestor.display()
                            )
                        })?
                    };
                    let points_into_repo = if self.overrides.contains_key(ancestor) {
                        resolved_target_points_into_repo(&resolved, self.repo_root)
                    } else {
                        linker::points_into_repo(ancestor, self.repo_root)
                    };
                    if points_into_repo {
                        return Err(format!(
                            "parent {} is a symlink into the repository; refusing to write through it",
                            ancestor.display()
                        ));
                    } else if resolved.is_dir() {
                        continue;
                    } else {
                        return Err(format!(
                            "parent {} resolves to {} which is not a directory",
                            ancestor.display(),
                            resolved.display()
                        ));
                    }
                }
                TargetState::SymlinkIntoRepo => {
                    // The symlink target cannot be read, so we cannot verify that the
                    // parent resolves to a directory. Keep the conservative error.
                    return Err(format!("parent {} is a symlink", ancestor.display()));
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

                // For wildcard removals, require the link to point into the
                // specific store's source or staging tree (not just the repo).
                if source.is_none() {
                    let store = find_store_for_target(self.repo_root, self.config, target_path)
                        .ok_or_else(|| {
                            format!("target {target} is not under any configured store target")
                        })?;
                    let store_dir = self.repo_root.join(&store);
                    let staged_dir = render::store_render_dir(self.repo_root, &store);
                    if !linker::points_into(target_path, &store_dir)
                        && !linker::points_into(target_path, &staged_dir)
                    {
                        return Err(format!(
                            "target {target} does not point into store '{store}'"
                        ));
                    }
                }

                self.overrides
                    .insert(target_path.to_path_buf(), TargetState::Absent);
                Ok(())
            }
            PlanFileOp::RemoveStaged { store, rel } => {
                let staged_dir = render::store_render_dir(self.repo_root, store);
                let staged_path = staged_dir.join(rel);
                if !staged_path.starts_with(&staged_dir) {
                    return Err("staged path escapes render tree".into());
                }
                // A stale render may already be gone; missing is not a failure.
                Ok(())
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

    // Untrusted-input validation: every op must be justified by the pinned
    // config. This is the security boundary for hand-edited plan files.
    let validation_context = ValidationContext::new(repo_root, &loaded.config);
    let mut rendered: BTreeMap<(String, String), RenderPin> = BTreeMap::new();
    for (idx, op) in plan.ops.iter().enumerate() {
        validate_op(&validation_context, idx, op, &mut rendered).map_err(|e| {
            PlanExecError::new(
                base_report(plan),
                StitchError::plan_stale(format!("plan validation failed: {e}")),
            )
        })?;
    }

    // Build the store-grouped execution sequence first. The executor runs each
    // selected store's ops in `selected_stores` order (see the loop below); the
    // preflight must simulate that exact order so cross-store ordering and path
    // interactions are checked before any filesystem mutation.
    let mut report = base_report(plan);
    let mut remaining: BTreeSet<usize> = (0..plan.ops.len()).collect();

    // Group ops by store, preserving each store's plan order while retaining
    // the original operation indices for accurate remainder reporting.
    let mut ops_by_store: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, op) in plan.ops.iter().enumerate() {
        let Some(op_store) = op.op_store(repo_root, &loaded.config) else {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!("op {idx}: cannot derive store for execution")),
            ));
        };
        ops_by_store.entry(op_store).or_default().push(idx);
    }

    let selected_stores: Vec<String> = if plan.stores.is_empty() {
        loaded.config.stores.keys().cloned().collect()
    } else {
        plan.stores.clone()
    };
    let selected_set: BTreeSet<String> = selected_stores.iter().cloned().collect();

    for store_name in &selected_stores {
        if !loaded.config.stores.contains_key(store_name) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(
                report,
                StitchError::plan_stale(format!("selected store '{store_name}' not in config")),
            ));
        }
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
    for store_name in &selected_stores {
        let store = &loaded.config.stores[store_name];
        if !platform.matches_when(&store.when)
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

    // Preflight the execution sequence against a simulated filesystem state.
    let mut state = PreflightState::new(repo_root, &loaded.config, &platform);
    for &idx in &exec_order {
        let op = &plan.ops[idx];
        state.apply_op(loaded, op).map_err(|e| {
            PlanExecError::new(
                base_report(plan),
                StitchError::plan_stale(format!("preflight failed for op {idx}: {e}")),
            )
        })?;
    }

    if dry_run {
        sync_ops_remaining(&mut report, plan, &remaining);
        if !plan.conflicts.is_empty() || !plan.errors.is_empty() {
            return Err(PlanExecError::new(report, plan_exec_error(plan)));
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
    if let Err(e) = hooks::run_global_hook(repo_root, "pre-apply", &env, &platform) {
        sync_ops_remaining(&mut report, plan, &remaining);
        return Err(PlanExecError::new(
            report,
            StitchError::hook("pre-apply", e),
        ));
    }

    for store_name in &selected_stores {
        let store = &loaded.config.stores[store_name];
        if !platform.matches_when(&store.when) {
            continue;
        }

        if let Err(e) = run_store_pre_hook(repo_root, store_name, &loaded.config, &platform) {
            sync_ops_remaining(&mut report, plan, &remaining);
            return Err(PlanExecError::new(report, e));
        }

        if let Some(indices) = ops_by_store.get(store_name) {
            for &idx in indices {
                let op = &plan.ops[idx];

                // Re-check the precondition immediately before acting.
                if let Err(e) = preflight_op(repo_root, loaded, &platform, op) {
                    sync_ops_remaining(&mut report, plan, &remaining);
                    return Err(PlanExecError::new(
                        report,
                        StitchError::plan_stale(format!(
                            "op {idx} ({}) precondition changed: {e}",
                            op_description(op)
                        )),
                    ));
                }

                match execute_op(repo_root, loaded, &platform, op, idx, &mut report) {
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

        if let Some(warning) = run_store_post_hook(repo_root, store_name, &loaded.config, &platform)
        {
            report.warnings.push(warning);
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
    if let Err(e) = hooks::run_global_hook(repo_root, "post-apply", &env, &platform) {
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
) -> Option<String> {
    let store = config.stores.get(store_name)?;
    if let Some(post) = &store.hooks.post {
        let env = HookEnv {
            root: repo_root,
            store: Some(store_name),
            target: store.target.as_deref(),
            action: "apply",
        };
        if let Err(e) = hooks::run_store_hook(post, &env, platform) {
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
        PlanFileOp::RemoveStaged { store, rel } => format!("remove_staged {store}/{rel}"),
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

/// Check that every ancestor of `target` is safe to create directories under.
/// This is the per-operation re-check that runs after hooks, so a hook cannot
/// swap in a repo-pointing symlink after the initial preflight has passed.
fn check_ancestors_writable(
    repo_root: &Path,
    loaded: &Loaded,
    platform: &Platform,
    target: &Path,
) -> Result<(), String> {
    let state = PreflightState::new(repo_root, &loaded.config, platform);
    state.parent_is_writable_dir(target)
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
            Ok(())
        }
        PlanFileOp::CreateLink {
            target,
            source,
            requires,
        } => {
            check_ancestors_writable(repo_root, loaded, platform, Path::new(target))?;
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
            check_ancestors_writable(repo_root, loaded, platform, Path::new(target))?;
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
            check_ancestors_writable(repo_root, loaded, platform, Path::new(target))?;
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
            check_ancestors_writable(repo_root, loaded, platform, Path::new(target))?;
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
                    // The link points exactly at `expected` (checked above).
                    // Accept it as repo-owned if either the broad canonical
                    // check resolves the link into the repo, or the exact-entry
                    // check recognizes `expected` as a configured repo source.
                    // The OR is needed because `expected` may be the canonical
                    // readlink (whole-dir link, repo root reached through a
                    // symlink — only the broad check matches) or a configured
                    // source path that is itself a symlink resolving outside
                    // the repo (source-symlink entry — only the exact-entry
                    // check matches). This mirrors the remove_link_to /
                    // remove_link checks used at execution time.
                    if !(linker::points_into_repo(target_path, repo_root)
                        || linker::points_at_source(target_path, Path::new(&expected), repo_root))
                    {
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
        PlanFileOp::RemoveStaged { store, rel } => {
            if !loaded.config.stores.contains_key(store) {
                return Err(format!("unknown store '{store}'"));
            }
            let rel_path = Path::new(rel);
            if rel_path.is_absolute() || has_parent_dir(rel_path) || !is_safe_fragment(rel) {
                return Err(format!("invalid staged rel '{rel}'"));
            }
            let staged_dir = render::store_render_dir(repo_root, store);
            let staged_path = staged_dir.join(rel);
            if !staged_path.starts_with(&staged_dir) {
                return Err("staged path escapes render tree".into());
            }
            // Stale renders may already have been cleaned up by hand; a missing
            // file is not a preflight failure.
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

fn create_link_for_plan(repo_root: &Path, target: &Path, source: &Path) -> Result<(), String> {
    // Re-derive the configured source root at the mutation boundary so a hook
    // cannot replace a source ancestor with a gateway into another store or
    // outside the repo after plan preflight.
    let source_root = if let Some(store) = staged_store(source) {
        render::store_render_dir(repo_root, &store)
    } else {
        let store = source
            .strip_prefix(repo_root)
            .ok()
            .and_then(|path| path.components().next())
            .and_then(|component| component.as_os_str().to_str())
            .ok_or_else(|| format!("source {} is not under a store", source.display()))?;
        repo_root.join(store)
    };
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

            std::fs::rename(target_path, backup_path).map_err(|e| format!("{e}"))?;
            if let Err(e) = create_link_for_plan(repo_root, target_path, source_path) {
                // Restore the original on failure.
                let _ = std::fs::rename(backup_path, target_path);
                return Err(e);
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
        PlanFileOp::RemoveStaged { store, rel } => {
            let staged_dir = render::store_render_dir(repo_root, store);
            let staged_path = staged_dir.join(rel);
            if !staged_path.starts_with(&staged_dir) {
                return Err("staged path escapes render tree".into());
            }
            if staged_path.is_file()
                && let Err(e) = std::fs::remove_file(&staged_path)
            {
                return Err(format!("could not remove {}: {e}", staged_path.display()));
            }
            // Prune empty parent directories up to (but not including) the
            // store render directory.
            let mut parent = staged_path.parent();
            while let Some(p) = parent {
                if p == staged_dir || !p.starts_with(&staged_dir) {
                    break;
                }
                if is_dir_empty(p) {
                    if let Err(e) = std::fs::remove_dir(p) {
                        return Err(format!("could not remove {}: {e}", p.display()));
                    }
                    parent = p.parent();
                } else {
                    break;
                }
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
    let mut best: Option<(usize, String)> = None;
    for (name, store) in &config.stores {
        for target_path in target_paths_for_store(store) {
            if target == target_path || target.starts_with(&target_path) {
                let depth = target_path.components().count();
                if best
                    .as_ref()
                    .is_none_or(|(best_depth, _)| depth > *best_depth)
                {
                    best = Some((depth, name.clone()));
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

fn is_under_any_target(config: &Config, store: &str, target: &Path) -> bool {
    config.stores.get(store).is_some_and(|store| {
        target_paths_for_store(store)
            .iter()
            .any(|p| target == p || target.starts_with(p))
    })
}

fn validate_op(
    ctx: &ValidationContext,
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
        PlanFileOp::CreateLink { target, source, .. }
        | PlanFileOp::ReplaceLink { target, source, .. } => {
            validate_link_op(ctx, idx, target, source, rendered)?;
            Ok(())
        }
        PlanFileOp::BackupAndLink {
            target,
            source,
            backup,
            ..
        } => {
            validate_link_op(ctx, idx, target, source, rendered)?;
            validate_backup_path(idx, target, backup)?;
            Ok(())
        }
        PlanFileOp::RemoveLink { target, source, .. } => {
            validate_remove_link_op(ctx, idx, target, source.as_deref())?;
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
    if !is_under_any_target(ctx.config, &source_store, target_path) {
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
    idx: usize,
    target: &str,
    source: Option<&str>,
) -> Result<(), String> {
    let target_path = Path::new(target);
    if has_parent_dir(target_path) {
        return Err(format!("op {idx}: target '{target}' contains '..'"));
    }

    let store = if let Some(src) = source {
        let src_path = Path::new(src);
        if has_parent_dir(src_path) {
            return Err(format!("op {idx}: source '{src}' contains '..'"));
        }
        source_store(src, ctx.repo_root)
            .ok_or_else(|| format!("op {idx}: cannot derive store from source '{src}'"))?
    } else {
        find_store_for_target(ctx.repo_root, ctx.config, target_path).ok_or_else(|| {
            format!("op {idx}: target {target} is not under any configured store target")
        })?
    };

    if !ctx.config.stores.contains_key(&store) {
        return Err(format!("op {idx}: unknown store '{store}'"));
    }
    if !is_under_any_target(ctx.config, &store, target_path) {
        return Err(format!(
            "op {idx}: target {target} is not under a configured target for store '{store}'"
        ));
    }

    let store_dir = ctx.repo_root.join(&store);
    let staged_dir = render::store_render_dir(ctx.repo_root, &store);

    if let Some(src) = source {
        let src_path = Path::new(src);
        if !src_path.starts_with(ctx.repo_root) {
            return Err(format!("op {idx}: source {src} is not under the repo"));
        }

        // The source must be the exact source that the config resolves to for
        // this target. This guards hand-edited plans that swap a stale link's
        // source for an arbitrary repo path.
        let store_config = ctx.config.stores.get(&store).unwrap();
        let expected = store::resolve_link_source(
            ctx.repo_root,
            &store_dir,
            Some(store_config),
            &store,
            target_path,
        )
        .ok_or_else(|| {
            format!(
                "op {idx}: target {target} does not resolve to a configured source in store '{store}'"
            )
        })?;
        if expected != *src {
            return Err(format!(
                "op {idx}: source '{src}' does not match expected source '{expected}'"
            ));
        }
    } else {
        // Wildcard removal: the actual symlink must resolve into this store's
        // source or staging tree, not merely any repo-owned link.
        if !target_path.is_symlink() {
            return Err(format!("op {idx}: target {target} is not a symlink"));
        }
        if !linker::points_into(target_path, &store_dir)
            && !linker::points_into(target_path, &staged_dir)
        {
            return Err(format!(
                "op {idx}: target {target} does not point into store '{store}'"
            ));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ApplyOpts;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn config_hash_distinguishes_missing_from_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::write(tmp.path().join("stitch.toml"), "").unwrap();

        let missing = compute_config_hash(tmp.path()).unwrap();
        fs::write(stitch_dir.join("state.toml"), "").unwrap();
        let empty = compute_config_hash(tmp.path()).unwrap();

        assert_ne!(missing, empty);
    }

    #[test]
    fn whole_dir_removal_from_symlinked_repo_root_executes() {
        let tmp = tempfile::tempdir().unwrap();
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
            },
        );
        let plan = build_plan_file(&repo_alias, &loaded, &computed, &platform).unwrap();
        assert!(plan.ops.iter().any(|op| {
            matches!(op, PlanFileOp::RemoveLink { target: path, .. } if path == &target.display().to_string())
        }));

        execute_plan(&repo_alias, &loaded, &plan, false).unwrap();

        assert!(!target.is_symlink());
        assert!(target.join("profile").is_symlink());
    }
}
