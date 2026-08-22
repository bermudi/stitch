use crate::audit;
use crate::error::StitchError;
use crate::report;

pub(crate) fn cmd_log(
    root: &std::path::Path,
    limit: Option<usize>,
    json: bool,
) -> Result<(), StitchError> {
    let (entries, warnings) = audit::read(root, limit).map_err(StitchError::internal)?;

    if json {
        report::write("log", entries, warnings);
        return Ok(());
    }

    for w in &warnings {
        eprintln!("warning: {w}");
    }

    if entries.is_empty() {
        println!("(no audit log entries)");
        return Ok(());
    }

    for entry in &entries {
        println!("{}", format_log_entry(entry));
    }
    Ok(())
}

/// Escape a string for safe embedding in `key=value` text output. Uses
/// debug-style quoting so spaces, newlines, and terminal control characters
/// in paths/store names or corrupted JSONL entries cannot make the log
/// ambiguous or misleading.
fn esc(s: &str) -> String {
    format!("{s:?}")
}

fn format_log_entry(entry: &crate::audit::AuditEntry) -> String {
    let store = entry.store.as_deref().unwrap_or("-");
    let target = entry.target.as_deref().unwrap_or("-");
    let class = entry.exit_class.as_deref().unwrap_or("-");
    format!(
        "{} {} store={} target={} outcome={} exit_code={} class={}",
        esc(&entry.timestamp),
        esc(&entry.command),
        esc(store),
        esc(target),
        esc(&entry.outcome),
        entry.exit_code,
        esc(class),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEntry, append};
    use tempfile::tempdir;

    fn root_with_log() -> tempfile::TempDir {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        let entry = AuditEntry {
            timestamp: "unix:1".to_string(),
            command: "apply".to_string(),
            store: Some("bash".to_string()),
            target: Some("/home/.bashrc".to_string()),
            outcome: "ok".to_string(),
            exit_class: None,
            exit_code: 0,
        };
        append(tmp.path(), &entry);
        tmp
    }

    #[test]
    fn cmd_log_missing_log_ok() {
        let tmp = tempdir().unwrap();
        cmd_log(tmp.path(), None, false).unwrap();
        cmd_log(tmp.path(), None, true).unwrap();
    }

    #[test]
    fn cmd_log_with_entries_ok() {
        let tmp = root_with_log();
        cmd_log(tmp.path(), None, false).unwrap();
        cmd_log(tmp.path(), Some(1), true).unwrap();
    }

    #[test]
    fn cmd_log_limit_filters_entries() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".stitch")).unwrap();
        for i in 0..3 {
            let e = AuditEntry {
                timestamp: format!("unix:{i}"),
                command: "apply".to_string(),
                store: None,
                target: None,
                outcome: "ok".to_string(),
                exit_class: None,
                exit_code: 0,
            };
            append(tmp.path(), &e);
        }
        cmd_log(tmp.path(), Some(2), false).unwrap();
        let (entries, warnings) = crate::audit::read(tmp.path(), Some(2)).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].timestamp, "unix:1");
        assert_eq!(entries[1].timestamp, "unix:2");
    }

    #[test]
    fn format_log_entry_uses_defaults_for_missing_fields() {
        let entry = AuditEntry {
            timestamp: "unix:1".to_string(),
            command: "apply".to_string(),
            store: None,
            target: None,
            outcome: "ok".to_string(),
            exit_class: None,
            exit_code: 0,
        };
        let formatted = format_log_entry(&entry);
        assert_eq!(
            formatted,
            r#""unix:1" "apply" store="-" target="-" outcome="ok" exit_code=0 class="-""#
        );
    }

    #[test]
    fn format_log_entry_includes_optional_fields() {
        let entry = AuditEntry {
            timestamp: "unix:2".to_string(),
            command: "add".to_string(),
            store: Some("bash".to_string()),
            target: Some("/home/.bashrc".to_string()),
            outcome: "error".to_string(),
            exit_class: Some("conflict-real".to_string()),
            exit_code: 6,
        };
        let formatted = format_log_entry(&entry);
        assert_eq!(
            formatted,
            r#""unix:2" "add" store="bash" target="/home/.bashrc" outcome="error" exit_code=6 class="conflict-real""#
        );
    }

    #[test]
    fn format_log_entry_escapes_special_chars() {
        let entry = AuditEntry {
            timestamp: "unix:1".to_string(),
            command: "apply".to_string(),
            store: Some("my store".to_string()),
            target: Some("/home/my file".to_string()),
            outcome: "ok".to_string(),
            exit_class: None,
            exit_code: 0,
        };
        let formatted = format_log_entry(&entry);
        assert_eq!(
            formatted,
            r#""unix:1" "apply" store="my store" target="/home/my file" outcome="ok" exit_code=0 class="-""#
        );
    }
}
