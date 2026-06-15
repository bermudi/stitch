mod cli;
mod config;
mod linker;
mod platform;
mod store;

use clap::Parser;
use config::{Config, expand_home, find_root};
use platform::Platform;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        cli::Commands::Init => cmd_init(),
        cli::Commands::Apply {
            only,
            dry_run,
            force,
        } => cmd_apply(&only, dry_run, force),
        cli::Commands::Status { name } => cmd_status(&name),
        cli::Commands::Diff { only } => cmd_diff(&only),
        cli::Commands::List => cmd_list(),
        cli::Commands::Adopt {
            path,
            name,
            dry_run,
        } => cmd_adopt(&path, &name, dry_run),
        cli::Commands::Add {
            name,
            target,
            target_flag,
            files,
            patterns,
        } => cmd_add(
            &name,
            target.as_deref().or(target_flag.as_deref()),
            &files,
            &patterns,
        ),
        cli::Commands::Remove { name } => cmd_remove(&name),
        cli::Commands::Edit => cmd_edit(),
        cli::Commands::Doctor => cmd_doctor(),
    }
}

/// Resolve the repo root from cwd, or error.
fn resolve_root() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    find_root(&cwd).ok_or_else(|| "not inside a stitch repo (no .stitch/ found)".into())
}

fn cmd_init() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let stitch_dir = cwd.join(".stitch");
    std::fs::create_dir_all(&stitch_dir)?;

    let config_path = stitch_dir.join("config.toml");
    if config_path.exists() {
        return Err(format!("config already exists at {}", config_path.display()).into());
    }

    let config = Config::empty();
    config.save(&cwd)?;
    println!("Initialized stitch config at {}", config_path.display());
    Ok(())
}

fn cmd_apply(
    only: &[String],
    dry_run: bool,
    _force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let config = Config::load(&root)?;
    let platform = Platform::detect();

    let mut filtered_config = config.clone();
    if !only.is_empty() {
        filtered_config.stores.retain(|name, _| only.contains(name));
    }

    if dry_run {
        println!("Dry run — no changes will be made.\n");
    }

    let results = store::apply_all(&root, &filtered_config, &platform, dry_run);

    let mut created = 0;
    let mut replaced = 0;
    let mut conflicts = 0;
    let mut errors = 0;
    let mut skipped = 0;
    let mut already = 0;

    for result in &results {
        print!("  {} ", result.store_name);
        for action in &result.actions {
            match action {
                store::ApplyAction::Created(p) => {
                    created += 1;
                    println!("→ {}", p.display());
                }
                store::ApplyAction::Replaced(p) => {
                    replaced += 1;
                    println!("↻ {}", p.display());
                }
                store::ApplyAction::Conflict(p) => {
                    conflicts += 1;
                    println!("✗ conflict: {}", p.display());
                }
                store::ApplyAction::SkippedPlatform => {
                    skipped += 1;
                    println!("(skipped: platform)");
                }
                store::ApplyAction::AlreadyLinked => {
                    already += 1;
                    println!("✓");
                }
                store::ApplyAction::Error(e) => {
                    errors += 1;
                    println!("error: {e}");
                }
            }
        }
    }

    println!(
        "\nSummary: {} ok, {} created, {} replaced, {} conflicts, {} errors, {} skipped",
        already, created, replaced, conflicts, errors, skipped
    );

    if errors > 0 || conflicts > 0 {
        Err(format!("{} errors, {} conflicts", errors, conflicts).into())
    } else {
        Ok(())
    }
}

fn cmd_status(name: &Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let config = Config::load(&root)?;
    let platform = Platform::detect();

    let entries = store::status_all(&root, &config, &platform);

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
        };

        let source_name = entry
            .source
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();

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

fn cmd_diff(only: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    cmd_apply(only, true, false)
}

fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let config = Config::load(&root)?;

    let mut sorted: Vec<_> = config.stores.iter().collect();
    sorted.sort_by_key(|(name, _)| name.to_string());

    for (name, store) in &sorted {
        if store.is_multi_target() {
            println!("  {} ({} targets)", name, store.targets.len());
            for target_entry in &store.targets {
                println!("      {}", target_entry.target);
            }
        } else if let Some(ref target) = store.target {
            println!("  {:20} → {}", name, target);
        } else {
            println!("  {:20} (no target)", name);
        }
    }

    Ok(())
}

/// Reverse the move step of adopt: restore the user's file/dir to its
/// original path and clean up the store dir created for file mode.
/// Propagates the io::Error only if restoring the original fails — a
/// leftover empty store dir is non-critical and ignored.
fn rollback_adopt_move(
    source: &std::path::Path,
    store_dir: &std::path::Path,
    raw_name: &str,
    is_dir: bool,
) -> Result<(), std::io::Error> {
    if is_dir {
        // Dir mode: store_dir is the moved directory itself.
        std::fs::rename(store_dir, source)
    } else {
        // File mode: the file lives at store_dir/<raw_name>. Move it back,
        // then remove the (now empty) store dir we created.
        std::fs::rename(store_dir.join(raw_name), source)?;
        let _ = std::fs::remove_dir(store_dir);
        Ok(())
    }
}

fn cmd_adopt(
    path: &str,
    name: &Option<String>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let mut config = Config::load(&root)?;

    let source = expand_home(path);
    if source.is_symlink() {
        return Err(format!(
            "{} is already a symlink — use `stitch import` instead",
            source.display()
        )
        .into());
    }
    if !source.exists() {
        return Err(format!("path does not exist: {}", source.display()).into());
    }

    // Determine store name: strip leading dot for the directory name in the repo.
    let raw_name = source
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let store_name = name
        .clone()
        .unwrap_or_else(|| raw_name.trim_start_matches('.').to_string());

    let store_dir = root.join(&store_name);
    let is_dir = source.is_dir();

    // Determine target path BEFORE moving the file.
    let target_str = if is_dir {
        source.to_string_lossy().into_owned()
    } else {
        source
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "~".into())
    };

    if dry_run {
        println!("Would adopt:");
        println!("  {} → {}/", source.display(), store_dir.display());
        println!(
            "  then symlink back to {}",
            expand_home(&target_str).display()
        );
        return Ok(());
    }

    // --- Pre-checks: reject any collision BEFORE mutating anything. ---
    if config.stores.contains_key(&store_name) {
        return Err(format!("store '{}' already exists in config", store_name).into());
    }
    if store_dir.exists() {
        return Err(format!("destination already exists: {}", store_dir.display()).into());
    }

    // Build the store record in memory. Persisted only after both move and
    // link succeed, so adopt is all-or-nothing.
    let new_store = config::Store {
        target: Some(target_str.clone()),
        files: if is_dir {
            vec![]
        } else {
            vec![raw_name.clone()]
        },
        patterns: vec![],
        ignore: vec![],
        when: config::WhenClause::default(),
        hooks: config::Hooks::default(),
        targets: vec![],
    };

    // --- Move: relocate the file/dir into the repo. ---
    if is_dir {
        std::fs::rename(&source, &store_dir)?;
    } else {
        std::fs::create_dir_all(&store_dir)?;
        std::fs::rename(&source, store_dir.join(&raw_name))?;
    }

    // --- Link: create the return symlink using the in-memory store. ---
    // If this fails, roll back the move so the user's file is back where it
    // was. config was never touched.
    let platform = Platform::detect();
    let results = store::apply_store(&root, &store_name, &new_store, &platform, false);
    if results.actions.iter().any(|a| {
        matches!(
            a,
            store::ApplyAction::Conflict(_) | store::ApplyAction::Error(_)
        )
    }) {
        // Roll back: remove any link that was created, then move back.
        for action in &results.actions {
            if let store::ApplyAction::Created(p) | store::ApplyAction::Replaced(p) = action {
                let _ = linker::remove_link(p, &root);
            }
        }
        rollback_adopt_move(&source, &store_dir, &raw_name, is_dir).map_err(|e| {
            format!(
                "ADOPT FAILED and rollback also failed: {} is stranded in {} ({})",
                source.display(),
                store_dir.display(),
                e
            )
        })?;
        return Err(format!(
            "could not link {} back; rolled back (file restored to {})",
            store_name,
            source.display()
        )
        .into());
    }

    // --- Record: persist the config. ---
    // If save fails, roll back the link and the move to stay all-or-nothing.
    config.stores.insert(store_name.clone(), new_store);
    if let Err(e) = config.save(&root) {
        for action in &results.actions {
            if let store::ApplyAction::Created(p) | store::ApplyAction::Replaced(p) = action {
                let _ = linker::remove_link(p, &root);
            }
        }
        rollback_adopt_move(&source, &store_dir, &raw_name, is_dir).map_err(|re| {
            format!(
                "config save failed ({e}) and rollback also failed: {} is stranded in {} ({re})",
                source.display(),
                store_dir.display(),
            )
        })?;
        return Err(e.into());
    }

    println!("Adopted:");
    for action in &results.actions {
        match action {
            store::ApplyAction::Created(p) => {
                println!("  {} → {}", store_name, p.display())
            }
            store::ApplyAction::AlreadyLinked => {
                println!("  {} → already linked", store_name)
            }
            _ => {}
        }
    }
    Ok(())
}

/// Undo a partial `add`: remove any links `apply_store` managed to create,
/// then remove the (empty) store directory. Unlike `rollback_adopt_move`, add
/// relocates no user data, so there is nothing to rename back — only links we
/// created and an empty dir we made are torn down. Errors are ignored: this is
/// best-effort cleanup on an already-failing path, and a leftover empty dir or
/// stale link is far less harmful than a half-recorded store.
fn discard_uncommitted_add(
    results: Option<&store::ApplyResult>,
    store_dir: &std::path::Path,
    repo_root: &std::path::Path,
) {
    if let Some(results) = results {
        for action in &results.actions {
            if let store::ApplyAction::Created(p) | store::ApplyAction::Replaced(p) = action {
                let _ = linker::remove_link(p, repo_root);
            }
        }
    }
    let _ = std::fs::remove_dir(store_dir);
}

fn cmd_add(
    name: &str,
    target: Option<&str>,
    files: &[String],
    patterns: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let mut config = Config::load(&root)?;

    if config.stores.contains_key(name) {
        return Err(format!("store '{}' already exists", name).into());
    }

    // Validate user-supplied fragments before touching the filesystem: a
    // `--file ../x` would otherwise escape the store/target dirs during the
    // apply below (and leave an orphaned store dir on failure).
    config::validate_fragments(files, patterns, &format!("store '{name}'"))?;

    let store_dir = root.join(name);
    std::fs::create_dir_all(&store_dir)?;

    let new_store = config::Store {
        target: target.map(|t| t.to_string()),
        files: files.to_vec(),
        patterns: patterns.to_vec(),
        ignore: vec![],
        when: config::WhenClause::default(),
        hooks: config::Hooks::default(),
        targets: vec![],
    };

    // Apply first against the in-memory store, BEFORE persisting config — so a
    // failed add leaves no trace. A store with a target must link cleanly or
    // the add aborts; a store with no target has nothing to link and persists
    // directly. add moves no user data, so the unwind is unlinking anything
    // apply created plus removing the empty store dir, not adopt's rename-back.
    let results = target.is_some().then(|| {
        let platform = Platform::detect();
        store::apply_store(&root, name, &new_store, &platform, false)
    });

    if let Some(results) = results.as_ref() {
        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked => println!("  already linked"),
                store::ApplyAction::Conflict(p) => println!("  conflict at {}", p.display()),
                store::ApplyAction::Error(e) => println!("  error: {e}"),
                _ => {}
            }
        }
    }

    let failed = results.as_ref().is_some_and(|r| {
        r.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict(_) | store::ApplyAction::Error(_)
            )
        })
    });

    if failed {
        discard_uncommitted_add(results.as_ref(), &store_dir, &root);
        return Err("apply reported conflicts or errors".into());
    }

    // Persist config. If save fails after apply already created links, undo
    // them and the empty store dir so no half-applied store is left without a
    // config entry — same all-or-nothing contract as adopt.
    config.stores.insert(name.to_string(), new_store);
    if let Err(e) = config.save(&root) {
        discard_uncommitted_add(results.as_ref(), &store_dir, &root);
        return Err(e.into());
    }

    println!("Added store '{}'", name);
    Ok(())
}

fn cmd_remove(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let mut config = Config::load(&root)?;

    // Get status before removing from config.
    let platform = Platform::detect();
    let statuses = store::status_all(&root, &config, &platform);

    // Now remove the config entry.
    config
        .stores
        .remove(name)
        .ok_or_else(|| format!("store '{}' not found in config", name))?;

    // Remove symlinks for this store.
    let linked: Vec<_> = statuses
        .iter()
        .filter(|e| e.store_name == *name && !e.skipped_platform)
        .filter(|e| e.status == linker::LinkStatus::Linked)
        .collect();
    for entry in &linked {
        linker::remove_link(&entry.target, &root)?;
        println!("  removed {}", entry.target.display());
    }

    config.save(&root)?;
    println!("Removed store '{}' (directory left untouched)", name);
    Ok(())
}

fn cmd_edit() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let config_path = root.join(".stitch").join("config.toml");

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(editor)
        .arg(&config_path)
        .status()?;

    if !status.success() {
        return Err("editor exited with error".into());
    }
    Ok(())
}

fn cmd_doctor() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let config = Config::load(&root)?;
    let platform = Platform::detect();

    println!("Checking stitch health...\n");

    let result = store::doctor(&root, &config, &platform);

    for msg in &result.info {
        println!("  [info]  {msg}");
    }
    for msg in &result.warnings {
        println!("  [warn]  {msg}");
    }
    for msg in &result.errors {
        println!("  [error] {msg}");
    }

    let total = result.errors.len() + result.warnings.len() + result.info.len();
    if total == 0 {
        println!("  All checks passed ✓");
    } else {
        println!(
            "\n  {} issues ({} errors, {} warnings, {} info)",
            total,
            result.errors.len(),
            result.warnings.len(),
            result.info.len(),
        );
    }

    if !result.errors.is_empty() {
        Err(format!("{} errors found", result.errors.len()).into())
    } else {
        Ok(())
    }
}
