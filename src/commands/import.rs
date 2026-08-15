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

    // store_name → (optional whole-dir target, file entries: (source_rel, target_parent))
    #[derive(Default)]
    struct ImportBucket {
        /// Whole-dir target path string (with ~), if any link points at the store dir.
        whole_dir_target: Option<String>,
        /// File-mode: source relative path → target path string for the parent dir.
        files: std::collections::BTreeMap<String, String>,
    }
    let mut buckets: std::collections::BTreeMap<String, ImportBucket> =
        std::collections::BTreeMap::new();

    let repo_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut skipped_owned = 0;

    for fl in &found {
        if owned
            .iter()
            .any(|t| crate::commands::add::paths_equal(t, &fl.link))
        {
            skipped_owned += 1;
            continue;
        }
        // resolves_to is absolute (canonical when possible). Must live under a
        // top-level store directory.
        let Ok(rel) = fl.resolves_to.strip_prefix(&repo_canon) else {
            continue;
        };
        let mut comps = rel.components();
        let Some(std::path::Component::Normal(store_os)) = comps.next() else {
            continue;
        };
        let store_name = store_os.to_string_lossy().into_owned();
        // Skip tool-owned / VCS dirs.
        if store_name == ".stitch" || store_name == ".git" {
            continue;
        }
        let rest: std::path::PathBuf = comps.collect();
        let target_str = crate::commands::add::collapse_home(&fl.link)?;

        let bucket = buckets.entry(store_name).or_default();
        if rest.as_os_str().is_empty() {
            // Link points at the store directory itself → whole-dir.
            bucket.whole_dir_target = Some(target_str);
        } else {
            let source_rel = rest.to_string_lossy().into_owned();
            // The store target is the directory that the source path is
            // relative to. Strip the entire source-rel portion from where the
            // symlink lives (so nested files like lua/plugin.lua resolve to
            // the common target dir, e.g. ~/.config/nvim, not its immediate
            // parent ~/.config/nvim/lua).
            let Some(target_dir) = crate::commands::add::target_dir_for_file_link(&fl.link, &rest)
            else {
                continue;
            };
            let parent = crate::commands::add::collapse_home(&target_dir)?;
            bucket.files.insert(source_rel, parent);
        }
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
        // Refuse to clobber an existing store entry.
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
            if !bucket.files.is_empty() {
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
                targets: Vec::new(),
            };
            let entry = config::GeneratedStore {
                target: Some(whole.clone()),
                files: vec![],
                patterns: vec![],
                targets: std::collections::BTreeMap::new(),
            };
            (store, Some(entry))
        } else if !bucket.files.is_empty() {
            // Group file links by their target parent. One parent → a plain
            // single-target file-mode store; multiple parents → a stow-style
            // fan-in, imported as a multi-target store (one named target per
            // parent, each with its own file set) instead of being dropped.
            let mut groups: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for (source_rel, parent) in &bucket.files {
                groups
                    .entry(parent.clone())
                    .or_default()
                    .push(source_rel.clone());
            }

            if groups.len() == 1 {
                let (target, mut files) = groups.into_iter().next().unwrap();
                files.sort();
                if !json {
                    println!(
                        "  import '{store_name}' → {target} (files: {})",
                        files.join(", ")
                    );
                }
                let store = ImportedStore {
                    store: store_name.clone(),
                    target,
                    mode: "file-mode".into(),
                    files,
                    targets: Vec::new(),
                };
                let entry = config::GeneratedStore {
                    target: Some(store.target.clone()),
                    files: store.files.clone(),
                    patterns: vec![],
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
                for (i, (target, mut files)) in groups.into_iter().enumerate() {
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
                            "  import '{store_name}' → {target} as {name} (files: {})",
                            files.join(", ")
                        );
                    }
                    report_targets.push(ImportedTarget {
                        name: name.clone(),
                        target: target.clone(),
                        files: files.clone(),
                    });
                    gen_targets.insert(
                        name,
                        config::GeneratedTarget {
                            target,
                            files,
                            patterns: vec![],
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
                    targets: report_targets,
                };
                let entry = config::GeneratedStore {
                    target: None,
                    files: vec![],
                    patterns: vec![],
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
