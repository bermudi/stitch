use super::common::print_warnings;
use crate::ancestor::TargetAncestorSnapshot;
use crate::config::{self, Config, Loaded};
use crate::error::StitchError;
use crate::fsutil::{ensure_filesystem_identity, filesystem_identity};
use crate::hooks;
use crate::linker;
use crate::platform::Platform;
use crate::render;
use crate::report::{self, RemoveData};
use crate::safety;
use crate::store;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) fn cmd_remove(
    root: &std::path::Path,
    name: &str,
    dry_run: bool,
    force: bool,
    json: bool,
) -> Result<(), StitchError> {
    // No lock yet: the pre-remove hook runs first and may itself invoke a
    // mutating stitch command (which takes the lock). The lock is acquired
    // after the hook, and the state is reloaded under it.
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    let platform = Platform::detect();

    // Check existence (borrow) before removing, so the config stays intact for
    // status_all and the hook env.
    let target = loaded
        .config
        .stores
        .get(name)
        .ok_or_else(|| {
            let valid: Vec<_> = loaded.config.stores.keys().cloned().collect();
            StitchError::unknown_store(vec![name.to_string()], valid)
        })?
        .target
        .as_deref()
        .map(str::to_owned);

    // InventoryCheck: validate the store's inventory regardless of platform
    // match. A platform-skipped store with a symlinked source root or
    // colliding sources is still invalid and must not be silently removed.
    // "Skipped" changes whether we act, not whether we validate.
    //
    // For active stores, the existing classify logic below already detects
    // these errors and produces the expected error messages and exit codes.
    // The InventoryCheck here covers the gap: platform-skipped stores that
    // the classify logic's status_all filter would skip entirely.
    let store_config = loaded.config.stores.get(name);
    let is_platform_skipped = store_config.is_some_and(|s| !platform.matches_when(&s.when));
    if is_platform_skipped {
        let inventory_errors = safety::validate_inventory(root, &loaded.config);
        if safety::store_has_inventory_error(&inventory_errors, name) {
            let inv_err = inventory_errors
                .iter()
                .find(|e| e.store == name)
                .expect("store_has_inventory_error confirmed presence");
            return Err(StitchError::path_validation(format!(
                "cannot remove store '{name}': inventory error: {inv_err}"
            )));
        }
    }

    // Classify this store's links from the current filesystem state. Shared by
    // dry-run (no lock needed — nothing mutates) and the real removal path
    // (recomputed after the pre-remove hook, under the lock). Returns the
    // linked entries (owned) and their paths.
    let classify =
        |loaded: &Loaded| -> Result<(Vec<store::StatusEntry>, Vec<String>), StitchError> {
            let statuses = store::status_all(root, &loaded.config, &platform);
            let store_statuses: Vec<_> = statuses
                .iter()
                .filter(|e| e.store_name == *name && !e.skipped_platform)
                .collect();
            let mut linked: Vec<store::StatusEntry> = store_statuses
                .iter()
                .copied()
                // A template link can outlive a missing staged render and therefore
                // report Broken. Include it only when the same exact-source predicate
                // used by real removal recognizes it; dry-run must not promise to
                // remove a foreign broken link.
                .filter(|e| linker::points_to_source(&e.target, &e.link_source, root))
                .cloned()
                .collect();
            if let Some(entry) = store_statuses
                .iter()
                .copied()
                .find(|e| matches!(e.status, linker::LinkStatus::StoreError(_)))
            {
                return match std::fs::symlink_metadata(&entry.target) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        Err(StitchError::conflict_foreign(
                            &entry.target,
                            std::fs::read_link(&entry.target).ok(),
                        ))
                    }
                    Ok(_) => Err(StitchError::conflict_real(&entry.target)),
                    Err(_) => Err(StitchError::internal(format!(
                        "store directory '{}' is missing, symlinked, or not a directory",
                        entry.target.display()
                    ))),
                };
            }

            if let Some(entry) = store_statuses
                .iter()
                .copied()
                .find(|e| matches!(e.status, linker::LinkStatus::ConfigError(_)))
                && let linker::LinkStatus::ConfigError(msg) = &entry.status
            {
                return Err(StitchError::path_validation(format!(
                    "store '{}': cannot remove store with configuration error at {}: {}",
                    name,
                    entry.target.display(),
                    msg
                )));
            }

            if let Some(entry) = store_statuses.iter().copied().find(|e| {
                (matches!(e.status, linker::LinkStatus::Broken(_))
                    || matches!(e.status, linker::LinkStatus::Foreign(_)))
                    && std::fs::symlink_metadata(&e.target)
                        .is_ok_and(|meta| meta.file_type().is_symlink())
                    && !linker::points_to_source(&e.target, &e.link_source, root)
            }) {
                return Err(StitchError::conflict_foreign(
                    &entry.target,
                    std::fs::read_link(&entry.target).ok(),
                ));
            }

            // `status_all` filters out platform-skipped stores, and
            // `collect_statuses` suppresses an owned whole-directory root when a
            // store resolves to file mode (pending promotion to per-file links).
            // `remove` must still unlink every owned link for the named store
            // before dropping state, regardless of whether the store is currently
            // active on this platform: the links exist on disk and were created
            // by stitch.
            let mut extra: Vec<store::StatusEntry> = Vec::new();
            let mut seen: BTreeSet<std::path::PathBuf> =
                linked.iter().map(|e| e.target.clone()).collect();
            if let Some(store) = loaded.config.stores.get(name) {
                let store_dir = root.join(name);
                let home = config::expand_home("~").ok();

                let mut add = |target: &std::path::Path,
                               source: std::path::PathBuf,
                               link_source: std::path::PathBuf,
                               source_name: String,
                               link_name: String,
                               is_template: bool,
                               target_name: Option<&str>,
                               allow_dir: bool|
                 -> Result<(), StitchError> {
                    if seen.contains(target) {
                        return Ok(());
                    }
                    match std::fs::symlink_metadata(target) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            if linker::points_to_source(target, &link_source, root) {
                                seen.insert(target.to_path_buf());
                                extra.push(store::StatusEntry {
                                    store_name: name.to_string(),
                                    target_name: target_name.map(str::to_owned),
                                    source,
                                    link_source,
                                    source_name,
                                    link_name,
                                    target: target.to_path_buf(),
                                    status: linker::LinkStatus::Linked,
                                    skipped_platform: false,
                                    is_template,
                                    from_sources: false,
                                });
                                Ok(())
                            } else {
                                Err(StitchError::conflict_foreign(
                                    target,
                                    std::fs::read_link(target).ok(),
                                ))
                            }
                        }
                        Ok(meta) if meta.is_dir() && allow_dir => Ok(()),
                        Ok(_) => Err(StitchError::conflict_real(target)),
                        Err(_) => Ok(()),
                    }
                };

                let mut process_target = |target_path: &std::path::Path,
                                          files: &[String],
                                          patterns: &[String],
                                          sources: &std::collections::BTreeMap<String, String>,
                                          ignore: &[String],
                                          target_name: Option<&str>|
                 -> Result<(), StitchError> {
                    if home.as_ref().is_some_and(|h| h == target_path) {
                        return Ok(());
                    }
                    match store::resolve_target_names(
                        root, &store_dir, files, patterns, sources, ignore,
                    ) {
                        store::LinkTargets::WholeDir => add(
                            target_path,
                            store_dir.clone(),
                            store_dir.clone(),
                            String::new(),
                            String::new(),
                            false,
                            target_name,
                            false,
                        ),
                        store::LinkTargets::Files(links) => {
                            // A former whole-directory root may be awaiting
                            // promotion to per-file links. A real directory at
                            // the root is a valid file-mode parent and not a
                            // conflict.
                            add(
                                target_path,
                                store_dir.clone(),
                                store_dir.clone(),
                                String::new(),
                                String::new(),
                                false,
                                target_name,
                                true,
                            )?;
                            // When the root itself is a symlink it is removed
                            // as a whole; the per-file paths underneath resolve
                            // through the link to the source tree, so they are
                            // not independent targets.
                            let root_is_link = std::fs::symlink_metadata(target_path)
                                .is_ok_and(|m| m.file_type().is_symlink());
                            if !root_is_link {
                                for link in &links {
                                    let link_source = if link.is_template() {
                                        render::staging_path(root, name, &link.name)
                                    } else {
                                        link.source.clone()
                                    };
                                    add(
                                        &target_path.join(&link.name),
                                        link.source.clone(),
                                        link_source,
                                        link.source_rel.clone(),
                                        link.name.clone(),
                                        link.is_template(),
                                        target_name,
                                        false,
                                    )?;
                                }
                            }
                            Ok(())
                        }
                    }
                };

                if store.is_multi_target() {
                    for (target_name, target_entry) in &store.targets {
                        let target_path = config::expand_home(&target_entry.target)
                            .expect("HOME was validated by Config::load");
                        process_target(
                            &target_path,
                            &target_entry.files,
                            &target_entry.patterns,
                            &target_entry.sources,
                            &target_entry.ignore,
                            Some(target_name),
                        )?;
                    }
                } else if let Some(target_str) = &store.target {
                    let target_path = config::expand_home(target_str)
                        .expect("HOME was validated by Config::load");
                    process_target(
                        &target_path,
                        &store.files,
                        &store.patterns,
                        &store.sources,
                        &store.ignore,
                        None,
                    )?;
                }
            }
            linked.extend(extra);

            let linked_paths: Vec<String> = linked
                .iter()
                .map(|e| e.target.to_string_lossy().into_owned())
                .collect();
            Ok((linked, linked_paths))
        };

    let (pre_hook_linked, linked_paths) = classify(&loaded)?;
    let staging = render::store_render_dir(root, name);
    let staging_str = staging.to_string_lossy().into_owned();
    let state_path = root.join(".stitch/state.toml");

    // v0.14 red line: `remove` never destroys a source another store still
    // references. Scan every other store's `sources` for values that resolve
    // inside this store's directory; refuse unless --force, and with --force
    // report the retained files so the dangling references are visible.
    // Store `inbound` messages and `retained` paths separately to avoid
    // corruption when a store name itself contains ": ".
    let (inbound, retained): (Vec<String>, Vec<String>) = {
        let mut inbound = Vec::new();
        let mut retained = Vec::new();
        for (other, store) in &loaded.config.stores {
            if other.as_str() == name {
                continue;
            }
            let values: Box<dyn Iterator<Item = &String>> = if store.is_multi_target() {
                Box::new(store.targets.values().flat_map(|te| te.sources.values()))
            } else {
                Box::new(store.sources.values())
            };
            for value in values {
                if Path::new(value)
                    .components()
                    .next()
                    .is_some_and(|c| c.as_os_str() == std::ffi::OsStr::new(name))
                {
                    inbound.push(format!("{other}: {value}"));
                    retained.push(value.clone());
                }
            }
        }
        (inbound, retained)
    };
    if !inbound.is_empty() && !force {
        return Err(StitchError::conflict_real_msg(format!(
            "refusing to remove store '{name}': other stores reference files inside its directory \
             via `sources`:\n  {}\n\
             remove those entries first, or pass --force to remove this store's links and state \
             while retaining the referenced source files in place",
            inbound.join("\n  ")
        )));
    }

    if dry_run {
        let data = RemoveData {
            store: name.into(),
            target,
            links: linked_paths,
            staging: staging_str,
            dry_run: true,
            behavior_orphaned: None,
            removed_staging: Vec::new(),
            state_entry_removed: None,
            retained_sources: retained.clone(),
        };
        if json {
            report::write("remove", data, loaded.warnings);
        } else {
            println!("Dry run — no changes will be made.");
            println!("Would remove store '{name}':");
            if !data.links.is_empty() {
                for t in &data.links {
                    println!("  remove link {t}");
                }
            } else {
                println!("  no links to remove");
            }
            println!("  remove staging {}", data.staging);
            if !data.retained_sources.is_empty() {
                println!("  retain referenced source files (--force):",);
                for src in &data.retained_sources {
                    println!("    {src}");
                }
            }
        }
        return Ok(());
    }

    // Pin target ancestors across the hook: a hook must not replace a
    // nested target directory with a symlink that would cause removal to follow
    // it outside the configured target.
    let home_path = config::expand_home("~").map_err(StitchError::from)?;
    let pre_hook_targets: Vec<std::path::PathBuf> =
        pre_hook_linked.iter().map(|e| e.target.clone()).collect();
    for target in &pre_hook_targets {
        for ancestor in target.ancestors().skip(1) {
            if ancestor == home_path {
                continue;
            }
            if !ancestor.starts_with(&home_path) {
                break;
            }
            if let Ok(meta) = std::fs::symlink_metadata(ancestor)
                && meta.file_type().is_symlink()
            {
                return Err(StitchError::internal(format!(
                    "target ancestor {} is a symlink; refusing to remove",
                    ancestor.display()
                )));
            }
        }
    }
    let pre_hook_snapshot = TargetAncestorSnapshot::capture(
        root,
        pre_hook_targets.clone(),
        &BTreeSet::new(),
        &home_path,
    )
    .map_err(|e| StitchError::internal(e.to_string()))?;

    // Global pre-remove hook — runs WITHOUT the state lock; a hook that
    // invokes a mutating stitch command acquires the lock itself. Pin both the
    // repository and its state directory: replacing either with another real
    // directory must not redirect cleanup or the later state write. Also pin
    // $HOME identity so a hook that replaces the directory behind a symlinked
    // $HOME cannot redirect removal to an external target.
    {
        let home_identity =
            safety::HomeIdentity::capture().map_err(|e| StitchError::internal(e.to_string()))?;
        let root_identity = filesystem_identity(root, "repository root")?;
        let stitch_dir = root.join(".stitch");
        let stitch_identity = filesystem_identity(&stitch_dir, "state directory")?;
        let env = hooks::HookEnv {
            root,
            store: Some(name),
            target: target.as_deref(),
            action: "remove",
        };
        hooks::run_global_hook(root, "pre-remove", &env, &platform, json)
            .map_err(|e| StitchError::hook("pre-remove", e))?;
        pre_hook_snapshot
            .revalidate()
            .map_err(|e| StitchError::internal(e.to_string()))?;
        home_identity
            .revalidate()
            .map_err(|e| StitchError::internal(e.to_string()))?;
        ensure_filesystem_identity(
            root,
            root_identity,
            "repository changed during pre-remove hook",
            "repository root",
        )?;
        ensure_filesystem_identity(
            &stitch_dir,
            stitch_identity,
            "state directory changed during pre-remove hook",
            "state directory",
        )?;
        config::validate_atomic_write_target(&state_path)?;
    }

    // Serialize with other mutating commands from here to the state save.
    let _state_lock = config::StateLock::exclusive(root).map_err(StitchError::from)?;
    // Reload: the pre-remove hook may have changed state (or even removed the
    // store itself). Removal must act on the state it serializes with.
    let mut loaded = Config::load(root)?;
    // Recompute inbound references from post-hook state: the hook may have
    // added a `sources` entry referencing this store, which must still require
    // --force.
    let (inbound, retained): (Vec<String>, Vec<String>) = {
        let mut inbound = Vec::new();
        let mut retained = Vec::new();
        for (other, store) in &loaded.config.stores {
            if other.as_str() == name {
                continue;
            }
            let values: Box<dyn Iterator<Item = &String>> = if store.is_multi_target() {
                Box::new(store.targets.values().flat_map(|te| te.sources.values()))
            } else {
                Box::new(store.sources.values())
            };
            for value in values {
                if Path::new(value)
                    .components()
                    .next()
                    .is_some_and(|c| c.as_os_str() == std::ffi::OsStr::new(name))
                {
                    inbound.push(format!("{other}: {value}"));
                    retained.push(value.clone());
                }
            }
        }
        (inbound, retained)
    };
    if !inbound.is_empty() && !force {
        return Err(StitchError::conflict_real_msg(format!(
            "refusing to remove store '{name}': other stores reference files inside its directory \
             via `sources`:\n  {}\n\
             remove those entries first, or pass --force to remove this store's links and state \
             while retaining the referenced source files in place",
            inbound.join("\n  ")
        )));
    }
    // Recompute the target from the reloaded state so the JSON report matches
    // what was actually reconciled, not what was captured before the hook.
    let target = loaded
        .config
        .stores
        .get(name)
        .and_then(|s| s.target.as_deref())
        .map(str::to_owned)
        .or(target);
    if !loaded.config.stores.contains_key(name) {
        // The pre-remove hook removed the store from state. The stitch-owned
        // links are still on disk — clean them up using the pre-hook
        // classification rather than leaving them as unmanaged orphans.
        // `remove_link_to` re-checks ownership, so a link repointed by the
        // hook is skipped (foreign), not clobbered.
        let mut removed_links: Vec<String> = Vec::new();
        for entry in &pre_hook_linked {
            match linker::remove_link_to(&entry.target, &entry.link_source, root) {
                Ok(true) => {
                    if !json {
                        println!("  removed {}", entry.target.display());
                    }
                    removed_links.push(entry.target.to_string_lossy().into_owned());
                }
                Ok(false) => {
                    // Link was repointed or is no longer repo-owned — skip it.
                    if !json {
                        eprintln!(
                            "  warning: {} no longer points into repo — skipped",
                            entry.target.display()
                        );
                    }
                }
                Err(e) => {
                    return Err(StitchError::internal(format!(
                        "could not remove link {} after state was removed by hook: {e}",
                        entry.target.display()
                    )));
                }
            }
        }
        // Clean up staging too, if it still exists.
        let removed_staging = render::remove_store_staging(root, name)
            .map_err(StitchError::internal)?
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if json {
            report::write(
                "remove",
                RemoveData {
                    store: name.into(),
                    target,
                    links: removed_links,
                    staging: staging_str,
                    dry_run: false,
                    retained_sources: retained.clone(),
                    behavior_orphaned: None,
                    removed_staging,
                    state_entry_removed: Some(true),
                },
                loaded.warnings,
            );
        } else {
            println!(
                "Store '{name}' was already removed (e.g. by the pre-remove hook); cleaned up {} link(s).",
                removed_links.len()
            );
            if !retained.is_empty() {
                println!("  retained referenced source files (--force):");
                for src in &retained {
                    println!("    {src}");
                }
            }
        }
        return Ok(());
    }
    let (mut linked, _) = classify(&loaded)?;
    // Pin target ancestors for the post-hook inventory as well: the hook may
    // have changed the target to introduce a gateway symlink. Check immediately
    // under lock before any unlink.
    for target in linked.iter().map(|e| &e.target) {
        for ancestor in target.ancestors().skip(1) {
            if ancestor == home_path {
                continue;
            }
            if !ancestor.starts_with(&home_path) {
                break;
            }
            if let Ok(meta) = std::fs::symlink_metadata(ancestor)
                && meta.file_type().is_symlink()
            {
                return Err(StitchError::internal(format!(
                    "target ancestor {} is a symlink; refusing to remove",
                    ancestor.display()
                )));
            }
        }
    }
    // If the pre-remove hook changed the store's inventory (e.g. from `a` to
    // `b`), the new classification no longer knows about the old `a` link. It
    // would be left behind as an orphan while removal reports success. Include
    // any pre-hook repo-owned link whose target is not in the new inventory so
    // the stale link is cleaned up as part of the store removal.
    {
        let new_targets: BTreeSet<PathBuf> = linked.iter().map(|e| e.target.clone()).collect();
        for entry in &pre_hook_linked {
            if new_targets.contains(&entry.target) {
                continue;
            }
            if let Ok(meta) = std::fs::symlink_metadata(&entry.target)
                && meta.file_type().is_symlink()
                && linker::points_to_source(&entry.target, &entry.link_source, root)
            {
                // Also verify the orphan's ancestors are not symlinked (hook could
                // have introduced a gateway for the old target's directory).
                let mut ok = true;
                for ancestor in entry.target.ancestors().skip(1) {
                    if ancestor == home_path {
                        continue;
                    }
                    if !ancestor.starts_with(&home_path) {
                        break;
                    }
                    if let Ok(meta) = std::fs::symlink_metadata(ancestor)
                        && meta.file_type().is_symlink()
                    {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    linked.push(entry.clone());
                }
            } else if let Ok(meta) = std::fs::symlink_metadata(&entry.target)
                && meta.file_type().is_symlink()
                && linker::points_into_repo(&entry.target, root)
            {
                let mut ok = true;
                for ancestor in entry.target.ancestors().skip(1) {
                    if ancestor == home_path {
                        continue;
                    }
                    if !ancestor.starts_with(&home_path) {
                        break;
                    }
                    if let Ok(meta) = std::fs::symlink_metadata(ancestor)
                        && meta.file_type().is_symlink()
                    {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    linked.push(entry.clone());
                }
            }
        }
    }
    // Snapshot final targets (including any orphan-expanded links) to pin
    // ancestors against a concurrent same-UID race between this check and the
    // unlink. A race that wins after revalidation is out of scope.
    let post_hook_snapshot = TargetAncestorSnapshot::capture(
        root,
        linked.iter().map(|e| e.target.clone()).collect::<Vec<_>>(),
        &BTreeSet::new(),
        &home_path,
    )
    .map_err(|e| StitchError::internal(e.to_string()))?;
    post_hook_snapshot
        .revalidate()
        .map_err(|e| StitchError::internal(e.to_string()))?;

    // Remove links before deleting state. If a link that was repo-owned when
    // status_all ran can no longer be removed (e.g. it was repointed to a
    // foreign target), preserve the store's state so the user can retry and
    // do not claim the store was removed.
    //
    // Removal uses the exact-entry `remove_link_to` with the effective link
    // source recorded by status_all, so a source-symlink entry that resolves
    // outside the repo (still stitch-owned) is removed, while a link repointed
    // to a foreign target between status and removal is left untouched.
    let mut removed_links: Vec<String> = Vec::new();
    for entry in &linked {
        if !linker::remove_link_to(&entry.target, &entry.link_source, root)? {
            match std::fs::symlink_metadata(&entry.target) {
                // A symlink that no longer points into the repo is a foreign
                // conflict: do not remove state and do not clobber it.
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(StitchError::conflict_foreign(
                        &entry.target,
                        std::fs::read_link(&entry.target).ok(),
                    ));
                }
                // A real file or dir now occupies the target. Something
                // replaced the symlink between status and removal; abort.
                Ok(_) => {
                    return Err(StitchError::conflict_real(&entry.target));
                }
                // The symlink is already gone (e.g. a pre-remove hook removed
                // it). The goal is achieved, so keep removing other links.
                Err(_) => {
                    if !json {
                        println!("  note: {} is already gone", entry.target.display());
                    }
                    removed_links.push(entry.target.to_string_lossy().into_owned());
                    continue;
                }
            }
        }
        if !json {
            println!("  removed {}", entry.target.display());
        }
        removed_links.push(entry.target.to_string_lossy().into_owned());
    }

    // All links removed safely: now drop the generated state entry.
    // stitch.toml behavior is deliberately left in place (the tool never
    // rewrites authored config); `doctor` flags the orphaned behavior if the
    // user wants to clean it up via `stitch edit`.
    let state_existed = loaded.generated.stores.contains_key(name);
    loaded.generated.stores.remove(name);

    // Staging is tool-owned: drop the store's render tree alongside its links.
    // A staging safety failure leaves generated state intact so the user can
    // retry rather than losing the inventory for still-present output.
    let removed_staging = render::remove_store_staging(root, name)
        .map_err(StitchError::internal)?
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    loaded.generated.save(root)?;

    // The state write is committed; release the lock before the post-remove
    // hook so a hook may invoke a mutating stitch command.
    drop(_state_lock);

    // Global post-remove hook.
    {
        let env = hooks::HookEnv {
            root,
            store: Some(name),
            target: target.as_deref(),
            action: "remove",
        };
        if let Err(e) = hooks::run_global_hook(root, "post-remove", &env, &platform, json) {
            eprintln!("warning: post-remove hook: {e}");
        }
    }

    if json {
        report::write(
            "remove",
            RemoveData {
                store: name.into(),
                target,
                links: removed_links,
                staging: staging_str,
                dry_run: false,
                retained_sources: retained.clone(),
                behavior_orphaned: Some(loaded.authored.stores.contains_key(name)),
                removed_staging,
                state_entry_removed: Some(state_existed),
            },
            loaded.warnings,
        );
    } else {
        println!("Removed store '{}' (directory left untouched)", name);
        if !retained.is_empty() {
            println!("  retained referenced source files (--force):");
            for src in &retained {
                println!("    {src}");
            }
        }
    }
    Ok(())
}
