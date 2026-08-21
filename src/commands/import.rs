use super::common::print_warnings;
use super::prune::prune_roots;
use crate::config::{self, Config};
use crate::error::StitchError;
use crate::platform::Platform;
use crate::report::{self, ImportData, ImportedStore, ImportedTarget};
use crate::scan;
use crate::store;

/// Import existing repo-pointing symlinks into `.stitch/state.toml`.
///
/// Groups found links by the store directory they resolve into. A link whose
/// target is exactly a store dir becomes a whole-dir store; links into files
/// under a store become file-mode entries. Skips links already covered by
/// config. Never rewrites `stitch.toml`.
pub(crate) fn cmd_import(
    root: &std::path::Path,
    scan_dirs: &[String],
    dry_run: bool,
    json: bool,
) -> Result<(), StitchError> {
    let _state_lock = if dry_run {
        None
    } else {
        Some(config::StateLock::exclusive(root).map_err(StitchError::from)?)
    };
    let mut loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    let platform = Platform::detect();

    let roots = prune_roots(scan_dirs).map_err(StitchError::from)?;

    let found = scan::scan_for_repo_links(root, &roots);
    // Already-owned links are not re-imported.
    let owned: std::collections::HashSet<_> = store::status_all(root, &loaded.config, &platform)
        .into_iter()
        .filter(|e| !e.skipped_platform)
        .map(|e| e.target)
        .collect();

    // store_name → inventory derived from found links.
    #[derive(Default)]
    struct ImportBucket {
        /// Whole-dir target path string (with ~), if any link points at the store dir.
        whole_dir_target: Option<String>,
        /// In-store file-mode: source relative path → target parent dir string.
        files: std::collections::BTreeMap<String, String>,
        /// v0.14 `sources`: link name → (repo-relative source, target parent
        /// dir). Populated when the link's source lives outside the store that
        /// owns the target directory — the hub fan-in / alias-symlink shape,
        /// imported declaratively instead of fabricating a store around the
        /// source path.
        sources: std::collections::BTreeMap<String, (String, String)>,
        /// When the owner store already exists in state (single-target),
        /// entries merge into it rather than creating a new store.
        merge_target: Option<String>,
    }
    let mut buckets: std::collections::BTreeMap<String, ImportBucket> =
        std::collections::BTreeMap::new();

    let repo_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut skipped_owned = 0;

    // Target-directory ownership: from existing state first, then from scan
    // votes. `sources` entries are attributed to the store that owns the
    // *target directory*, per the one-store-per-directory invariant.
    let dir_owner: std::collections::BTreeMap<std::path::PathBuf, String> =
        loaded
            .generated
            .stores
            .iter()
            .flat_map(|(name, store)| {
                let single = store
                    .target
                    .iter()
                    .map(|t| (t.clone(), name.clone()));
                let multi = store
                    .targets
                    .values()
                    .map(|te| (te.target.clone(), name.clone()));
                single.chain(multi)
            })
            .filter_map(|(target, name)| config::expand_home(&target).ok().map(|p| (p, name)))
            .collect();
    // Votes: every found link (owned or not) votes its source store for its
    // target dir. Used only when state has no owner for the dir.
    let mut votes: std::collections::BTreeMap<
        std::path::PathBuf,
        std::collections::BTreeMap<String, usize>,
    > = std::collections::BTreeMap::new();
    let classify = |fl: &scan::FoundLink,
                        votes: &mut std::collections::BTreeMap<
        std::path::PathBuf,
        std::collections::BTreeMap<String, usize>,
    >|
     -> Option<(String, std::path::PathBuf, String)> {
        let Ok(rel) = fl.resolves_to.strip_prefix(&repo_canon) else {
            return None;
        };
        let mut comps = rel.components();
        let Some(std::path::Component::Normal(store_os)) = comps.next() else {
            return None;
        };
        let source_store = store_os.to_string_lossy().into_owned();
        if source_store == ".stitch" || source_store == ".git" {
            return None;
        }
        let rest: std::path::PathBuf = comps.collect();
        if rest.as_os_str().is_empty() {
            return None; // whole-dir handled separately
        }
        let target_dir = crate::commands::add::target_dir_for_file_link(&fl.link, &rest)?;
        votes
            .entry(target_dir.clone())
            .or_default()
            .entry(source_store.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        Some((source_store, target_dir, rest.to_string_lossy().into_owned()))
    };
    // First pass: tally votes so an un-owned link can see its neighbors.
    let mut classified: Vec<(bool, &scan::FoundLink, String, std::path::PathBuf, String)> =
        Vec::new();
    for fl in &found {
        let is_owned = owned
            .iter()
            .any(|t| crate::commands::add::paths_equal(t, &fl.link));
        match classify(fl, &mut votes) {
            Some((source_store, target_dir, rest)) => {
                classified.push((is_owned, fl, source_store, target_dir, rest))
            }
            None if is_owned => {
                skipped_owned += 1;
            }
            None => {}
        }
    }

    for (is_owned, fl, source_store, target_dir, rest) in classified {
        if is_owned {
            skipped_owned += 1;
            continue;
        }
        let parent = crate::commands::add::collapse_home(&target_dir)?;
        let link_name = fl
            .link
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Resolve the owner of the link's target directory: state first,
        // then the heaviest voting source store (deterministic tie-break).
        let from_state = dir_owner.contains_key(&target_dir);
        let owner = dir_owner.get(&target_dir).cloned().or_else(|| {
            votes
                .get(&target_dir)?
                .iter()
                .max_by_key(|(store, count)| (**count, std::cmp::Reverse((*store).clone())))
                .map(|(store, _)| store.clone())
        });
        let Some(owner_store) = owner else {
            continue;
        };

        let bucket = buckets.entry(owner_store.clone()).or_default();
        if from_state {
            bucket.merge_target = Some(parent.clone());
        }
        // Leaf-name equality is the geometric rename signal: an in-store
        // symmetric link (including nested ones like `lua/plugin.lua`) keeps
        // its leaf; a fan-in/renamed link (`rules.md` → `agents/AGENTS.md`)
        // does not. Depth alone cannot distinguish — rest is relative to the
        // target dir by construction.
        let same_leaf = std::path::Path::new(&rest)
            .file_name()
            .zip(fl.link.file_name())
            .is_some_and(|(a, b)| a == b);
        if owner_store == source_store && same_leaf {
            // Symmetric in-store link: plain `files` entry.
            bucket.files.insert(rest, parent);
        } else {
            // Source outside the owner store, or renamed: a `sources`
            // declaration on the owner — never a fabricated store around
            // the source path.
            bucket
                .sources
                .insert(link_name, (format!("{source_store}/{rest}"), parent));
        }
    }

    // Whole-dir pass (unchanged shape): a link at exactly a store dir.
    for fl in &found {
        if owned
            .iter()
            .any(|t| crate::commands::add::paths_equal(t, &fl.link))
        {
            continue;
        }
        let Ok(rel) = fl.resolves_to.strip_prefix(&repo_canon) else {
            continue;
        };
        let mut comps = rel.components();
        let Some(std::path::Component::Normal(store_os)) = comps.next() else {
            continue;
        };
        let store_name = store_os.to_string_lossy().into_owned();
        if store_name == ".stitch" || store_name == ".git" {
            continue;
        }
        if comps.by_ref().next().is_some() {
            continue; // file link, handled above
        }
        let target_str = crate::commands::add::collapse_home(&fl.link)?;
        buckets
            .entry(store_name)
            .or_default()
            .whole_dir_target = Some(target_str);
    }

    if json && buckets.is_empty() {
        report::write(
            "import",
            ImportData {
                dry_run,
                imported: 0,
                skipped_owned,
                stores: Vec::new(),
            },
            loaded.warnings,
        );
        return Ok(());
    }

    if !json {
        if buckets.is_empty() {
            println!("No importable links found.");
            if skipped_owned > 0 {
                println!("  ({skipped_owned} already managed, skipped)");
            }
            return Ok(());
        }

        if dry_run {
            println!("Dry run — no changes will be made.\n");
        }
    }

    let mut imported = 0;
    let mut stores: Vec<ImportedStore> = Vec::new();
    let mut warnings: Vec<String> = loaded.warnings.clone();
    for (store_name, bucket) in &buckets {
        // Merge path: the owner store already exists in state (derived from
        // its configured target). Extend its inventory in place instead of
        // skipping or fabricating a second store for the same directory.
        if let Some(merge_parent) = bucket.merge_target.clone() {
            if bucket.whole_dir_target.is_some() {
                // A whole-dir link at a path another store's config already
                // owns is a conflict, not an import.
                let msg = format!(
                    "store '{store_name}': whole-dir link at a target already owned by this store; skipping"
                );
                if json {
                    warnings.push(msg);
                } else {
                    eprintln!("warning: {msg}");
                }
                continue;
            }
            let Some(existing) = loaded.generated.stores.get_mut(store_name) else {
                continue;
            };
            let mut added_files: Vec<String> = Vec::new();
            let mut added_sources: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            let mut inventory: Option<(&mut Vec<String>, &mut std::collections::BTreeMap<String, String>)> = None;
            if !existing.targets.is_empty() {
                for (tname, te) in existing.targets.iter_mut() {
                    if config::expand_home(&te.target).ok().as_ref() == Some(&merge_parent_path(&merge_parent)) {
                        let _ = tname;
                        inventory = Some((&mut te.files, &mut te.sources));
                        break;
                    }
                }
            } else if existing
                .target
                .as_ref()
                .and_then(|t| config::expand_home(t).ok())
                == Some(merge_parent_path(&merge_parent))
            {
                inventory = Some((&mut existing.files, &mut existing.sources));
            }
            let Some((files, sources)) = inventory else {
                let msg = format!(
                    "store '{store_name}': no inventory targets {}; skipping merge",
                    merge_parent
                );
                if json {
                    warnings.push(msg);
                } else {
                    eprintln!("warning: {msg}");
                }
                continue;
            };
            for (source_rel, parent) in &bucket.files {
                if *parent != merge_parent {
                    continue;
                }
                if !files.contains(source_rel) {
                    files.push(source_rel.clone());
                    files.sort();
                    added_files.push(source_rel.clone());
                }
            }
            for (link_name, (source, parent)) in &bucket.sources {
                if *parent != merge_parent {
                    continue;
                }
                if files.iter().any(|f| f == link_name) {
                    let msg = format!(
                        "store '{store_name}': imported source '{link_name}' collides with an existing file entry; skipping"
                    );
                    if json {
                        warnings.push(msg);
                    } else {
                        eprintln!("warning: {msg}");
                    }
                    continue;
                }
                if sources.get(link_name).is_none_or(|existing| existing == source) {
                    if !sources.contains_key(link_name) {
                        added_sources.insert(link_name.clone(), source.clone());
                    }
                    sources.insert(link_name.clone(), source.clone());
                } else {
                    let msg = format!(
                        "store '{store_name}': imported source '{link_name}' disagrees with existing declaration; skipping"
                    );
                    if json {
                        warnings.push(msg);
                    } else {
                        eprintln!("warning: {msg}");
                    }
                }
            }
            if added_files.is_empty() && added_sources.is_empty() {
                continue;
            }
            if !json {
                println!(
                    "  merge into '{store_name}' → {merge_parent} ({} file(s), {} source(s))",
                    added_files.len(),
                    added_sources.len()
                );
            }
            stores.push(ImportedStore {
                store: store_name.clone(),
                target: merge_parent,
                mode: "merge".into(),
                files: added_files,
                sources: added_sources,
                targets: Vec::new(),
            });
            imported += 1;
            continue;
        }

        // New-store path: refuse to clobber an existing store entry.
        if loaded.generated.stores.contains_key(store_name) {
            if json {
                warnings.push(format!("store '{store_name}': already in state.toml"));
            } else {
                println!("  skip '{store_name}': already in state.toml");
            }
            continue;
        }

        // Build the report record and the generated state entry together so the
        // two never diverge. `generated_entry` is None when there is nothing to
        // import for this store (e.g. an empty bucket).
        let (imported_store, generated_entry) = if let Some(ref whole) = bucket.whole_dir_target {
            // Whole-dir wins if present; file entries under the same store are
            // noted but not mixed (a store is one mode).
            if !bucket.files.is_empty() || !bucket.sources.is_empty() {
                let msg = format!(
                    "store '{store_name}': found both whole-dir and file links; \
                     importing as whole-dir, file links ignored"
                );
                if json {
                    warnings.push(msg);
                } else {
                    eprintln!("warning: {msg}");
                }
            }
            if !json {
                println!("  import '{store_name}' → {whole} (whole-dir)");
            }
            let store = ImportedStore {
                store: store_name.clone(),
                target: whole.clone(),
                mode: "whole-dir".into(),
                files: Vec::new(),
                sources: std::collections::BTreeMap::new(),
                targets: Vec::new(),
            };
            let entry = config::GeneratedStore {
                target: Some(whole.clone()),
                files: vec![],
                patterns: vec![],
                sources: std::collections::BTreeMap::new(),
                targets: std::collections::BTreeMap::new(),
            };
            (store, Some(entry))
        } else if !bucket.files.is_empty() || !bucket.sources.is_empty() {
            // Group file links by their target parent. One parent → a plain
            // single-target file-mode store; multiple parents → a stow-style
            // fan-in, imported as a multi-target store (one named target per
            // parent, each with its own file set) instead of being dropped.
            let mut groups: std::collections::BTreeMap<
                String,
                (
                    Vec<String>,
                    std::collections::BTreeMap<String, String>,
                ),
            > = std::collections::BTreeMap::new();
            for (source_rel, parent) in &bucket.files {
                groups
                    .entry(parent.clone())
                    .or_default()
                    .0
                    .push(source_rel.clone());
            }
            for (link_name, (source, parent)) in &bucket.sources {
                groups
                    .entry(parent.clone())
                    .or_default()
                    .1
                    .insert(link_name.clone(), source.clone());
            }

            if groups.len() == 1 {
                let (target, (mut files, sources)) = groups.into_iter().next().unwrap();
                files.sort();
                if !json {
                    println!(
                        "  import '{store_name}' → {target} (files: {}{})",
                        files.join(", "),
                        sources_label(&sources)
                    );
                }
                let store = ImportedStore {
                    store: store_name.clone(),
                    target: target.clone(),
                    mode: "file-mode".into(),
                    files: files.clone(),
                    sources: sources.clone(),
                    targets: Vec::new(),
                };
                let entry = config::GeneratedStore {
                    target: Some(target),
                    files,
                    patterns: vec![],
                    sources,
                    targets: std::collections::BTreeMap::new(),
                };
                (store, Some(entry))
            } else {
                // Stow-style fan-in: one store's links span several target
                // dirs. Emit a multi-target store with one named target per
                // parent. Names are positional (`target-{i}`, 1-indexed) with a
                // `-N` collision suffix, matching `migrate`'s fallback so the
                // cross-file join key is deterministic.
                let mut seen: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                let mut report_targets: Vec<ImportedTarget> = Vec::new();
                let mut gen_targets: std::collections::BTreeMap<String, config::GeneratedTarget> =
                    std::collections::BTreeMap::new();
                for (i, (target, (mut files, sources))) in groups.into_iter().enumerate() {
                    files.sort();
                    let base = format!("target-{}", i + 1);
                    let mut name = base.clone();
                    let mut n = 1;
                    while seen.contains(&name) {
                        name = format!("{base}-{n}");
                        n += 1;
                    }
                    seen.insert(name.clone());
                    if !json {
                        println!(
                            "  import '{store_name}' → {target} as {name} (files: {}{})",
                            files.join(", "),
                            sources_label(&sources)
                        );
                    }
                    report_targets.push(ImportedTarget {
                        name: name.clone(),
                        target: target.clone(),
                        files: files.clone(),
                        sources: sources.clone(),
                    });
                    gen_targets.insert(
                        name,
                        config::GeneratedTarget {
                            target,
                            files,
                            patterns: vec![],
                            sources,
                        },
                    );
                }
                if !json {
                    println!(
                        "  import '{store_name}' (multi-target: {} target(s))",
                        report_targets.len()
                    );
                }
                let store = ImportedStore {
                    store: store_name.clone(),
                    target: String::new(),
                    mode: "multi-target".into(),
                    files: Vec::new(),
                    sources: std::collections::BTreeMap::new(),
                    targets: report_targets,
                };
                let entry = config::GeneratedStore {
                    target: None,
                    files: vec![],
                    patterns: vec![],
                    sources: std::collections::BTreeMap::new(),
                    targets: gen_targets,
                };
                (store, Some(entry))
            }
        } else {
            continue;
        };

        if let Some(entry) = generated_entry
            && !dry_run
        {
            loaded.generated.stores.insert(store_name.clone(), entry);
        }
        stores.push(imported_store);
        imported += 1;
    }

    if !dry_run && imported > 0 {
        loaded.generated.save(root)?;
    }

    if json {
        report::write(
            "import",
            ImportData {
                dry_run,
                imported,
                skipped_owned,
                stores,
            },
            warnings,
        );
        return Ok(());
    }

    println!("\nImported {imported} store(s).");
    if skipped_owned > 0 {
        println!("  ({skipped_owned} already managed, skipped)");
    }
    Ok(())
}

/// Expand a `~`-prefixed target dir string for ownership comparison against
/// `config::expand_home` outputs.
fn merge_parent_path(parent: &str) -> std::path::PathBuf {
    config::expand_home(parent).unwrap_or_else(|_| std::path::PathBuf::from(parent))
}

/// Human-readable suffix listing imported `sources` entries.
fn sources_label(sources: &std::collections::BTreeMap<String, String>) -> String {
    if sources.is_empty() {
        return String::new();
    }
    let entries: Vec<String> = sources
        .iter()
        .map(|(name, source)| format!("{name} ← {source}"))
        .collect();
    format!(", sources: {}", entries.join(", "))
}
