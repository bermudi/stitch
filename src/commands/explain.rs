use super::common::print_warnings;
use crate::config::{Config, Store};
use crate::error::StitchError;
use crate::platform::Platform;
use crate::render;
use crate::report::{
    self, ExplainData, ExplainEntry, ExplainPlatform, ExplainStore, ExplainTarget,
};
use crate::store::{self, LinkTargets};

pub(crate) fn cmd_explain(
    root: &std::path::Path,
    active_only: bool,
    json: bool,
) -> Result<(), StitchError> {
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    let platform = Platform::detect();
    let data = build_explain_data(root, &loaded.config, &platform, active_only);

    if json {
        report::write("explain", data, loaded.warnings);
        return Ok(());
    }

    print_explain(&data);
    Ok(())
}

/// Build the `ExplainData` (resolved desired state) for a config. Reused by
/// `apply --json` as the `desired` field of the composite envelope.
pub(crate) fn build_explain_data(
    root: &std::path::Path,
    config: &Config,
    platform: &Platform,
    active_only: bool,
) -> ExplainData {
    let stores: Vec<ExplainStore> = config
        .stores
        .iter()
        .map(|(name, store)| explain_store(root, name, store, platform))
        .filter(|s| !active_only || s.active)
        .collect();

    ExplainData {
        platform: ExplainPlatform {
            os: platform.os.clone(),
            arch: platform.arch.clone(),
            distro: platform.distro.clone(),
            hostname: platform.hostname.clone(),
            shell: platform.shell.clone(),
        },
        stores,
    }
}

fn explain_store(
    root: &std::path::Path,
    name: &str,
    store: &Store,
    platform: &Platform,
) -> ExplainStore {
    let active = platform.matches_when(&store.when);
    let store_dir = root.join(name);

    if store.is_multi_target() {
        let targets = store
            .targets
            .iter()
            .map(|(target_name, target)| {
                let target_active = active && platform.matches_when(&target.when);
                let entries =
                    resolve_entries(&store_dir, &target.files, &target.patterns, &target.ignore);
                ExplainTarget {
                    name: target_name.clone(),
                    active: target_active,
                    when: target.when.clone(),
                    target: target.target.clone(),
                    mode: mode_for(
                        target.files.is_empty() && target.patterns.is_empty(),
                        &entries,
                    )
                    .to_string(),
                    entries,
                    ignore: target.ignore.clone(),
                }
            })
            .collect();
        ExplainStore {
            name: name.to_string(),
            active,
            when: store.when.clone(),
            mode: "multi-target".to_string(),
            target: None,
            entries: Vec::new(),
            targets,
            hooks: store.hooks.clone(),
            ignore: store.ignore.clone(),
        }
    } else {
        let entries = resolve_entries(&store_dir, &store.files, &store.patterns, &store.ignore);
        let has_files = !store.files.is_empty() || !store.patterns.is_empty();
        let mode = match (store.target.is_some(), has_files) {
            (false, false) => "none",
            (true, false) => "whole-dir",
            _ => "file-mode",
        };
        ExplainStore {
            name: name.to_string(),
            active,
            when: store.when.clone(),
            mode: mode.to_string(),
            target: store.target.clone(),
            entries,
            targets: Vec::new(),
            hooks: store.hooks.clone(),
            ignore: store.ignore.clone(),
        }
    }
}

fn resolve_entries(
    store_dir: &std::path::Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
) -> Vec<ExplainEntry> {
    match store::resolve_target_names(store_dir, files, patterns, ignore) {
        LinkTargets::WholeDir => Vec::new(),
        LinkTargets::Files(names) => names
            .into_iter()
            .map(|source_name| {
                let entry = render::resolve_entry(&source_name);
                ExplainEntry {
                    source: entry.source_rel,
                    templated: entry.is_template,
                    link_name: entry.link_rel,
                }
            })
            .collect(),
    }
}

fn mode_for(is_whole: bool, entries: &[ExplainEntry]) -> &'static str {
    if is_whole && entries.is_empty() {
        "whole-dir"
    } else {
        "file-mode"
    }
}

fn print_explain(data: &ExplainData) {
    let p = &data.platform;
    println!(
        "platform: {} {}/{}, hostname={}, shell={}",
        p.os,
        p.arch,
        p.distro.as_deref().unwrap_or(""),
        p.hostname,
        p.shell,
    );
    if data.stores.is_empty() {
        println!("no stores configured");
        return;
    }
    for store in &data.stores {
        let status = if store.active { "active" } else { "skipped" };
        if !store.targets.is_empty() {
            println!(
                "  {} [{}] ({} targets)",
                store.name,
                status,
                store.targets.len()
            );
            for target in &store.targets {
                let tstatus = if target.active { "active" } else { "skipped" };
                println!(
                    "    {} [{}] → {} ({} entries)",
                    target.name,
                    tstatus,
                    target.target,
                    target.entries.len()
                );
            }
        } else if let Some(target) = &store.target {
            println!(
                "  {} [{}] → {} ({} entries)",
                store.name,
                status,
                target,
                store.entries.len()
            );
        } else {
            println!("  {} [{}] (no target)", store.name, status);
        }
    }
}
