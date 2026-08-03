mod cli;
mod config;
mod hooks;
mod linker;
mod platform;
mod render;
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
    // `init` is cwd-anchored: it creates a new repo in the current directory,
    // so it must not honor --repo/STITCH_REPO. Every other command resolves
    // the repo once here (flag > env > cwd walk) and receives `&root`.
    match cli.command {
        cli::Commands::Init => cmd_init(),
        cli::Commands::Apply {
            only,
            dry_run,
            force,
        } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_apply(&root, &only, store::ApplyOpts { dry_run, force })
        }
        cli::Commands::Status { name } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_status(&root, &name)
        }
        cli::Commands::Diff { only, force } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_diff(&root, &only, force)
        }
        cli::Commands::List => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_list(&root)
        }
        cli::Commands::Add {
            path,
            name,
            files,
            patterns,
            dry_run,
        } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_add(&root, &path, &name, &files, &patterns, dry_run)
        }
        cli::Commands::Remove { name } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_remove(&root, &name)
        }
        cli::Commands::Edit { entry } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_edit(&root, entry.as_deref())
        }
        cli::Commands::Import { scan_dirs, dry_run } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_import(&root, &scan_dirs, dry_run)
        }
        cli::Commands::Doctor => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_doctor(&root)
        }
        cli::Commands::Migrate { dry_run } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_migrate(&root, dry_run)
        }
        cli::Commands::Prune {
            scan_dirs,
            dry_run,
            yes,
        } => {
            let root = resolve_root(cli.repo.as_deref())?;
            cmd_prune(&root, &scan_dirs, dry_run, yes)
        }
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

/// Resolve the repo root.
///
/// Precedence: an explicit `--repo` override > the `STITCH_REPO` env var > an
/// upward walk from cwd looking for `.stitch/`. `init` is cwd-anchored and
/// does not call this. An override (flag or env) must point at a directory
/// that actually contains `.stitch/` — we don't trust a bare path, so a typo
/// can't silently operate on the wrong directory.
fn resolve_root(
    override_path: Option<&str>,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    if let Some(p) = override_path {
        return resolve_override(p, "--repo");
    }
    if let Ok(p) = std::env::var("STITCH_REPO")
        && !p.is_empty()
    {
        return resolve_override(&p, "STITCH_REPO");
    }
    let cwd = std::env::current_dir()?;
    find_root(&cwd).ok_or_else(|| "not inside a stitch repo (no .stitch/ found)".into())
}

/// Validate an explicit repo override (from `--repo` or `STITCH_REPO`):
/// expand `~`, require a `.stitch/` dir so a typo can't silently operate on
/// the wrong directory, and canonicalize when possible. `label` prefixes the
/// error so the user knows which override was bad.
fn resolve_override(
    path: &str,
    label: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let root = expand_home(path);
    if !root.join(".stitch").is_dir() {
        return Err(format!(
            "{label} {} does not point at a stitch repo (no .stitch/ found)",
            root.display()
        )
        .into());
    }
    Ok(root.canonicalize().unwrap_or(root))
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

    // Trust foundation (v0.6): staging dir must never enter version control.
    // Append `.stitch/render/` to .gitignore (create if needed). Idempotent.
    render::ensure_render_gitignore(&cwd)?;

    // Pre-create the staging root at 0700 so the permission contract holds
    // before the first templated apply.
    render::ensure_render_dir(&render::render_root(&cwd))?;

    println!("Initialized stitch config:");
    println!("  {}", authored_path.display());
    println!("  {}", stitch_dir.join("state.toml").display());
    println!("  {}", cwd.join(".gitignore").display());
    Ok(())
}

fn cmd_apply(
    root: &std::path::Path,
    only: &[String],
    opts: store::ApplyOpts,
) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = Config::load(root)?;
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

    // Upgraded plain repos need no migration, but a real template apply must
    // not create sensitive staged output before Git is told to ignore it.
    if !opts.dry_run
        && store::has_active_template_sources(root, &filtered_config, &platform)
        && !render::repo_gitignore_covers_render(root)
    {
        return Err(format!(
            "repo .gitignore is missing `{}` — add that entry before applying templates",
            render::RENDER_GITIGNORE_ENTRY
        )
        .into());
    }

    // Global pre-apply hook (skipped under dry-run — hooks have side effects).
    if !opts.dry_run {
        let env = hooks::HookEnv {
            root,
            store: None,
            target: None,
            action: "apply",
        };
        hooks::run_global_hook(root, "pre-apply", &env, &platform)
            .map_err(|e| format!("pre-apply hook: {e}"))?;
    }

    let results = store::apply_all(root, &filtered_config, &platform, opts);

    let mut created = 0;
    let mut replaced = 0;
    let mut backed_up = 0;
    let mut removed = 0;
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
                store::ApplyAction::ContentChanged(p) => {
                    // Count as a real change so apply/diff are non-empty when
                    // only template content drifted (link state unchanged).
                    replaced += 1;
                    println!("content: {}", p.display());
                }
                store::ApplyAction::Removed(p) => {
                    removed += 1;
                    println!("remove: {}", p.display());
                }
                store::ApplyAction::Error(e) => {
                    errors += 1;
                    println!("error: {e}");
                }
            }
        }
    }

    println!(
        "\nSummary: {} ok, {} created, {} replaced, {} backed up, {} removed, {} conflicts, {} errors, {} skipped",
        already, created, replaced, backed_up, removed, conflicts, errors, skipped
    );

    // Global post-apply hook (skipped under dry-run). Warns on failure — the
    // apply already happened, so post-hook failure does not abort.
    if !opts.dry_run {
        let env = hooks::HookEnv {
            root,
            store: None,
            target: None,
            action: "apply",
        };
        if let Err(e) = hooks::run_global_hook(root, "post-apply", &env, &platform) {
            eprintln!("warning: post-apply hook: {e}");
        }
    }

    if errors > 0 || conflicts > 0 {
        Err(format!("{} errors, {} conflicts", errors, conflicts).into())
    } else {
        Ok(())
    }
}

fn cmd_status(
    root: &std::path::Path,
    name: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    if let Some(name) = name {
        check_unknown_names(std::iter::once(name.as_str()), &loaded.config)?;
    }
    let platform = Platform::detect();

    let entries = store::status_all(root, &loaded.config, &platform);

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

fn cmd_diff(
    root: &std::path::Path,
    only: &[String],
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    cmd_apply(
        root,
        only,
        store::ApplyOpts {
            dry_run: true,
            force,
        },
    )
}

fn cmd_list(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = Config::load(root)?;
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
    root: &std::path::Path,
    path: &str,
    name: &Option<String>,
    files: &[String],
    patterns: &[String],
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut loaded = Config::load(root)?;
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
            root,
            &store_name,
            &new_store,
            &platform,
            &loaded.config.vars,
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
                    let _ = linker::remove_link(p, root);
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
        if let Err(e) = loaded.generated.save(root) {
            for action in &results.actions {
                if let store::ApplyAction::Created(p) | store::ApplyAction::Replaced(p) = action {
                    let _ = linker::remove_link(p, root);
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
            root,
            &store_name,
            &new_store,
            &platform,
            &loaded.config.vars,
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
            discard_uncommitted_add(Some(&results), &store_dir, root);
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
        if let Err(e) = loaded.generated.save(root) {
            discard_uncommitted_add(Some(&results), &store_dir, root);
            return Err(e.into());
        }

        println!("Added store '{}'", store_name);
    }

    Ok(())
}

fn cmd_remove(root: &std::path::Path, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut loaded = Config::load(root)?;
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
            root,
            store: Some(name),
            target: target.as_deref(),
            action: "remove",
        };
        hooks::run_global_hook(root, "pre-remove", &env, &platform)
            .map_err(|e| format!("pre-remove hook: {e}"))?;
    }

    // Compute link statuses from the still-complete merged view, then drop the
    // entry from the generated half. stitch.toml behavior is deliberately left
    // in place (the tool never rewrites authored config); `doctor` flags the
    // orphaned behavior if the user wants to clean it up via `stitch edit`.
    let statuses = store::status_all(root, &loaded.config, &platform);
    loaded.generated.stores.remove(name);

    let linked: Vec<_> = statuses
        .iter()
        .filter(|e| e.store_name == *name && !e.skipped_platform)
        .filter(|e| e.status == linker::LinkStatus::Linked)
        .collect();
    for entry in &linked {
        linker::remove_link(&entry.target, root)?;
        println!("  removed {}", entry.target.display());
    }

    // Staging is tool-owned: drop the store's render tree alongside its links.
    if let Err(e) = render::remove_store_staging(root, name) {
        eprintln!("warning: {e}");
    }

    loaded.generated.save(root)?;

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

fn cmd_edit(root: &std::path::Path, entry: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let path = match entry {
        None => {
            let authored_path = root.join("stitch.toml");
            if !authored_path.exists() {
                return Err(format!(
                    "{} does not exist — run `stitch init` first",
                    authored_path.display()
                )
                .into());
            }
            authored_path
        }
        Some(e) => {
            let loaded = Config::load(root)?;
            print_warnings(&loaded);
            render::resolve_edit_source(root, &loaded.config, e)?
        }
    };

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status()?;

    if !status.success() {
        return Err("editor exited with error".into());
    }
    Ok(())
}

/// Import existing repo-pointing symlinks into `.stitch/state.toml`.
///
/// Groups found links by the store directory they resolve into. A link whose
/// target is exactly a store dir becomes a whole-dir store; links into files
/// under a store become file-mode entries. Skips links already covered by
/// config. Never rewrites `stitch.toml`.
fn cmd_import(
    root: &std::path::Path,
    scan_dirs: &[String],
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut loaded = Config::load(root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    let roots: Vec<scan::ScanRoot> = if scan_dirs.is_empty() {
        scan::default_scan_dirs()
    } else {
        scan_dirs
            .iter()
            .map(|s| scan::ScanRoot::from(expand_home(s)))
            .collect()
    };

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
        if owned.iter().any(|t| paths_equal(t, &fl.link)) {
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
        let target_str = collapse_home(&fl.link);

        let bucket = buckets.entry(store_name).or_default();
        if rest.as_os_str().is_empty() {
            // Link points at the store directory itself → whole-dir.
            bucket.whole_dir_target = Some(target_str);
        } else {
            let source_rel = rest.to_string_lossy().into_owned();
            // Target parent is where the file link lives; for file-mode the
            // store target is that parent.
            let parent = fl
                .link
                .parent()
                .map(collapse_home)
                .unwrap_or_else(|| target_str.clone());
            bucket.files.insert(source_rel, parent);
        }
    }

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

    let mut imported = 0;
    for (store_name, bucket) in &buckets {
        // Refuse to clobber an existing store entry.
        if loaded.generated.stores.contains_key(store_name) {
            println!("  skip '{store_name}': already in state.toml");
            continue;
        }

        let entry = if let Some(ref whole) = bucket.whole_dir_target {
            // Whole-dir wins if present; file entries under the same store are
            // noted but not mixed (a store is one mode).
            if !bucket.files.is_empty() {
                eprintln!(
                    "warning: store '{store_name}': found both whole-dir and file links; \
                     importing as whole-dir, file links ignored"
                );
            }
            println!("  import '{store_name}' → {whole} (whole-dir)");
            config::GeneratedStore {
                target: Some(whole.clone()),
                files: vec![],
                patterns: vec![],
                targets: std::collections::BTreeMap::new(),
            }
        } else if !bucket.files.is_empty() {
            // All file links must share the same target parent.
            let parents: std::collections::BTreeSet<_> = bucket.files.values().cloned().collect();
            if parents.len() != 1 {
                eprintln!(
                    "warning: store '{store_name}': file links point at multiple target \
                     dirs ({}); skipping",
                    parents.into_iter().collect::<Vec<_>>().join(", ")
                );
                continue;
            }
            let target = parents.into_iter().next().unwrap();
            let files: Vec<String> = bucket.files.keys().cloned().collect();
            println!(
                "  import '{store_name}' → {target} (files: {})",
                files.join(", ")
            );
            config::GeneratedStore {
                target: Some(target),
                files,
                patterns: vec![],
                targets: std::collections::BTreeMap::new(),
            }
        } else {
            continue;
        };

        if !dry_run {
            loaded.generated.stores.insert(store_name.clone(), entry);
        }
        imported += 1;
    }

    if !dry_run && imported > 0 {
        loaded.generated.save(root)?;
    }

    println!("\nImported {imported} store(s).");
    if skipped_owned > 0 {
        println!("  ({skipped_owned} already managed, skipped)");
    }
    Ok(())
}

/// True if two paths refer to the same location (canonical when possible).
fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Collapse `$HOME` prefix to `~` for state.toml target strings.
fn collapse_home(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = path.strip_prefix(&home)
    {
        if rel.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

fn cmd_doctor(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    println!("Checking stitch health...\n");

    let result = store::doctor(root, &loaded, &platform);

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

fn cmd_migrate(root: &std::path::Path, dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
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
    root: &std::path::Path,
    scan_dirs: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = Config::load(root)?;
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

    let found = scan::scan_for_repo_links(root, &roots);
    let orphans = scan::orphan_links(root, &found, &loaded.config, &platform);

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
        match linker::remove_link(&fl.link, root) {
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
