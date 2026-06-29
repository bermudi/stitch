mod cli;
mod config;
mod hooks;
mod linker;
mod platform;
mod scan;
mod store;

use clap::Parser;
use config::{Config, Loaded, expand_home, find_root};
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
        } => cmd_apply(&only, store::ApplyOpts { dry_run, force }),
        cli::Commands::Status { name } => cmd_status(&name),
        cli::Commands::Diff { only, force } => cmd_diff(&only, force),
        cli::Commands::List => cmd_list(),
        cli::Commands::Add {
            path,
            name,
            files,
            patterns,
            dry_run,
        } => cmd_add(&path, &name, &files, &patterns, dry_run),
        cli::Commands::Remove { name } => cmd_remove(&name),
        cli::Commands::Edit => cmd_edit(),
        cli::Commands::Doctor => cmd_doctor(),
        cli::Commands::Migrate { dry_run } => cmd_migrate(dry_run),
        cli::Commands::Prune {
            scan_dirs,
            dry_run,
            yes,
        } => cmd_prune(&scan_dirs, dry_run, yes),
    }
}

/// Print non-fatal load-time warnings (e.g. a stale v0.2 file alongside the new
/// format) to stderr. Each command calls this once after `Config::load`.
fn print_warnings(loaded: &Loaded) {
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
}

/// Validate that every name in `only` exists in the config. Returns an error
/// listing unknown names so a typo can't silently do nothing.
fn check_unknown_names(
    only: impl IntoIterator<Item = impl AsRef<str>>,
    config: &Config,
) -> Result<(), String> {
    let unknown: Vec<_> = only
        .into_iter()
        .filter(|n| !config.stores.contains_key(n.as_ref()))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        let names = unknown
            .iter()
            .map(|n| format!("'{}'", n.as_ref()))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("unknown store(s): {names}"))
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

    let authored_path = cwd.join("stitch.toml");
    if authored_path.exists() {
        return Err(format!("config already exists at {}", authored_path.display()).into());
    }
    // Refuse if a v0.2 repo is present — the user should `migrate`, not re-init.
    let legacy_path = stitch_dir.join("config.toml");
    if legacy_path.exists() {
        return Err(format!(
            "v0.2 config found at {} — run `stitch migrate` instead of init",
            legacy_path.display()
        )
        .into());
    }

    // Authored half: written exactly once, with a header explaining it is the
    // user's to edit. The tool never rewrites this file after init. Reuses the
    // same fsync+rename atomicity as state writes.
    let authored_content = format!("{}{}", config::AUTHORED_TEMPLATE, "\n[vars]\n");
    config::atomic_write(&authored_path, &authored_content)?;

    // Generated half: empty state. Reserialized by the tool on every mutation.
    config::GeneratedState::default().save(&cwd)?;

    println!("Initialized stitch config:");
    println!("  {}", authored_path.display());
    println!("  {}", stitch_dir.join("state.toml").display());
    Ok(())
}

fn cmd_apply(only: &[String], opts: store::ApplyOpts) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let loaded = Config::load(&root)?;
    print_warnings(&loaded);
    check_unknown_names(only.iter().map(|s| s.as_str()), &loaded.config)?;
    let platform = Platform::detect();

    let mut filtered_config = loaded.config.clone();
    if !only.is_empty() {
        filtered_config.stores.retain(|name, _| only.contains(name));
    }

    if opts.dry_run {
        println!("Dry run — no changes will be made.\n");
    }

    // Global pre-apply hook (skipped under dry-run — hooks have side effects).
    if !opts.dry_run {
        let env = hooks::HookEnv {
            root: &root,
            store: None,
            target: None,
            action: "apply",
        };
        hooks::run_global_hook(&root, "pre-apply", &env, &platform)
            .map_err(|e| format!("pre-apply hook: {e}"))?;
    }

    let results = store::apply_all(&root, &filtered_config, &platform, opts);

    let mut created = 0;
    let mut replaced = 0;
    let mut backed_up = 0;
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
                    println!("create: {}", p.display());
                }
                store::ApplyAction::Replaced(p) => {
                    replaced += 1;
                    println!("replace: {}", p.display());
                }
                store::ApplyAction::BackedUp { target, backup } => {
                    backed_up += 1;
                    println!("backed up: {} → {}", target.display(), backup.display());
                }
                store::ApplyAction::Conflict(p) => {
                    conflicts += 1;
                    println!("conflict: {}", p.display());
                }
                store::ApplyAction::SkippedPlatform => {
                    skipped += 1;
                    println!("(skipped: platform)");
                }
                store::ApplyAction::AlreadyLinked => {
                    already += 1;
                    println!("ok");
                }
                store::ApplyAction::Error(e) => {
                    errors += 1;
                    println!("error: {e}");
                }
            }
        }
    }

    println!(
        "\nSummary: {} ok, {} created, {} replaced, {} backed up, {} conflicts, {} errors, {} skipped",
        already, created, replaced, backed_up, conflicts, errors, skipped
    );

    // Global post-apply hook (skipped under dry-run). Warns on failure — the
    // apply already happened, so post-hook failure does not abort.
    if !opts.dry_run {
        let env = hooks::HookEnv {
            root: &root,
            store: None,
            target: None,
            action: "apply",
        };
        if let Err(e) = hooks::run_global_hook(&root, "post-apply", &env, &platform) {
            eprintln!("warning: post-apply hook: {e}");
        }
    }

    if errors > 0 || conflicts > 0 {
        Err(format!("{} errors, {} conflicts", errors, conflicts).into())
    } else {
        Ok(())
    }
}

fn cmd_status(name: &Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let loaded = Config::load(&root)?;
    print_warnings(&loaded);
    if let Some(name) = name {
        check_unknown_names(std::iter::once(name.as_str()), &loaded.config)?;
    }
    let platform = Platform::detect();

    let entries = store::status_all(&root, &loaded.config, &platform);

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

fn cmd_diff(only: &[String], force: bool) -> Result<(), Box<dyn std::error::Error>> {
    cmd_apply(
        only,
        store::ApplyOpts {
            dry_run: true,
            force,
        },
    )
}

fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let loaded = Config::load(&root)?;
    print_warnings(&loaded);

    for (name, store) in &loaded.config.stores {
        if store.is_multi_target() {
            println!("  {} ({} targets)", name, store.targets.len());
            for (tname, target_entry) in &store.targets {
                println!("      {} → {}", tname, target_entry.target);
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

/// Undo a partial `add` (create-empty path): remove any links `apply_store`
/// managed to create, then remove the (empty) store directory. Unlike
/// `rollback_adopt_move`, this path relocates no user data, so there is
/// nothing to rename back — only links we created and an empty dir we made
/// are torn down. Errors are ignored: best-effort cleanup on an already-
/// failing path.
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
    path: &str,
    name: &Option<String>,
    files: &[String],
    patterns: &[String],
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let mut loaded = Config::load(&root)?;
    print_warnings(&loaded);

    let source = expand_home(path);

    // A symlink at the target is always an error — we never silently clobber
    // or repoint a foreign symlink.
    if source.is_symlink() {
        return Err(format!(
            "{} is already a symlink — add expects a real file or directory \
             (remove the symlink first if you want stitch to manage it)",
            source.display()
        )
        .into());
    }

    // Derive store name from basename, leading dot stripped. Override via --name.
    let raw_name = source
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let store_name = name
        .clone()
        .unwrap_or_else(|| raw_name.trim_start_matches('.').to_string());
    let store_dir = root.join(&store_name);

    // Pre-checks: reject any collision BEFORE mutating anything.
    if loaded.config.stores.contains_key(&store_name) {
        return Err(format!("store '{}' already exists", store_name).into());
    }
    if store_dir.symlink_metadata().is_ok() {
        return Err(format!("store path '{}' already exists", store_dir.display()).into());
    }

    // Validate user-supplied fragments before touching the filesystem: a
    // `--file ../x` would otherwise escape the store/target dirs during apply
    // (and leave an orphaned store dir on failure).
    config::validate_fragments(files, patterns, &format!("store '{store_name}'"))?;

    let source_exists = source.exists();

    // --files/--patterns only apply when creating an empty store (path doesn't
    // exist). On the adopt path the moved content determines the store layout,
    // so passing them is a user error — silently ignoring them would repeat the
    // "stitch says done, did nothing useful" footgun this command was created to
    // fix.
    if source_exists && (!files.is_empty() || !patterns.is_empty()) {
        return Err(format!(
            "{} exists — --files/--patterns only apply when creating a new empty store \
             (the existing content is moved into the repo as-is)",
            source.display()
        )
        .into());
    }

    if dry_run {
        if source_exists {
            let is_dir = source.is_dir();
            let target_str = if is_dir {
                source.to_string_lossy().into_owned()
            } else {
                source
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "~".into())
            };
            println!("Would add (adopt existing):");
            println!("  {} → {}/", source.display(), store_dir.display());
            println!(
                "  then symlink back to {}",
                expand_home(&target_str).display()
            );
        } else {
            println!("Would add (create empty store):");
            println!(
                "  {} → {} (empty store, linked to {})",
                store_name,
                store_dir.display(),
                source.display()
            );
        }
        return Ok(());
    }

    if source_exists {
        // --- Adopt path: move existing content into the repo, link back. ---
        // --files/--patterns are not used here; the moved content determines
        // the store layout (whole-dir for dirs, single-file for files).
        let is_dir = source.is_dir();
        let target_str = if is_dir {
            source.to_string_lossy().into_owned()
        } else {
            source
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "~".into())
        };

        let adopt_files = if is_dir {
            vec![]
        } else {
            vec![raw_name.clone()]
        };

        let new_store = config::Store {
            target: Some(target_str.clone()),
            files: adopt_files.clone(),
            patterns: vec![],
            ignore: vec![],
            when: config::WhenClause::default(),
            hooks: config::Hooks::default(),
            targets: std::collections::BTreeMap::new(),
        };

        // Move: relocate the file/dir into the repo.
        if is_dir {
            std::fs::rename(&source, &store_dir)?;
        } else {
            std::fs::create_dir_all(&store_dir)?;
            std::fs::rename(&source, store_dir.join(&raw_name))?;
        }

        // Link: create the return symlink using the in-memory store.
        // If this fails, roll back the move so the user's file is back where
        // it was. State was never touched.
        let platform = Platform::detect();
        let results = store::apply_store(
            &root,
            &store_name,
            &new_store,
            &platform,
            store::ApplyOpts {
                dry_run: false,
                force: false,
            },
        );
        if results.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict(_) | store::ApplyAction::Error(_)
            )
        }) {
            for action in &results.actions {
                if let store::ApplyAction::Created(p) | store::ApplyAction::Replaced(p) = action {
                    let _ = linker::remove_link(p, &root);
                }
            }
            rollback_adopt_move(&source, &store_dir, &raw_name, is_dir).map_err(|e| {
                format!(
                    "ADD FAILED and rollback also failed: {} is stranded in {} ({})",
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

        // Record: persist state.toml (generated half only). stitch.toml is
        // never rewritten by the tool after init, so comments/formatting survive.
        loaded.generated.stores.insert(
            store_name.clone(),
            config::GeneratedStore {
                target: Some(target_str.clone()),
                files: adopt_files,
                patterns: vec![],
                targets: std::collections::BTreeMap::new(),
            },
        );
        if let Err(e) = loaded.generated.save(&root) {
            for action in &results.actions {
                if let store::ApplyAction::Created(p) | store::ApplyAction::Replaced(p) = action {
                    let _ = linker::remove_link(p, &root);
                }
            }
            rollback_adopt_move(&source, &store_dir, &raw_name, is_dir).map_err(|re| {
                format!(
                    "state save failed ({e}) and rollback also failed: {} is stranded in {} ({re})",
                    source.display(),
                    store_dir.display(),
                )
            })?;
            return Err(e.into());
        }

        println!(
            "Added store '{}' (adopted from {})",
            store_name,
            source.display()
        );
        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked => println!("  already linked"),
                _ => {}
            }
        }
    } else {
        // --- Create-empty path: fresh store, link to target. ---
        let target_str = path.to_string();

        let new_store = config::Store {
            target: Some(target_str.clone()),
            files: files.to_vec(),
            patterns: patterns.to_vec(),
            ignore: vec![],
            when: config::WhenClause::default(),
            hooks: config::Hooks::default(),
            targets: std::collections::BTreeMap::new(),
        };

        std::fs::create_dir_all(&store_dir)?;

        let platform = Platform::detect();
        let results = store::apply_store(
            &root,
            &store_name,
            &new_store,
            &platform,
            store::ApplyOpts {
                dry_run: false,
                force: false,
            },
        );

        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked => println!("  already linked"),
                store::ApplyAction::Conflict(p) => println!("  conflict at {}", p.display()),
                store::ApplyAction::Error(e) => println!("  error: {e}"),
                _ => {}
            }
        }

        let failed = results.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict(_) | store::ApplyAction::Error(_)
            )
        });

        if failed {
            discard_uncommitted_add(Some(&results), &store_dir, &root);
            return Err("apply reported conflicts or errors".into());
        }

        // Persist state.toml (generated half only). If save fails after apply
        // already created links, undo them and the empty store dir so no
        // half-applied store is left without a state entry.
        loaded.generated.stores.insert(
            store_name.clone(),
            config::GeneratedStore {
                target: Some(target_str.clone()),
                files: files.to_vec(),
                patterns: patterns.to_vec(),
                targets: std::collections::BTreeMap::new(),
            },
        );
        if let Err(e) = loaded.generated.save(&root) {
            discard_uncommitted_add(Some(&results), &store_dir, &root);
            return Err(e.into());
        }

        println!("Added store '{}'", store_name);
    }

    Ok(())
}

fn cmd_remove(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let mut loaded = Config::load(&root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    // Check existence (borrow) before removing, so the config stays intact for
    // status_all and the hook env.
    let target = loaded
        .config
        .stores
        .get(name)
        .ok_or_else(|| format!("store '{}' not found in config", name))?
        .target
        .as_deref()
        .map(str::to_owned);

    // Global pre-remove hook.
    {
        let env = hooks::HookEnv {
            root: &root,
            store: Some(name),
            target: target.as_deref(),
            action: "remove",
        };
        hooks::run_global_hook(&root, "pre-remove", &env, &platform)
            .map_err(|e| format!("pre-remove hook: {e}"))?;
    }

    // Compute link statuses from the still-complete merged view, then drop the
    // entry from the generated half. stitch.toml behavior is deliberately left
    // in place (the tool never rewrites authored config); `doctor` flags the
    // orphaned behavior if the user wants to clean it up via `stitch edit`.
    let statuses = store::status_all(&root, &loaded.config, &platform);
    loaded.generated.stores.remove(name);

    let linked: Vec<_> = statuses
        .iter()
        .filter(|e| e.store_name == *name && !e.skipped_platform)
        .filter(|e| e.status == linker::LinkStatus::Linked)
        .collect();
    for entry in &linked {
        linker::remove_link(&entry.target, &root)?;
        println!("  removed {}", entry.target.display());
    }

    loaded.generated.save(&root)?;

    // Global post-remove hook.
    {
        let env = hooks::HookEnv {
            root: &root,
            store: Some(name),
            target: target.as_deref(),
            action: "remove",
        };
        if let Err(e) = hooks::run_global_hook(&root, "post-remove", &env, &platform) {
            eprintln!("warning: post-remove hook: {e}");
        }
    }

    println!("Removed store '{}' (directory left untouched)", name);
    Ok(())
}

fn cmd_edit() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let authored_path = root.join("stitch.toml");
    if !authored_path.exists() {
        return Err(format!(
            "{} does not exist — run `stitch init` first",
            authored_path.display()
        )
        .into());
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(editor)
        .arg(&authored_path)
        .status()?;

    if !status.success() {
        return Err("editor exited with error".into());
    }
    Ok(())
}

fn cmd_doctor() -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let loaded = Config::load(&root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    println!("Checking stitch health...\n");

    let result = store::doctor(&root, &loaded, &platform);

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

fn cmd_migrate(dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let legacy_path = root.join(".stitch").join("config.toml");
    let authored_path = root.join("stitch.toml");
    let state_path = root.join(".stitch").join("state.toml");

    if !legacy_path.exists() {
        if authored_path.exists() {
            return Err("already migrated: stitch.toml exists".into());
        }
        return Err(format!("nothing to migrate: {} not found", legacy_path.display()).into());
    }
    // Refuse to overwrite an existing stitch.toml — a half-finished migrate
    // should not clobber the user's authored file.
    if authored_path.exists() {
        return Err(format!(
            "{} already exists — refusing to overwrite; remove it if you want to re-migrate",
            authored_path.display()
        )
        .into());
    }
    // Refuse if the .bak backup target already exists — we'd have nowhere to
    // preserve the original. Checked up front (before parse, before any write)
    // so a .bak collision fails before touching anything, matching the
    // fail-before-mutate invariant the other writers uphold.
    let backup_path = legacy_path.with_extension("toml.bak");
    if backup_path.exists() {
        return Err(format!(
            "{} already exists — move it aside first (it's where the original \
             .stitch/config.toml would be backed up during migration)",
            backup_path.display()
        )
        .into());
    }

    // Parse the v0.2 file into the frozen LegacyConfig shape (not the
    // post-split types, which no longer carry the v0.2 layout).
    let contents = std::fs::read_to_string(&legacy_path)?;
    let legacy: config::LegacyConfig = toml::from_str(&contents)
        .map_err(|e| format!("could not parse {}: {e}", legacy_path.display()))?;

    let (authored, generated) = config::split_legacy(&legacy);

    // Render both halves once: authored (with the read-only header prepended)
    // and generated (sorted + header-stamped). The state string is reused for
    // both the dry-run preview and the real write — no double-serialize, and a
    // serialization error aborts before any file is touched.
    let authored_str = format!(
        "{}{}",
        config::AUTHORED_TEMPLATE,
        toml::to_string_pretty(&authored)?
    );
    let state_str = generated.render_for_display()?;

    if dry_run {
        println!("Dry run — no changes will be made.\n");
        println!(
            "note: comments in {} are not carried into stitch.toml; the \
             original is preserved as {}.bak on write",
            legacy_path.display(),
            legacy_path.display()
        );
        println!(
            "\n--- would write {} ---\n{}",
            authored_path.display(),
            authored_str
        );
        println!(
            "--- would write {} ---\n{}",
            state_path.display(),
            state_str
        );
        return Ok(());
    }

    // Write both new files first; only after both succeed do we move the legacy
    // file aside. A crash during writes leaves the original intact. The .bak
    // target was pre-checked above, so this rename can't clobber.
    config::atomic_write(&authored_path, &authored_str)?;
    config::atomic_write(&state_path, &state_str)?;

    // Preserve the original as a .bak rather than delete — the user's comments
    // and formatting are the recovery path (migrate is comment-lossy by design).
    std::fs::rename(&legacy_path, &backup_path)?;

    println!("Migrated v0.2 config:");
    println!("  wrote {}", authored_path.display());
    println!("  wrote {}", state_path.display());
    println!(
        "  backed up {} → {}",
        legacy_path.display(),
        backup_path.display()
    );
    eprintln!(
        "note: comments in the old config were not carried into stitch.toml \
         (structural conversion drops them). The original is preserved at {}. \
         Re-add any comments you want to keep.",
        backup_path.display()
    );
    Ok(())
}

fn cmd_prune(
    scan_dirs: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = resolve_root()?;
    let loaded = Config::load(&root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    // Scan dirs are a parameter (never hardwired to home_dir) so the scanner is
    // testable without overriding $HOME, and so a user can scope a prune. No
    // args → the defaults: `~` shallow (top-level dotfiles only), `~/.config`
    // and `~/.local/share` full depth. An explicit `--scan-dir` is always full
    // depth, so `--scan-dir ~` is the escape hatch for a complete home sweep.
    let roots: Vec<scan::ScanRoot> = if scan_dirs.is_empty() {
        scan::default_scan_dirs()
    } else {
        scan_dirs
            .iter()
            .map(|s| scan::ScanRoot::from(expand_home(s)))
            .collect()
    };

    let found = scan::scan_for_repo_links(&root, &roots);
    let orphans = scan::orphan_links(&root, &found, &loaded.config, &platform);

    if orphans.is_empty() {
        println!("No orphaned links found.");
        return Ok(());
    }

    println!("Found {} orphaned link(s):", orphans.len());
    for fl in &orphans {
        println!("  {} → {}", fl.link.display(), fl.resolves_to.display());
    }

    // Removal requires an explicit opt-in: the default lists only. --dry-run is
    // an explicit alias for the same safe default, so `--yes --dry-run` still
    // removes nothing (explicit over implicit). Removal routes through
    // remove_link, which re-checks points_into_repo — a foreign symlink is
    // never clobbered even if classification raced between scan and unlink.
    if !yes || dry_run {
        println!("\n  (to remove these, run: stitch prune --yes)");
        return Ok(());
    }

    let mut removed = 0;
    let mut failed = 0;
    for fl in &orphans {
        match linker::remove_link(&fl.link, &root) {
            Ok(true) => {
                removed += 1;
                println!("  removed {}", fl.link.display());
            }
            Ok(false) => {
                // No longer repo-pointing between scan and unlink (e.g. user
                // repointed it). Skip rather than touch a now-foreign link.
                failed += 1;
                eprintln!(
                    "  warning: {} no longer points into repo — skipped",
                    fl.link.display()
                );
            }
            Err(e) => {
                failed += 1;
                eprintln!("  warning: could not remove {}: {e}", fl.link.display());
            }
        }
    }

    println!("\nRemoved {removed} link(s).");
    if failed > 0 {
        // Red line: honest exit codes. A scripted `stitch prune --yes && …`
        // must not sail past links that couldn't be removed — mirror the
        // non-zero exit cmd_apply returns on conflicts/errors.
        eprintln!("{failed} link(s) could not be removed — see warnings above.");
        return Err("prune could not remove some links — see warnings above".into());
    }
    Ok(())
}
