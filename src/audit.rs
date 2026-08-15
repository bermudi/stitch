//! Audit log: append-only JSONL record of mutating operations.
//!
//! One JSON line per mutating op (apply, add, remove, migrate, import, prune
//! --yes). Readable via `stitch log`. The log is visible, documented, and
//! unbounded (Q3 decision: rotation is premature for a personal dotfile
//! tool; `stitch log --limit N` reads the tail).
//!
//! Respects the "no hidden state" red line: the log is a plain JSONL file
//! under `.stitch/`, documented in SPEC.md, and `stitch log` is the only
//! reader. It is not a quarantine or retention mechanism.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// One audit-log entry. Serialized as one JSON line.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// ISO 8601 timestamp (UTC).
    pub timestamp: String,
    /// The stitch command that mutated (e.g. "apply", "add", "remove").
    pub command: String,
    /// The store name, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// The target path, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// "ok" or "error".
    pub outcome: String,
    /// Exit class on error (e.g. "conflict-real"); omitted on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_class: Option<String>,
    /// Exit code.
    pub exit_code: i32,
}

/// Append an audit entry to `.stitch/log.jsonl`. Best-effort: a write failure
/// is reported as a warning on stderr, not a hard error, so the log never
/// blocks a mutation.
pub fn append(root: &Path, entry: &AuditEntry) {
    let log_path = root.join(".stitch").join("log.jsonl");
    let line = match serde_json::to_string(entry) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: could not serialize audit entry: {e}");
            return;
        }
    };
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        eprintln!(
            "warning: could not append to audit log {}: {e}",
            log_path.display()
        );
    }
}

/// Read the audit log, returning the last `limit` entries (or all if
/// `limit` is None). Returns an empty vec if the log doesn't exist.
pub fn read(root: &Path, limit: Option<usize>) -> Vec<AuditEntry> {
    let log_path = root.join(".stitch").join("log.jsonl");
    let contents = match std::fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut entries: Vec<AuditEntry> = contents
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if let Some(limit) = limit {
        let start = entries.len().saturating_sub(limit);
        entries = entries.split_off(start);
    }
    entries
}
