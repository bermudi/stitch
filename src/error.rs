//! Typed error taxonomy for v0.7.
//!
//! Every non-zero exit is a documented, branchable failure class with a
//! resolution hint. The mapping lives here so callers in `main.rs` cannot
//! accidentally invent new exit codes.

use crate::config::ConfigError;
use crate::linker::LinkError;
use std::path::PathBuf;

/// Failure class. Each variant maps to a stable exit code and a generic hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FailureClass {
    /// Generic/internal or I/O not otherwise classified (exit 1).
    Internal,
    /// CLI usage error (exit 2). Clap already uses 2 for parse failures.
    Usage,
    /// Config load, parse, or v0.2 migration issue (exit 3).
    Config,
    /// Repo root resolution failed (exit 4).
    RepoResolution,
    /// Unknown store name (exit 5).
    UnknownStore,
    /// Real file or directory blocks the target (exit 6).
    ConflictReal,
    /// Foreign symlink blocks the target (exit 7).
    ConflictForeign,
    /// Template render failed (exit 8).
    Render,
    /// Path fragment validation failed (exit 9).
    PathValidation,
    /// Hook execution failed (exit 10).
    Hook,
    /// Multiple failure classes in one run (exit 11).
    Mixed,
    /// Plan is stale or invalid (exit 12).
    PlanStale,
    /// `doctor` reported error-severity findings (exit 13). Distinct from
    /// `Internal` (io/unexpected): doctor findings are the command's purpose,
    /// not an unexpected failure. Per-finding hints ride in the JSON envelope.
    Doctor,
}

impl FailureClass {
    pub fn code(&self) -> i32 {
        match self {
            FailureClass::Internal => 1,
            FailureClass::Usage => 2,
            FailureClass::Config => 3,
            FailureClass::RepoResolution => 4,
            FailureClass::UnknownStore => 5,
            FailureClass::ConflictReal => 6,
            FailureClass::ConflictForeign => 7,
            FailureClass::Render => 8,
            FailureClass::PathValidation => 9,
            FailureClass::Hook => 10,
            FailureClass::Mixed => 11,
            FailureClass::PlanStale => 12,
            FailureClass::Doctor => 13,
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            FailureClass::Internal => "internal",
            FailureClass::Usage => "usage",
            FailureClass::Config => "config",
            FailureClass::RepoResolution => "repo-resolution",
            FailureClass::UnknownStore => "unknown-store",
            FailureClass::ConflictReal => "conflict-real",
            FailureClass::ConflictForeign => "conflict-foreign",
            FailureClass::Render => "render",
            FailureClass::PathValidation => "path-validation",
            FailureClass::Hook => "hook",
            FailureClass::Mixed => "mixed",
            FailureClass::PlanStale => "plan-stale",
            FailureClass::Doctor => "doctor",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "internal" => Some(FailureClass::Internal),
            "usage" => Some(FailureClass::Usage),
            "config" => Some(FailureClass::Config),
            "repo-resolution" => Some(FailureClass::RepoResolution),
            "unknown-store" => Some(FailureClass::UnknownStore),
            "conflict-real" => Some(FailureClass::ConflictReal),
            "conflict-foreign" => Some(FailureClass::ConflictForeign),
            "render" => Some(FailureClass::Render),
            "path-validation" => Some(FailureClass::PathValidation),
            "hook" => Some(FailureClass::Hook),
            "mixed" => Some(FailureClass::Mixed),
            "plan-stale" => Some(FailureClass::PlanStale),
            "doctor" => Some(FailureClass::Doctor),
            _ => None,
        }
    }

    /// Generic hint for this failure class, used when no more specific
    /// context is available (e.g. aggregated `apply` results).
    pub fn hint(&self) -> Option<String> {
        match self {
            FailureClass::Internal => None,
            FailureClass::Usage => Some("check the command arguments".into()),
            FailureClass::Config => Some("check the config files or run `stitch migrate`".into()),
            FailureClass::RepoResolution => {
                Some("run `stitch init` or pass a valid `--repo` path".into())
            }
            FailureClass::UnknownStore => Some("list valid stores with `stitch list`".into()),
            FailureClass::ConflictReal => {
                Some("remove the conflicting target or run `stitch apply --force`".into())
            }
            FailureClass::ConflictForeign => {
                Some("remove or repoint the conflicting symlink yourself".into())
            }
            FailureClass::Render => {
                Some("set missing environment variables or fix the template".into())
            }
            FailureClass::PathValidation => {
                Some("use relative paths without `..` and no leading `/`".into())
            }
            FailureClass::Hook => Some("fix or disable the failing hook".into()),
            FailureClass::Mixed => Some("see the per-entry messages above".into()),
            FailureClass::PlanStale => Some("re-run `stitch plan`".into()),
            FailureClass::Doctor => {
                Some("address the findings above (per-finding hints in JSON)".into())
            }
        }
    }
}

/// A typed, branchable stitch error. The `exit_code` and `hint` are derived
/// from the failure class; `thiserror` provides the human message.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum StitchError {
    #[error("internal error: {message}")]
    Internal { message: String },

    #[error("I/O error: {source}")]
    Io { source: std::io::Error },

    #[error("usage: {message}")]
    Usage { message: String },

    #[error("config error: {0}")]
    Config(ConfigError),

    #[error("{label} {path} does not point at a stitch repo (no .stitch/ found)")]
    RepoResolution { label: String, path: PathBuf },

    #[error("unknown store(s): {names}")]
    UnknownStore { names: String, valid: Vec<String> },

    #[error("conflict: real file/dir at {target}")]
    ConflictReal { target: PathBuf },

    #[error("conflict: foreign symlink at {target}")]
    ConflictForeign {
        target: PathBuf,
        resolves_to: Option<PathBuf>,
    },

    #[error("render failed for {source_path}: {detail}")]
    Render {
        source_path: PathBuf,
        detail: String,
    },

    #[error("path validation failed: {message}")]
    PathValidation { message: String },

    #[error("hook failed: {name}: {detail}")]
    Hook { name: String, detail: String },

    #[error("apply failed: {message}")]
    Apply {
        classes: Vec<FailureClass>,
        message: String,
    },

    #[error("mixed failures: {message}")]
    Mixed {
        classes: Vec<FailureClass>,
        message: String,
    },

    #[error("plan not executable: {detail}")]
    PlanStale { detail: String },

    #[error("doctor found {errors} error(s)")]
    Doctor { errors: usize },
}

#[allow(dead_code)]
impl StitchError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    pub fn io(source: std::io::Error) -> Self {
        Self::Io { source }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage {
            message: message.into(),
        }
    }

    pub fn config(source: ConfigError) -> Self {
        Self::Config(source)
    }

    pub fn repo_resolution(label: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::RepoResolution {
            label: label.into(),
            path: path.into(),
        }
    }

    pub fn unknown_store(names: Vec<String>, valid: Vec<String>) -> Self {
        Self::UnknownStore {
            names: names.join(", "),
            valid,
        }
    }

    pub fn conflict_real(target: impl Into<PathBuf>) -> Self {
        Self::ConflictReal {
            target: target.into(),
        }
    }

    pub fn conflict_foreign(
        target: impl Into<PathBuf>,
        resolves_to: Option<impl Into<PathBuf>>,
    ) -> Self {
        Self::ConflictForeign {
            target: target.into(),
            resolves_to: resolves_to.map(Into::into),
        }
    }

    pub fn render(source: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self::Render {
            source_path: source.into(),
            detail: detail.into(),
        }
    }

    pub fn path_validation(message: impl Into<String>) -> Self {
        Self::PathValidation {
            message: message.into(),
        }
    }

    pub fn hook(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Hook {
            name: name.into(),
            detail: detail.into(),
        }
    }

    pub fn apply(classes: Vec<FailureClass>, message: impl Into<String>) -> Self {
        Self::Apply {
            classes,
            message: message.into(),
        }
    }

    pub fn plan_stale(detail: impl Into<String>) -> Self {
        Self::PlanStale {
            detail: detail.into(),
        }
    }

    pub fn doctor(errors: usize) -> Self {
        Self::Doctor { errors }
    }

    pub fn class(&self) -> FailureClass {
        match self {
            Self::Internal { .. } | Self::Io { .. } => FailureClass::Internal,
            Self::Usage { .. } => FailureClass::Usage,
            Self::Config(_) => FailureClass::Config,
            Self::RepoResolution { .. } => FailureClass::RepoResolution,
            Self::UnknownStore { .. } => FailureClass::UnknownStore,
            Self::ConflictReal { .. } => FailureClass::ConflictReal,
            Self::ConflictForeign { .. } => FailureClass::ConflictForeign,
            Self::Render { .. } => FailureClass::Render,
            Self::PathValidation { .. } => FailureClass::PathValidation,
            Self::Hook { .. } => FailureClass::Hook,
            Self::Mixed { .. } => FailureClass::Mixed,
            Self::PlanStale { .. } => FailureClass::PlanStale,
            Self::Doctor { .. } => FailureClass::Doctor,
            Self::Apply { classes, .. } => match classes.as_slice() {
                [] => FailureClass::Internal,
                [c] => *c,
                _ => FailureClass::Mixed,
            },
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.class().code()
    }

    /// A resolution hint for this specific error, more specific than the
    /// generic class hint where possible.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::Internal { .. } | Self::Io { .. } => None,
            Self::Usage { .. } => FailureClass::Usage.hint(),
            Self::Config(source) => match source {
                ConfigError::LegacyV02(_) => Some("run `stitch migrate`".into()),
                ConfigError::Parse(_, _) => Some("fix the TOML syntax and reload".into()),
                ConfigError::InvalidPath(_) => FailureClass::PathValidation.hint(),
                _ => FailureClass::Config.hint(),
            },
            Self::RepoResolution { .. } => FailureClass::RepoResolution.hint(),
            Self::UnknownStore { valid, .. } => {
                if valid.is_empty() {
                    Some("no stores are configured".into())
                } else {
                    Some(format!("valid stores: {}", valid.join(", ")))
                }
            }
            Self::ConflictReal { target } => Some(format!(
                "remove or move `{}`, or run `stitch apply --force`",
                target.display()
            )),
            Self::ConflictForeign { .. } => FailureClass::ConflictForeign.hint(),
            Self::Render { source_path, .. } => Some(format!(
                "set missing env vars or fix `{}`",
                source_path.display()
            )),
            Self::PathValidation { .. } => FailureClass::PathValidation.hint(),
            Self::Hook { name, .. } => Some(format!("fix or disable the `{name}` hook")),
            Self::Mixed { classes, .. } | Self::Apply { classes, .. } => {
                if classes.is_empty() {
                    None
                } else if classes.len() == 1 {
                    classes[0].hint()
                } else {
                    Some(format!(
                        "multiple failure classes: {}",
                        classes
                            .iter()
                            .map(|c| c.id())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            Self::PlanStale { .. } => FailureClass::PlanStale.hint(),
            Self::Doctor { .. } => FailureClass::Doctor.hint(),
        }
    }
}

impl From<std::io::Error> for StitchError {
    fn from(source: std::io::Error) -> Self {
        Self::Io { source }
    }
}

impl From<ConfigError> for StitchError {
    fn from(source: ConfigError) -> Self {
        match source {
            ConfigError::InvalidPath(msg) => Self::PathValidation { message: msg },
            other => Self::Config(other),
        }
    }
}

impl From<toml::ser::Error> for StitchError {
    fn from(source: toml::ser::Error) -> Self {
        Self::Config(ConfigError::Serialize(source))
    }
}

impl From<LinkError> for StitchError {
    fn from(source: LinkError) -> Self {
        Self::Internal {
            message: source.to_string(),
        }
    }
}
