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
        let store = entry.store.as_deref().unwrap_or("-");
        let target = entry.target.as_deref().unwrap_or("-");
        let class = entry.exit_class.as_deref().unwrap_or("-");
        println!(
            "{} {} store={} target={} outcome={} exit_code={} class={}",
            entry.timestamp, entry.command, store, target, entry.outcome, entry.exit_code, class,
        );
    }
    Ok(())
}
