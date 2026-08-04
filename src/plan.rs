//! The v0.7 `Plan` type: an executable upgrade of `ApplyAction`.
//!
//! Each planned op carries the source path and the preconditions that must hold
//! for the op to be safe (M4: `apply --plan` uses these to replay verbatim).
//! For M3 the `Plan` is the shared struct consumed by both text and JSON
//! rendering so the two views cannot drift apart.

use crate::error::FailureClass;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The result of `store::compute_plan`: a list of per-store operations.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub stores: Vec<PlanStore>,
    pub summary: PlanSummary,
}

impl Plan {
    pub fn from_stores(stores: Vec<PlanStore>) -> Self {
        let summary = compute_summary(&stores);
        Self { stores, summary }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PlanSummary {
    pub created: usize,
    pub replaced: usize,
    pub backed_up: usize,
    pub removed: usize,
    pub content_changed: usize,
    pub already_linked: usize,
    pub conflicts: usize,
    pub errors: usize,
    pub skipped: usize,
}

fn compute_summary(stores: &[PlanStore]) -> PlanSummary {
    let mut summary = PlanSummary::default();
    for store in stores {
        for op in &store.ops {
            match op {
                PlanOp::CreateLink { .. } => summary.created += 1,
                PlanOp::ReplaceLink { .. } => summary.replaced += 1,
                PlanOp::BackupAndLink { .. } => summary.backed_up += 1,
                PlanOp::RemoveLink { .. } => summary.removed += 1,
                PlanOp::ContentChanged { .. } => summary.content_changed += 1,
                PlanOp::AlreadyLinked { .. } => summary.already_linked += 1,
                PlanOp::Conflict { .. } => summary.conflicts += 1,
                PlanOp::Error { .. } => summary.errors += 1,
                PlanOp::SkippedPlatform => summary.skipped += 1,
                PlanOp::StageRender { .. } => {}
            }
        }
    }
    summary
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanStore {
    pub store_name: String,
    pub ops: Vec<PlanOp>,
}

/// One executable op in a plan. The serialization shape is the JSON that
/// `apply --json` and `diff --json` emit, so text and JSON consume exactly the
/// same struct.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum PlanOp {
    /// Render a template to the staging tree. Not printed in text mode.
    StageRender {
        store: String,
        source_rel: String,
        source: String,
        staged: String,
        sha256: String,
    },

    CreateLink {
        target: String,
        source: String,
        requires: LinkRequires,
    },

    ReplaceLink {
        target: String,
        source: String,
        old_resolves_to: Option<String>,
        requires: LinkRequires,
    },

    BackupAndLink {
        target: String,
        source: String,
        backup: String,
        requires: LinkRequires,
    },

    RemoveLink {
        target: String,
        source: Option<String>,
        requires: LinkRequires,
    },

    ContentChanged {
        target: String,
        source: String,
        requires: LinkRequires,
    },

    AlreadyLinked {
        target: String,
        source: String,
        requires: LinkRequires,
    },

    Conflict {
        target: String,
        resolves_to: Option<String>,
    },

    SkippedPlatform,

    Error {
        message: String,
        class: String,
    },
}

#[allow(dead_code)]
impl PlanOp {
    pub fn target(&self) -> Option<&str> {
        match self {
            PlanOp::StageRender { .. } => None,
            PlanOp::CreateLink { target, .. }
            | PlanOp::ReplaceLink { target, .. }
            | PlanOp::BackupAndLink { target, .. }
            | PlanOp::RemoveLink { target, .. }
            | PlanOp::ContentChanged { target, .. }
            | PlanOp::AlreadyLinked { target, .. }
            | PlanOp::Conflict { target, .. } => Some(target),
            PlanOp::SkippedPlatform | PlanOp::Error { .. } => None,
        }
    }

    /// Human-readable target path for the text summary. `None` for non-target
    /// ops such as `SkippedPlatform` and `Error`.
    pub fn target_path(&self) -> Option<PathBuf> {
        self.target().map(PathBuf::from)
    }
}

/// Preconditions an op needs at exec time. Kept simple and explicit so M4 can
/// preflight and re-check each op.
#[derive(Debug, Clone, Serialize)]
pub struct LinkRequires {
    pub target: TargetState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<TargetState>,
}

impl LinkRequires {
    pub fn new(target: TargetState) -> Self {
        Self {
            target,
            backup: None,
        }
    }

    pub fn with_backup(target: TargetState, backup: TargetState) -> Self {
        Self {
            target,
            backup: Some(backup),
        }
    }
}

/// The expected state of a path when an op is executed. Used as the value of
/// `requires.target` / `requires.backup`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "target", content = "value", rename_all = "snake_case")]
pub enum TargetState {
    Absent,
    RealEntry,
    SymlinkTo(String),
    SymlinkIntoRepo,
}

/// Convert a `Path` to the string form used inside `Plan` (and serialized JSON).
pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Build an `Error` op from a typed `StitchError`, preserving its class id.
#[allow(dead_code)]
pub fn error_op(error: &crate::error::StitchError) -> PlanOp {
    PlanOp::Error {
        message: error.to_string(),
        class: error.class().id().to_string(),
    }
}

/// Build an `Error` op with a specific failure class.
#[allow(dead_code)]
pub fn error_op_class(class: FailureClass, message: impl Into<String>) -> PlanOp {
    PlanOp::Error {
        message: message.into(),
        class: class.id().to_string(),
    }
}
