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

use crate::error::StitchError;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// One audit-log entry. Serialized as one JSON line.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// unix:SECONDS timestamp (UTC).
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

/// Append an audit entry for a command result. Used by the central runner
/// and by JSON error paths that must exit before returning to the runner.
pub fn append_command_result(root: &Path, command: &str, result: Result<(), &StitchError>) {
    let (outcome, exit_class, exit_code) = match result {
        Ok(()) => ("ok".to_string(), None, 0),
        Err(error) => (
            "error".to_string(),
            Some(error.class().id().to_string()),
            error.exit_code(),
        ),
    };
    let entry = AuditEntry {
        timestamp: now_timestamp(),
        command: command.to_string(),
        store: None,
        target: None,
        outcome,
        exit_class,
        exit_code,
    };
    append(root, &entry);
}

fn now_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // unix:SECONDS timestamp — a stable machine-readable timestamp without
    // a chrono dependency. The prefix makes it clear this is not ISO 8601.
    format!("unix:{secs}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StitchError;
    use std::path::Path;
    use tempfile::tempdir;

    fn sample_entry(timestamp: &str, command: &str, outcome: &str, exit_code: i32) -> AuditEntry {
        AuditEntry {
            timestamp: timestamp.to_string(),
            command: command.to_string(),
            store: Some("store".to_string()),
            target: Some("/home/.bashrc".to_string()),
            outcome: outcome.to_string(),
            exit_class: None,
            exit_code,
        }
    }

    fn write_log(root: &Path, lines: &[&str]) {
        let stitch_dir = root.join(".stitch");
        std::fs::create_dir_all(&stitch_dir).unwrap();
        let log_path = stitch_dir.join("log.jsonl");
        std::fs::write(&log_path, lines.join("\n")).unwrap();
    }

    #[test]
    fn read_skips_malformed_lines() {
        let good = r#"{"timestamp":"unix:1","command":"apply","store":"s","target":"/t","outcome":"ok","exit_code":0}"#;
        let lines = ["not json", good];
        let tmp = tempdir().unwrap();
        write_log(tmp.path(), &lines);
        let entries = read(tmp.path(), None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "apply");
        assert_eq!(entries[0].outcome, "ok");
    }

    #[test]
    fn read_limit_greater_than_entries_returns_all() {
        let lines = [
            r#"{"timestamp":"unix:1","command":"a","outcome":"ok","exit_code":0}"#,
            r#"{"timestamp":"unix:2","command":"b","outcome":"ok","exit_code":0}"#,
            r#"{"timestamp":"unix:3","command":"c","outcome":"ok","exit_code":0}"#,
        ];
        let tmp = tempdir().unwrap();
        write_log(tmp.path(), &lines);
        let entries = read(tmp.path(), Some(5));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].command, "c");
    }

    #[test]
    fn read_limit_less_than_entries_returns_last_n() {
        let lines = [
            r#"{"timestamp":"unix:1","command":"a","outcome":"ok","exit_code":0}"#,
            r#"{"timestamp":"unix:2","command":"b","outcome":"ok","exit_code":0}"#,
            r#"{"timestamp":"unix:3","command":"c","outcome":"ok","exit_code":0}"#,
        ];
        let tmp = tempdir().unwrap();
        write_log(tmp.path(), &lines);
        let entries = read(tmp.path(), Some(2));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "b");
        assert_eq!(entries[1].command, "c");
    }

    #[test]
    fn read_missing_log_returns_empty() {
        let tmp = tempdir().unwrap();
        let entries = read(tmp.path(), None);
        assert!(entries.is_empty());
    }

    #[test]
    fn append_writes_valid_json_line() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        let entry = sample_entry("unix:10", "add", "ok", 0);
        append(tmp.path(), &entry);
        let read_back = read(tmp.path(), None);
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].timestamp, "unix:10");
        assert_eq!(read_back[0].command, "add");
        assert_eq!(read_back[0].outcome, "ok");
        assert_eq!(read_back[0].exit_code, 0);

        let log_path = tmp.path().join(".stitch").join("log.jsonl");
        let line = std::fs::read_to_string(log_path).unwrap();
        let parsed: AuditEntry = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed.command, "add");
    }

    #[test]
    fn append_command_result_ok_writes_ok_entry() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        append_command_result(tmp.path(), "apply", Ok(()));
        let entries = read(tmp.path(), None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "ok");
        assert_eq!(entries[0].exit_code, 0);
        assert!(entries[0].exit_class.is_none());
    }

    #[test]
    fn append_command_result_err_writes_error_entry() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();

        let err = StitchError::usage("bad args");
        append_command_result(tmp.path(), "apply", Err(&err));
        let entries = read(tmp.path(), None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "error");
        assert_eq!(entries[0].exit_class.as_deref(), Some("usage"));
        assert_eq!(entries[0].exit_code, 2);

        let err2 = StitchError::conflict_real("/home/.bashrc");
        append_command_result(tmp.path(), "apply", Err(&err2));
        let entries = read(tmp.path(), Some(1));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "error");
        assert_eq!(entries[0].exit_class.as_deref(), Some("conflict-real"));
        assert_eq!(entries[0].exit_code, 6);
    }

    #[test]
    fn now_timestamp_starts_with_unix_prefix() {
        let ts = now_timestamp();
        assert!(ts.starts_with("unix:"));
        let suffix = ts.strip_prefix("unix:").unwrap();
        assert!(suffix.parse::<u64>().is_ok());
    }
}
