//! Plan file format, validation, and executor for `stitch plan` and
//! `stitch apply --plan`.
//!
//! The on-disk plan file is a stable, versioned artifact. It carries hashes
//! for staged renders and the config+platform fingerprint so that `apply --plan`
//! can refuse to execute anything that shifted between capture and execution.

use crate::config::{self, Config, ConfigError, Loaded, Store, is_safe_fragment};
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
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub const PLAN_SCHEMA: u32 = 2;
pub const PLAN_KIND: &str = "stitch/plan";

fn directory_identity(path: &Path) -> Result<(u64, u64), String> {
    // Repository aliases are supported; pin the directory they resolve to.
    // Repointing an alias during a hook changes this device/inode identity.
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.display()))?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "{} does not resolve to a directory",
            path.display()
        ));
    }
    Ok((meta.dev(), meta.ino()))
}

fn require_directory_identity(
    path: &Path,
    expected: (u64, u64),
    context: &str,
) -> Result<(), String> {
    if directory_identity(path)? != expected {
        return Err(format!("{context}: {}", path.display()));
    }
    Ok(())
}

// ===========================================================================
// Target-ancestor identity pinning across pre-apply and pre-store hooks.
// A hook must not be able to introduce or repoint a symlinked ancestor that
// a link operation would traverse, or silently replace a real directory with
// a different one (bind mount / rename / copy) before that operation runs.
// ===========================================================================

/// The pre-hook identity of one filesystem entry that a link operation would
/// traverse.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetAncestorEntry {
    RealDir { dev: u64, ino: u64 },
    Symlink { dev: u64, ino: u64, target: PathBuf },
    Other { dev: u64, ino: u64 },
}

fn target_ancestor_entry(path: &Path) -> Result<Option<TargetAncestorEntry>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => {
            return Err(format!(
                "could not inspect target ancestor {}: {e}",
                path.display()
            ));
        }
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|e| {
            format!(
                "could not read target ancestor symlink {}: {e}",
                path.display()
            )
        })?;
        return Ok(Some(TargetAncestorEntry::Symlink {
            dev: meta.dev(),
            ino: meta.ino(),
            target,
        }));
    }
    if meta.is_dir() {
        return Ok(Some(TargetAncestorEntry::RealDir {
            dev: meta.dev(),
            ino: meta.ino(),
        }));
    }
    Ok(Some(TargetAncestorEntry::Other {
        dev: meta.dev(),
        ino: meta.ino(),
    }))
}

/// A concrete redirect detected by [`TargetAncestorSnapshot::revalidate`].
#[derive(Debug, Clone)]
pub(crate) enum TargetAncestorRedirect {
    /// The path is a symlink (pre-existing or created by the hook) and a link
    /// operation would have to traverse it.
    Symlinked {
        path: PathBuf,
        resolves_to: Option<PathBuf>,
    },
    /// The path was a real directory and is now a different real directory, or
    /// a to-be-removed ancestor was replaced by something other than the same
    /// entry or absence.
    Redirected {
        path: PathBuf,
        resolves_to: Option<PathBuf>,
    },
    /// The path was a real directory and no longer exists.
    Removed { path: PathBuf },
}

impl std::fmt::Display for TargetAncestorRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlinked { path, resolves_to } => {
                write!(
                    f,
                    "target ancestor {} is a symlinked redirect",
                    path.display()
                )?;
                if let Some(target) = resolves_to {
                    write!(f, " -> {}", target.display())?;
                }
                Ok(())
            }
            Self::Redirected { path, resolves_to } => {
                write!(f, "target ancestor {} was redirected", path.display())?;
                if let Some(target) = resolves_to {
                    write!(f, " -> {}", target.display())?;
                }
                Ok(())
            }
            Self::Removed { path } => {
                write!(f, "target ancestor {} was removed", path.display())
            }
        }
    }
}

/// Snapshot of the filesystem identity of target ancestor directories.
///
/// The capture is taken before a hook; [`TargetAncestorSnapshot::revalidate`]
/// is called after the hook and before any link creation through those
/// ancestors.
#[derive(Debug, Clone)]
pub(crate) struct TargetAncestorSnapshot {
    removed: BTreeSet<PathBuf>,
    identities: BTreeMap<PathBuf, Option<TargetAncestorEntry>>,
}

impl TargetAncestorSnapshot {
    /// Capture the pre-hook identity of every target ancestor from the target's
    /// parent up to and including `$HOME`. Ancestors above `$HOME` are not pinned.
    pub fn capture<I: IntoIterator<Item = PathBuf>>(
        _repo_root: &Path,
        targets: I,
        removed_ancestors: &BTreeSet<PathBuf>,
        home: &Path,
    ) -> Result<Self, TargetAncestorRedirect> {
        let mut identities = BTreeMap::new();
        for target in targets {
            for ancestor in target.ancestors().skip(1) {
                if !ancestor.starts_with(home) {
                    break;
                }
                let path = ancestor.to_path_buf();
                if identities.contains_key(&path) {
                    continue;
                }
                match target_ancestor_entry(ancestor) {
                    Ok(id) => {
                        identities.insert(path, id);
                    }
                    Err(_) => {
                        return Err(TargetAncestorRedirect::Redirected {
                            path,
                            resolves_to: None,
                        });
                    }
                }
            }
        }
        Ok(Self {
            removed: removed_ancestors.clone(),
            identities,
        })
    }

    /// Revalidate every captured ancestor after a hook.
    ///
    /// Allowed transitions:
    /// - a real directory stays the same real directory;
    /// - an absent ancestor stays absent or becomes a real directory
    ///   (a hook may `mkdir -p` a missing parent);
    /// - a removed ancestor (e.g., a whole-directory promotion root) stays the
    ///   same symlink or becomes absent; anything else is a redirect.
    pub fn revalidate(&self) -> Result<(), TargetAncestorRedirect> {
        for (path, expected) in &self.identities {
            let actual =
                target_ancestor_entry(path).map_err(|_| TargetAncestorRedirect::Redirected {
                    path: path.clone(),
                    resolves_to: None,
                })?;
            match (expected, actual) {
                (Some(expected_id), Some(actual_id)) if *expected_id == actual_id => {}
                (None, None) => {}
                (None, Some(TargetAncestorEntry::RealDir { .. })) => {
                    // Previously absent: a hook may have `mkdir -p`d a real
                    // directory. This is the benign case.
                }
                (Some(TargetAncestorEntry::Symlink { .. }), None)
                    if self.removed.contains(path) =>
                {
                    // A removed ancestor (e.g., whole-dir promotion root) may
                    // already be gone; the operation will create the
                    // replacement itself.
                }
                (None, Some(TargetAncestorEntry::Symlink { target, .. })) => {
                    return Err(TargetAncestorRedirect::Symlinked {
                        path: path.clone(),
                        resolves_to: Some(target),
                    });
                }
                (None, Some(TargetAncestorEntry::Other { .. })) => {
                    return Err(TargetAncestorRedirect::Redirected {
                        path: path.clone(),
                        resolves_to: None,
                    });
                }
                (Some(_), None) => {
                    return Err(TargetAncestorRedirect::Removed { path: path.clone() });
                }
                (Some(_), Some(TargetAncestorEntry::Symlink { target, .. })) => {
                    return Err(TargetAncestorRedirect::Symlinked {
                        path: path.clone(),
                        resolves_to: Some(target),
                    });
                }
                (Some(_), Some(TargetAncestorEntry::RealDir { .. }))
                | (Some(_), Some(TargetAncestorEntry::Other { .. })) => {
                    return Err(TargetAncestorRedirect::Redirected {
                        path: path.clone(),
                        resolves_to: None,
                    });
                }
            }
        }
        Ok(())
    }
}

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
    pub fn op_store(&self, repo_root: &Path) -> Option<String> {
        match self {
            PlanFileOp::StageRender { store, .. } => Some(store.clone()),
            PlanFileOp::CreateLink { source, .. }
            | PlanFileOp::ReplaceLink { source, .. }
            | PlanFileOp::BackupAndLink { source, .. } => source_store(source, repo_root),
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
            } else if let PlanOp::Error { message, class } = op {
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
    for ancestor in target.ancestors().skip(1) {
        if removed_ancestors.contains(ancestor) {
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
fn plan_link_targets(ops: &[PlanFileOp]) -> (Vec<PathBuf>, BTreeSet<PathBuf>) {
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
                    store_dir,
                    &target_path,
                    &target.files,
                    &target.patterns,
                    &target.ignore,
                    &mut keep_staged,
                    &mut target_keep_links,
                );
            }
        } else if let Some(target) = &store.target {
            let target_path = config::expand_home(target).map_err(StitchError::from)?;
            store::collect_reconciliation_keeps(
                store_dir,
                &target_path,
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
fn plan_source_root(repo_root: &Path, source: &Path) -> Result<PathBuf, String> {
    if let Some(store) = staged_store(source) {
        return Ok(render::store_render_dir(repo_root, &store));
    }
    let store = source
        .strip_prefix(repo_root)
        .ok()
        .and_then(|path| path.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .ok_or_else(|| format!("source {} is not under a store", source.display()))?;
    Ok(repo_root.join(store))
}

fn check_source_exists_for_preflight(repo_root: &Path, source: &str) -> Result<(), String> {
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

#[derive(Debug, Clone)]
struct RenderPin {
    source_rel: String,
    staged: String,
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
                    &render::resolve_entry(source_rel).link_rel,
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
    if let Err(e) = hooks::run_global_hook(repo_root, "pre-apply", &env, &platform) {
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

        if let Err(e) =
            run_store_pre_hook(repo_root, store_name, &pre_hook_loaded.config, &platform)
        {
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

        if let Some(warning) =
            run_store_post_hook(repo_root, store_name, &locked_loaded.config, &platform)
        {
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
                &render::resolve_entry(source_rel).link_rel,
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

type LinkRemovalKey = (String, String, Option<String>);
type StagedRemovalKey = (String, String);

#[derive(Default)]
struct CurrentRemovals {
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
fn current_removals(
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

fn validate_cleanup_dependencies(
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

fn validate_op(
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
    use crate::store::ApplyOpts;
    use std::collections::BTreeSet;
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

    #[test]
    fn backup_path_must_be_exact_target_suffix() {
        assert!(validate_backup_path(0, "/home/user/.bashrc", "/home/user/.bashrc.bak").is_ok());
        let error =
            validate_backup_path(0, "/home/user/.bashrc", "/home/user/other.bak").unwrap_err();
        assert!(error.contains("must be exactly"), "got: {error}");
    }

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
            },
        );
        let plan = build_plan_file(&repo_alias, &loaded, &computed, &platform).unwrap();
        assert!(plan.ops.iter().any(|op| {
            matches!(op, PlanFileOp::RemoveLink { target: path, .. } if path == &target.display().to_string())
        }));

        assert!(plan.conflicts.is_empty());
        execute_plan(&repo_alias, &loaded, &plan, false, false).unwrap();
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

        let result = execute_plan(&repo_root, &stale_loaded, &plan, false, false);
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

        let result = execute_plan(&repo_root, &loaded, &plan, false, false);
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

    #[test]
    fn target_ancestor_snapshot_includes_home_and_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cfg = home.join(".config");
        fs::create_dir_all(&cfg).unwrap();

        let targets = vec![cfg.join("a").join("f"), cfg.join("b").join("g")];
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), targets, &BTreeSet::new(), &home).unwrap();

        assert!(snapshot.identities.contains_key(&cfg));
        assert!(snapshot.identities.contains_key(&cfg.join("a")));
        assert!(snapshot.identities.contains_key(&cfg.join("b")));
        assert!(snapshot.identities.contains_key(&home));
    }

    #[test]
    fn target_ancestor_snapshot_allows_absent_to_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let target = home.join(".config").join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::create_dir_all(home.join(".config")).unwrap();
        snapshot.revalidate().unwrap();
    }

    #[test]
    fn target_ancestor_snapshot_rejects_absent_to_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let target = home.join(".config").join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        symlink(&other, home.join(".config")).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Symlinked { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_rejects_real_dir_identity_change() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".config");
        fs::create_dir_all(&config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        // Replace the real directory with a different one (same as a bind
        // mount / rename / copy attack). Use rename to guarantee a new inode.
        let old = tmp.path().join("old_config");
        fs::rename(&config, &old).unwrap();
        fs::create_dir_all(&config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(
                err,
                TargetAncestorRedirect::Redirected {
                    resolves_to: None,
                    ..
                }
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_rejects_real_dir_to_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".config");
        fs::create_dir_all(&config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::remove_dir(&config).unwrap();
        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        symlink(&other, &config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Symlinked { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_allows_symlink_identity_preservation() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        let config = home.join(".config");
        symlink(&other, &config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        // Revalidate sees the same symlink and is fine; the per-link
        // confinement check, not the snapshot, decides whether this is
        // traversable.
        snapshot.revalidate().unwrap();
    }

    #[test]
    fn target_ancestor_snapshot_rejects_symlink_repointing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let other = home.join(".ssh");
        fs::create_dir_all(&other).unwrap();
        let config = home.join(".config");
        symlink(&other, &config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::remove_file(&config).unwrap();
        let third = home.join(".third");
        fs::create_dir_all(&third).unwrap();
        symlink(&third, &config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Symlinked { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn target_ancestor_snapshot_rejects_existing_dir_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".config");
        fs::create_dir_all(&config).unwrap();

        let target = config.join("f");
        let snapshot =
            TargetAncestorSnapshot::capture(tmp.path(), vec![target], &BTreeSet::new(), &home)
                .unwrap();

        fs::remove_dir(&config).unwrap();

        let err = snapshot.revalidate().unwrap_err();
        assert!(
            matches!(err, TargetAncestorRedirect::Removed { .. }),
            "got: {err:?}"
        );
    }
}
