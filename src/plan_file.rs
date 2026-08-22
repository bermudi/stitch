//! Plan file format and construction for `stitch plan` and `stitch apply --plan`.
//!
//! The on-disk plan file is a stable, versioned artifact. It carries hashes
//! for staged renders and the config+platform fingerprint so that `apply --plan`
//! can refuse to execute anything that shifted between capture and execution.
//!
//! This module owns the serialized shape (`PlanFile`/`PlanFileOp`/...), plan
//! construction (`build_plan_file`), the config fingerprint (`compute_config_hash`),
//! and the preflight source/path helpers shared with the executor. Execution
//! lives in `plan_exec`; untrusted-input validation lives in `plan_validate`.

use crate::config::{self, Loaded};
use crate::error::{FailureClass, StitchError};
use crate::linker;
use crate::plan::{LinkRequires, Plan, PlanOp, TargetState, path_to_string};
use crate::platform::Platform;
use crate::render;
use crate::store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

pub const PLAN_SCHEMA: u32 = 3;
pub const PLAN_KIND: &str = "stitch/plan";

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
    /// Stores owning executable operations. This list must exactly match the
    /// operations and controls which per-store hooks run; it does not preserve
    /// or authenticate the `plan --only` capture filter.
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
        #[serde(default)]
        store: String,
        target: String,
        source: String,
        requires: PlanFileRequires,
    },

    ReplaceLink {
        #[serde(default)]
        store: String,
        target: String,
        source: String,
        requires: PlanFileRequires,
    },

    BackupAndLink {
        #[serde(default)]
        store: String,
        target: String,
        backup: String,
        source: String,
        requires: PlanFileRequires,
    },

    RemoveLink {
        /// Store that owns this stale-link cleanup. Required even when the
        /// vanished source cannot be reconstructed from current config.
        store: String,
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
    pub fn op_store(&self, _repo_root: &Path) -> Option<String> {
        match self {
            PlanFileOp::StageRender { store, .. } => Some(store.clone()),
            PlanFileOp::CreateLink { store, .. }
            | PlanFileOp::ReplaceLink { store, .. }
            | PlanFileOp::BackupAndLink { store, .. } => Some(store.clone()),
            PlanFileOp::RemoveLink { store, .. } => Some(store.clone()),
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
        let mut removed_ancestors = BTreeSet::new();
        for op in link_ops.drain(..) {
            let Some(target) = link_target(&op) else {
                ops.push(op);
                continue;
            };
            match symlinked_ancestor(Path::new(target), &removed_ancestors) {
                Ok(Some(ancestor)) => conflicts.push(PlanConflict {
                    target: path_to_string(&ancestor),
                    kind: "symlink_ancestor".into(),
                    resolves_to: std::fs::read_link(&ancestor)
                        .ok()
                        .map(|p| path_to_string(&p)),
                }),
                Ok(None) => {
                    if matches!(op, PlanFileOp::RemoveLink { .. }) {
                        removed_ancestors.insert(PathBuf::from(target));
                    }
                    ops.push(op);
                }
                Err(message) => errors.push(PlanError {
                    target: Some(target.into()),
                    message,
                    class: FailureClass::Internal.id().into(),
                }),
            }
        }

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
            } else if let PlanOp::Error { message, class, .. } = op {
                errors.push(PlanError {
                    target: PlanOp::target(op).map(str::to_owned),
                    message: message.clone(),
                    class: class.clone(),
                });
            }
        }
    }

    // Only stores owning executable operations may run plan hooks. An editable
    // `stores` list is not authority to run an otherwise unrelated hook.
    let op_stores: BTreeSet<String> = ops.iter().filter_map(|op| op.op_store(repo_root)).collect();
    let stores = plan
        .stores
        .iter()
        .map(|store| store.store_name.clone())
        .filter(|store| op_stores.contains(store))
        .collect();

    Ok(PlanFile {
        schema: PLAN_SCHEMA,
        kind: PLAN_KIND.into(),
        repo: path_to_string(repo_root),
        config_sha256,
        platform: platform_fp,
        ops,
        stores,
        conflicts,
        errors,
    })
}

/// Link operations can never traverse a symlinked target ancestor. Keeping
/// this check at plan capture makes the serialized plan safe to inspect and
/// prevents an external target gateway from becoming executable later.
fn symlinked_ancestor(
    target: &Path,
    removed_ancestors: &BTreeSet<PathBuf>,
) -> Result<Option<PathBuf>, String> {
    let home = config::expand_home("~").ok();
    for ancestor in target.ancestors().skip(1) {
        if removed_ancestors.contains(ancestor) {
            continue;
        }
        if let Some(ref h) = home
            && ancestor == h
        {
            continue;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(ancestor.to_path_buf())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "could not inspect target ancestor {}: {e}",
                    ancestor.display()
                ));
            }
        }
    }
    Ok(None)
}

fn link_target(op: &PlanFileOp) -> Option<&str> {
    match op {
        PlanFileOp::CreateLink { target, .. }
        | PlanFileOp::ReplaceLink { target, .. }
        | PlanFileOp::BackupAndLink { target, .. }
        | PlanFileOp::RemoveLink { target, .. } => Some(target),
        PlanFileOp::StageRender { .. } | PlanFileOp::RemoveStaged { .. } => None,
    }
}

/// Collect the link targets and the set of ancestor paths that will be
/// explicitly removed before child links are created (whole-directory →
/// file-mode promotion roots).
pub(crate) fn plan_link_targets(ops: &[PlanFileOp]) -> (Vec<PathBuf>, BTreeSet<PathBuf>) {
    let mut removed = BTreeSet::new();
    for op in ops {
        if let PlanFileOp::RemoveLink { target, .. } = op {
            removed.insert(PathBuf::from(target));
        }
    }
    let mut targets = Vec::new();
    for op in ops {
        if let Some(target) = link_target(op) {
            targets.push(PathBuf::from(target));
        }
    }
    (targets, removed)
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
                let target_path = config::expand_home(&target.target).map_err(StitchError::from)?;
                store::collect_reconciliation_keeps(
                    repo_root,
                    store_dir,
                    &target_path,
                    &target.files,
                    &target.patterns,
                    &target.sources,
                    &target.ignore,
                    &mut keep_staged,
                    &mut target_keep_links,
                );
            }
        } else if let Some(target) = &store.target {
            let target_path = config::expand_home(target).map_err(StitchError::from)?;
            store::collect_reconciliation_keeps(
                repo_root,
                store_dir,
                &target_path,
                &store.files,
                &store.patterns,
                &store.sources,
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
            if let PlanFileOp::StageRender { staged, .. } = &render {
                // Keep-set membership is by staging identity: the link name
                // (a `sources` key keeps its literal spelling).
                if let Ok(rel) =
                    Path::new(staged).strip_prefix(render::store_render_dir(repo_root, store_name))
                {
                    keep_staged.insert(rel.to_string_lossy().into_owned());
                }
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
                    store: store_name.to_owned(),
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
                    store: store_name.to_owned(),
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
                    store: store_name.to_owned(),
                    target: target.clone(),
                    source: source.clone(),
                    backup: backup.clone(),
                    requires: requires.clone().into(),
                });
            }
            PlanOp::RemoveLink {
                store,
                target,
                source,
                requires,
            } => {
                links.push(PlanFileOp::RemoveLink {
                    store: store.clone(),
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
            PlanOp::StageRender { .. }
            | PlanOp::RemoveStaged { .. }
            | PlanOp::SkippedPlatform
            | PlanOp::Error { .. } => {}
        }
    }

    // Emit staged-render cleanup for any stale renders in this store.
    // A store that is skipped on this platform is not swept: its ops would be
    // rejected as unexecutable at apply time.
    let store_active = store_config.is_some_and(|s| platform.matches_when(&s.when));
    let staged_dir = render::store_render_dir(repo_root, store_name);
    if store_active {
        render::preflight_staged_path(repo_root, store_name, ".stitch-scan")
            .map_err(StitchError::internal)?;
        match std::fs::symlink_metadata(&staged_dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(StitchError::internal(format!(
                    "could not inspect staging {}: {e}",
                    staged_dir.display()
                )));
            }
            Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_dir() => {
                return Err(StitchError::internal(format!(
                    "staging {} is symlinked or not a directory",
                    staged_dir.display()
                )));
            }
            Ok(_) => {
                for entry in walkdir::WalkDir::new(&staged_dir).follow_links(false) {
                    let entry = entry.map_err(|e| {
                        StitchError::internal(format!(
                            "could not scan staging {}: {e}",
                            staged_dir.display()
                        ))
                    })?;
                    if entry.depth() == 0 || entry.file_type().is_dir() {
                        continue;
                    }
                    if !entry.file_type().is_file() {
                        return Err(StitchError::internal(format!(
                            "unexpected non-regular staging entry {}",
                            entry.path().display()
                        )));
                    }
                    let rel = entry.path().strip_prefix(&staged_dir).map_err(|_| {
                        StitchError::internal(format!(
                            "staged path escapes render tree: {}",
                            entry.path().display()
                        ))
                    })?;
                    let rel_str = rel.to_str().ok_or_else(|| {
                        StitchError::internal(format!(
                            "staged path is not valid UTF-8: {}",
                            entry.path().display()
                        ))
                    })?;
                    if !keep_staged.contains(rel_str) {
                        links.push(PlanFileOp::RemoveStaged {
                            store: store_name.into(),
                            rel: rel_str.into(),
                        });
                    }
                }
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
    _store_dir: &Path,
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
    // v0.14: the staging name (the link identity) does not determine the
    // template path — a `sources` template stages under its declared key while
    // its repo path lives elsewhere. Resolve the entry from the loaded state.
    let (tmpl_source, source_rel) =
        resolve_staged_template_source(repo_root, loaded, store_name, &link_rel)
            .map_err(StitchError::plan_stale)?;

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

/// Resolve the configured template entry that stages at `link_rel` for
/// `store_name`, from the loaded (pinned) state. Returns the absolute source
/// path and its identity string (store-relative for `files` entries,
/// repo-relative for `sources` entries).
///
/// In-store reconstruction (`link_rel + ".tmpl"` under the store dir) stopped
/// being sufficient in v0.14: a `sources` template's staging name is its
/// declared key and does not determine the repo path of its source.
pub(crate) fn resolve_staged_template_source(
    repo_root: &Path,
    loaded: &Loaded,
    store_name: &str,
    link_rel: &str,
) -> Result<(PathBuf, String), String> {
    let Some(store) = loaded.config.stores.get(store_name) else {
        return Err(format!("store '{store_name}' is not configured"));
    };
    if !crate::platform::Platform::detect().matches_when(&store.when) {
        return Err(format!(
            "no template entry stages at '{link_rel}' for store '{store_name}'"
        ));
    }
    let store_dir = repo_root.join(store_name);
    let mut found: Option<(PathBuf, String)> = None;
    let mut check = |files: &[String],
                     patterns: &[String],
                     sources: &std::collections::BTreeMap<String, String>,
                     ignore: &[String]| {
        if found.is_some() {
            return;
        }
        if let store::LinkTargets::Files(links) =
            store::resolve_target_names(repo_root, &store_dir, files, patterns, sources, ignore)
        {
            found = links
                .into_iter()
                .find(|link| link.is_template() && link.name == link_rel)
                .map(|link| (link.source, link.source_rel));
        }
    };
    if store.is_multi_target() {
        for target in store.targets.values() {
            if !crate::platform::Platform::detect().matches_when(&target.when) {
                continue;
            }
            check(
                &target.files,
                &target.patterns,
                &target.sources,
                &target.ignore,
            );
        }
    } else {
        check(&store.files, &store.patterns, &store.sources, &store.ignore);
    }
    found
        .ok_or_else(|| format!("no template entry stages at '{link_rel}' for store '{store_name}'"))
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

pub(crate) fn target_state_id(state: &TargetState) -> String {
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

pub(crate) fn target_state_from(
    target: &str,
    value: &Option<String>,
) -> Result<TargetState, String> {
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
    let stitch_dir = repo_root.join(".stitch");
    let state_path = stitch_dir.join("state.toml");
    let authored_path = repo_root.join("stitch.toml");
    config::validate_stitch_dir(&stitch_dir)?;
    config::validate_state_file(&state_path)?;
    config::validate_authored_file(&authored_path)?;

    let authored = read_bytes_or_none(&authored_path)?;
    let state = read_bytes_or_none(&state_path)?;
    Ok(config::hash_config_bytes(
        authored.as_deref(),
        state.as_deref(),
    ))
}

/// Read file bytes, returning `None` for `NotFound` and `Some(bytes)` for a
/// present file (including an empty one). Mirrors [`config::hash_config_bytes`]
/// semantics: missing and empty are distinct.
fn read_bytes_or_none(path: &Path) -> Result<Option<Vec<u8>>, StitchError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StitchError::io_context(
            format!("computing config hash: reading {}", path.display()),
            e,
        )),
    }
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    sha256_hex_bytes(&hasher.finalize())
}

fn sha256_hex_bytes(digest: &sha2::digest::Output<Sha256>) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn base_report(plan: &PlanFile) -> PlanExecReport {
    PlanExecReport {
        ops_total: plan.ops.len(),
        ops_executed: Vec::new(),
        ops_remaining: plan.ops.iter().map(op_description).collect(),
        conflicts: plan.conflicts.clone(),
        staged: Vec::new(),
        warnings: Vec::new(),
    }
}

pub(crate) fn sync_ops_remaining(
    report: &mut PlanExecReport,
    plan: &PlanFile,
    remaining: &BTreeSet<usize>,
) {
    report.ops_remaining = remaining
        .iter()
        .map(|&idx| op_description(&plan.ops[idx]))
        .collect();
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

pub(crate) fn source_store(source: &str, repo_root: &Path) -> Option<String> {
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

pub(crate) fn staged_store(path: &Path) -> Option<String> {
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

/// Verify that a `StageRender` op's staged path and pinned hash are consistent
/// with the fresh in-memory render of its template source.
pub(crate) fn verify_stage_render(
    repo_root: &Path,
    loaded: &Loaded,
    platform: &Platform,
    store: &str,
    source_rel: &str,
    staged: &str,
    sha256: &str,
) -> Result<PathBuf, String> {
    let staged_path = Path::new(staged);
    let link_rel = staged_path
        .strip_prefix(render::store_render_dir(repo_root, store))
        .map_err(|_| format!("staged path outside render tree: {staged}"))?
        .to_string_lossy()
        .into_owned();
    if link_rel.is_empty() {
        return Err(format!("staged path has no link identity: {staged}"));
    }
    let (source_path, resolved_rel) =
        resolve_staged_template_source(repo_root, loaded, store, &link_rel)?;
    if resolved_rel != source_rel {
        return Err(format!(
            "template identity drifted: plan says '{source_rel}', state says '{resolved_rel}'"
        ));
    }
    let expected_staged = render::staging_path(repo_root, store, &link_rel);
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
pub(crate) fn plan_source_root(repo_root: &Path, source: &Path) -> Result<PathBuf, String> {
    if let Some(store) = staged_store(source) {
        return Ok(render::store_render_dir(repo_root, &store));
    }
    let rel = source
        .strip_prefix(repo_root)
        .map_err(|_| format!("source {} is not under a store", source.display()))?;
    let mut comps = rel.components();
    let first = comps
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| format!("source {} is not under a store", source.display()))?;
    // Root-level shared sources (e.g. "hub.txt" at repo root) have a single
    // component; their source root is the repo itself, not a file-named store dir.
    if comps.next().is_none() {
        // Check if the first component is a file at repo root (shared) vs a
        // store directory. If it's a file directly under repo_root, use repo_root.
        // Otherwise treat as store dir (e.g. "shared/hub.txt" where "shared" is
        // a directory). Heuristic: if repo_root/first is a file, it's a root-level
        // shared source; if it's a directory, it's a store or shared dir.
        let candidate = repo_root.join(first);
        if std::fs::symlink_metadata(&candidate).is_ok_and(|m| m.is_file() || m.is_symlink()) {
            return Ok(repo_root.to_path_buf());
        }
        // Fallback: single-component source that is not a file at repo root is
        // likely a whole-dir store path; treat as store (will be validated as dir later).
        return Ok(candidate);
    }
    Ok(repo_root.join(first))
}

pub(crate) fn check_source_exists_for_preflight(
    repo_root: &Path,
    source: &str,
) -> Result<(), String> {
    let source_path = Path::new(source);
    match std::fs::symlink_metadata(source_path) {
        Ok(_) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && source_path.starts_with(render::render_root(repo_root)) =>
        {
            // A preceding StageRender op creates this source.
            return Ok(());
        }
        Err(_) => return Err(format!("source does not exist: {source}")),
    }
    let source_root = plan_source_root(repo_root, source_path)?;
    linker::validate_source_in(source_path, &source_root).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn config_hash_rejects_symlinked_state_file() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::write(tmp.path().join("stitch.toml"), "").unwrap();

        let external = tempfile::tempdir().unwrap();
        let external_state = external.path().join("state.toml");
        fs::write(&external_state, "[stores.app]\ntarget = \"~\"\n").unwrap();

        let state = stitch_dir.join("state.toml");
        symlink(external_state, &state).unwrap();

        let err = compute_config_hash(tmp.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing symlinked or non-regular state file"),
            "got: {err}"
        );
    }

    #[test]
    fn config_hash_rejects_hard_linked_state_file() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::write(tmp.path().join("stitch.toml"), "").unwrap();

        let external = tempfile::tempdir().unwrap();
        let external_state = external.path().join("state.toml");
        fs::write(&external_state, "[stores.app]\ntarget = \"~\"\n").unwrap();

        let state = stitch_dir.join("state.toml");
        fs::hard_link(&external_state, &state).unwrap();

        let err = compute_config_hash(tmp.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing hard-linked state file (multiple paths to the same inode)"),
            "got: {err}"
        );
    }

    #[test]
    fn config_hash_rejects_symlinked_stitch_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::write(stitch_dir.join("state.toml"), "").unwrap();

        let external = tempfile::tempdir().unwrap();
        let external_authored = external.path().join("stitch.toml");
        fs::write(
            &external_authored,
            "[stores.app]\nhooks = { pre = 'touch /tmp/pwned' }\n",
        )
        .unwrap();

        let authored = tmp.path().join("stitch.toml");
        symlink(external_authored, &authored).unwrap();

        let err = compute_config_hash(tmp.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing symlinked or non-regular authored config file"),
            "got: {err}"
        );
    }

    #[test]
    fn config_hash_rejects_hard_linked_stitch_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        fs::create_dir_all(&stitch_dir).unwrap();
        fs::write(stitch_dir.join("state.toml"), "").unwrap();

        let external = tempfile::tempdir().unwrap();
        let external_authored = external.path().join("stitch.toml");
        fs::write(
            &external_authored,
            "[stores.app]\nhooks = { pre = 'touch /tmp/pwned' }\n",
        )
        .unwrap();

        let authored = tmp.path().join("stitch.toml");
        fs::hard_link(&external_authored, &authored).unwrap();

        let err = compute_config_hash(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains(
                "refusing hard-linked authored config file (multiple paths to the same inode)"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn config_hash_rejects_symlinked_stitch_dir() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("stitch.toml"), "").unwrap();

        let external = tempfile::tempdir().unwrap();
        fs::create_dir_all(external.path().join(".stitch")).unwrap();
        fs::write(
            external.path().join(".stitch").join("state.toml"),
            "[stores.app]\ntarget = \"~\"\n",
        )
        .unwrap();

        let stitch = tmp.path().join(".stitch");
        symlink(external.path().join(".stitch"), &stitch).unwrap();

        let err = compute_config_hash(tmp.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing symlinked or non-directory state directory"),
            "got: {err}"
        );
    }
}
