mod cli;
mod config;
mod linker;
mod platform;
mod snapshot;
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
        cli::Commands::Apply { only, dry_run, force } => {
            cmd_apply(&only, dry_run, force)
        }
        cli::Commands::Status { name } => cmd_status(&name),
        cli::Commands::Diff { only } => cmd_diff(&only),
        cli::Commands::List => cmd_list(),
        cli::Commands::Adopt { path, name, dry_run } => {
            cmd_adopt(&path, &name, dry_run)
        }
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
        cli::Commands::Undo => cmd_undo(),
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
        return Err(
            format!("config already exists at {}", config_path.display()).into(),
        );
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
        filtered_config
            .stores
            .retain(|name, _| only.contains(name));
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
        if let Some(filter) = name {
            if &entry.store_name != filter {
                continue;
            }
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

    if dry_run {
        println!("Would adopt:");
        println!("  {} → {}/", source.display(), store_dir.display());
        println!("  then symlink back");
        return Ok(());
    }

    // Snapshot the original before mutating.
    snapshot::ensure_gh()?;
    let snap_files = if is_dir {
        collect_dir_files(&source, "adopt")?
    } else {
        vec![snapshot::SnapshotFile {
            path: source.clone(),
            gist_name: snapshot::gist_filename("adopt", &source.canonicalize()?),
        }]
    };
    let gist_url = snapshot::snapshot(&root, &snap_files)?;
    println!("  snapshot → {}", gist_url);

    // Determine target path BEFORE moving the file.
    let target_str = if is_dir {
        source.to_string_lossy().into_owned()
    } else {
        source
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "~".into())
    };

    // Move the file/dir into the repo. For a file, place it inside a
    // same-named subdirectory so File mode can resolve `store_dir/<name>`.
    let adopted_files: Vec<String> = if is_dir {
        std::fs::rename(&source, &store_dir)?;
        vec![]
    } else {
        std::fs::create_dir_all(&store_dir)?;
        std::fs::rename(&source, &store_dir.join(&raw_name))?;
        vec![raw_name.clone()]
    };

    // Add to config.
    let new_store = config::Store {
        target: Some(target_str),
        files: adopted_files,
        patterns: vec![],
        ignore: vec![],
        when: config::WhenClause::default(),
        hooks: config::Hooks::default(),
        targets: vec![],
    };
    config.stores.insert(store_name.clone(), new_store);
    config.save(&root)?;

    // Create the symlink.
    let platform = Platform::detect();
    let results = store::apply_store(
        &root,
        &store_name,
        config.stores.get(&store_name).unwrap(),
        &platform,
        false,
    );

    println!("Adopted:");
    for action in &results.actions {
        match action {
            store::ApplyAction::Created(p) => {
                println!("  {} → {}", store_name, p.display())
            }
            store::ApplyAction::AlreadyLinked => {
                println!("  {} → already linked", store_name)
            }
            store::ApplyAction::Error(e) => println!("  {} → error: {e}", store_name),
            _ => {}
        }
    }

    Ok(())
}

/// Recursively collect all files under `dir` as snapshot entries.
fn collect_dir_files(
    dir: &std::path::Path,
    tag: &str,
) -> Result<Vec<snapshot::SnapshotFile>, Box<dyn std::error::Error>> {
    let canon = dir.canonicalize()?;
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                let p = entry.path();
                let rel = p.strip_prefix(&canon).unwrap_or(&p);
                out.push(snapshot::SnapshotFile {
                    path: p.clone(),
                    gist_name: snapshot::gist_filename(tag, &p.canonicalize()?),
                });
                let _ = (rel, &canon); // used for gist_name only
            }
        }
    }
    Ok(out)
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

    config.stores.insert(name.to_string(), new_store);
    config.save(&root)?;

    if target.is_some() {
        let platform = Platform::detect();
        let results = store::apply_store(
            &root,
            name,
            config.stores.get(name).unwrap(),
            &platform,
            false,
        );
        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => {
                    println!("  linked {}", p.display())
                }
                store::ApplyAction::AlreadyLinked => println!("  already linked"),
                store::ApplyAction::Conflict(p) => {
                    println!("  conflict at {}", p.display())
                }
                store::ApplyAction::Error(e) => println!("  error: {e}"),
                _ => {}
            }
        }
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

fn cmd_undo() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let url = snapshot::gist_url(&root)
        .ok_or("no snapshot gist found for this repo")?;
    println!("Snapshot history:
  {}", url);
    println!("\nOpen the 'Revisions' tab to browse and restore previous file states.");
    Ok(())
}
