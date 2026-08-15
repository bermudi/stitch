use super::common::print_warnings;
use crate::config::{self, Config};
use crate::error::StitchError;
use crate::linker::LinkStatus;
use crate::platform::Platform;
use crate::report::{self, WhyData, WhyEntry};
use crate::store;

pub(crate) fn cmd_why(root: &std::path::Path, query: &str, json: bool) -> Result<(), StitchError> {
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    let platform = Platform::detect();

    // Expand the query path the same way config targets are expanded, so a
    // user can pass `~/.bashrc` and match a target stored as `~/.bashrc`.
    let query_path = config::expand_home(query)
        .map_err(|e| StitchError::usage(format!("invalid target path: {e}")))?;
    let query_canonical = canonicalize_or_path(&query_path);

    // Run status_all and find the entry whose target matches the query.
    let entries = store::status_all(root, &loaded.config, &platform);

    let mut matched: Option<&store::StatusEntry> = None;
    let mut skipped_platform = false;
    for entry in &entries {
        if entry.skipped_platform {
            // Check if the query is under this store's target (so we can
            // report skipped_platform even when no active entry matches).
            if path_matches(&entry.target, &query_canonical, &query_path) {
                skipped_platform = true;
            }
            continue;
        }
        if path_matches(&entry.target, &query_canonical, &query_path) {
            matched = Some(entry);
            break;
        }
    }

    // If no active entry matched but a skipped store covers the path, report
    // skipped_platform.
    if matched.is_none() && !skipped_platform {
        // Also check skipped stores' target paths directly.
        for (name, store) in &loaded.config.stores {
            if !platform.matches_when(&store.when)
                && let Some(ref target_str) = store.target
                && let Ok(target) = config::expand_home(target_str)
                && path_matches(&target, &query_canonical, &query_path)
            {
                skipped_platform = true;
                break;
            }
            let _ = name;
        }
    }

    let entry = matched.map(|e| build_why_entry(e, root));

    let data = WhyData {
        query: query.to_string(),
        entry,
        skipped_platform,
    };

    if json {
        report::write("why", data, loaded.warnings);
        return Ok(());
    }

    print_why(&data);
    Ok(())
}

fn build_why_entry(entry: &store::StatusEntry, _root: &std::path::Path) -> WhyEntry {
    let (state, resolves_to) = match &entry.status {
        LinkStatus::Linked => ("linked".to_string(), None),
        LinkStatus::Missing => ("missing".to_string(), None),
        LinkStatus::Conflict(_) => ("conflict".to_string(), None),
        LinkStatus::Broken(p) => ("broken".to_string(), Some(p.to_string_lossy().into_owned())),
        LinkStatus::Foreign(p) => (
            "foreign".to_string(),
            Some(p.to_string_lossy().into_owned()),
        ),
        LinkStatus::StoreError(p) => (
            "store-error".to_string(),
            Some(p.to_string_lossy().into_owned()),
        ),
        LinkStatus::ConfigError(msg) => ("config-error".to_string(), Some(msg.clone())),
    };
    WhyEntry {
        store: entry.store_name.clone(),
        target_name: entry.target_name.clone(),
        target: entry.target.to_string_lossy().into_owned(),
        source: entry.source.to_string_lossy().into_owned(),
        templated: entry.is_template,
        state,
        resolves_to,
        owning_config: "state.toml".to_string(),
    }
}

/// Check if a status entry's target path matches the query. Compares both
/// canonical and literal paths to handle symlinked home dirs.
fn path_matches(
    target: &std::path::Path,
    query_canonical: &std::path::Path,
    query_literal: &std::path::Path,
) -> bool {
    if target == query_literal {
        return true;
    }
    let target_canonical = canonicalize_or_path(target);
    target_canonical == *query_canonical
}

/// Canonicalize a path, falling back to the literal path if canonicalization
/// fails (e.g. the path doesn't exist yet).
fn canonicalize_or_path(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn print_why(data: &WhyData) {
    println!("query: {}", data.query);
    if data.skipped_platform {
        println!("skipped: platform (store's `when` does not match this host)");
    }
    match &data.entry {
        Some(e) => {
            println!("store: {}", e.store);
            if let Some(ref name) = e.target_name {
                println!("target_name: {name}");
            }
            println!("target: {}", e.target);
            println!("source: {}", e.source);
            println!("templated: {}", e.templated);
            println!("state: {}", e.state);
            if let Some(ref r) = e.resolves_to {
                println!("resolves_to: {r}");
            }
            println!("owning_config: {}", e.owning_config);
        }
        None => {
            if !data.skipped_platform {
                println!("no store owns this path");
            }
        }
    }
}
