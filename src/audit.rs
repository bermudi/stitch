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
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
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
///
/// Safety: the `.stitch` directory and an existing `log.jsonl` are validated
/// with `symlink_metadata` before opening. A symlinked `.stitch` or a
/// symlinked/non-regular log file is refused so a hostile replacement cannot
/// redirect audit writes to an arbitrary user-writable file. The final open
/// uses `O_NOFOLLOW` so a same-UID process cannot swap the log for a symlink
/// between the metadata check and the open — the open itself fails on a
/// symlink rather than following it.
pub fn append(root: &Path, entry: &AuditEntry) {
    let stitch_dir = root.join(".stitch");
    let log_path = stitch_dir.join("log.jsonl");
    let line = match serde_json::to_string(entry) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: could not serialize audit entry: {e}");
            return;
        }
    };
    // Validate the state directory: refuse if it is a symlink or not a dir.
    match std::fs::symlink_metadata(&stitch_dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            eprintln!(
                "warning: refusing unsafe audit log directory {} (symlink or not a directory)",
                stitch_dir.display()
            );
            return;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "warning: could not inspect audit log directory {}: {e}",
                stitch_dir.display()
            );
            return;
        }
    }
    // Refuse to append through a symlinked or non-regular log file.
    if std::fs::symlink_metadata(&log_path)
        .is_ok_and(|meta| meta.file_type().is_symlink() || !meta.is_file())
    {
        eprintln!(
            "warning: refusing unsafe audit log file {} (symlink or not a regular file)",
            log_path.display()
        );
        return;
    }
    // Open with O_NOFOLLOW so the open itself rejects a symlink at the final
    // component, closing the TOCTOU gap between the metadata check above and
    // the open. A same-UID process that swaps the log for a symlink after the
    // check but before the open gets ELOOP, not a redirected write.
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
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

/// Append an audit entry for a command result with store/target context.
/// Used by command paths that know which store/target was involved (e.g.
/// `remove <name>`, `add <path>`), so the audit log is useful for
/// post-hoc investigation without cross-referencing `list`.
pub fn append_with_context(
    root: &Path,
    command: &str,
    store: Option<&str>,
    target: Option<&str>,
    result: Result<(), &StitchError>,
) {
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
        store: store.map(|s| s.to_string()),
        target: target.map(|t| t.to_string()),
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
/// `limit` is None). Returns the entries and any warnings (e.g. malformed
/// JSON lines, read errors). A missing log is not an error — it returns an
/// empty vec with no warnings.
///
/// Lines are streamed from a `BufReader`; when `limit` is set, a bounded
/// `VecDeque` is used so memory usage is proportional to the requested limit,
/// not the full log.
pub fn read(root: &Path, limit: Option<usize>) -> Result<(Vec<AuditEntry>, Vec<String>), String> {
    let log_path = root.join(".stitch").join("log.jsonl");
    let file = match std::fs::File::open(&log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(e) => {
            return Err(format!(
                "could not read audit log {}: {e}",
                log_path.display()
            ));
        }
    };
    let reader = std::io::BufReader::new(file);
    let mut warnings = Vec::new();
    let entries: Vec<AuditEntry> = if let Some(limit) = limit {
        if limit == 0 {
            return Ok((Vec::new(), warnings));
        }
        // Bounded buffer: keep only the last `limit` parsed entries.
        let mut buf: VecDeque<AuditEntry> = VecDeque::with_capacity(limit + 1);
        for (i, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("could not read {}: {e}", log_path.display()))?;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<AuditEntry>(&line) {
                Ok(entry) => {
                    if buf.len() == limit {
                        buf.pop_front();
                    }
                    buf.push_back(entry);
                }
                Err(_) => {
                    warnings.push(format!("malformed JSON at line {} — skipped", i + 1));
                }
            }
        }
        buf.into_iter().collect()
    } else {
        reader
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        warnings.push(format!(
                            "could not read line {} of {}: {e}",
                            i + 1,
                            log_path.display()
                        ));
                        return None;
                    }
                };
                if line.is_empty() {
                    return None;
                }
                match serde_json::from_str::<AuditEntry>(&line) {
                    Ok(entry) => Some(entry),
                    Err(_) => {
                        warnings.push(format!("malformed JSON at line {} — skipped", i + 1));
                        None
                    }
                }
            })
            .collect()
    };
    Ok((entries, warnings))
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
    fn read_skips_malformed_lines_with_warning() {
        let good = r#"{"timestamp":"unix:1","command":"apply","store":"s","target":"/t","outcome":"ok","exit_code":0}"#;
        let lines = ["not json", good];
        let tmp = tempdir().unwrap();
        write_log(tmp.path(), &lines);
        let (entries, warnings) = read(tmp.path(), None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "apply");
        assert_eq!(entries[0].outcome, "ok");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("line 1"));
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
        let (entries, warnings) = read(tmp.path(), Some(5)).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].command, "c");
        assert!(warnings.is_empty());
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
        let (entries, _warnings) = read(tmp.path(), Some(2)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "b");
        assert_eq!(entries[1].command, "c");
    }

    #[test]
    fn read_limit_zero_returns_empty() {
        let lines = [
            r#"{"timestamp":"unix:1","command":"a","outcome":"ok","exit_code":0}"#,
            r#"{"timestamp":"unix:2","command":"b","outcome":"ok","exit_code":0}"#,
        ];
        let tmp = tempdir().unwrap();
        write_log(tmp.path(), &lines);
        let (entries, warnings) = read(tmp.path(), Some(0)).unwrap();
        assert!(entries.is_empty(), "limit=0 must return no entries");
        assert!(warnings.is_empty());
    }

    #[test]
    fn read_missing_log_returns_empty() {
        let tmp = tempdir().unwrap();
        let (entries, warnings) = read(tmp.path(), None).unwrap();
        assert!(entries.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn append_writes_valid_json_line() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        let entry = sample_entry("unix:10", "add", "ok", 0);
        append(tmp.path(), &entry);
        let (read_back, warnings) = read(tmp.path(), None).unwrap();
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].timestamp, "unix:10");
        assert_eq!(read_back[0].command, "add");
        assert_eq!(read_back[0].outcome, "ok");
        assert_eq!(read_back[0].exit_code, 0);
        assert!(warnings.is_empty());

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
        let (entries, _) = read(tmp.path(), None).unwrap();
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
        let (entries, _) = read(tmp.path(), None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "error");
        assert_eq!(entries[0].exit_class.as_deref(), Some("usage"));
        assert_eq!(entries[0].exit_code, 2);

        let err2 = StitchError::conflict_real("/home/.bashrc");
        append_command_result(tmp.path(), "apply", Err(&err2));
        let (entries, _) = read(tmp.path(), Some(1)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "error");
        assert_eq!(entries[0].exit_class.as_deref(), Some("conflict-real"));
        assert_eq!(entries[0].exit_code, 6);
    }

    #[test]
    fn append_with_context_records_store_and_target() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        append_with_context(
            tmp.path(),
            "remove",
            Some("git"),
            Some("/home/.gitconfig"),
            Ok(()),
        );
        let (entries, _) = read(tmp.path(), None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].store.as_deref(), Some("git"));
        assert_eq!(entries[0].target.as_deref(), Some("/home/.gitconfig"));
        assert_eq!(entries[0].outcome, "ok");
    }

    #[test]
    fn append_refuses_symlinked_log_file() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        // Create a symlink at log.jsonl pointing outside .stitch.
        let outside = tmp.path().join("outside.log");
        std::fs::write(&outside, "").unwrap();
        let log_link = tmp.path().join(".stitch").join("log.jsonl");
        std::os::unix::fs::symlink(&outside, &log_link).unwrap();
        let entry = sample_entry("unix:10", "add", "ok", 0);
        append(tmp.path(), &entry); // should refuse, not write through the symlink
        // The symlink target must not have received the audit line.
        let outside_contents = std::fs::read_to_string(&outside).unwrap();
        assert!(
            outside_contents.is_empty(),
            "symlinked log must not be written"
        );
    }

    #[test]
    fn append_refuses_symlinked_stitch_dir() {
        let tmp = tempdir().unwrap();
        // Create a symlink at .stitch pointing to a real dir outside.
        let real_dir = tmp.path().join("real_stitch");
        std::fs::create_dir_all(&real_dir).unwrap();
        let stitch_link = tmp.path().join(".stitch");
        std::os::unix::fs::symlink(&real_dir, &stitch_link).unwrap();
        let entry = sample_entry("unix:10", "add", "ok", 0);
        append(tmp.path(), &entry); // should refuse
        let log = real_dir.join("log.jsonl");
        assert!(
            !log.exists(),
            "symlinked .stitch must not receive audit writes"
        );
    }

    #[test]
    fn now_timestamp_starts_with_unix_prefix() {
        let ts = now_timestamp();
        assert!(ts.starts_with("unix:"));
        let suffix = ts.strip_prefix("unix:").unwrap();
        assert!(suffix.parse::<u64>().is_ok());
    }
}
