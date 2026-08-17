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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Hooks, Store, TargetEntry, WhenClause};
    use crate::platform::Platform;
    use crate::report::ExplainEntry;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn test_platform() -> Platform {
        Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: Some("testdistro".into()),
            hostname: "testhost".into(),
            shell: "bash".into(),
        }
    }

    #[test]
    fn mode_for_whole_dir() {
        assert_eq!(mode_for(true, &[]), "whole-dir");
    }

    #[test]
    fn mode_for_file_mode_when_not_whole() {
        assert_eq!(mode_for(false, &[]), "file-mode");
    }

    #[test]
    fn mode_for_file_mode_with_entries() {
        let entry = ExplainEntry {
            source: "a".into(),
            templated: false,
            link_name: "a".into(),
        };
        assert_eq!(mode_for(true, &[entry]), "file-mode");
    }

    #[test]
    fn resolve_entries_empty_store_is_whole_dir() {
        let tmp = tempdir().unwrap();
        let store_dir = tmp.path().join("empty");
        std::fs::create_dir_all(&store_dir).unwrap();
        let entries = resolve_entries(&store_dir, &[], &[], &[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_entries_template_strips_tmpl_suffix() {
        let tmp = tempdir().unwrap();
        let store_dir = tmp.path().join("tmpl");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("gitconfig.tmpl"), "host={{ hostname }}\n").unwrap();
        let entries = resolve_entries(&store_dir, &["gitconfig.tmpl".into()], &[], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "gitconfig.tmpl");
        assert_eq!(entries[0].link_name, "gitconfig");
        assert!(entries[0].templated);
    }

    #[test]
    fn resolve_entries_plain_file_uses_basename() {
        let tmp = tempdir().unwrap();
        let store_dir = tmp.path().join("plain");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("foo.conf"), "x").unwrap();
        let entries = resolve_entries(&store_dir, &[], &["*".into()], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "foo.conf");
        assert_eq!(entries[0].link_name, "foo.conf");
        assert!(!entries[0].templated);
    }

    #[test]
    fn explain_store_single_target_none_mode() {
        let tmp = tempdir().unwrap();
        let store = Store {
            target: None,
            files: Vec::new(),
            patterns: Vec::new(),
            ignore: Vec::new(),
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets: BTreeMap::new(),
        };
        let out = explain_store(tmp.path(), "s", &store, &test_platform());
        assert_eq!(out.mode, "none");
        assert!(out.target.is_none());
        assert!(out.entries.is_empty());
        assert!(out.active);
    }

    #[test]
    fn explain_store_single_target_whole_dir() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("s")).unwrap();
        let store = Store {
            target: Some("/home/.config/s".into()),
            files: Vec::new(),
            patterns: Vec::new(),
            ignore: Vec::new(),
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets: BTreeMap::new(),
        };
        let out = explain_store(tmp.path(), "s", &store, &test_platform());
        assert_eq!(out.mode, "whole-dir");
        assert_eq!(out.target.as_deref(), Some("/home/.config/s"));
        assert!(out.entries.is_empty());
    }

    #[test]
    fn explain_store_single_target_file_mode() {
        let tmp = tempdir().unwrap();
        let store_dir = tmp.path().join("s");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("a"), "x").unwrap();
        let store = Store {
            target: Some("/home/.config/s".into()),
            files: vec!["a".into()],
            patterns: Vec::new(),
            ignore: Vec::new(),
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets: BTreeMap::new(),
        };
        let out = explain_store(tmp.path(), "s", &store, &test_platform());
        assert_eq!(out.mode, "file-mode");
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].source, "a");
        assert!(!out.entries[0].templated);
    }

    #[test]
    fn explain_store_multi_target_modes_and_activity() {
        let tmp = tempdir().unwrap();
        let store_dir = tmp.path().join("s");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("a"), "x").unwrap();
        let mut targets = BTreeMap::new();
        targets.insert(
            "home".into(),
            TargetEntry {
                target: "/home".into(),
                files: vec!["a".into()],
                patterns: Vec::new(),
                ignore: Vec::new(),
                when: WhenClause::default(),
            },
        );
        targets.insert(
            "work".into(),
            TargetEntry {
                target: "/work".into(),
                files: Vec::new(),
                patterns: Vec::new(),
                ignore: Vec::new(),
                when: WhenClause {
                    os: Some("macos".into()),
                    ..Default::default()
                },
            },
        );
        let store = Store {
            target: None,
            files: Vec::new(),
            patterns: Vec::new(),
            ignore: Vec::new(),
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets,
        };
        let out = explain_store(tmp.path(), "s", &store, &test_platform());
        assert_eq!(out.mode, "multi-target");
        assert_eq!(out.targets.len(), 2);
        assert_eq!(out.targets[0].name, "home");
        assert_eq!(out.targets[0].mode, "file-mode");
        assert!(out.targets[0].active);
        assert_eq!(out.targets[1].name, "work");
        assert_eq!(out.targets[1].mode, "whole-dir");
        assert!(!out.targets[1].active);
    }

    #[test]
    fn build_explain_data_active_only_filters() {
        let tmp = tempdir().unwrap();
        let mut stores = BTreeMap::new();
        stores.insert(
            "active".into(),
            Store {
                target: None,
                files: Vec::new(),
                patterns: Vec::new(),
                ignore: Vec::new(),
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets: BTreeMap::new(),
            },
        );
        stores.insert(
            "inactive".into(),
            Store {
                target: None,
                files: Vec::new(),
                patterns: Vec::new(),
                ignore: Vec::new(),
                when: WhenClause {
                    os: Some("macos".into()),
                    ..Default::default()
                },
                hooks: Hooks::default(),
                targets: BTreeMap::new(),
            },
        );
        let config = Config {
            vars: BTreeMap::new(),
            stores,
        };
        let data = build_explain_data(tmp.path(), &config, &test_platform(), true);
        assert_eq!(data.stores.len(), 1);
        assert_eq!(data.stores[0].name, "active");
        let data_all = build_explain_data(tmp.path(), &config, &test_platform(), false);
        assert_eq!(data_all.stores.len(), 2);
    }
}
