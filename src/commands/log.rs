use crate::audit;
use crate::error::StitchError;
use crate::report;

pub(crate) fn cmd_log(
    root: &std::path::Path,
    limit: Option<usize>,
    json: bool,
) -> Result<(), StitchError> {
    let entries = audit::read(root, limit);

    if json {
        report::write("log", entries, Vec::new());
        return Ok(());
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

fn format_log_entry(entry: &crate::audit::AuditEntry) -> String {
    let store = entry.store.as_deref().unwrap_or("-");
    let target = entry.target.as_deref().unwrap_or("-");
    let class = entry.exit_class.as_deref().unwrap_or("-");
    format!(
        "{} {} store={} target={} outcome={} exit_code={} class={}",
        entry.timestamp, entry.command, store, target, entry.outcome, entry.exit_code, class,
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
        let entries = crate::audit::read(tmp.path(), Some(2));
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
            "unix:1 apply store=- target=- outcome=ok exit_code=0 class=-"
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
            "unix:2 add store=bash target=/home/.bashrc outcome=error exit_code=6 class=conflict-real"
        );
    }
}
