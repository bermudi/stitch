use super::common::{check_unknown_names, print_warnings};
use crate::config::Config;
use crate::error::StitchError;
use crate::linker;
use crate::platform::Platform;
use crate::report;
use crate::store;

/// Final path component of a link target, lossily.
fn link_name_of(target: &std::path::Path) -> String {
    target
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn cmd_status(
    root: &std::path::Path,
    name: &Option<String>,
    json: bool,
) -> Result<(), StitchError> {
    if json {
        return report::run_json("status", None, || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            let warnings = loaded.warnings;
            if let Some(filter) = name {
                check_unknown_names(std::iter::once(filter.as_str()), &loaded.config)
                    .map_err(|e| Box::new((e, warnings.clone())))?;
            }
            let platform = Platform::detect();
            let entries = store::status_all(root, &loaded.config, &platform);
            let filtered: Vec<_> = if let Some(filter) = name {
                entries
                    .into_iter()
                    .filter(|e| &e.store_name == filter)
                    .collect()
            } else {
                entries
            };
            let data = report::status(root, &filtered);
            Ok((data, warnings))
        });
    }

    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    if let Some(name) = name {
        check_unknown_names(std::iter::once(name.as_str()), &loaded.config)?;
    }
    let platform = Platform::detect();

    let entries = store::status_all(root, &loaded.config, &platform);

    for entry in &entries {
        if let Some(filter) = name
            && &entry.store_name != filter
        {
            continue;
        }

        if entry.skipped_platform {
            println!("  {:20} (skipped: platform)", entry.store_name);
            continue;
        }

        let status_str = match &entry.status {
            linker::LinkStatus::Linked => "✓ linked".to_string(),
            linker::LinkStatus::Missing => "○ missing".to_string(),
            linker::LinkStatus::Conflict(p) => {
                format!("✗ conflict ({})", p.display())
            }
            linker::LinkStatus::Broken(p) => {
                format!("⚠ broken → {}", p.display())
            }
            linker::LinkStatus::Foreign(p) => {
                format!("◆ foreign → {}", p.display())
            }
            linker::LinkStatus::StoreError(p) => {
                format!(
                    "✗ error: store directory '{}' is missing, symlinked, or not a directory",
                    p.display()
                )
            }
            linker::LinkStatus::ConfigError(msg) => {
                format!("✗ error: {msg}")
            }
        };

        let source_name = if entry.from_sources {
            // v0.14 marker: `AGENTS.md ← agents/AGENTS.md` so shared files are
            // visible in the plain-text tree, not only in JSON.
            format!("{} ← {}", link_name_of(&entry.target), entry.source_name)
        } else {
            entry
                .source
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        };

        if source_name.is_empty() {
            println!(
                "  {:20} {:30} {}",
                entry.store_name,
                entry.target.display(),
                status_str
            );
        } else {
            println!(
                "  {:20} {:15} {:30} {}",
                entry.store_name,
                source_name,
                entry.target.display(),
                status_str
            );
        }
    }

    Ok(())
}
