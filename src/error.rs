//! Typed error taxonomy for v0.7.
//!
//! Every non-zero exit is a documented, branchable failure class with a
//! resolution hint. The mapping lives here so callers in `main.rs` cannot
//! accidentally invent new exit codes.

use crate::config::ConfigError;
use crate::linker::LinkError;
use serde::Serialize;
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
    /// `diff --exit-code` found safe operations required for convergence
    /// (exit 14). Conflicts and errors retain their more specific classes.
    Drift,
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
            FailureClass::Drift => 14,
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
            FailureClass::Drift => "drift",
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
            "drift" => Some(FailureClass::Drift),
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
            FailureClass::Mixed => Some("see the per-entry messages in JSON".into()),
            FailureClass::PlanStale => Some("re-run `stitch plan`".into()),
            FailureClass::Doctor => {
                Some("address the findings above (per-finding hints in JSON)".into())
            }
            FailureClass::Drift => Some("run `stitch apply` to reconcile the filesystem".into()),
        }
    }
}

/// A machine-readable recovery action for an error. Appears in the JSON
/// envelope's `error.recoverable_via` array so an agent can branch without
/// parsing the prose `hint`.
///
/// Kinds:
/// - `force-apply` — run `apply --force` to back up and replace conflicts.
/// - `adopt` — run `add <path>` to move the conflicting file into the repo.
/// - `edit-template` — run `edit <source>` to open the template for fixing.
/// - `list` — run `list` to see valid stores.
/// - `init` — run `init` to create a new repo.
/// - `migrate` — run `migrate` to convert a v0.2 config.
/// - `replan` — run `plan` to capture a fresh plan.
/// - `apply` — run `apply` to reconcile drift.
/// - `manual` — no stitch command can fix this; `reason` explains why. The
///   agent should escalate rather than retry stitch commands.
/// - `set-env` — set environment variables (listed in `vars`) and retry.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryAction {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `vars` is always present so a `set-env` action signals whether
    /// extraction succeeded (`vars` populated) or extraction was unreliable
    /// (`vars: []` — the prose `hint` carries the detail).
    pub vars: Vec<String>,
}

impl RecoveryAction {
    /// A recovery that runs a stitch command with no flags or args.
    fn command(kind: &str, command: &str) -> Self {
        Self {
            kind: kind.to_string(),
            command: Some(command.to_string()),
            flags: Vec::new(),
            args: Vec::new(),
            reason: None,
            vars: Vec::new(),
        }
    }

    /// A recovery that runs a stitch command with flags.
    fn with_flags(kind: &str, command: &str, flags: &[&str]) -> Self {
        Self {
            kind: kind.to_string(),
            command: Some(command.to_string()),
            flags: flags.iter().map(|s| s.to_string()).collect(),
            args: Vec::new(),
            reason: None,
            vars: Vec::new(),
        }
    }

    /// A recovery that runs a stitch command with positional args.
    fn with_args(kind: &str, command: &str, args: &[String]) -> Self {
        Self {
            kind: kind.to_string(),
            command: Some(command.to_string()),
            flags: Vec::new(),
            args: args.to_vec(),
            reason: None,
            vars: Vec::new(),
        }
    }

    /// A manual recovery — no stitch command; the agent should escalate.
    fn manual(reason: &str) -> Self {
        Self {
            kind: "manual".to_string(),
            command: None,
            flags: Vec::new(),
            args: Vec::new(),
            reason: Some(reason.to_string()),
            vars: Vec::new(),
        }
    }

    /// A recovery that sets environment variables and retries. `vars` lists the
    /// missing `env()` keys when extractable; empty when extraction is unreliable
    /// (the prose `hint` carries the detail).
    fn set_env(vars: Vec<String>) -> Self {
        Self {
            kind: "set-env".to_string(),
            command: None,
            flags: Vec::new(),
            args: Vec::new(),
            reason: None,
            vars,
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

    #[error("{message}: {source}")]
    IoContext {
        message: String,
        source: std::io::Error,
    },

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
    Hook {
        name: String,
        detail: String,
        /// Store name for per-store hooks; None for global hooks.
        store: Option<String>,
        /// "global" or "per-store".
        scope: String,
    },

    #[error("{message}")]
    Apply {
        classes: Vec<FailureClass>,
        message: String,
        /// Structured details (JSON string) for aggregate errors where the
        /// single class has structured info (e.g. hook store/scope). Populated
        /// by `plan_error` when the only class is `Hook`.
        details: Option<String>,
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

    #[error("filesystem differs from desired state: {changes} change(s) needed")]
    Drift { changes: usize },
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

    pub fn io_context(message: impl Into<String>, source: std::io::Error) -> Self {
        Self::IoContext {
            message: message.into(),
            source,
        }
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
            store: None,
            scope: "global".into(),
        }
    }

    /// Create a hook error with store and scope context (for per-store hooks).
    pub fn hook_store(
        name: impl Into<String>,
        detail: impl Into<String>,
        store: impl Into<String>,
    ) -> Self {
        Self::Hook {
            name: name.into(),
            detail: detail.into(),
            store: Some(store.into()),
            scope: "per-store".into(),
        }
    }

    pub fn apply(classes: Vec<FailureClass>, message: impl Into<String>) -> Self {
        Self::Apply {
            classes,
            message: message.into(),
            details: None,
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

    pub fn drift(changes: usize) -> Self {
        Self::Drift { changes }
    }

    pub fn class(&self) -> FailureClass {
        match self {
            Self::Internal { .. } | Self::Io { .. } | Self::IoContext { .. } => {
                FailureClass::Internal
            }
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
            Self::Drift { .. } => FailureClass::Drift,
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
            Self::Internal { .. } | Self::Io { .. } | Self::IoContext { .. } => None,
            Self::Usage { .. } => FailureClass::Usage.hint(),
            Self::Config(source) => match source {
                ConfigError::LegacyV02(_) => Some("run `stitch migrate`".into()),
                ConfigError::Parse(_, _) => Some("fix the TOML syntax and reload".into()),
                ConfigError::Home(_) => Some("set $HOME to an existing directory".into()),
                ConfigError::InvalidPath(_) => FailureClass::PathValidation.hint(),
                ConfigError::Conflict(_) => Some(
                    "edit stitch.toml to give each store a distinct target, or gate them with \
                     mutually-exclusive `when` clauses"
                        .into(),
                ),
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
            Self::Drift { .. } => FailureClass::Drift.hint(),
        }
    }

    /// Machine-readable structured details for this error, serialized as a
    /// JSON string in the envelope's `error.details` field. Currently
    /// populated only for `Hook` errors (store, scope, hook name).
    pub fn details(&self) -> Option<String> {
        match self {
            Self::Hook {
                name, store, scope, ..
            } => {
                // Build the JSON object with serde_json so special characters
                // in hook names and store names are escaped correctly.
                let value = serde_json::json!({
                    "hook": name,
                    "store": store.as_deref(),
                    "scope": scope,
                });
                Some(serde_json::to_string(&value).expect("hook details are serializable"))
            }
            Self::Apply { details, .. } => details.as_ref().and_then(|s| {
                // Validate that any caller-supplied details string is valid JSON
                // before returning it; malformed details fall back to None.
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|v| serde_json::to_string(&v).ok())
            }),
            _ => None,
        }
    }

    /// Machine-readable recovery actions for this error. Empty when the error
    /// is not recoverable via a stitch command (e.g. `internal`, `usage`,
    /// `path-validation`). `manual` entries signal "escalate, don't retry."
    ///
    /// For `Apply` with a single failure class, delegates to that class's
    /// generic recovery (without target-specific args — the agent should
    /// consult the plan's `conflict`/`error` ops in the JSON `data` field for
    /// per-entry targets). `Apply` with multiple classes and `Mixed` return
    /// empty; the per-entry detail is in the plan ops.
    pub fn recoverable_via(&self) -> Vec<RecoveryAction> {
        match self {
            Self::Internal { .. } | Self::Io { .. } | Self::IoContext { .. } => vec![],
            Self::Usage { .. } => vec![],
            Self::Config(source) => match source {
                ConfigError::LegacyV02(_) => vec![RecoveryAction::command("migrate", "migrate")],
                _ => vec![],
            },
            Self::RepoResolution { .. } => vec![RecoveryAction::command("init", "init")],
            Self::UnknownStore { .. } => vec![RecoveryAction::command("list", "list")],
            Self::ConflictReal { target } => vec![
                RecoveryAction::with_flags("force-apply", "apply", &["--force"]),
                RecoveryAction::with_args("adopt", "add", &[target.display().to_string()]),
            ],
            Self::ConflictForeign { .. } => vec![RecoveryAction::manual(
                "foreign symlinks are never auto-clobbered; remove or repoint the symlink manually",
            )],
            Self::Render { source_path, .. } => vec![
                RecoveryAction::with_args(
                    "edit-template",
                    "edit",
                    &[source_path.display().to_string()],
                ),
                RecoveryAction::set_env(vec![]),
            ],
            Self::PathValidation { .. } => vec![],
            Self::Hook { name, .. } => vec![RecoveryAction::manual(&format!(
                "fix or disable the `{name}` hook"
            ))],
            Self::Apply { classes, .. } => match classes.as_slice() {
                // Single class: delegate to generic recovery for that class.
                // Target-specific args are unavailable at the aggregate level;
                // the agent should consult the plan ops in `data` for per-entry targets.
                [FailureClass::ConflictReal] => vec![RecoveryAction::with_flags(
                    "force-apply",
                    "apply",
                    &["--force"],
                )],
                [FailureClass::ConflictForeign] => vec![RecoveryAction::manual(
                    "foreign symlinks are never auto-clobbered; remove or repoint the symlink manually",
                )],
                // The aggregate knows a template render failed, but it does not
                // record which source path failed. `edit-template` requires a
                // specific source; an unqualified `edit` would open the wrong
                // file. The source is in the JSON `data` plan ops, so keep this
                // as `manual` and point the agent at the plan.
                [FailureClass::Render] => vec![RecoveryAction::manual(
                    "fix the failing template(s); see the source in the plan ops",
                )],
                [FailureClass::Drift] => vec![RecoveryAction::command("apply", "apply")],
                [FailureClass::Hook] => {
                    vec![RecoveryAction::manual("fix or disable the failing hook")]
                }
                _ => vec![],
            },
            Self::Mixed { .. } => vec![],
            Self::PlanStale { .. } => vec![RecoveryAction::command("replan", "plan")],
            Self::Doctor { .. } => vec![],
            Self::Drift { .. } => vec![RecoveryAction::command("apply", "apply")],
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
            ConfigError::Write(e, path) => Self::Internal {
                message: format!("could not write {}: {e}", path.display()),
            },
            ConfigError::CommittedWrite(e, path) => Self::Internal {
                message: format!(
                    "wrote {} but could not sync its parent directory: {e}; the new state remains in place but may not survive power loss",
                    path.display()
                ),
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigError;
    use serde_json::Value;
    use std::path::PathBuf;

    #[test]
    fn failure_class_codes_are_stable() {
        let cases = [
            (FailureClass::Internal, 1, "internal"),
            (FailureClass::Usage, 2, "usage"),
            (FailureClass::Config, 3, "config"),
            (FailureClass::RepoResolution, 4, "repo-resolution"),
            (FailureClass::UnknownStore, 5, "unknown-store"),
            (FailureClass::ConflictReal, 6, "conflict-real"),
            (FailureClass::ConflictForeign, 7, "conflict-foreign"),
            (FailureClass::Render, 8, "render"),
            (FailureClass::PathValidation, 9, "path-validation"),
            (FailureClass::Hook, 10, "hook"),
            (FailureClass::Mixed, 11, "mixed"),
            (FailureClass::PlanStale, 12, "plan-stale"),
            (FailureClass::Doctor, 13, "doctor"),
            (FailureClass::Drift, 14, "drift"),
        ];
        for (class, code, id) in cases {
            assert_eq!(class.code(), code, "code for {id}");
            assert_eq!(class.id(), id);
            assert_eq!(FailureClass::from_id(id), Some(class));
        }
        assert_eq!(FailureClass::from_id("nope"), None);
        assert_eq!(FailureClass::from_id(""), None);
    }

    #[test]
    fn failure_class_id_roundtrips() {
        for class in [
            FailureClass::Internal,
            FailureClass::Usage,
            FailureClass::Config,
            FailureClass::RepoResolution,
            FailureClass::UnknownStore,
            FailureClass::ConflictReal,
            FailureClass::ConflictForeign,
            FailureClass::Render,
            FailureClass::PathValidation,
            FailureClass::Hook,
            FailureClass::Mixed,
            FailureClass::PlanStale,
            FailureClass::Doctor,
            FailureClass::Drift,
        ] {
            let id = class.id();
            assert_eq!(FailureClass::from_id(id), Some(class));
        }
    }

    #[test]
    fn failure_class_hints_are_stable() {
        assert_eq!(FailureClass::Internal.hint(), None);
        assert!(
            FailureClass::Usage
                .hint()
                .unwrap()
                .contains("command arguments")
        );
        assert!(FailureClass::Config.hint().unwrap().contains("migrate"));
        assert!(
            FailureClass::RepoResolution
                .hint()
                .unwrap()
                .contains("stitch init")
        );
        assert!(FailureClass::UnknownStore.hint().is_some());
        assert!(
            FailureClass::ConflictReal
                .hint()
                .unwrap()
                .contains("--force")
        );
        assert!(
            FailureClass::ConflictForeign
                .hint()
                .unwrap()
                .contains("symlink")
        );
        assert!(FailureClass::Render.hint().is_some());
        assert!(FailureClass::PathValidation.hint().unwrap().contains(".."));
        assert!(FailureClass::Hook.hint().is_some());
        assert!(FailureClass::Mixed.hint().unwrap().contains("JSON"));
        assert!(FailureClass::PlanStale.hint().unwrap().contains("plan"));
        assert!(FailureClass::Doctor.hint().unwrap().contains("findings"));
        assert!(FailureClass::Drift.hint().unwrap().contains("apply"));
    }

    #[test]
    fn stitch_error_class_mapping() {
        assert_eq!(StitchError::internal("x").class(), FailureClass::Internal);
        assert_eq!(
            StitchError::io(std::io::Error::other("x")).class(),
            FailureClass::Internal
        );
        assert_eq!(
            StitchError::io_context("ctx", std::io::Error::other("x")).class(),
            FailureClass::Internal
        );
        assert_eq!(StitchError::usage("x").class(), FailureClass::Usage);
        assert_eq!(
            StitchError::config(ConfigError::Home("x".into())).class(),
            FailureClass::Config
        );
        assert_eq!(
            StitchError::repo_resolution("cwd", "/tmp").class(),
            FailureClass::RepoResolution
        );
        assert_eq!(
            StitchError::unknown_store(vec!["a".into()], vec![]).class(),
            FailureClass::UnknownStore
        );
        assert_eq!(
            StitchError::conflict_real("/t").class(),
            FailureClass::ConflictReal
        );
        assert_eq!(
            StitchError::conflict_foreign("/t", None::<PathBuf>).class(),
            FailureClass::ConflictForeign
        );
        assert_eq!(StitchError::render("/s", "d").class(), FailureClass::Render);
        assert_eq!(
            StitchError::path_validation("bad").class(),
            FailureClass::PathValidation
        );
        assert_eq!(StitchError::hook("pre", "d").class(), FailureClass::Hook);
        assert_eq!(
            StitchError::plan_stale("x").class(),
            FailureClass::PlanStale
        );
        assert_eq!(StitchError::doctor(1).class(), FailureClass::Doctor);
        assert_eq!(StitchError::drift(2).class(), FailureClass::Drift);
    }

    #[test]
    fn apply_class_aggregation() {
        assert_eq!(
            StitchError::apply(vec![], "m").class(),
            FailureClass::Internal
        );
        assert_eq!(
            StitchError::apply(vec![FailureClass::ConflictReal], "m").class(),
            FailureClass::ConflictReal
        );
        assert_eq!(
            StitchError::apply(vec![FailureClass::ConflictReal, FailureClass::Render], "m").class(),
            FailureClass::Mixed
        );
    }

    #[test]
    fn exit_code_is_concrete_for_every_class() {
        // One StitchError per FailureClass with its expected concrete code.
        // This locks the exit-code contract; adding a new class requires updating this table.
        let cases: Vec<(StitchError, i32, &'static str)> = vec![
            (StitchError::internal("x"), 1, "internal"),
            (
                StitchError::Io {
                    source: std::io::Error::other("x"),
                },
                1,
                "internal-io",
            ),
            (StitchError::usage("x"), 2, "usage"),
            (
                StitchError::config(ConfigError::Home("x".into())),
                3,
                "config",
            ),
            (
                StitchError::repo_resolution("cwd", "/tmp"),
                4,
                "repo-resolution",
            ),
            (
                StitchError::unknown_store(vec!["a".into()], vec![]),
                5,
                "unknown-store",
            ),
            (StitchError::conflict_real("/t"), 6, "conflict-real"),
            (
                StitchError::conflict_foreign("/t", None::<PathBuf>),
                7,
                "conflict-foreign",
            ),
            (StitchError::render("/s", "d"), 8, "render"),
            (StitchError::path_validation("bad"), 9, "path-validation"),
            (StitchError::hook("pre", "d"), 10, "hook"),
            (
                StitchError::Mixed {
                    classes: vec![FailureClass::ConflictReal, FailureClass::Render],
                    message: "m".into(),
                },
                11,
                "mixed",
            ),
            (StitchError::plan_stale("x"), 12, "plan-stale"),
            (StitchError::doctor(1), 13, "doctor"),
            (StitchError::drift(1), 14, "drift"),
        ];
        for (err, expected_code, label) in cases {
            assert_eq!(
                err.exit_code(),
                expected_code,
                "exit code for {label} ({:?})",
                err.class()
            );
            assert_eq!(err.class().code(), expected_code);
            // Label is the expected id except for the io-internal alias.
            let expected_id = if label == "internal-io" {
                "internal"
            } else {
                label
            };
            assert_eq!(err.class().id(), expected_id, "id for {label}");
        }
        // Also verify the indirect mapping: exit_code == class().code() for all.
        for err in [
            StitchError::internal("x"),
            StitchError::usage("x"),
            StitchError::conflict_real("/t"),
            StitchError::plan_stale("x"),
            StitchError::doctor(1),
            StitchError::drift(1),
        ] {
            assert_eq!(err.exit_code(), err.class().code());
        }
    }

    #[test]
    fn hint_specificity() {
        assert_eq!(StitchError::internal("x").hint(), None);
        assert_eq!(StitchError::io(std::io::Error::other("x")).hint(), None);
        assert!(
            StitchError::usage("x")
                .hint()
                .unwrap()
                .contains("arguments")
        );
        // ConfigError variants
        assert!(
            StitchError::config(ConfigError::LegacyV02(PathBuf::from("/p")))
                .hint()
                .unwrap()
                .contains("migrate")
        );
        assert!(
            StitchError::config(ConfigError::Parse(
                toml::from_str::<toml::Value>("x = '").unwrap_err(),
                PathBuf::from("/p")
            ))
            .hint()
            .unwrap()
            .contains("TOML")
        );
        // UnknownStore empty vs non-empty
        assert_eq!(
            StitchError::unknown_store(vec!["a".into()], vec![])
                .hint()
                .unwrap(),
            "no stores are configured"
        );
        assert!(
            StitchError::unknown_store(vec!["a".into()], vec!["nvim".into()])
                .hint()
                .unwrap()
                .contains("nvim")
        );
        // ConflictReal includes path
        assert!(
            StitchError::conflict_real("/my/target")
                .hint()
                .unwrap()
                .contains("/my/target")
        );
        assert!(
            StitchError::conflict_foreign("/t", None::<PathBuf>)
                .hint()
                .is_some()
        );
        assert!(
            StitchError::render("/src.tmpl", "d")
                .hint()
                .unwrap()
                .contains("/src.tmpl")
        );
        assert!(StitchError::path_validation("bad").hint().is_some());
        assert!(
            StitchError::hook("pre", "d")
                .hint()
                .unwrap()
                .contains("pre")
        );
        assert!(
            StitchError::plan_stale("x")
                .hint()
                .unwrap()
                .contains("plan")
        );
        assert!(StitchError::doctor(1).hint().is_some());
        assert!(StitchError::drift(1).hint().unwrap().contains("apply"));
    }

    #[test]
    fn mixed_and_apply_hint_formatting() {
        let e = StitchError::Mixed {
            classes: vec![],
            message: "m".into(),
        };
        assert_eq!(e.hint(), None);
        let e = StitchError::Mixed {
            classes: vec![FailureClass::ConflictReal],
            message: "m".into(),
        };
        assert!(e.hint().unwrap().contains("--force"));
        let e = StitchError::Mixed {
            classes: vec![FailureClass::ConflictReal, FailureClass::Render],
            message: "m".into(),
        };
        let h = e.hint().unwrap();
        assert!(h.contains("conflict-real") && h.contains("render"));

        // Apply with single vs multiple
        let e = StitchError::apply(vec![FailureClass::Hook], "m");
        assert!(e.hint().unwrap().contains("hook"));
        let e = StitchError::apply(vec![FailureClass::Hook, FailureClass::Config], "m");
        assert!(e.hint().unwrap().contains("multiple"));
    }

    #[test]
    fn from_impls_map_correctly() {
        // ConfigError::InvalidPath -> PathValidation
        let e: StitchError = ConfigError::InvalidPath("bad".into()).into();
        assert_eq!(e.class(), FailureClass::PathValidation);
        // ConfigError::Write -> Internal
        let e: StitchError =
            ConfigError::Write(std::io::Error::other("w"), PathBuf::from("/p")).into();
        assert_eq!(e.class(), FailureClass::Internal);
        assert!(e.to_string().contains("could not write"));
        // CommittedWrite -> Internal with sync message
        let e: StitchError =
            ConfigError::CommittedWrite(std::io::Error::other("c"), PathBuf::from("/p")).into();
        assert_eq!(e.class(), FailureClass::Internal);
        assert!(e.to_string().contains("sync"));
        // Other ConfigError stays Config
        let e: StitchError = ConfigError::Home("h".into()).into();
        assert_eq!(e.class(), FailureClass::Config);
        // io::Error -> Io -> Internal
        let e: StitchError = std::io::Error::other("io").into();
        assert_eq!(e.class(), FailureClass::Internal);
        // LinkError -> Internal
        let e: StitchError = crate::linker::LinkError::SourceMissing(PathBuf::from("/x")).into();
        assert_eq!(e.class(), FailureClass::Internal);
    }

    #[test]
    fn display_messages_are_non_empty() {
        for err in [
            StitchError::internal("boom"),
            StitchError::usage("bad arg"),
            StitchError::conflict_real("/t"),
            StitchError::conflict_foreign("/t", Some(PathBuf::from("/r"))),
            StitchError::render("/s", "detail"),
            StitchError::plan_stale("stale"),
            StitchError::doctor(2),
            StitchError::drift(3),
        ] {
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn recoverable_via_conflict_real_offers_force_and_adopt() {
        let err = StitchError::conflict_real("/home/user/.gitconfig");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].kind, "force-apply");
        assert_eq!(actions[0].command.as_deref(), Some("apply"));
        assert_eq!(actions[0].flags, vec!["--force"]);
        assert!(actions[0].args.is_empty());

        assert_eq!(actions[1].kind, "adopt");
        assert_eq!(actions[1].command.as_deref(), Some("add"));
        assert_eq!(actions[1].args, vec!["/home/user/.gitconfig"]);
    }

    #[test]
    fn recoverable_via_conflict_foreign_is_manual() {
        let err = StitchError::conflict_foreign("/t", Some(PathBuf::from("/r")));
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "manual");
        assert!(actions[0].command.is_none());
        assert!(actions[0].reason.is_some());
        // Red line: foreign symlinks are never auto-clobbered.
        assert!(
            actions[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("never auto-clobbered")
        );
    }

    #[test]
    fn recoverable_via_render_offers_edit_template_and_set_env() {
        let err = StitchError::render("/repo/git/gitconfig.tmpl", "missing env");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].kind, "edit-template");
        assert_eq!(actions[0].command.as_deref(), Some("edit"));
        assert_eq!(actions[0].args, vec!["/repo/git/gitconfig.tmpl"]);
        assert_eq!(actions[1].kind, "set-env");
        assert!(actions[1].command.is_none());
        assert!(actions[1].vars.is_empty());
    }

    #[test]
    fn recoverable_via_hook_is_manual() {
        let err = StitchError::hook("pre-apply", "exit 1");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "manual");
        assert!(actions[0].reason.as_ref().unwrap().contains("pre-apply"));
    }

    #[test]
    fn recoverable_via_drift_offers_apply() {
        let err = StitchError::drift(3);
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "apply");
        assert_eq!(actions[0].command.as_deref(), Some("apply"));
    }

    #[test]
    fn recoverable_via_plan_stale_offers_replan() {
        let err = StitchError::plan_stale("config changed");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "replan");
        assert_eq!(actions[0].command.as_deref(), Some("plan"));
    }

    #[test]
    fn recoverable_via_unknown_store_offers_list() {
        let err = StitchError::unknown_store(vec!["foo".into()], vec!["bar".into()]);
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "list");
        assert_eq!(actions[0].command.as_deref(), Some("list"));
    }

    #[test]
    fn recoverable_via_repo_resolution_offers_init() {
        let err = StitchError::repo_resolution("cwd", "/tmp");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "init");
        assert_eq!(actions[0].command.as_deref(), Some("init"));
    }

    #[test]
    fn recoverable_via_config_legacy_offers_migrate() {
        let err = StitchError::config(ConfigError::LegacyV02(PathBuf::from("/p")));
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "migrate");
        assert_eq!(actions[0].command.as_deref(), Some("migrate"));
    }

    #[test]
    fn recoverable_via_unrecoverable_is_empty() {
        // These errors have no stitch-command recovery path.
        for err in [
            StitchError::internal("x"),
            StitchError::usage("x"),
            StitchError::path_validation("bad"),
            StitchError::doctor(1),
        ] {
            assert!(
                err.recoverable_via().is_empty(),
                "{:?} should have no recovery actions",
                err.class()
            );
        }
    }

    #[test]
    fn recoverable_via_apply_single_class_delegates() {
        // Single conflict-real class → force-apply (no target-specific args).
        let err = StitchError::apply(vec![FailureClass::ConflictReal], "m");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "force-apply");
        assert_eq!(actions[0].command.as_deref(), Some("apply"));

        // Single conflict-foreign class → manual.
        let err = StitchError::apply(vec![FailureClass::ConflictForeign], "m");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "manual");
    }

    #[test]
    fn recoverable_via_apply_multi_class_and_mixed_are_empty() {
        // Multiple classes → empty; per-entry recovery is in the plan ops.
        let err = StitchError::apply(vec![FailureClass::ConflictReal, FailureClass::Render], "m");
        assert!(err.recoverable_via().is_empty());

        let err = StitchError::Mixed {
            classes: vec![FailureClass::ConflictReal, FailureClass::Render],
            message: "m".into(),
        };
        assert!(err.recoverable_via().is_empty());
    }

    #[test]
    fn recovery_action_serializes_minimal_fields() {
        let action = RecoveryAction::command("list", "list");
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["kind"], "list");
        assert_eq!(v["command"], "list");
        // Optional command fields are skipped when empty; vars is always
        // present (even empty) so `set-env` can signal unreliable extraction.
        assert!(v.get("flags").is_none());
        assert!(v.get("args").is_none());
        assert!(v.get("reason").is_none());
        assert_eq!(v["vars"], serde_json::json!([]));
    }

    #[test]
    fn recovery_action_manual_serializes_reason_only() {
        let action = RecoveryAction::manual("escalate");
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["kind"], "manual");
        assert!(v.get("command").is_none());
        assert_eq!(v["reason"], "escalate");
    }

    #[test]
    fn recoverable_via_apply_single_render_is_manual() {
        // The aggregate has the failure class but not the source path, so it
        // cannot emit a useful `edit-template` action without other files.
        let err = StitchError::apply(vec![FailureClass::Render], "m");
        let actions = err.recoverable_via();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "manual");
        assert!(actions[0].command.is_none());
        assert!(actions[0].reason.as_ref().unwrap().contains("template"));
    }

    #[test]
    fn details_basic_hook() {
        let err = StitchError::hook("pre-apply", "exit 1");
        let details = err.details().expect("details should be present");
        let parsed: Value = serde_json::from_str(&details).expect("details should be valid JSON");
        assert_eq!(parsed["hook"], "pre-apply");
        assert!(parsed["store"].is_null());
        assert_eq!(parsed["scope"], "global");
    }

    #[test]
    fn details_per_store_hook() {
        let err = StitchError::hook_store("pre", "exit 1", "dotfiles");
        let details = err.details().expect("details should be present");
        let parsed: Value = serde_json::from_str(&details).expect("details should be valid JSON");
        assert_eq!(parsed["hook"], "pre");
        assert_eq!(parsed["store"], "dotfiles");
        assert_eq!(parsed["scope"], "per-store");
    }

    #[test]
    fn details_special_characters_are_escaped() {
        // Quotes and backslashes must be escaped in the JSON output.
        let name = "pre\"apply";
        let store = "store\\backslash";
        let err = StitchError::hook_store(name, "detail", store);
        let details = err.details().expect("details should be present");
        let parsed: Value = serde_json::from_str(&details).expect("details should be valid JSON");
        assert_eq!(parsed["hook"], name);
        assert_eq!(parsed["store"], store);
        assert_eq!(parsed["scope"], "per-store");

        // Unicode characters should pass through unchanged.
        let err = StitchError::hook_store("日本語", "detail", "mañana");
        let details = err.details().expect("details should be present");
        let parsed: Value = serde_json::from_str(&details).expect("details should be valid JSON");
        assert_eq!(parsed["hook"], "日本語");
        assert_eq!(parsed["store"], "mañana");
    }

    #[test]
    fn details_all_outputs_are_valid_json() {
        for err in [
            StitchError::hook("pre-apply", "exit 1"),
            StitchError::hook_store("pre", "exit 1", "dotfiles"),
            StitchError::hook_store("quote\"hook", "d", "store\\name"),
            StitchError::Apply {
                classes: vec![FailureClass::Hook],
                message: "m".into(),
                details: Some(r#"{"hook":"pre","store":"s","scope":"per-store"}"#.into()),
            },
        ] {
            if let Some(details) = err.details() {
                serde_json::from_str::<Value>(&details).expect("details should be valid JSON");
            }
        }
    }

    #[test]
    fn details_apply_passes_through_valid_json() {
        let err = StitchError::Apply {
            classes: vec![FailureClass::Hook],
            message: "m".into(),
            details: Some(r#"{"hook":"pre","store":"s","scope":"per-store"}"#.into()),
        };
        let details = err.details().expect("details should be present");
        let parsed: Value = serde_json::from_str(&details).expect("details should be valid JSON");
        assert_eq!(parsed["hook"], "pre");
        assert_eq!(parsed["store"], "s");
        assert_eq!(parsed["scope"], "per-store");
    }

    #[test]
    fn details_apply_re_escapes_caller_supplied_strings() {
        // A valid caller-supplied details string with escaped quotes and
        // backslashes round-trips through serde_json correctly.
        let err = StitchError::Apply {
            classes: vec![FailureClass::Hook],
            message: "m".into(),
            details: Some(
                r#"{"hook":"quote\"hook","store":"slash\\store","scope":"per-store"}"#.into(),
            ),
        };
        let details = err.details().expect("details should be present");
        let parsed: Value = serde_json::from_str(&details).expect("details should be valid JSON");
        assert_eq!(parsed["hook"], "quote\"hook");
        assert_eq!(parsed["store"], "slash\\store");
        assert_eq!(parsed["scope"], "per-store");
    }

    #[test]
    fn details_apply_malformed_returns_none() {
        // Malformed caller-supplied details must not be returned as-is.
        let err = StitchError::Apply {
            classes: vec![FailureClass::Hook],
            message: "m".into(),
            details: Some(r#"{"hook":"pre","store":"unescaped"quote","scope":"per-store"}"#.into()),
        };
        assert!(err.details().is_none());
    }

    #[test]
    fn details_is_none_for_non_hook_errors() {
        for err in [
            StitchError::internal("x"),
            StitchError::render("/s", "d"),
            StitchError::conflict_real("/t"),
            StitchError::Apply {
                classes: vec![FailureClass::Render],
                message: "m".into(),
                details: None,
            },
        ] {
            assert!(
                err.details().is_none(),
                "{:?} should have no details",
                err.class()
            );
        }
    }
}
