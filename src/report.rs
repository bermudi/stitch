//! JSON reporting and the shared v0.7 envelope for `--json` output.
//!
//! This module is the presentation layer for the read commands (`status`,
//! `list`, `doctor`, `prune`, `render`). It keeps the text and JSON views of
//! the same data structurally close so they do not drift.

use crate::config::{Config, Hooks, WhenClause};
use crate::error::{RecoveryAction, StitchError};
use crate::linker::LinkStatus;
use crate::render;
use crate::scan::FoundLink;
use crate::store::{DoctorResult, Severity, StatusEntry};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SCHEMA: u32 = 1;

/// The stable JSON envelope used by every `--json` command.
#[derive(Serialize)]
pub struct Envelope<T: Serialize> {
    pub schema: u32,
    pub command: &'static str,
    pub ok: bool,
    pub warnings: Vec<String>,
    pub data: Option<T>,
    pub error: Option<ErrorDetail>,
}

#[derive(Serialize)]
pub struct ErrorDetail {
    pub class: String,
    pub code: i32,
    pub message: String,
    pub hint: Option<String>,
    /// Reserved, machine-readable structured detail for future milestones
    /// (e.g. M3/M4 plan-stale precondition diffs). No command populates it
    /// today; it is `None` and serialized as `null`. Kept in the struct so
    /// the envelope shape is locked now and later fills are additive-only.
    pub details: Option<String>,
    /// Machine-readable recovery actions an agent can take to resolve this
    /// error. Empty when the error is not recoverable via a stitch command.
    /// `manual` entries signal "escalate, don't retry stitch commands."
    pub recoverable_via: Vec<RecoveryAction>,
}

/// Print a successful JSON envelope to stdout.
pub fn write<T: Serialize>(command: &'static str, data: T, warnings: Vec<String>) {
    let envelope = Envelope {
        schema: SCHEMA,
        command,
        ok: true,
        warnings,
        data: Some(data),
        error: None,
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("JSON serializable")
    );
}

/// Print a failed JSON envelope to stdout.
pub fn write_error(command: &'static str, error: &StitchError, warnings: Vec<String>) {
    let detail = ErrorDetail {
        class: error.class().id().to_string(),
        code: error.exit_code(),
        message: error.to_string(),
        hint: error.hint(),
        details: error.details(),
        recoverable_via: error.recoverable_via(),
    };
    let envelope = Envelope::<()> {
        schema: SCHEMA,
        command,
        ok: false,
        warnings,
        data: None,
        error: Some(detail),
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("JSON serializable")
    );
}

/// Print a JSON envelope that contains both data and an error, then exit with
/// the error's class code. Used when a command produced partial output (e.g.
/// `doctor` findings include errors) but must still report a non-zero result.
pub fn write_data_error<T: Serialize>(
    command: &'static str,
    data: T,
    error: &StitchError,
    warnings: Vec<String>,
) -> ! {
    let detail = ErrorDetail {
        class: error.class().id().to_string(),
        code: error.exit_code(),
        message: error.to_string(),
        hint: error.hint(),
        details: error.details(),
        recoverable_via: error.recoverable_via(),
    };
    let envelope = Envelope {
        schema: SCHEMA,
        command,
        ok: false,
        warnings,
        data: Some(data),
        error: Some(detail),
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("JSON serializable")
    );
    std::process::exit(error.exit_code());
}

/// The result of a JSON command: data plus warnings, or a boxed error plus
/// warnings. The error is boxed to keep the `Err` variant small.
pub type JsonResult<T> = Result<(T, Vec<String>), Box<(StitchError, Vec<String>)>>;

/// Run a computation and emit the appropriate JSON envelope.
///
/// On error the JSON envelope is written to stdout and the process exits with
/// the error's class code so callers never return a successful exit code for a
/// failed run. The closure returns warnings alongside the result so load-time
/// warnings are preserved even when a later step fails.
///
/// `audit_root` is `Some` for mutating commands (e.g. `prune --yes`) so the
/// audit log captures JSON-mode failures that would otherwise bypass the
/// central runner.
pub fn run_json<T: Serialize, F: FnOnce() -> JsonResult<T>>(
    command: &'static str,
    audit_root: Option<&std::path::Path>,
    f: F,
) -> Result<(), StitchError> {
    match f() {
        Ok((data, warnings)) => {
            write(command, data, warnings);
            Ok(())
        }
        Err(boxed) => {
            let (error, warnings) = *boxed;
            if let Some(root) = audit_root {
                crate::audit::append_command_result(root, command, Err(&error));
            }
            write_error(command, &error, warnings);
            std::process::exit(error.exit_code());
        }
    }
}

// ---------------------------------------------------------------------------
// status --json
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct StatusRow {
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    pub target: String,
    pub source: String,
    pub templated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<String>,
    pub state: String,
    pub skipped_platform: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolves_to: Option<String>,
}

pub fn status(repo_root: &Path, entries: &[StatusEntry]) -> Vec<StatusRow> {
    entries
        .iter()
        .map(|entry| status_row(repo_root, entry))
        .collect()
}

fn status_row(repo_root: &Path, entry: &StatusEntry) -> StatusRow {
    let staged_path = if entry.is_template && !entry.skipped_platform {
        let store_dir = repo_root.join(&entry.store_name);
        entry
            .source
            .strip_prefix(&store_dir)
            .ok()
            .and_then(|rel| rel.to_str())
            .map(|source_rel| {
                let resolved = render::resolve_entry(source_rel);
                render::staging_path(repo_root, &entry.store_name, &resolved.link_rel)
            })
            .map(|p| path_to_string(&p))
    } else {
        None
    };

    let (state, resolves_to) = match &entry.status {
        LinkStatus::Linked => ("linked".to_string(), None),
        LinkStatus::Missing => ("missing".to_string(), None),
        LinkStatus::Conflict(_) => ("conflict".to_string(), None),
        LinkStatus::Broken(p) => ("broken".to_string(), Some(path_to_string(p))),
        LinkStatus::Foreign(p) => ("foreign".to_string(), Some(path_to_string(p))),
        LinkStatus::StoreError(p) => ("store-error".to_string(), Some(path_to_string(p))),
        LinkStatus::ConfigError(msg) => ("config-error".to_string(), Some(msg.clone())),
    };

    StatusRow {
        store: entry.store_name.clone(),
        target_name: entry.target_name.clone(),
        target: path_to_string(&entry.target),
        source: path_to_string(&entry.source),
        templated: entry.is_template,
        staged_path,
        state,
        skipped_platform: entry.skipped_platform,
        resolves_to,
    }
}

// ---------------------------------------------------------------------------
// list --json
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ListStore {
    pub name: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<ListTarget>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(skip_serializing_if = "WhenClause::is_default")]
    pub when: WhenClause,
}

#[derive(Serialize)]
pub struct ListTarget {
    pub name: String,
    pub target: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(skip_serializing_if = "WhenClause::is_default")]
    pub when: WhenClause,
}

pub fn list(config: &Config) -> Vec<ListStore> {
    config
        .stores
        .iter()
        .map(|(name, store)| list_store(name, store))
        .collect()
}

fn list_store(name: &str, store: &crate::config::Store) -> ListStore {
    if store.is_multi_target() {
        let targets = Some(
            store
                .targets
                .iter()
                .map(|(target_name, target)| ListTarget {
                    name: target_name.clone(),
                    target: target.target.clone(),
                    mode: mode_for(target.files.is_empty() && target.patterns.is_empty()),
                    files: target.files.clone(),
                    patterns: target.patterns.clone(),
                    when: target.when.clone(),
                })
                .collect(),
        );
        ListStore {
            name: name.to_string(),
            mode: "multi-target".to_string(),
            target: None,
            targets,
            files: Vec::new(),
            patterns: Vec::new(),
            when: store.when.clone(),
        }
    } else {
        let has_files = !store.files.is_empty() || !store.patterns.is_empty();
        let mode = match (store.target.is_some(), has_files) {
            (false, false) => "none",
            (true, false) => "whole-dir",
            _ => "file-mode",
        };
        ListStore {
            name: name.to_string(),
            mode: mode.to_string(),
            target: store.target.clone(),
            targets: None,
            files: store.files.clone(),
            patterns: store.patterns.clone(),
            when: store.when.clone(),
        }
    }
}

fn mode_for(is_whole_dir: bool) -> String {
    if is_whole_dir {
        "whole-dir".to_string()
    } else {
        "file-mode".to_string()
    }
}

// ---------------------------------------------------------------------------
// doctor --json
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DoctorData {
    pub findings: Vec<DoctorRow>,
    pub summary: DoctorSummary,
}

#[derive(Serialize)]
pub struct DoctorSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
}

#[derive(Serialize)]
pub struct DoctorRow {
    pub id: String,
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

pub fn doctor(result: &DoctorResult) -> DoctorData {
    let findings: Vec<_> = result.findings.iter().map(doctor_row).collect();
    let summary = result.findings.iter().fold(
        DoctorSummary {
            errors: 0,
            warnings: 0,
            info: 0,
        },
        |acc, f| match f.severity {
            Severity::Error => DoctorSummary {
                errors: acc.errors + 1,
                ..acc
            },
            Severity::Warning => DoctorSummary {
                warnings: acc.warnings + 1,
                ..acc
            },
            Severity::Info => DoctorSummary {
                info: acc.info + 1,
                ..acc
            },
        },
    );
    DoctorData { findings, summary }
}

fn doctor_row(finding: &crate::store::DoctorFinding) -> DoctorRow {
    DoctorRow {
        id: finding.id.to_string(),
        severity: severity_to_string(finding.severity),
        message: finding.message.clone(),
        path: finding.path.as_ref().map(|p| path_to_string(p)),
        hint: finding.hint.clone(),
    }
}

fn severity_to_string(severity: Severity) -> String {
    match severity {
        Severity::Error => "error".to_string(),
        Severity::Warning => "warning".to_string(),
        Severity::Info => "info".to_string(),
    }
}

// ---------------------------------------------------------------------------
// prune --json
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PruneData {
    pub orphans: Vec<PruneRow>,
    pub removed: usize,
    pub failed: usize,
}

#[derive(Serialize)]
pub struct PruneRow {
    pub link: String,
    pub resolves_to: String,
    pub status: String,
}

pub fn prune(orphans: &[FoundLink], removed: usize, failed: usize) -> PruneData {
    PruneData {
        orphans: orphans
            .iter()
            .map(|link| prune_row(link, "listed"))
            .collect(),
        removed,
        failed,
    }
}

pub fn prune_with_status(
    orphans: &[FoundLink],
    statuses: &[String],
    removed: usize,
    failed: usize,
) -> PruneData {
    assert_eq!(
        orphans.len(),
        statuses.len(),
        "prune statuses must match orphan count"
    );
    PruneData {
        orphans: orphans
            .iter()
            .zip(statuses.iter())
            .map(|(link, status)| prune_row(link, status))
            .collect(),
        removed,
        failed,
    }
}

fn prune_row(link: &FoundLink, status: &str) -> PruneRow {
    PruneRow {
        link: path_to_string(&link.link),
        resolves_to: path_to_string(&link.resolves_to),
        status: status.to_string(),
    }
}

// ---------------------------------------------------------------------------
// render --json
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct RenderData {
    pub source: String,
    pub link_name: String,
    pub sha256: String,
    pub content: String,
}

pub fn render(source: &Path, source_rel: &str, content: &str) -> RenderData {
    RenderData {
        source: path_to_string(source),
        link_name: render::resolve_entry(source_rel).link_rel,
        sha256: sha256_hex(content),
        content: content.to_string(),
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

// --- Command DTOs (moved from main.rs) ---
// These are the JSON data shapes for the mutating commands (`add`, `remove`,
// `import`, `migrate`). They live in the report module alongside the read
// command DTOs so all JSON output types are in one place.

#[derive(Serialize)]
pub struct AddData {
    pub store: String,
    pub target: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    /// Absolute path of the created symlink(s). Present on post-op reports
    /// (real mutations); omitted on dry-run previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_created: Option<String>,
}

#[derive(Serialize)]
pub struct RemoveData {
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    pub staging: String,
    pub dry_run: bool,
    /// Present on post-op reports (real mutations); omitted on dry-run.
    /// `true` when `stitch.toml` behavior was left in place (the tool never
    /// rewrites authored config) — `doctor` will flag it as orphaned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior_orphaned: Option<bool>,
}

#[derive(Serialize)]
pub struct ImportedStore {
    pub store: String,
    pub target: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Present only for `multi-target` imports (stow-style fan-in: one store's
    /// file links span several target dirs). Each entry is one named target
    /// with its own file set. Empty for whole-dir and single-target file-mode
    /// imports, which use `target`/`files` instead.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<ImportedTarget>,
}

#[derive(Serialize)]
pub struct ImportedTarget {
    pub name: String,
    pub target: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

#[derive(Serialize)]
pub struct ImportData {
    pub dry_run: bool,
    pub imported: usize,
    pub skipped_owned: usize,
    pub stores: Vec<ImportedStore>,
}

#[derive(Serialize)]
pub struct MigrateData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The `.toml.bak` path where the original v0.2 config was preserved.
    /// Present on real migrations; omitted on dry-run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
}

// ---------------------------------------------------------------------------
// apply --json (composite)
// ---------------------------------------------------------------------------

/// Composite `apply --json` data: the full agent loop in one call.
///
/// `desired` is the host-evaluated merge (same shape as `explain --json`).
/// `plan` is the existing `Plan` shape — byte-identical to the pre-composite
/// `apply --json` output, so key-access consumers keep working.
/// `result` is `null` on `--dry-run`; on real apply it carries per-op
/// execution outcome. `post_status` is the status of the applied stores
/// after execution (the verification an agent would otherwise do via a
/// second `status` call).
#[derive(Serialize)]
pub struct ApplyData {
    pub desired: ExplainData,
    pub plan: crate::plan::Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ApplyResult>,
    pub post_status: Vec<StatusRow>,
}

/// Per-op execution outcome. On a clean apply, every op is `ok`. On a
/// partial failure, the agent sees which target failed and the error class.
#[derive(Serialize)]
pub struct ApplyResult {
    pub ops_executed: usize,
    pub ops_total: usize,
    pub conflicts: usize,
    pub errors: usize,
    /// Per-store outcome summary. Each store's ops are classified as
    /// `ok`, `conflict`, or `error` based on the plan ops.
    pub stores: Vec<ApplyResultStore>,
}

#[derive(Serialize)]
pub struct ApplyResultStore {
    pub store: String,
    pub ok: usize,
    pub conflicts: usize,
    pub errors: usize,
    pub skipped: usize,
}

// ---------------------------------------------------------------------------
// explain --json
// ---------------------------------------------------------------------------

/// The fully-resolved, host-evaluated desired state. Reused as `desired` in
/// the composite `apply --json` envelope (§3 of the agent-loop plan).
#[derive(Serialize)]
pub struct ExplainData {
    pub platform: ExplainPlatform,
    pub stores: Vec<ExplainStore>,
}

#[derive(Serialize)]
pub struct ExplainPlatform {
    pub os: String,
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    pub hostname: String,
    pub shell: String,
}

#[derive(Serialize)]
pub struct ExplainStore {
    pub name: String,
    /// Whether this store's `when` matches the current host.
    pub active: bool,
    #[serde(skip_serializing_if = "WhenClause::is_default")]
    pub when: WhenClause,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ExplainEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<ExplainTarget>,
    #[serde(skip_serializing_if = "Hooks::is_default")]
    pub hooks: Hooks,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

#[derive(Serialize)]
pub struct ExplainTarget {
    pub name: String,
    pub active: bool,
    #[serde(skip_serializing_if = "WhenClause::is_default")]
    pub when: WhenClause,
    pub target: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ExplainEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

#[derive(Serialize)]
pub struct ExplainEntry {
    /// Source path relative to the store directory (may end in `.tmpl`).
    pub source: String,
    /// `true` when the source ends in `.tmpl`.
    pub templated: bool,
    /// Link name under the target (`.tmpl` stripped).
    pub link_name: String,
}

// ---------------------------------------------------------------------------
// why --json
// ---------------------------------------------------------------------------

/// Single-path investigation: what owns this target, its source, link state.
/// Merges config + filesystem for one target so an agent doesn't cross-
/// reference `list` + `status` + `doctor`.
#[derive(Serialize)]
pub struct WhyData {
    pub query: String,
    /// The matched entry, or null when no store owns this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<WhyEntry>,
    /// True when the query path is under a store's target but the store is
    /// skipped on this host (when mismatch).
    #[serde(skip_serializing_if = "skip_bool_false")]
    pub skipped_platform: bool,
}

#[derive(Serialize)]
pub struct WhyEntry {
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    pub target: String,
    /// Repo source path (absolute). For templates, this is the `.tmpl` source
    /// — never the staged render.
    pub source: String,
    pub templated: bool,
    /// Link state: linked|missing|conflict|broken|foreign|store-error|config-error.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolves_to: Option<String>,
    /// Which config file owns the inventory for this entry.
    pub owning_config: String,
}

fn skip_bool_false(b: &bool) -> bool {
    !b
}

// ---------------------------------------------------------------------------
// bulk add --json
// ---------------------------------------------------------------------------

/// Result of a bulk `add` operation (multiple paths in one invocation).
#[derive(Serialize)]
pub struct BulkAddData {
    /// One entry per path, in order.
    pub results: Vec<BulkAddResult>,
    /// True if every path succeeded.
    pub all_ok: bool,
}

/// One path's outcome in a bulk add.
#[derive(Serialize)]
pub struct BulkAddResult {
    /// The path that was added.
    pub path: String,
    /// The store name it was added as (derived from basename).
    pub store: String,
    /// True if this path was added successfully.
    pub ok: bool,
    /// Error message if `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Dry-run: no changes were made.
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditEntry;
    use crate::config::{Config, Hooks, Store, WhenClause};
    use crate::linker::LinkStatus;
    use crate::plan::*;
    use crate::scan::FoundLink;
    use crate::store::{DoctorFinding, DoctorResult, Severity, StatusEntry};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from("/tmp/repo")
    }

    fn status_entry(
        store_name: &str,
        target: &str,
        source: &str,
        status: LinkStatus,
        is_template: bool,
        skipped: bool,
    ) -> StatusEntry {
        StatusEntry {
            store_name: store_name.to_string(),
            target_name: None,
            source: PathBuf::from(source),
            link_source: PathBuf::from(source),
            target: PathBuf::from(target),
            status,
            skipped_platform: skipped,
            is_template,
        }
    }

    #[test]
    fn schema_constant_is_one() {
        assert_eq!(SCHEMA, 1);
    }

    #[test]
    fn envelope_serializes_with_required_fields() {
        let env = Envelope {
            schema: SCHEMA,
            command: "status",
            ok: true,
            warnings: vec!["w".to_string()],
            data: Some(vec!["x"]),
            error: None,
        };
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["command"], "status");
        assert_eq!(v["ok"], true);
        assert_eq!(v["warnings"][0], "w");
        assert_eq!(v["data"][0], "x");
        assert!(v["error"].is_null());
    }

    #[test]
    fn envelope_error_serializes_details_as_null_when_absent() {
        let detail = ErrorDetail {
            class: "internal".to_string(),
            code: 1,
            message: "boom".to_string(),
            hint: None,
            details: None,
            recoverable_via: Vec::new(),
        };
        let env = Envelope::<()> {
            schema: SCHEMA,
            command: "apply",
            ok: false,
            warnings: vec![],
            data: None,
            error: Some(detail),
        };
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["data"].is_null());
        assert_eq!(v["error"]["class"], "internal");
        assert_eq!(v["error"]["code"], 1);
        assert!(v["error"]["hint"].is_null());
        assert!(v["error"]["details"].is_null());
        assert!(v["error"]["recoverable_via"].is_array());
        assert!(v["error"]["recoverable_via"].as_array().unwrap().is_empty());
        assert_eq!(v["schema"], 1);
    }

    #[test]
    fn status_maps_link_status_variants() {
        let repo = repo_root();
        let cases = vec![
            (LinkStatus::Linked, "linked", None),
            (LinkStatus::Missing, "missing", None),
            (LinkStatus::Conflict(PathBuf::from("/a")), "conflict", None),
            (
                LinkStatus::Broken(PathBuf::from("/gone")),
                "broken",
                Some("/gone"),
            ),
            (
                LinkStatus::Foreign(PathBuf::from("/foreign")),
                "foreign",
                Some("/foreign"),
            ),
            (
                LinkStatus::StoreError(PathBuf::from("/store")),
                "store-error",
                Some("/store"),
            ),
            (
                LinkStatus::ConfigError("bad".to_string()),
                "config-error",
                Some("bad"),
            ),
        ];
        for (status, expected_state, expected_resolves) in cases {
            let entry = status_entry("s", "/tgt", "/src", status, false, false);
            let rows = super::status(&repo, &[entry]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].state, expected_state);
            match expected_resolves {
                Some(v) => assert_eq!(rows[0].resolves_to.as_deref(), Some(v)),
                None => assert!(rows[0].resolves_to.is_none()),
            }
        }
    }

    #[test]
    fn status_row_omits_resolves_to_for_linked_and_missing() {
        let repo = repo_root();
        for status in [LinkStatus::Linked, LinkStatus::Missing] {
            let entry = status_entry("s", "/tgt", "/src", status, false, false);
            let rows = super::status(&repo, &[entry]);
            let v = serde_json::to_value(&rows[0]).unwrap();
            assert!(
                v.get("resolves_to").is_none() || v["resolves_to"].is_null(),
                "resolves_to should be omitted for {:?}",
                rows[0].state
            );
        }
    }

    #[test]
    fn status_row_staged_path_only_for_active_template() {
        let repo = PathBuf::from("/repo");
        // Active template: is_template true, not skipped, source under store dir
        let source = "/repo/git/gitconfig.tmpl";
        let entry = StatusEntry {
            store_name: "git".to_string(),
            target_name: None,
            source: PathBuf::from(source),
            link_source: PathBuf::from(source),
            target: PathBuf::from("/home/.gitconfig"),
            status: LinkStatus::Linked,
            skipped_platform: false,
            is_template: true,
        };
        let rows = super::status(&repo, &[entry]);
        assert!(rows[0].staged_path.is_some());
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert!(v.get("staged_path").is_some());
        assert!(
            v["staged_path"]
                .as_str()
                .unwrap()
                .contains(".stitch/render")
        );
        assert!(rows[0].templated);

        // Same entry but skipped_platform => no staged_path
        let entry2 = StatusEntry {
            store_name: "git".to_string(),
            target_name: None,
            source: PathBuf::from(source),
            link_source: PathBuf::from(source),
            target: PathBuf::from("/home/.gitconfig"),
            status: LinkStatus::Linked,
            skipped_platform: true,
            is_template: true,
        };
        let rows2 = super::status(&repo, &[entry2]);
        assert!(rows2[0].staged_path.is_none());
        let v2 = serde_json::to_value(&rows2[0]).unwrap();
        assert!(v2.get("staged_path").is_none());
    }

    #[test]
    fn status_row_non_template_never_has_staged_path() {
        let repo = PathBuf::from("/repo");
        let entry = status_entry(
            "s",
            "/tgt",
            "/repo/s/file",
            LinkStatus::Linked,
            false,
            false,
        );
        let rows = super::status(&repo, &[entry]);
        assert!(rows[0].staged_path.is_none());
    }

    #[test]
    fn status_preserves_target_name_and_templated_flag() {
        let repo = repo_root();
        let mut entry = status_entry("s", "/tgt", "/src", LinkStatus::Linked, false, false);
        entry.target_name = Some("laptop".to_string());
        let rows = super::status(&repo, &[entry]);
        assert_eq!(rows[0].target_name.as_deref(), Some("laptop"));
        assert!(!rows[0].templated);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(v["target_name"], "laptop");
    }

    #[test]
    fn status_row_omits_optional_fields_when_none() {
        let repo = repo_root();
        let entry = status_entry("s", "/tgt", "/src", LinkStatus::Linked, false, false);
        let rows = super::status(&repo, &[entry]);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert!(v.get("target_name").is_none());
        assert!(v.get("staged_path").is_none());
        assert!(v.get("resolves_to").is_none());
    }

    fn store_with(target: Option<&str>, files: &[&str], patterns: &[&str]) -> Store {
        Store {
            target: target.map(|t| t.to_string()),
            files: files.iter().map(|s| s.to_string()).collect(),
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            ignore: vec![],
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets: BTreeMap::new(),
        }
    }

    #[test]
    fn list_mode_whole_dir() {
        let mut cfg = Config::empty();
        cfg.stores.insert(
            "nvim".to_string(),
            store_with(Some("~/.config/nvim"), &[], &[]),
        );
        let rows = super::list(&cfg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "whole-dir");
        assert_eq!(rows[0].target.as_deref(), Some("~/.config/nvim"));
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(v["mode"], "whole-dir");
        // files/patterns empty => omitted
        assert!(v.get("files").is_none());
    }

    #[test]
    fn list_mode_file_mode() {
        let mut cfg = Config::empty();
        cfg.stores.insert(
            "shells".to_string(),
            store_with(Some("~"), &[".bashrc"], &[]),
        );
        let rows = super::list(&cfg);
        assert_eq!(rows[0].mode, "file-mode");
    }

    #[test]
    fn list_mode_none_when_no_target() {
        let mut cfg = Config::empty();
        cfg.stores
            .insert("blank".to_string(), store_with(None, &[], &[]));
        let rows = super::list(&cfg);
        assert_eq!(rows[0].mode, "none");
    }

    #[test]
    fn list_multi_target_mode() {
        let mut cfg = Config::empty();
        let mut targets = BTreeMap::new();
        targets.insert(
            "laptop".to_string(),
            crate::config::TargetEntry {
                target: "~/.config/helix".to_string(),
                files: vec![],
                patterns: vec![],
                ignore: vec![],
                when: WhenClause::default(),
            },
        );
        cfg.stores.insert(
            "helix".to_string(),
            Store {
                target: None,
                files: vec![],
                patterns: vec![],
                ignore: vec![],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets,
            },
        );
        let rows = super::list(&cfg);
        assert_eq!(rows[0].mode, "multi-target");
        assert!(rows[0].targets.is_some());
        assert_eq!(rows[0].targets.as_ref().unwrap().len(), 1);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(v["mode"], "multi-target");
        assert!(v.get("target").is_none());
    }

    #[test]
    fn list_when_omitted_when_default() {
        let mut cfg = Config::empty();
        cfg.stores
            .insert("s".to_string(), store_with(Some("~"), &[], &[]));
        let rows = super::list(&cfg);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert!(v.get("when").is_none());
    }

    #[test]
    fn doctor_summary_counts_by_severity() {
        let result = DoctorResult {
            findings: vec![
                DoctorFinding {
                    id: "a",
                    severity: Severity::Error,
                    message: "e".to_string(),
                    path: None,
                    hint: None,
                },
                DoctorFinding {
                    id: "b",
                    severity: Severity::Warning,
                    message: "w".to_string(),
                    path: Some(PathBuf::from("/p")),
                    hint: Some("h".to_string()),
                },
                DoctorFinding {
                    id: "c",
                    severity: Severity::Info,
                    message: "i".to_string(),
                    path: None,
                    hint: None,
                },
                DoctorFinding {
                    id: "d",
                    severity: Severity::Error,
                    message: "e2".to_string(),
                    path: None,
                    hint: None,
                },
            ],
        };
        let data = super::doctor(&result);
        assert_eq!(data.summary.errors, 2);
        assert_eq!(data.summary.warnings, 1);
        assert_eq!(data.summary.info, 1);
        assert_eq!(data.findings.len(), 4);
        // severity strings
        assert_eq!(data.findings[0].severity, "error");
        assert_eq!(data.findings[1].severity, "warning");
        assert_eq!(data.findings[2].severity, "info");
        // path/hint omission
        let v = serde_json::to_value(&data.findings[0]).unwrap();
        assert!(v.get("path").is_none());
        assert!(v.get("hint").is_none());
        let v1 = serde_json::to_value(&data.findings[1]).unwrap();
        assert_eq!(v1["path"], "/p");
        assert_eq!(v1["hint"], "h");
    }

    #[test]
    fn prune_row_fields() {
        let links = vec![FoundLink {
            link: PathBuf::from("/home/.oldrc"),
            resolves_to: PathBuf::from("/repo/old/.oldrc"),
        }];
        let data = super::prune(&links, 0, 0);
        assert_eq!(data.orphans.len(), 1);
        assert_eq!(data.orphans[0].link, "/home/.oldrc");
        assert_eq!(data.orphans[0].resolves_to, "/repo/old/.oldrc");
        assert_eq!(data.orphans[0].status, "listed");
        assert_eq!(data.removed, 0);
        assert_eq!(data.failed, 0);
    }

    #[test]
    fn prune_with_status_pairs_correctly() {
        let links = vec![
            FoundLink {
                link: PathBuf::from("/a"),
                resolves_to: PathBuf::from("/r/a"),
            },
            FoundLink {
                link: PathBuf::from("/b"),
                resolves_to: PathBuf::from("/r/b"),
            },
        ];
        let data =
            super::prune_with_status(&links, &["removed".to_string(), "failed".to_string()], 1, 1);
        assert_eq!(data.orphans[0].status, "removed");
        assert_eq!(data.orphans[1].status, "failed");
        assert_eq!(data.removed, 1);
        assert_eq!(data.failed, 1);
    }

    #[test]
    #[should_panic(expected = "prune statuses must match orphan count")]
    fn prune_with_status_panics_on_mismatch() {
        let links = vec![FoundLink {
            link: PathBuf::from("/a"),
            resolves_to: PathBuf::from("/r/a"),
        }];
        super::prune_with_status(&links, &[], 0, 0);
    }

    #[test]
    fn render_data_sha256_is_hex_and_deterministic() {
        let source = PathBuf::from("/repo/git/gitconfig.tmpl");
        let d1 = super::render(&source, "gitconfig.tmpl", "hello");
        let d2 = super::render(&source, "gitconfig.tmpl", "hello");
        assert_eq!(d1.sha256, d2.sha256);
        assert_eq!(d1.sha256.len(), 64);
        assert!(d1.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        // Known value for "hello"
        assert_eq!(
            d1.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(d1.link_name, "gitconfig");
        assert_eq!(d1.content, "hello");
        assert_eq!(d1.source, "/repo/git/gitconfig.tmpl");
    }

    #[test]
    fn sha256_hex_empty_string() {
        // SHA256 of empty string is well-known
        let d = super::render(&PathBuf::from("/src"), "file", "");
        assert_eq!(
            d.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    fn assert_keys_eq(value: &Value, expected: &[&str]) {
        let obj = value.as_object().expect("expected object");
        let actual: std::collections::BTreeSet<String> = obj.keys().cloned().collect();
        let expected_set: std::collections::BTreeSet<String> =
            expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual, expected_set,
            "keys mismatch: got {actual:?} want {expected_set:?} full value={value}"
        );
    }

    fn agent_schema() -> Value {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/agent-schema.json"
        ));
        serde_json::from_str(raw).expect("docs/agent-schema.json is valid JSON")
    }

    fn base_shape_name(type_str: &str) -> Option<&str> {
        let delimiters = ['|', '(', ' ', '['];
        let end = type_str
            .char_indices()
            .find(|(_, c)| delimiters.contains(c))
            .map(|(i, _)| i)
            .unwrap_or(type_str.len());
        let base = &type_str[..end];
        let base = base.trim();
        if base.is_empty() { None } else { Some(base) }
    }

    fn check_value_against_shape(value: &Value, shape: &Value, path: &str, schema: &Value) {
        let obj = value
            .as_object()
            .unwrap_or_else(|| panic!("{path}: expected a JSON object"));

        let (field_map, tag) = if let Some(fields) = shape.get("fields").and_then(Value::as_object)
        {
            (fields.clone(), None)
        } else if let Some(tag) = shape.get("tag").and_then(Value::as_str) {
            let variants = shape["variants"]
                .as_object()
                .expect("enum shape must declare variants");
            let variant_name = obj[tag]
                .as_str()
                .unwrap_or_else(|| panic!("{path}: missing enum tag {tag}"));
            let variant = variants
                .get(variant_name)
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("{path}: unknown variant {variant_name}"));
            (variant.clone(), Some(tag.to_string()))
        } else {
            panic!("{path}: schema node is not an object or enum shape: {shape}");
        };

        let mut expected: std::collections::BTreeSet<String> = field_map.keys().cloned().collect();
        if let Some(tag) = &tag {
            expected.insert(tag.clone());
        }
        let actual: std::collections::BTreeSet<String> = obj.keys().cloned().collect();
        assert_eq!(actual, expected, "{path}: JSON keys do not match schema");

        for (key, field_value) in obj {
            if Some(key) == tag.as_ref() {
                continue;
            }
            let field_schema = &field_map[key];
            if let Some(type_str) = field_schema.as_str()
                && let Some(base) = base_shape_name(type_str)
                && let Some(nested_shape) = schema["shapes"].get(base)
            {
                match field_value {
                    Value::Object(_) => {
                        check_value_against_shape(
                            field_value,
                            nested_shape,
                            &format!("{path}.{key}"),
                            schema,
                        );
                    }
                    Value::Array(arr) => {
                        for (i, item) in arr.iter().enumerate() {
                            if !item.is_null() {
                                check_value_against_shape(
                                    item,
                                    nested_shape,
                                    &format!("{path}.{key}[{i}]"),
                                    schema,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn envelope_schema_is_exact() {
        let ok_env = Envelope {
            schema: SCHEMA,
            command: "status",
            ok: true,
            warnings: vec![],
            data: Some(vec!["x"]),
            error: None,
        };
        let v = serde_json::to_value(&ok_env).unwrap();
        assert_keys_eq(
            &v,
            &["command", "data", "error", "ok", "schema", "warnings"],
        );
        assert_eq!(v["schema"], 1);
        assert_eq!(v["command"], "status");
        assert_eq!(v["ok"], true);
        assert!(v["error"].is_null());

        let detail = ErrorDetail {
            class: "internal".to_string(),
            code: 1,
            message: "boom".to_string(),
            hint: None,
            details: None,
            recoverable_via: Vec::new(),
        };
        let err_env = Envelope::<()> {
            schema: SCHEMA,
            command: "apply",
            ok: false,
            warnings: vec![],
            data: None,
            error: Some(detail),
        };
        let v = serde_json::to_value(&err_env).unwrap();
        assert_keys_eq(
            &v,
            &["command", "data", "error", "ok", "schema", "warnings"],
        );
        assert_eq!(v["ok"], false);
        assert!(v["data"].is_null());
        assert_keys_eq(
            &v["error"],
            &[
                "class",
                "code",
                "details",
                "hint",
                "message",
                "recoverable_via",
            ],
        );
    }

    #[test]
    fn status_schema_exact_and_optional_fields() {
        let repo = PathBuf::from("/repo");
        // Minimal: linked, no optional fields
        let entry = status_entry("s", "/tgt", "/src", LinkStatus::Linked, false, false);
        let rows = super::status(&repo, &[entry]);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_keys_eq(
            &v,
            &[
                "skipped_platform",
                "source",
                "state",
                "store",
                "target",
                "templated",
            ],
        );
        assert_eq!(v["state"], "linked");

        // With every optional populated: broken templated active
        let entry = StatusEntry {
            store_name: "git".to_string(),
            target_name: Some("laptop".to_string()),
            source: PathBuf::from("/repo/git/gitconfig.tmpl"),
            link_source: PathBuf::from("/repo/git/gitconfig.tmpl"),
            target: PathBuf::from("/home/.gitconfig"),
            status: LinkStatus::Broken(PathBuf::from("/gone")),
            skipped_platform: false,
            is_template: true,
        };
        let rows = super::status(&repo, &[entry]);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_keys_eq(
            &v,
            &[
                "resolves_to",
                "skipped_platform",
                "source",
                "staged_path",
                "state",
                "store",
                "target",
                "target_name",
                "templated",
            ],
        );
        assert_eq!(v["state"], "broken");
        assert_eq!(v["resolves_to"], "/gone");
        assert_eq!(v["target_name"], "laptop");
        assert!(
            v["staged_path"]
                .as_str()
                .unwrap()
                .contains(".stitch/render")
        );

        // Skipped platform must still omit staged_path even if templated
        let entry = StatusEntry {
            store_name: "git".to_string(),
            target_name: None,
            source: PathBuf::from("/repo/git/gitconfig.tmpl"),
            link_source: PathBuf::from("/repo/git/gitconfig.tmpl"),
            target: PathBuf::from("/home/.gitconfig"),
            status: LinkStatus::Missing,
            skipped_platform: true,
            is_template: true,
        };
        let rows = super::status(&repo, &[entry]);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert!(v.get("staged_path").is_none());
        assert_keys_eq(
            &v,
            &[
                "skipped_platform",
                "source",
                "state",
                "store",
                "target",
                "templated",
            ],
        );
    }

    #[test]
    fn list_schema_exact() {
        // Single-target whole-dir
        let mut cfg = Config::empty();
        cfg.stores.insert(
            "nvim".to_string(),
            store_with(Some("~/.config/nvim"), &[], &[]),
        );
        let rows = super::list(&cfg);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_keys_eq(&v, &["mode", "name", "target"]);
        assert_eq!(v["mode"], "whole-dir");

        // Single-target file-mode with files visible
        let mut cfg = Config::empty();
        cfg.stores.insert(
            "shells".to_string(),
            store_with(Some("~"), &[".bashrc"], &["*.bak"]),
        );
        let rows = super::list(&cfg);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_keys_eq(&v, &["files", "mode", "name", "patterns", "target"]);
        assert_eq!(v["mode"], "file-mode");
        assert_eq!(v["files"][0], ".bashrc");
        assert_eq!(v["patterns"][0], "*.bak");

        // Multi-target
        let mut cfg = Config::empty();
        let mut targets = BTreeMap::new();
        targets.insert(
            "laptop".to_string(),
            crate::config::TargetEntry {
                target: "~/.config/helix".to_string(),
                files: vec!["config.toml".to_string()],
                patterns: vec![],
                ignore: vec![],
                when: WhenClause {
                    hostname: Some("laptop".to_string()),
                    ..Default::default()
                },
            },
        );
        cfg.stores.insert(
            "helix".to_string(),
            Store {
                target: None,
                files: vec![],
                patterns: vec![],
                ignore: vec![],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets,
            },
        );
        let rows = super::list(&cfg);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_keys_eq(&v, &["mode", "name", "targets"]);
        assert_eq!(v["mode"], "multi-target");
        let t = &v["targets"][0];
        assert_keys_eq(t, &["files", "mode", "name", "target", "when"]);
        assert_eq!(t["name"], "laptop");
        assert_eq!(t["mode"], "file-mode");
        // when populated
        assert_eq!(t["when"]["hostname"], "laptop");

        // none mode (no target)
        let mut cfg = Config::empty();
        cfg.stores
            .insert("blank".to_string(), store_with(None, &[], &[]));
        let rows = super::list(&cfg);
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_keys_eq(&v, &["mode", "name"]);
        assert_eq!(v["mode"], "none");
    }

    #[test]
    fn doctor_schema_exact() {
        // Empty
        let data = super::doctor(&DoctorResult { findings: vec![] });
        let v = serde_json::to_value(&data).unwrap();
        assert_keys_eq(&v, &["findings", "summary"]);
        assert_keys_eq(&v["summary"], &["errors", "info", "warnings"]);
        assert!(v["findings"].as_array().unwrap().is_empty());

        // Populated: check DoctorRow keys exact with and without optionals
        let data = super::doctor(&DoctorResult {
            findings: vec![
                DoctorFinding {
                    id: "broken-link",
                    severity: Severity::Error,
                    message: "bad".to_string(),
                    path: Some(PathBuf::from("/p")),
                    hint: Some("fix".to_string()),
                },
                DoctorFinding {
                    id: "info",
                    severity: Severity::Info,
                    message: "ok".to_string(),
                    path: None,
                    hint: None,
                },
            ],
        });
        let v = serde_json::to_value(&data).unwrap();
        let row_with = &v["findings"][0];
        assert_keys_eq(row_with, &["hint", "id", "message", "path", "severity"]);
        assert_eq!(row_with["severity"], "error");
        let row_without = &v["findings"][1];
        assert_keys_eq(row_without, &["id", "message", "severity"]);
    }

    #[test]
    fn prune_schema_exact() {
        let links = vec![FoundLink {
            link: PathBuf::from("/home/.oldrc"),
            resolves_to: PathBuf::from("/repo/old/.oldrc"),
        }];
        let data = super::prune(&links, 1, 2);
        let v = serde_json::to_value(&data).unwrap();
        assert_keys_eq(&v, &["failed", "orphans", "removed"]);
        assert_eq!(v["removed"], 1);
        assert_eq!(v["failed"], 2);
        let row = &v["orphans"][0];
        assert_keys_eq(row, &["link", "resolves_to", "status"]);
        assert_eq!(row["status"], "listed");

        let links2 = vec![
            FoundLink {
                link: PathBuf::from("/a"),
                resolves_to: PathBuf::from("/r/a"),
            },
            FoundLink {
                link: PathBuf::from("/b"),
                resolves_to: PathBuf::from("/r/b"),
            },
        ];
        let data = super::prune_with_status(
            &links2,
            &["removed".to_string(), "failed".to_string()],
            1,
            1,
        );
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["orphans"][0]["status"], "removed");
        assert_keys_eq(&v["orphans"][0], &["link", "resolves_to", "status"]);
        // empty orphans still exact
        let data = super::prune(&[], 0, 0);
        let v = serde_json::to_value(&data).unwrap();
        assert_keys_eq(&v, &["failed", "orphans", "removed"]);
        assert!(v["orphans"].as_array().unwrap().is_empty());
    }

    #[test]
    fn render_schema_exact() {
        let source = PathBuf::from("/repo/git/gitconfig.tmpl");
        let d = super::render(&source, "gitconfig.tmpl", "hello");
        let v = serde_json::to_value(&d).unwrap();
        assert_keys_eq(&v, &["content", "link_name", "sha256", "source"]);
        assert_eq!(v["link_name"], "gitconfig");
        assert_eq!(v["content"], "hello");
        assert_eq!(v["source"], "/repo/git/gitconfig.tmpl");
        assert_eq!(
            v["sha256"],
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );

        // Ensure .tmpl stripping vs plain file
        let d2 = super::render(&PathBuf::from("/repo/s/file"), "file", "x");
        let v2 = serde_json::to_value(&d2).unwrap();
        assert_eq!(v2["link_name"], "file");
    }

    #[allow(clippy::too_many_lines)]
    fn dto_samples() -> Vec<(&'static str, Value)> {
        let mut samples = Vec::new();

        let wc = WhenClause {
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            distro: Some("ubuntu".into()),
            hostname: Some("h".into()),
            shell: Some("bash".into()),
        };

        let hooks = Hooks {
            pre: Some("echo pre".into()),
            post: Some("echo post".into()),
        };

        // StatusRow (and reused later in ApplyData.post_status)
        let status_row = StatusRow {
            store: "git".into(),
            target_name: Some("laptop".into()),
            target: "/home/.gitconfig".into(),
            source: "/repo/git/gitconfig.tmpl".into(),
            templated: true,
            staged_path: Some("/repo/.stitch/render/git/gitconfig".into()),
            state: "broken".into(),
            skipped_platform: false,
            resolves_to: Some("/gone".into()),
        };
        samples.push(("StatusRow", serde_json::to_value(&status_row).unwrap()));

        // ListStore (covers ListTarget and WhenClause via recursion)
        samples.push((
            "ListStore",
            serde_json::to_value(ListStore {
                name: "shells".into(),
                mode: "file-mode".into(),
                target: Some("~".into()),
                targets: Some(vec![ListTarget {
                    name: "laptop".into(),
                    target: "~/.config/helix".into(),
                    mode: "file-mode".into(),
                    files: vec!["config.toml".into()],
                    patterns: vec!["*.bak".into()],
                    when: wc.clone(),
                }]),
                files: vec![".bashrc".into()],
                patterns: vec!["*.bak".into()],
                when: wc.clone(),
            })
            .unwrap(),
        ));

        // Plan (covers PlanStore, PlanSummary, PlanOp, LinkRequires, TargetState)
        let plan = Plan::from_stores(vec![PlanStore {
            store_name: "git".into(),
            ops: vec![PlanOp::CreateLink {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                requires: LinkRequires::with_backup(
                    TargetState::SymlinkTo("/repo/git/gitconfig".into()),
                    TargetState::Absent,
                ),
            }],
        }]);
        samples.push(("Plan", serde_json::to_value(&plan).unwrap()));

        // ExplainData (covers ExplainPlatform, ExplainStore, ExplainTarget, ExplainEntry, WhenClause, Hooks)
        let explain_data = ExplainData {
            platform: ExplainPlatform {
                os: "linux".into(),
                arch: "x86_64".into(),
                distro: Some("ubuntu".into()),
                hostname: "h".into(),
                shell: "bash".into(),
            },
            stores: vec![ExplainStore {
                name: "git".into(),
                active: true,
                when: wc.clone(),
                mode: "file-mode".into(),
                target: Some("~".into()),
                entries: vec![ExplainEntry {
                    source: "gitconfig.tmpl".into(),
                    templated: true,
                    link_name: "gitconfig".into(),
                }],
                targets: vec![ExplainTarget {
                    name: "laptop".into(),
                    active: true,
                    when: wc.clone(),
                    target: "~/.config/helix".into(),
                    mode: "file-mode".into(),
                    entries: vec![ExplainEntry {
                        source: "config.toml".into(),
                        templated: false,
                        link_name: "config.toml".into(),
                    }],
                    ignore: vec!["*.bak".into()],
                }],
                hooks: hooks.clone(),
                ignore: vec!["*.log".into()],
            }],
        };
        samples.push(("ExplainData", serde_json::to_value(&explain_data).unwrap()));

        // ApplyData (covers ApplyResult, ApplyResultStore, StatusRow, Plan, ExplainData)
        samples.push((
            "ApplyData",
            serde_json::to_value(ApplyData {
                desired: explain_data,
                plan,
                result: Some(ApplyResult {
                    ops_executed: 1,
                    ops_total: 2,
                    conflicts: 0,
                    errors: 0,
                    stores: vec![ApplyResultStore {
                        store: "git".into(),
                        ok: 1,
                        conflicts: 0,
                        errors: 0,
                        skipped: 0,
                    }],
                }),
                post_status: vec![status_row],
            })
            .unwrap(),
        ));

        // DoctorData (covers DoctorRow, DoctorSummary)
        samples.push((
            "DoctorData",
            serde_json::to_value(DoctorData {
                findings: vec![DoctorRow {
                    id: "broken-link".into(),
                    severity: "error".into(),
                    message: "bad".into(),
                    path: Some("/p".into()),
                    hint: Some("fix".into()),
                }],
                summary: DoctorSummary {
                    errors: 1,
                    warnings: 2,
                    info: 3,
                },
            })
            .unwrap(),
        ));

        // PruneData (covers PruneRow)
        samples.push((
            "PruneData",
            serde_json::to_value(PruneData {
                orphans: vec![PruneRow {
                    link: "/home/.oldrc".into(),
                    resolves_to: "/repo/old/.oldrc".into(),
                    status: "removed".into(),
                }],
                removed: 1,
                failed: 2,
            })
            .unwrap(),
        ));

        // RenderData
        samples.push((
            "RenderData",
            serde_json::to_value(RenderData {
                source: "/repo/git/gitconfig.tmpl".into(),
                link_name: "gitconfig".into(),
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
                content: "hello".into(),
            })
            .unwrap(),
        ));

        // AddData
        samples.push((
            "AddData",
            serde_json::to_value(AddData {
                store: "git".into(),
                target: "~/.gitconfig".into(),
                mode: "add-to-store".into(),
                source: Some("/repo/git/gitconfig".into()),
                files: vec!["gitconfig".into()],
                patterns: vec!["*.tmpl".into()],
                link_created: Some("/home/.gitconfig".into()),
            })
            .unwrap(),
        ));

        // RemoveData
        samples.push((
            "RemoveData",
            serde_json::to_value(RemoveData {
                store: "git".into(),
                target: Some("laptop".into()),
                links: vec!["/home/.gitconfig".into()],
                staging: "/repo/.stitch/render".into(),
                dry_run: false,
                behavior_orphaned: Some(true),
            })
            .unwrap(),
        ));

        // MigrateData
        samples.push((
            "MigrateData",
            serde_json::to_value(MigrateData {
                authored_path: Some("/repo/stitch.toml".into()),
                authored: Some("# authored".into()),
                state_path: Some("/repo/.stitch/state.toml".into()),
                state: Some("# state".into()),
                backup_path: Some("/repo/.stitch/config.toml.bak".into()),
            })
            .unwrap(),
        ));

        // ImportData (covers ImportedStore, ImportedTarget)
        samples.push((
            "ImportData",
            serde_json::to_value(ImportData {
                dry_run: false,
                imported: 1,
                skipped_owned: 2,
                stores: vec![ImportedStore {
                    store: "git".into(),
                    target: "~".into(),
                    mode: "file-mode".into(),
                    files: vec![".bashrc".into()],
                    targets: vec![ImportedTarget {
                        name: "laptop".into(),
                        target: "~/.config/helix".into(),
                        files: vec![".bashrc".into()],
                    }],
                }],
            })
            .unwrap(),
        ));

        // BulkAddData (covers BulkAddResult)
        samples.push((
            "BulkAddData",
            serde_json::to_value(BulkAddData {
                results: vec![BulkAddResult {
                    path: "/home/.bashrc".into(),
                    store: "bashrc".into(),
                    ok: false,
                    error: Some("boom".into()),
                    dry_run: false,
                }],
                all_ok: false,
            })
            .unwrap(),
        ));

        // WhyData (covers WhyEntry)
        samples.push((
            "WhyData",
            serde_json::to_value(WhyData {
                query: "/home/.gitconfig".into(),
                entry: Some(WhyEntry {
                    store: "git".into(),
                    target_name: Some("laptop".into()),
                    target: "/home/.gitconfig".into(),
                    source: "/repo/git/gitconfig.tmpl".into(),
                    templated: true,
                    state: "broken".into(),
                    resolves_to: Some("/gone".into()),
                    owning_config: "state.toml".into(),
                }),
                skipped_platform: true,
            })
            .unwrap(),
        ));

        // AuditEntry
        samples.push((
            "AuditEntry",
            serde_json::to_value(AuditEntry {
                timestamp: "unix:1700000000".into(),
                command: "apply".into(),
                store: Some("git".into()),
                target: Some("/home/.gitconfig".into()),
                outcome: "ok".into(),
                exit_class: Some("internal".into()),
                exit_code: 1,
            })
            .unwrap(),
        ));

        // WhenClause and Hooks (referenced by many shapes; also tested directly)
        samples.push(("WhenClause", serde_json::to_value(wc).unwrap()));
        samples.push(("Hooks", serde_json::to_value(hooks).unwrap()));

        // PlanOp: every variant
        let op_requires = LinkRequires::with_backup(
            TargetState::SymlinkTo("/repo/git/gitconfig".into()),
            TargetState::Absent,
        );
        for op in [
            PlanOp::StageRender {
                store: "git".into(),
                source_rel: "gitconfig.tmpl".into(),
                source: "/repo/git/gitconfig.tmpl".into(),
                staged: "/repo/.stitch/render/git/gitconfig".into(),
                sha256: "abc".into(),
            },
            PlanOp::CreateLink {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                requires: op_requires.clone(),
            },
            PlanOp::ReplaceLink {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                old_resolves_to: Some("/old".into()),
                requires: op_requires.clone(),
            },
            PlanOp::BackupAndLink {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                backup: "/home/.gitconfig.bak".into(),
                requires: op_requires.clone(),
            },
            PlanOp::RemoveLink {
                store: "git".into(),
                target: "/home/.gitconfig".into(),
                source: Some("/repo/git/gitconfig".into()),
                requires: op_requires.clone(),
            },
            PlanOp::RemoveStaged {
                path: "/repo/.stitch/render/git/gitconfig".into(),
            },
            PlanOp::ContentChanged {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                requires: op_requires.clone(),
            },
            PlanOp::AlreadyLinked {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                requires: op_requires.clone(),
            },
            PlanOp::Conflict {
                target: "/home/.gitconfig".into(),
                resolves_to: Some("/foreign".into()),
            },
            PlanOp::SkippedPlatform,
            PlanOp::Error {
                message: "boom".into(),
                class: "internal".into(),
                hook_name: None,
            },
        ] {
            samples.push(("PlanOp", serde_json::to_value(op).unwrap()));
        }

        // TargetState: every variant
        for state in [
            TargetState::Absent,
            TargetState::RealEntry,
            TargetState::SymlinkTo("/repo/git/gitconfig".into()),
            TargetState::SymlinkIntoRepo,
        ] {
            samples.push(("TargetState", serde_json::to_value(state).unwrap()));
        }

        samples
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn schema_shape_keys_match_dto_serializations() {
        let schema = agent_schema();
        let shapes = schema["shapes"]
            .as_object()
            .expect("shapes must be an object");

        for (name, value) in dto_samples() {
            let shape = &shapes[name];
            check_value_against_shape(&value, shape, name, &schema);
        }
    }
}
