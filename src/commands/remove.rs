use super::common::print_warnings;
use crate::RemoveData;
use crate::config::{self, Config, Loaded};
use crate::error::StitchError;
use crate::fsutil::{ensure_filesystem_identity, filesystem_identity};
use crate::hooks;
use crate::linker;
use crate::platform::Platform;
use crate::render;
use crate::report;
use crate::safety;
use crate::store;
use std::collections::BTreeSet;

pub(crate) fn cmd_remove(
    root: &std::path::Path,
    name: &str,
    dry_run: bool,
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

    if json && !dry_run {
        return Err(StitchError::usage(
            "--json is not supported for remove without --dry-run",
        ));
    }

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
                                    target: target.to_path_buf(),
                                    status: linker::LinkStatus::Linked,
                                    skipped_platform: false,
                                    is_template,
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
                                          ignore: &[String],
                                          target_name: Option<&str>|
                 -> Result<(), StitchError> {
                    if home.as_ref().is_some_and(|h| h == target_path) {
                        return Ok(());
                    }
                    match store::resolve_target_names(&store_dir, files, patterns, ignore) {
                        store::LinkTargets::WholeDir => add(
                            target_path,
                            store_dir.clone(),
                            store_dir.clone(),
                            false,
                            target_name,
                            false,
                        ),
                        store::LinkTargets::Files(names) => {
                            // A former whole-directory root may be awaiting
                            // promotion to per-file links. A real directory at
                            // the root is a valid file-mode parent and not a
                            // conflict.
                            add(
                                target_path,
                                store_dir.clone(),
                                store_dir.clone(),
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
                                for source_name in &names {
                                    let entry = render::resolve_entry(source_name);
                                    let repo_source = store_dir.join(&entry.source_rel);
                                    let target = target_path.join(&entry.link_rel);
                                    let link_source = if entry.is_template {
                                        render::staging_path(root, name, &entry.link_rel)
                                    } else {
                                        repo_source.clone()
                                    };
                                    add(
                                        &target,
                                        repo_source,
                                        link_source,
                                        entry.is_template,
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

    let (_, linked_paths) = classify(&loaded)?;
    let staging = render::store_render_dir(root, name);
    let staging_str = staging.to_string_lossy().into_owned();
    let state_path = root.join(".stitch/state.toml");

    if dry_run {
        let data = RemoveData {
            store: name.into(),
            target,
            links: linked_paths,
            staging: staging_str,
            dry_run: true,
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
        }
        return Ok(());
    }

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
        hooks::run_global_hook(root, "pre-remove", &env, &platform)
            .map_err(|e| StitchError::hook("pre-remove", e))?;
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
    if !loaded.config.stores.contains_key(name) {
        println!("Store '{name}' was already removed (e.g. by the pre-remove hook).");
        return Ok(());
    }
    let (linked, _) = classify(&loaded)?;

    // Remove links before deleting state. If a link that was repo-owned when
    // status_all ran can no longer be removed (e.g. it was repointed to a
    // foreign target), preserve the store's state so the user can retry and
    // do not claim the store was removed.
    //
    // Removal uses the exact-entry `remove_link_to` with the effective link
    // source recorded by status_all, so a source-symlink entry that resolves
    // outside the repo (still stitch-owned) is removed, while a link repointed
    // to a foreign target between status and removal is left untouched.
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
                    println!("  note: {} is already gone", entry.target.display());
                    continue;
                }
            }
        }
        println!("  removed {}", entry.target.display());
    }

    // All links removed safely: now drop the generated state entry.
    // stitch.toml behavior is deliberately left in place (the tool never
    // rewrites authored config); `doctor` flags the orphaned behavior if the
    // user wants to clean it up via `stitch edit`.
    loaded.generated.stores.remove(name);

    // Staging is tool-owned: drop the store's render tree alongside its links.
    // A staging safety failure leaves generated state intact so the user can
    // retry rather than losing the inventory for still-present output.
    render::remove_store_staging(root, name).map_err(StitchError::internal)?;

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
        if let Err(e) = hooks::run_global_hook(root, "post-remove", &env, &platform) {
            eprintln!("warning: post-remove hook: {e}");
        }
    }

    println!("Removed store '{}' (directory left untouched)", name);
    Ok(())
}
