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
        /// In-store file-mode: (target parent, source relative path). Keyed by
        /// parent so identical source names under different target directories
        /// do not overwrite each other.
        files: std::collections::BTreeSet<(String, String)>,
        /// v0.14 `sources`: (target parent, link name) → repo-relative source.
        /// Keyed by parent to allow identical alias names under different target
        /// directories without overwriting.
        sources: std::collections::BTreeMap<(String, String), String>,
        /// When the owner store already exists in state, entries merge into it
        /// rather than creating a new store. For multi-target stores there may
        /// be multiple merge parents (one per named target).
        merge_targets: std::collections::BTreeSet<String>,
    }
    let mut buckets: std::collections::BTreeMap<String, ImportBucket> =
        std::collections::BTreeMap::new();

    let repo_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut skipped_owned = 0;

    // Target-directory ownership: from existing state first, then from scan
    // votes. `sources` entries are attributed to the store that owns the
    // *target directory*, per the one-store-per-directory invariant.
    // Only active targets (matching platform) can own a directory for import.
    // We keep per-target entries (not just store name) so a store with
    // mutually-exclusive targets sharing a path does not lose the active
    // identity when the map would dedup identical paths.
    let platform = Platform::detect();
    let dir_owner: Vec<(std::path::PathBuf, String, Option<String>)> = loaded
        .generated
        .stores
        .iter()
        .flat_map(|(name, store)| {
            let config_store = loaded.config.stores.get(name);
            let store_active = config_store
                .map(|s| platform.matches_when(&s.when))
                .unwrap_or(true);
            if !store_active {
                return Vec::new();
            }
            let single: Vec<(std::path::PathBuf, String, Option<String>)> = store
                .target
                .iter()
                .filter_map(|t| config::expand_home(t).ok().map(|p| (p, name.clone(), None)))
                .collect();
            let multi: Vec<(std::path::PathBuf, String, Option<String>)> = store
                .targets
                .iter()
                .filter(|(tname, _)| {
                    config_store
                        .and_then(|s| s.targets.get(*tname))
                        .map(|te| platform.matches_when(&te.when))
                        .unwrap_or(true)
                })
                .filter_map(|(tname, te)| {
                    config::expand_home(&te.target)
                        .ok()
                        .map(|p| (p, name.clone(), Some(tname.clone())))
                })
                .collect();
            [single, multi].concat()
        })
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
            // Root-level file like `repo/hub.txt` is not a whole-dir store;
            // treat it as a file source at the repo root so it can be
            // imported as a `sources` entry (e.g. `alias -> hub.txt`).
            let candidate = repo_canon.join(&source_store);
            if candidate.is_file() {
                let target_dir = fl.link.parent().map(|p| p.to_path_buf())?;
                let target_dir_canon = config::canonical_target_for_comparison(&target_dir);
                votes
                    .entry(target_dir_canon)
                    .or_default()
                    .entry(String::new())
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
                return Some((String::new(), target_dir, source_store.clone()));
            }
            return None; // whole-dir handled separately
        }
        // For fan-in/renamed links the link path does not end with the
        // source-relative path (e.g. `rules.md` → `agents/AGENTS.md`). Fall
        // back to the link's parent directory so the link is not skipped before
        // the rename handling in the bucket phase.
        let target_dir = crate::commands::add::target_dir_for_file_link(&fl.link, &rest)
            .or_else(|| fl.link.parent().map(|p| p.to_path_buf()))?;
        let target_dir_canon = config::canonical_target_for_comparison(&target_dir);
        votes
            .entry(target_dir_canon)
            .or_default()
            .entry(source_store.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        Some((
            source_store,
            target_dir,
            rest.to_string_lossy().into_owned(),
        ))
    };
    // First pass: tally votes so an un-owned link can see its neighbors.
    let mut classified: Vec<(bool, &scan::FoundLink, String, std::path::PathBuf, String)> =
        Vec::new();
    for fl in &found {
        // Compare target path identity, not canonical destination. Two
        // distinct fan-in links (e.g. `~/.config/a/out` and
        // `~/.config/b/out`) may resolve to the same repo file via
        // `canonicalize`, but they are distinct link locations and must
        // not be considered already-owned.
        let is_owned = owned.iter().any(|t| t == &fl.link);
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
        // Resolve the owner of the link's target directory: state first
        // via longest ancestor (handles nested fan-in like `nested/alias.txt`
        // under `~/.consumer`), then the heaviest voting source store.
        // Parent-canonicalized comparison so `~/out` and `/realhome/out`
        // are recognised as the same physical directory when `$HOME` is
        // symlinked.
        let mut best: Option<(String, std::path::PathBuf)> = None;
        let mut best_len = 0usize;
        let fl_canon = config::canonical_target_for_comparison(&fl.link);
        for (dir, owner_name, _tname) in &dir_owner {
            let dir_canon = config::canonical_target_for_comparison(dir);
            if fl_canon == dir_canon || fl_canon.starts_with(&dir_canon) {
                let len = dir.components().count();
                if len > best_len {
                    best_len = len;
                    best = Some((owner_name.clone(), dir.clone()));
                }
            }
        }
        let from_state = best.is_some();
        // Use canonical target_dir for vote lookup so `~/out` and
        // `/realhome/out` share the same vote bucket when `$HOME` is
        // symlinked.
        let target_dir_canon = config::canonical_target_for_comparison(&target_dir);
        let (owner, target_dir_for_owner) = if let Some((owner_name, dir)) = best {
            (Some(owner_name), dir)
        } else {
            let owner = votes.get(&target_dir_canon).and_then(|map| {
                map.iter()
                    .max_by_key(|(store, count)| (**count, std::cmp::Reverse((*store).clone())))
                    .map(|(store, _)| store.clone())
            });
            // If canonical lookup missed (e.g. votes key was lexical), fall
            // back to lexical lookup for backward compat.
            let owner = owner.or_else(|| {
                votes.get(&target_dir).and_then(|map| {
                    map.iter()
                        .max_by_key(|(store, count)| (**count, std::cmp::Reverse((*store).clone())))
                        .map(|(store, _)| store.clone())
                })
            });
            (owner, target_dir.clone())
        };
        let parent = crate::commands::add::collapse_home(&target_dir_for_owner)?;
        // Preserve nested relative path (e.g. "nested/alias") not just leaf name.
        // Use canonical comparison so `~/out/file` under `/realhome/out`
        // yields `file` even when the lexical prefixes differ due to HOME
        // symlink alias.
        let link_name = {
            let target_canon = config::canonical_target_for_comparison(&target_dir_for_owner);
            if fl_canon == target_canon {
                String::new()
            } else if let Ok(rel) = fl_canon.strip_prefix(&target_canon) {
                rel.to_string_lossy().into_owned()
            } else if let Ok(rel) = fl.link.strip_prefix(&target_dir_for_owner) {
                rel.to_string_lossy().into_owned()
            } else {
                fl.link
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }
        };
        let Some(owner_store) = owner else {
            continue;
        };
        if owner_store.is_empty() {
            continue;
        }

        let bucket = buckets.entry(owner_store.clone()).or_default();
        if from_state {
            bucket.merge_targets.insert(parent.clone());
        }
        // Full relative path equality is the rename signal: an in-store
        // symmetric link keeps its exact relative path (including nested
        // like `lua/plugin.lua`); a fan-in/renamed link (`rules.md` →
        // `agents/AGENTS.md` or `foo/x` → `bar/x`) does not. Leaf-only
        // comparison misclassifies `target/bar/x` → `repo/app/foo/x` as
        // a plain file.
        let is_symmetric = link_name == rest;
        if owner_store == source_store && is_symmetric {
            // Symmetric in-store link: plain `files` entry.
            bucket.files.insert((parent.clone(), rest.clone()));
        } else {
            // Source outside the owner store, or renamed: a `sources`
            // declaration on the owner — never a fabricated store around
            // the source path.
            let source_rel = if source_store.is_empty() {
                rest.clone()
            } else {
                format!("{source_store}/{rest}")
            };
            bucket
                .sources
                .insert((parent.clone(), link_name.clone()), source_rel);
        }
    }

    // Whole-dir pass (unchanged shape): a link at exactly a store dir.
    for fl in &found {
        if owned.iter().any(|t| t == &fl.link) {
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
        // Root-level files like `repo/hub.txt` have a single component but are
        // not store directories — a whole-dir store must be a directory at
        // repo/<store>. Reject non-directories so hub.txt becomes a source
        // entry via the file pass, not a fabricated whole-dir store.
        if !root.join(&store_name).is_dir() {
            continue;
        }
        // Also require the resolved target itself to be a directory; a file
        // such as hub.txt must not be promoted to whole-dir.
        if let Ok(meta) = std::fs::symlink_metadata(&fl.resolves_to)
            && !meta.is_dir()
        {
            continue;
        }
        let target_str = crate::commands::add::collapse_home(&fl.link)?;
        buckets.entry(store_name).or_default().whole_dir_target = Some(target_str);
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
        // For multi-target stores there may be multiple merge parents.
        if !bucket.merge_targets.is_empty() {
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
            let mut any_imported = false;
            for merge_parent in &bucket.merge_targets {
                let mut added_files: Vec<String> = Vec::new();
                let mut added_sources: std::collections::BTreeMap<String, String> =
                    std::collections::BTreeMap::new();
                let mut inventory: Option<(
                    &mut Vec<String>,
                    &mut std::collections::BTreeMap<String, String>,
                )> = None;
                if !existing.targets.is_empty() {
                    for (tname, te) in existing.targets.iter_mut() {
                        // Only active targets can be merge destinations.
                        // When two named targets share a path with mutually
                        // exclusive `when` clauses, the first entry in map
                        // order may be inactive; selecting it would write to
                        // the inactive target and lose the active identity.
                        if let Some(cfg) = loaded.config.stores.get(store_name) {
                            if !platform.matches_when(&cfg.when) {
                                continue;
                            }
                            if let Some(at) = cfg.targets.get(tname)
                                && !platform.matches_when(&at.when)
                            {
                                continue;
                            }
                        }
                        if let Ok(expanded) = config::expand_home(&te.target)
                            && config::canonical_target_for_comparison(&expanded)
                                == config::canonical_target_for_comparison(&merge_parent_path(
                                    merge_parent,
                                ))
                        {
                            let _ = tname;
                            inventory = Some((&mut te.files, &mut te.sources));
                            break;
                        }
                    }
                } else if existing
                    .target
                    .as_ref()
                    .and_then(|t| config::expand_home(t).ok())
                    .is_some_and(|p| {
                        config::canonical_target_for_comparison(&p)
                            == config::canonical_target_for_comparison(&merge_parent_path(
                                merge_parent,
                            ))
                    })
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
                for (parent_key, source_rel) in &bucket.files {
                    if parent_key != merge_parent {
                        continue;
                    }
                    if !files.contains(source_rel) {
                        files.push(source_rel.clone());
                        files.sort();
                        added_files.push(source_rel.clone());
                    }
                }
                for ((parent_key, link_name), source) in &bucket.sources {
                    if parent_key != merge_parent {
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
                    if sources
                        .get(link_name)
                        .is_none_or(|existing| existing == source)
                    {
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
                    target: merge_parent.clone(),
                    mode: "merge".into(),
                    files: added_files,
                    sources: added_sources,
                    targets: Vec::new(),
                });
                any_imported = true;
            }
            if any_imported {
                imported += 1;
            }
            // If this store had merge targets, don't fall through to the
            // new-store creation path for the same store — remaining non-merge
            // parents for this store would be new targets, but creating them
            // is handled by the grouping logic below when merge_targets is empty.
            // For now, handled merge parents are done; if there are leftover
            // parents not in merge_targets, create them as new named targets.
            let remaining_files: Vec<_> = bucket
                .files
                .iter()
                .filter(|(p, _)| !bucket.merge_targets.contains(p))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let remaining_sources: Vec<_> = bucket
                .sources
                .iter()
                .filter(|((p, _), _)| !bucket.merge_targets.contains(p))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if remaining_files.is_empty() && remaining_sources.is_empty() {
                continue;
            }
            // Fall through to create new targets for remaining parents under the
            // existing store. Build groups for the remaining parents.
            let mut groups: std::collections::BTreeMap<
                String,
                (Vec<String>, std::collections::BTreeMap<String, String>),
            > = std::collections::BTreeMap::new();
            for (parent, source_rel) in remaining_files {
                groups.entry(parent).or_default().0.push(source_rel);
            }
            for ((parent_key, link_name), source) in remaining_sources {
                groups
                    .entry(parent_key)
                    .or_default()
                    .1
                    .insert(link_name, source);
            }
            // If the existing store is still single-target, promote it to
            // multi-target before adding new named targets — otherwise we
            // would create an invalid mixed store (both `target` and
            // `targets` set).
            if existing.target.is_some() {
                let orig_target = existing.target.take().unwrap();
                let orig_files = std::mem::take(&mut existing.files);
                let orig_patterns = std::mem::take(&mut existing.patterns);
                let orig_sources = std::mem::take(&mut existing.sources);
                // Pick a name that does not collide with incoming groups or
                // existing targets.
                let mut base_idx = 1;
                let mut orig_name = format!("target-{base_idx}");
                while existing.targets.contains_key(&orig_name) || groups.contains_key(&orig_name) {
                    base_idx += 1;
                    orig_name = format!("target-{base_idx}");
                }
                existing.targets.insert(
                    orig_name,
                    config::GeneratedTarget {
                        target: orig_target,
                        files: orig_files,
                        patterns: orig_patterns,
                        sources: orig_sources,
                    },
                );
            }
            // Create new named targets for remaining groups
            for (target, (mut files, sources)) in groups {
                files.sort();
                // Find an unused target name
                let mut base_idx = existing.targets.len() + 1;
                let mut name = format!("target-{base_idx}");
                while existing.targets.contains_key(&name) {
                    base_idx += 1;
                    name = format!("target-{base_idx}");
                }
                if !json {
                    println!(
                        "  import '{store_name}' → {target} as {name} (files: {}{})",
                        files.join(", "),
                        sources_label(&sources)
                    );
                }
                existing.targets.insert(
                    name.clone(),
                    config::GeneratedTarget {
                        target: target.clone(),
                        files: files.clone(),
                        patterns: vec![],
                        sources: sources.clone(),
                    },
                );
                stores.push(ImportedStore {
                    store: store_name.clone(),
                    target: target.clone(),
                    mode: "merge".into(),
                    files,
                    sources,
                    targets: Vec::new(),
                });
                imported += 1;
            }
            continue;
        }

        // New-store path: if the store already exists, extend it with new
        // targets (promoting single-target to multi-target if needed) rather
        // than dropping or creating a mixed store. This handles the case where
        // an existing single-target store's second target is discovered via
        // scan (e.g. `app/a` at `~/.config/appA` already in state, plus
        // `app/b` at `~/.config/appB` found on disk).
        if let Some(existing) = loaded.generated.stores.get_mut(store_name) {
            // Build groups for the bucket's parents.
            let mut groups: std::collections::BTreeMap<
                String,
                (Vec<String>, std::collections::BTreeMap<String, String>),
            > = std::collections::BTreeMap::new();
            for (parent, source_rel) in &bucket.files {
                groups
                    .entry(parent.clone())
                    .or_default()
                    .0
                    .push(source_rel.clone());
            }
            for ((parent_key, link_name), source) in &bucket.sources {
                groups
                    .entry(parent_key.clone())
                    .or_default()
                    .1
                    .insert(link_name.clone(), source.clone());
            }
            if groups.is_empty() {
                continue;
            }
            // If the existing store is still single-target, promote it.
            if existing.target.is_some() {
                let orig_target = existing.target.take().unwrap();
                let orig_files = std::mem::take(&mut existing.files);
                let orig_patterns = std::mem::take(&mut existing.patterns);
                let orig_sources = std::mem::take(&mut existing.sources);
                let mut base_idx = 1;
                let mut orig_name = format!("target-{base_idx}");
                while existing.targets.contains_key(&orig_name) || groups.contains_key(&orig_name) {
                    base_idx += 1;
                    orig_name = format!("target-{base_idx}");
                }
                existing.targets.insert(
                    orig_name,
                    config::GeneratedTarget {
                        target: orig_target,
                        files: orig_files,
                        patterns: orig_patterns,
                        sources: orig_sources,
                    },
                );
            }
            // Filter to only truly new parents (not already in state).
            let existing_parents: std::collections::BTreeSet<String> =
                if existing.targets.is_empty() {
                    std::collections::BTreeSet::new()
                } else {
                    existing
                        .targets
                        .values()
                        .map(|te| te.target.clone())
                        .collect()
                };
            // Also include the original single target's collapsed form if it was just promoted?
            // The promotion above already moved it, so existing_parents now includes it.
            let mut added_any = false;
            for (target, (mut files, sources)) in groups {
                // Skip parents already present in state (should not happen for
                // new-store path, but guard against duplicates).
                if existing_parents.contains(&target)
                    || existing.targets.values().any(|te| te.target == target)
                {
                    continue;
                }
                files.sort();
                let mut base_idx = existing.targets.len() + 1;
                let mut name = format!("target-{base_idx}");
                while existing.targets.contains_key(&name) {
                    base_idx += 1;
                    name = format!("target-{base_idx}");
                }
                if !json {
                    println!(
                        "  import '{store_name}' → {target} as {name} (files: {}{})",
                        files.join(", "),
                        sources_label(&sources)
                    );
                }
                existing.targets.insert(
                    name.clone(),
                    config::GeneratedTarget {
                        target: target.clone(),
                        files: files.clone(),
                        patterns: vec![],
                        sources: sources.clone(),
                    },
                );
                stores.push(ImportedStore {
                    store: store_name.clone(),
                    target: target.clone(),
                    mode: "merge".into(),
                    files,
                    sources,
                    targets: Vec::new(),
                });
                added_any = true;
            }
            if added_any {
                imported += 1;
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
                (Vec<String>, std::collections::BTreeMap<String, String>),
            > = std::collections::BTreeMap::new();
            for (parent, source_rel) in &bucket.files {
                groups
                    .entry(parent.clone())
                    .or_default()
                    .0
                    .push(source_rel.clone());
            }
            for ((parent_key, link_name), source) in &bucket.sources {
                groups
                    .entry(parent_key.clone())
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

        if let Some(entry) = generated_entry {
            loaded.generated.stores.insert(store_name.clone(), entry);
        }
        stores.push(imported_store);
        imported += 1;
    }

    // Full candidate validation so dry-run and real import share the same
    // failure mode. A dry-run that would write invalid state must not report
    // success.
    if imported > 0 {
        config::validate_merged_with_repo(&loaded.authored, &loaded.generated, root)?;
    }

    if !dry_run && imported > 0 {
        loaded.generated.save(root)?;
    } else if dry_run && imported > 0 {
        // Dry-run mutated `loaded.generated` for validation; do not persist.
        // No further action needed — the report below reflects the candidate.
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
