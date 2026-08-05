//! JSON reporting and the shared v0.7 envelope for `--json` output.
//!
//! This module is the presentation layer for the read commands (`status`,
//! `list`, `doctor`, `prune`, `render`). It keeps the text and JSON views of
//! the same data structurally close so they do not drift.

use crate::config::{Config, WhenClause};
use crate::error::StitchError;
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
        details: None,
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
        details: None,
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
pub fn run_json<T: Serialize, F: FnOnce() -> JsonResult<T>>(
    command: &'static str,
    f: F,
) -> Result<(), StitchError> {
    match f() {
        Ok((data, warnings)) => {
            write(command, data, warnings);
            Ok(())
        }
        Err(boxed) => {
            let (error, warnings) = *boxed;
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
