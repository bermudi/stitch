mod cli;
mod config;
mod error;
mod hooks;
mod linker;
mod plan;
mod plan_exec;
mod platform;
mod render;
mod report;
mod safety;
mod scan;
mod store;

use clap::Parser;
use config::{Config, ConfigError, Loaded, expand_home, find_root};
use error::{FailureClass, StitchError};
use plan_exec::{
    PlanExecError, PlanFile, PlanFileOp, TargetAncestorRedirect, TargetAncestorSnapshot,
};
use platform::Platform;
use serde::Serialize;
use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::Component;

fn global_redirect_to_error(redirect: TargetAncestorRedirect) -> StitchError {
    match redirect {
        TargetAncestorRedirect::Symlinked { path, resolves_to } => {
            StitchError::conflict_foreign(path, resolves_to)
        }
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: Some(resolves_to),
        } => StitchError::conflict_foreign(path, Some(resolves_to)),
        TargetAncestorRedirect::Removed { path } => StitchError::internal(format!(
            "target ancestor {} was removed by the pre-apply hook",
            path.display()
        )),
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: None,
        } => StitchError::internal(format!(
            "target ancestor {} changed identity during the pre-apply hook",
            path.display()
        )),
    }
}

fn filesystem_identity(path: &std::path::Path, label: &str) -> Result<(u64, u64), StitchError> {
    // Repository aliases are supported, so follow the root entry and pin the
    // directory it resolves to. Repointing the alias changes this identity.
    let meta = std::fs::metadata(path)
        .map_err(|e| StitchError::io_context(format!("{label} {}: metadata", path.display()), e))?;
    if !meta.file_type().is_dir() {
        return Err(StitchError::internal(format!(
            "{label} {} does not resolve to a directory",
            path.display()
        )));
    }
    Ok((meta.dev(), meta.ino()))
}

fn ensure_filesystem_identity(
    path: &std::path::Path,
    expected: (u64, u64),
    context: &str,
    label: &str,
) -> Result<(), StitchError> {
    let actual = filesystem_identity(path, label)?;
    if actual != expected {
        return Err(StitchError::internal(format!(
            "{context}: {}",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct AddData {
    store: String,
    target: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    patterns: Vec<String>,
}

#[derive(Serialize)]
struct RemoveData {
    store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    links: Vec<String>,
    staging: String,
    dry_run: bool,
}

#[derive(Serialize)]
struct ImportedStore {
    store: String,
    target: String,
    mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
}

#[derive(Serialize)]
struct ImportData {
    dry_run: bool,
    imported: usize,
    skipped_owned: usize,
    stores: Vec<ImportedStore>,
}

#[derive(Serialize)]
struct MigrateData {
    #[serde(skip_serializing_if = "Option::is_none")]
    authored_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authored: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
}

fn main() {
    let cli = cli::Cli::parse();
    let json = cli.json;
    let command_name = command_name(&cli.command);
    if let Err(e) = run(cli) {
        if json {
            report::write_error(command_name, &e, Vec::new());
        } else {
            eprintln!("error: {e}");
            if let Some(hint) = e.hint() {
                eprintln!("hint: {hint}");
            }
        }
        std::process::exit(e.exit_code());
    }
}

fn command_name(command: &cli::Commands) -> &'static str {
    use cli::Commands;
    match command {
        Commands::Init => "init",
        Commands::Apply { .. } => "apply",
        Commands::Status { .. } => "status",
        Commands::Diff { .. } => "diff",
        Commands::Plan { .. } => "plan",
        Commands::List => "list",
        Commands::Add { .. } => "add",
        Commands::Remove { .. } => "remove",
        Commands::Edit { .. } => "edit",
        Commands::Doctor => "doctor",
        Commands::Import { .. } => "import",
        Commands::Migrate { .. } => "migrate",
        Commands::Prune { .. } => "prune",
        Commands::Render { .. } => "render",
    }
}

fn run(cli: cli::Cli) -> Result<(), StitchError> {
    // `init` is cwd-anchored: it creates a new repo in the current directory,
    // so it must not honor --repo/STITCH_REPO. Every other command resolves
    // the repo once here (flag > env > cwd walk) and receives `&root`.
    let cli::Cli {
        repo,
        json,
        command,
    } = cli;
    match command {
        cli::Commands::Init => {
            if json {
                return Err(StitchError::usage("--json is not supported for init"));
            }
            cmd_init()
        }
        cli::Commands::Apply {
            only,
            dry_run,
            force,
            plan,
        } => {
            let root = resolve_root(repo.as_deref())?;
            if let Some(plan_file) = plan {
                if !only.is_empty() {
                    return Err(StitchError::usage("--plan is not compatible with --only"));
                }
                cmd_apply_plan(&root, &plan_file, dry_run, force, json)
            } else {
                cmd_apply(&root, &only, store::ApplyOpts { dry_run, force }, json)
            }
        }
        cli::Commands::Plan { only, force } => {
            let root = resolve_root(repo.as_deref())?;
            cmd_plan(&root, &only, force, json)
        }
        cli::Commands::Status { name } => {
            let root = resolve_root(repo.as_deref())?;
            cmd_status(&root, &name, json)
        }
        cli::Commands::Diff {
            only,
            force,
            exit_code,
        } => {
            let root = resolve_root(repo.as_deref())?;
            cmd_diff(&root, &only, force, exit_code, json)
        }
        cli::Commands::List => {
            let root = resolve_root(repo.as_deref())?;
            cmd_list(&root, json)
        }
        cli::Commands::Add {
            path,
            name,
            files,
            patterns,
            dry_run,
        } => {
            if json && !dry_run {
                return Err(StitchError::usage(
                    "--json is not supported for add without --dry-run",
                ));
            }
            let root = resolve_root(repo.as_deref())?;
            cmd_add(&root, &path, &name, &files, &patterns, dry_run, json)
        }
        cli::Commands::Remove { name, dry_run } => {
            if json && !dry_run {
                return Err(StitchError::usage(
                    "--json is not supported for remove without --dry-run",
                ));
            }
            let root = resolve_root(repo.as_deref())?;
            cmd_remove(&root, &name, dry_run, json)
        }
        cli::Commands::Edit { entry } => {
            if json {
                return Err(StitchError::usage("--json is not supported for edit"));
            }
            let root = resolve_root(repo.as_deref())?;
            cmd_edit(&root, entry.as_deref())
        }
        cli::Commands::Import { scan_dirs, dry_run } => {
            let root = resolve_root(repo.as_deref())?;
            cmd_import(&root, &scan_dirs, dry_run, json)
        }
        cli::Commands::Doctor => {
            let root = resolve_root(repo.as_deref())?;
            cmd_doctor(&root, json)
        }
        cli::Commands::Migrate { dry_run } => {
            if json && !dry_run {
                return Err(StitchError::usage(
                    "--json is not supported for migrate without --dry-run",
                ));
            }
            let root = resolve_root(repo.as_deref())?;
            cmd_migrate(&root, dry_run, json)
        }
        cli::Commands::Prune {
            scan_dirs,
            dry_run,
            yes,
        } => {
            let root = resolve_root(repo.as_deref())?;
            cmd_prune(&root, &scan_dirs, dry_run, yes, json)
        }
        cli::Commands::Render { spec } => {
            let root = resolve_root(repo.as_deref())?;
            cmd_render(&root, &spec, json)
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

/// Clone the config and retain only the named stores. Used by commands that
/// need a filtered view for pre-apply checks (template gitignore, global hook
/// ancestor capture) without splitting the snapshot passed to the executor.
fn filter_config(config: &Config, only: &[String]) -> Config {
    let mut filtered = config.clone();
    if !only.is_empty() {
        filtered.stores.retain(|name, _| only.contains(name));
    }
    filtered
}

/// Validate that every name in `only` exists in the config. Returns an error
/// listing unknown names so a typo can't silently do nothing.
fn check_unknown_names(
    only: impl IntoIterator<Item = impl AsRef<str>>,
    config: &Config,
) -> Result<(), StitchError> {
    let unknown: Vec<_> = only
        .into_iter()
        .filter(|n| !config.stores.contains_key(n.as_ref()))
        .map(|n| n.as_ref().to_string())
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        let valid: Vec<_> = config.stores.keys().cloned().collect();
        Err(StitchError::unknown_store(unknown, valid))
    }
}

/// Build an apply error from the failure actions in a single store result.
fn apply_error_from_actions(actions: &[store::ApplyAction]) -> Option<StitchError> {
    let mut classes = BTreeSet::new();
    for action in actions {
        match action {
            store::ApplyAction::Conflict {
                resolves_to: Some(_),
                ..
            } => {
                classes.insert(FailureClass::ConflictForeign);
            }
            store::ApplyAction::Conflict {
                resolves_to: None, ..
            } => {
                classes.insert(FailureClass::ConflictReal);
            }
            store::ApplyAction::Error(e) => {
                classes.insert(e.class());
            }
            _ => {}
        }
    }
    if classes.is_empty() {
        None
    } else {
        Some(StitchError::apply(
            classes.into_iter().collect(),
            "apply reported conflicts or errors",
        ))
    }
}

/// Resolve the repo root.
///
/// Precedence: an explicit `--repo` override > the `STITCH_REPO` env var > an
/// upward walk from cwd looking for `.stitch/`. `init` is cwd-anchored and
/// does not call this. An override (flag or env) must point at a directory
/// that actually contains `.stitch/` — we don't trust a bare path, so a typo
/// can't silently operate on the wrong directory.
fn resolve_root(override_path: Option<&str>) -> Result<std::path::PathBuf, StitchError> {
    if let Some(p) = override_path {
        return resolve_override(p, "--repo");
    }
    if let Ok(p) = std::env::var("STITCH_REPO")
        && !p.is_empty()
    {
        return resolve_override(&p, "STITCH_REPO");
    }
    let cwd = std::env::current_dir()
        .map_err(|e| StitchError::io_context("getting current working directory", e))?;
    find_root(&cwd).ok_or_else(|| StitchError::repo_resolution("cwd", cwd))
}

/// Validate an explicit repo override (from `--repo` or `STITCH_REPO`):
/// expand `~`, require a `.stitch/` dir so a typo can't silently operate on
/// the wrong directory, and canonicalize when possible. `label` prefixes the
/// error so the user knows which override was bad.
fn resolve_override(path: &str, label: &str) -> Result<std::path::PathBuf, StitchError> {
    let root = expand_home(path).map_err(StitchError::from)?;
    if !root.join(".stitch").is_dir() {
        return Err(StitchError::repo_resolution(label, root));
    }
    Ok(root.canonicalize().unwrap_or(root))
}

fn render_plan(plan: &plan::Plan, dry_run: bool) {
    if dry_run {
        println!("Dry run — no changes will be made.\n");
    }

    for store in &plan.stores {
        print!("  {} ", store.store_name);
        for op in &store.ops {
            match op {
                plan::PlanOp::CreateLink { target, .. } => println!("create: {target}"),
                plan::PlanOp::ReplaceLink { target, .. } => println!("replace: {target}"),
                plan::PlanOp::BackupAndLink { target, backup, .. } => {
                    println!("backed up: {target} → {backup}");
                }
                plan::PlanOp::Conflict { target, .. } => {
                    println!("conflict: {target}");
                }
                plan::PlanOp::SkippedPlatform => println!("(skipped: platform)"),
                plan::PlanOp::AlreadyLinked { .. } => println!("ok"),
                plan::PlanOp::ContentChanged { target, .. } => println!("content: {target}"),
                plan::PlanOp::RemoveLink { target, .. } => println!("remove: {target}"),
                plan::PlanOp::RemoveStaged { path } => println!("remove staged: {path}"),
                plan::PlanOp::Error { message, .. } => println!("error: {message}"),
                plan::PlanOp::StageRender { .. } => {}
            }
        }
    }

    let s = &plan.summary;
    let replaced = s.replaced + s.content_changed;
    println!(
        "\nSummary: {} ok, {} created, {} replaced, {} backed up, {} removed, {} conflicts, {} errors, {} skipped",
        s.already_linked,
        s.created,
        replaced,
        s.backed_up,
        s.removed,
        s.conflicts,
        s.errors,
        s.skipped
    );
}

fn plan_error(plan: &plan::Plan) -> StitchError {
    let mut classes = BTreeSet::new();
    for store in &plan.stores {
        for op in &store.ops {
            match op {
                plan::PlanOp::Conflict { resolves_to, .. } => {
                    if resolves_to.is_some() {
                        classes.insert(FailureClass::ConflictForeign);
                    } else {
                        classes.insert(FailureClass::ConflictReal);
                    }
                }
                plan::PlanOp::Error { class, .. } => {
                    if let Some(c) = FailureClass::from_id(class) {
                        classes.insert(c);
                    }
                }
                _ => {}
            }
        }
    }
    let conflicts = plan.summary.conflicts;
    let errors = plan.summary.errors;
    StitchError::apply(
        classes.into_iter().collect(),
        format!("{conflicts} conflict(s), {errors} error(s)"),
    )
}

fn cmd_init() -> Result<(), StitchError> {
    let cwd = std::env::current_dir()
        .map_err(|e| StitchError::io_context("getting current working directory", e))?;
    let gitignore = cwd.join(".gitignore");
    if std::fs::symlink_metadata(&gitignore)
        .is_ok_and(|meta| meta.file_type().is_symlink() || !meta.file_type().is_file())
    {
        return Err(StitchError::internal(format!(
            "refusing non-regular or symlinked {}",
            gitignore.display()
        )));
    }
    let stitch_dir = cwd.join(".stitch");
    match std::fs::symlink_metadata(&stitch_dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(StitchError::internal(format!(
                "refusing non-directory or symlinked {}",
                stitch_dir.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&stitch_dir).map_err(|e| {
                StitchError::io_context(
                    format!("creating config directory {}", stitch_dir.display()),
                    e,
                )
            })?;
        }
        Err(error) => {
            return Err(StitchError::io_context(
                format!("inspecting config directory {}", stitch_dir.display()),
                error,
            ));
        }
    }

    let authored_path = cwd.join("stitch.toml");
    if std::fs::symlink_metadata(&authored_path).is_ok() {
        return Err(StitchError::internal(format!(
            "config already exists at {}",
            authored_path.display()
        )));
    }
    // Refuse if a v0.2 repo is present — the user should `migrate`, not re-init.
    let legacy_path = stitch_dir.join("config.toml");
    if std::fs::symlink_metadata(&legacy_path).is_ok() {
        return Err(StitchError::config(ConfigError::LegacyV02(legacy_path)));
    }

    // Refuse if the generated state already exists — `init` must not silently
    // overwrite an existing link inventory.
    let state_path = stitch_dir.join("state.toml");
    if std::fs::symlink_metadata(&state_path).is_ok() {
        return Err(StitchError::internal(format!(
            "state already exists at {}",
            state_path.display()
        )));
    }

    // Authored half: written exactly once, with a header explaining it is the
    // user's to edit. The tool never rewrites this file after init. Reuses the
    // same fsync+rename atomicity as state writes.
    let authored_content = format!("{}{}", config::AUTHORED_TEMPLATE, "\n[vars]\n");
    let mut durability_warnings = Vec::new();
    match config::atomic_write(&authored_path, &authored_content) {
        Ok(()) => {}
        Err(error) if error.write_committed() => durability_warnings.push(error.to_string()),
        Err(error) => return Err(error.into()),
    }

    // Generated half: empty state. Reserialized by the tool on every mutation.
    match config::GeneratedState::default().save(&cwd) {
        Ok(()) => {}
        Err(error) if error.write_committed() => durability_warnings.push(error.to_string()),
        Err(error) => return Err(error.into()),
    }

    // Trust foundation (v0.6): staging dir must never enter version control.
    // Append `.stitch/render/` to .gitignore (create if needed). Idempotent.
    render::ensure_render_gitignore(&cwd).map_err(|e| {
        StitchError::io_context(format!("updating .gitignore in {}", cwd.display()), e)
    })?;

    // Pre-create the staging root at 0700 so the permission contract holds
    // before the first templated apply.
    render::ensure_render_root(&cwd).map_err(StitchError::internal)?;

    if !durability_warnings.is_empty() {
        return Err(StitchError::internal(format!(
            "initialization completed, but its config directory could not be synced: {}",
            durability_warnings.join("; ")
        )));
    }

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
    json: bool,
) -> Result<(), StitchError> {
    let snapshot = config::ConfigSnapshot::load(root)?;
    // Test-only seam: deterministically simulate a config swap between
    // `ConfigSnapshot::load` (above) and the rest of the direct-apply
    // handler (global pre-apply hook revalidation, per-store pre-hook
    // revalidation). This lets unit tests reproduce the parse-then-restore
    // TOCTOU without a flaky race, exercising the actual `cmd_apply` /
    // `apply_json` production path — both text and JSON.
    #[cfg(test)]
    test_pause_after_snapshot();
    if !json {
        print_warnings(&snapshot.loaded);
    }
    check_unknown_names(only.iter().map(|s| s.as_str()), &snapshot.loaded.config)?;

    if json {
        return apply_json(
            root,
            &snapshot,
            only,
            opts,
            "apply",
            snapshot.loaded.warnings.clone(),
        );
    }

    let platform = Platform::detect();

    // Upgraded plain repos need no migration, but template apply and its
    // dry-run must agree that staging is blocked until Git ignores it.
    let filtered_config = filter_config(&snapshot.loaded.config, only);
    if store::has_active_template_sources(root, &filtered_config, &platform)
        && !render::repo_gitignore_covers_render(root)
    {
        return Err(StitchError::internal(format!(
            "repo .gitignore is missing `{}` — add that entry before applying templates",
            render::RENDER_GITIGNORE_ENTRY
        )));
    }

    // Global pre-apply hook (skipped under dry-run — hooks have side effects).
    if !opts.dry_run {
        // Pin $HOME identity (including the resolved directory behind a
        // symlinked $HOME) across the global pre-apply hook. A hook that
        // replaces the directory behind the symlink without changing the
        // symlink itself must be detected.
        let home_identity =
            safety::HomeIdentity::capture().map_err(|e| StitchError::internal(e.to_string()))?;

        // Pin every target ancestor across the global pre-apply hook, the same
        // way execute_plan and per-store pre-hooks do.
        let home = expand_home("~")?;
        let mut all_targets = Vec::new();
        let mut all_removed = BTreeSet::new();
        for (name, store) in &filtered_config.stores {
            let (targets, removed) =
                store::collect_store_link_targets(root, name, store, &platform)
                    .map_err(StitchError::internal)?;
            all_targets.extend(targets);
            all_removed.extend(removed);
        }
        let global_ancestors =
            TargetAncestorSnapshot::capture(root, all_targets, &all_removed, &home)
                .map_err(global_redirect_to_error)?;

        let root_identity = filesystem_identity(root, "repository root")?;
        let pinned_hash = snapshot.hash().to_string();
        let env = hooks::HookEnv {
            root,
            store: None,
            target: None,
            action: "apply",
        };
        hooks::run_global_hook(root, "pre-apply", &env, &platform)
            .map_err(|e| StitchError::hook("pre-apply", e))?;
        global_ancestors
            .revalidate()
            .map_err(global_redirect_to_error)?;
        // Revalidate $HOME identity: detect a hook that replaced the directory
        // behind a symlinked $HOME.
        home_identity
            .revalidate()
            .map_err(|e| StitchError::internal(e.to_string()))?;
        ensure_filesystem_identity(
            root,
            root_identity,
            "repository changed during pre-apply hook",
            "repository root",
        )?;
        if config::revalidate_config_hash(root)? != pinned_hash {
            return Err(StitchError::plan_stale(
                "config changed during pre-apply hook",
            ));
        }
    }

    let (plan, warnings) = store::apply_all(
        root,
        &snapshot.loaded.config,
        Some(snapshot.hash()),
        only,
        &platform,
        opts,
    );

    for w in &warnings {
        eprintln!("warning: {w}");
    }

    render_plan(&plan, opts.dry_run);

    if plan.summary.errors > 0 || plan.summary.conflicts > 0 {
        return Err(plan_error(&plan));
    }

    // Global post-apply hook (skipped under dry-run). Per-store execution has
    // revalidated the repository after every post-hook, so this pathname still
    // identifies the repository whose apply just completed.
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
    Ok(())
}

fn cmd_plan(
    root: &std::path::Path,
    only: &[String],
    force: bool,
    json: bool,
) -> Result<(), StitchError> {
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    check_unknown_names(only.iter().map(|s| s.as_str()), &loaded.config)?;

    let mut filtered_config = loaded.config.clone();
    if !only.is_empty() {
        filtered_config.stores.retain(|name, _| only.contains(name));
    }

    let platform = Platform::detect();
    let plan = store::compute_plan(
        root,
        &filtered_config,
        &platform,
        store::ApplyOpts {
            dry_run: true,
            force,
        },
    );
    let plan_file = plan_exec::build_plan_file(root, &loaded, &plan, &platform)?;

    if json {
        let error = if plan_file.conflicts.is_empty() && plan_file.errors.is_empty() {
            None
        } else {
            Some(plan_exec::plan_exec_error(&plan_file))
        };
        if let Some(ref e) = error {
            report::write_data_error("plan", &plan_file, e, loaded.warnings);
        } else {
            report::write("plan", &plan_file, loaded.warnings);
        }
        Ok(())
    } else {
        println!(
            "{}",
            serde_json::to_string(&plan_file).expect("plan serializable")
        );
        if plan_file.conflicts.is_empty() && plan_file.errors.is_empty() {
            Ok(())
        } else {
            Err(plan_exec::plan_exec_error(&plan_file))
        }
    }
}

fn cmd_apply_plan(
    root: &std::path::Path,
    plan_path: &str,
    dry_run: bool,
    force: bool,
    json: bool,
) -> Result<(), StitchError> {
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }

    let plan_data = std::fs::read_to_string(plan_path).map_err(|e| {
        StitchError::plan_stale(format!("could not read plan file {plan_path}: {e}"))
    })?;
    let plan: PlanFile = serde_json::from_str(&plan_data)
        .map_err(|e| StitchError::plan_stale(format!("invalid plan file: {e}")))?;

    // Real template staging requires the render tree to be gitignored.
    if plan
        .ops
        .iter()
        .any(|op| matches!(op, PlanFileOp::StageRender { .. }))
        && !render::repo_gitignore_covers_render(root)
    {
        return Err(StitchError::internal(format!(
            "repo .gitignore is missing `{}` — add that entry before applying templates",
            render::RENDER_GITIGNORE_ENTRY
        )));
    }

    let result = plan_exec::execute_plan(root, &loaded, &plan, dry_run, force);

    if json {
        match result {
            Ok(report) => {
                let mut warnings = loaded.warnings;
                warnings.extend(report.warnings.iter().cloned());
                report::write("apply", report, warnings);
                Ok(())
            }
            Err(e) => {
                let mut warnings = loaded.warnings;
                warnings.extend(e.report.warnings.iter().cloned());
                report::write_data_error("apply", e.report, &e.error, warnings);
            }
        }
    } else {
        match result {
            Ok(report) => {
                for w in &report.warnings {
                    eprintln!("warning: {w}");
                }
                if dry_run {
                    println!("Dry run — no changes will be made.");
                }
                println!(
                    "Executed {}/{} ops",
                    report.ops_executed.len(),
                    report.ops_total
                );
                if !report.conflicts.is_empty() {
                    println!("{} conflict(s) in plan", report.conflicts.len());
                }
                if !report.staged.is_empty() {
                    for s in &report.staged {
                        println!("  staged: {s}");
                    }
                }
                if !plan.conflicts.is_empty() || !plan.errors.is_empty() {
                    Err(plan_exec::plan_exec_error(&plan))
                } else {
                    Ok(())
                }
            }
            Err(e) => {
                let PlanExecError { report, error } = e;
                for w in &report.warnings {
                    eprintln!("warning: {w}");
                }
                eprintln!(
                    "Aborted after {} of {} ops: {}",
                    report.ops_executed.len(),
                    report.ops_total,
                    error
                );
                if !report.ops_executed.is_empty() {
                    eprintln!("  executed:");
                    for op in &report.ops_executed {
                        eprintln!("    {op}");
                    }
                }
                if !report.ops_remaining.is_empty() {
                    eprintln!("  remaining:");
                    for op in &report.ops_remaining {
                        eprintln!("    {op}");
                    }
                }
                Err(*error)
            }
        }
    }
}

fn apply_json(
    root: &std::path::Path,
    snapshot: &config::ConfigSnapshot,
    only: &[String],
    opts: store::ApplyOpts,
    command: &'static str,
    loaded_warnings: Vec<String>,
) -> Result<(), StitchError> {
    let platform = Platform::detect();
    let config = &snapshot.loaded.config;
    let filtered_config = filter_config(config, only);

    if store::has_active_template_sources(root, &filtered_config, &platform)
        && !render::repo_gitignore_covers_render(root)
    {
        return Err(StitchError::internal(format!(
            "repo .gitignore is missing `{}` — add that entry before applying templates",
            render::RENDER_GITIGNORE_ENTRY
        )));
    }

    if !opts.dry_run {
        // Pin $HOME identity (including the resolved directory behind a
        // symlinked $HOME) across the global pre-apply hook.
        let home_identity =
            safety::HomeIdentity::capture().map_err(|e| StitchError::internal(e.to_string()))?;

        // Pin every target ancestor across the global pre-apply hook.
        let home = expand_home("~")?;
        let mut all_targets = Vec::new();
        let mut all_removed = BTreeSet::new();
        for (name, store) in &filtered_config.stores {
            let (targets, removed) =
                store::collect_store_link_targets(root, name, store, &platform)
                    .map_err(StitchError::internal)?;
            all_targets.extend(targets);
            all_removed.extend(removed);
        }
        let global_ancestors =
            TargetAncestorSnapshot::capture(root, all_targets, &all_removed, &home)
                .map_err(global_redirect_to_error)?;

        let root_identity = filesystem_identity(root, "repository root")?;
        let pinned_hash = snapshot.hash().to_string();
        let env = hooks::HookEnv {
            root,
            store: None,
            target: None,
            action: "apply",
        };
        hooks::run_global_hook(root, "pre-apply", &env, &platform)
            .map_err(|e| StitchError::hook("pre-apply", e))?;
        global_ancestors
            .revalidate()
            .map_err(global_redirect_to_error)?;
        home_identity
            .revalidate()
            .map_err(|e| StitchError::internal(e.to_string()))?;
        ensure_filesystem_identity(
            root,
            root_identity,
            "repository changed during pre-apply hook",
            "repository root",
        )?;
        if config::revalidate_config_hash(root)? != pinned_hash {
            return Err(StitchError::plan_stale(
                "config changed during pre-apply hook",
            ));
        }
    }

    let (plan, mut warnings) = store::apply_all(
        root,
        &snapshot.loaded.config,
        Some(snapshot.hash()),
        only,
        &platform,
        opts,
    );
    warnings.extend(loaded_warnings);

    if !opts.dry_run && plan.summary.errors == 0 && plan.summary.conflicts == 0 {
        let env = hooks::HookEnv {
            root,
            store: None,
            target: None,
            action: "apply",
        };
        if let Err(e) = hooks::run_global_hook(root, "post-apply", &env, &platform) {
            warnings.push(format!("post-apply hook: {e}"));
        }
    }

    if plan.summary.errors > 0 || plan.summary.conflicts > 0 {
        let error = plan_error(&plan);
        report::write_data_error(command, plan, &error, warnings);
    }

    report::write(command, plan, warnings);
    Ok(())
}

fn cmd_status(
    root: &std::path::Path,
    name: &Option<String>,
    json: bool,
) -> Result<(), StitchError> {
    if json {
        return report::run_json("status", || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            let warnings = loaded.warnings;
            if let Some(filter) = name {
                check_unknown_names(std::iter::once(filter.as_str()), &loaded.config)
                    .map_err(|e| Box::new((e, warnings.clone())))?;
            }
            let platform = Platform::detect();
            let entries = store::status_all(root, &loaded.config, &platform);
            let filtered: Vec<_> = if let Some(filter) = name {
                entries
                    .into_iter()
                    .filter(|e| &e.store_name == filter)
                    .collect()
            } else {
                entries
            };
            let data = report::status(root, &filtered);
            Ok((data, warnings))
        });
    }

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
            linker::LinkStatus::Foreign(p) => {
                format!("◆ foreign → {}", p.display())
            }
            linker::LinkStatus::StoreError(p) => {
                format!(
                    "✗ error: store directory '{}' is missing, symlinked, or not a directory",
                    p.display()
                )
            }
            linker::LinkStatus::ConfigError(msg) => {
                format!("✗ error: {msg}")
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

fn pending_change_count(plan: &plan::Plan) -> usize {
    let summary = &plan.summary;
    summary.created
        + summary.replaced
        + summary.backed_up
        + summary.removed
        + summary.content_changed
}

fn cmd_diff(
    root: &std::path::Path,
    only: &[String],
    force: bool,
    exit_code: bool,
    json: bool,
) -> Result<(), StitchError> {
    let loaded = Config::load(root)?;
    if !json {
        print_warnings(&loaded);
    }
    check_unknown_names(only.iter().map(|s| s.as_str()), &loaded.config)?;

    let mut filtered_config = loaded.config.clone();
    if !only.is_empty() {
        filtered_config.stores.retain(|name, _| only.contains(name));
    }

    if json {
        return report::run_json("diff", || {
            let platform = Platform::detect();
            let plan = store::compute_plan(
                root,
                &filtered_config,
                &platform,
                store::ApplyOpts {
                    dry_run: true,
                    force,
                },
            );
            if plan.summary.errors > 0 || plan.summary.conflicts > 0 {
                let error = plan_error(&plan);
                report::write_data_error("diff", plan, &error, loaded.warnings);
            }
            let changes = pending_change_count(&plan);
            if exit_code && changes > 0 {
                let error = StitchError::drift(changes);
                report::write_data_error("diff", plan, &error, loaded.warnings);
            }
            Ok((plan, loaded.warnings))
        });
    }

    let platform = Platform::detect();
    let plan = store::compute_plan(
        root,
        &filtered_config,
        &platform,
        store::ApplyOpts {
            dry_run: true,
            force,
        },
    );

    // When the plan contains no actionable changes, `diff` is a no-op.
    // Report that clearly instead of rendering the full dry-run summary.
    let clean = plan.summary.created == 0
        && plan.summary.replaced == 0
        && plan.summary.backed_up == 0
        && plan.summary.removed == 0
        && plan.summary.content_changed == 0
        && plan.summary.conflicts == 0
        && plan.summary.errors == 0
        && plan.summary.skipped == 0;
    if clean {
        println!("no differences");
        return Ok(());
    }

    render_plan(&plan, true);

    if plan.summary.errors > 0 || plan.summary.conflicts > 0 {
        Err(plan_error(&plan))
    } else {
        let changes = pending_change_count(&plan);
        if exit_code && changes > 0 {
            Err(StitchError::drift(changes))
        } else {
            Ok(())
        }
    }
}

fn cmd_list(root: &std::path::Path, json: bool) -> Result<(), StitchError> {
    if json {
        return report::run_json("list", || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            let data = report::list(&loaded.config);
            Ok((data, loaded.warnings))
        });
    }

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
///
/// The destination is revalidated immediately before rename. If the return
/// link was repointed (or any other entry appeared), leave both that entry and
/// the adopted data untouched rather than letting `rename` overwrite it.
/// Propagates the io::Error only if restoring the original fails — a leftover
/// empty store dir is non-critical and ignored.
fn rollback_adopt_move(
    source: &std::path::Path,
    store_dir: &std::path::Path,
    raw_name: &str,
    is_dir: bool,
) -> Result<(), std::io::Error> {
    match std::fs::symlink_metadata(source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to restore over entry that appeared at {}",
                    source.display()
                ),
            ));
        }
        Err(error) => return Err(error),
    }

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

/// Remove links and staged renders created by an `add` attempt using their
/// exact desired sources. A link repointed meanwhile remains untouched.
///
/// Cleanup is best effort only in the sense that every independent step is
/// attempted. Its failures are returned to the user: a failed `add` must not
/// quietly leave an unrecorded link or rendered output behind.
fn cleanup_uncommitted_add(
    repo_root: &std::path::Path,
    store_name: &str,
    new_store: &config::Store,
    platform: &Platform,
) -> Vec<String> {
    let mut config = Config {
        vars: std::collections::BTreeMap::new(),
        stores: std::collections::BTreeMap::new(),
    };
    config
        .stores
        .insert(store_name.to_string(), new_store.clone());
    let mut errors = Vec::new();
    for entry in store::status_all(repo_root, &config, platform) {
        if let Err(error) = linker::remove_link_to(&entry.target, &entry.link_source, repo_root) {
            errors.push(format!(
                "could not remove uncommitted link {}: {error}",
                entry.target.display()
            ));
        }
    }
    if let Err(error) = render::remove_store_staging(repo_root, store_name) {
        errors.push(error);
    }
    errors
}

fn discard_uncommitted_add(store_dir: &std::path::Path) -> Option<String> {
    match std::fs::remove_dir(store_dir) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!(
            "could not remove uncommitted store directory {}: {error}",
            store_dir.display()
        )),
    }
}

fn add_cleanup_error(primary: StitchError, errors: Vec<String>) -> StitchError {
    if errors.is_empty() {
        primary
    } else {
        StitchError::internal(format!(
            "add failed ({primary}); cleanup also failed: {}. Inspect the listed paths before retrying.",
            errors.join("; ")
        ))
    }
}

fn cmd_add(
    root: &std::path::Path,
    path: &str,
    name: &Option<String>,
    files: &[String],
    patterns: &[String],
    dry_run: bool,
    json: bool,
) -> Result<(), StitchError> {
    // Serialize state mutations: load must see latest state and save must not
    // race. Hold the exclusive lock for the entire non-dry-run operation.
    let _state_lock = if dry_run {
        None
    } else {
        Some(config::StateLock::exclusive(root).map_err(StitchError::from)?)
    };
    let mut loaded = Config::load(root)?;
    print_warnings(&loaded);

    if json && !dry_run {
        return Err(StitchError::usage(
            "--json is not supported for add without --dry-run",
        ));
    }
    if let Some(name) = name
        && !config::is_store_name(name)
    {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{name}': store names must be exactly one normal path component"
        )));
    }

    let expanded_source = expand_home(path)?;
    let raw_source = if expanded_source.is_absolute() {
        expanded_source
    } else {
        std::env::current_dir()
            .map_err(|e| StitchError::io_context("getting current working directory", e))?
            .join(expanded_source)
    };
    // Symlink-aware normalization: gateway/../victim must resolve through
    // the gateway symlink (POSIX: symlink target spliced before ..), not
    // collapse lexically to ~/victim. Ancestors resolve fully, but the
    // terminal component is never followed — a terminal symlink must be
    // rejected below, not silently adopted (its referent would be moved and
    // the original link repointed during reconciliation). Only apply
    // canonical resolution when the path contains ".." — otherwise preserve
    // lexical HOME spelling so a symlinked $HOME (home_link -> real_home)
    // doesn't canonicalize ~/.bashrc to /real_home/.bashrc and break
    // collapse_home. Resolution failure is a hard error: falling back to
    // lexical normalization could silently pick a different file.
    let source = if raw_source
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        crate::linker::resolve_ancestors_with_missing(&raw_source).ok_or_else(|| {
            StitchError::internal(format!(
                "could not resolve {} through symlinks — refusing to guess at the path",
                raw_source.display()
            ))
        })?
    } else {
        lexically_normalize(&raw_source)
    };

    // A symlink at the target is always an error — we never silently clobber
    // or repoint a foreign symlink.
    if source.is_symlink() {
        return Err(StitchError::internal(format!(
            "{} is already a symlink — add expects a real file or directory \
             (remove the symlink first if you want stitch to manage it)",
            source.display()
        )));
    }

    // Derive store name from basename, leading dot stripped. Override via --name.
    let raw_name = source
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let store_name = name
        .clone()
        .unwrap_or_else(|| raw_name.trim_start_matches('.').to_string());
    if !config::is_store_name(&store_name) {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{store_name}': store names must be exactly one normal path component"
        )));
    }
    let store_dir = root.join(&store_name);

    // Pre-checks: reject any collision BEFORE mutating anything.
    if loaded.config.stores.contains_key(&store_name) {
        return Err(StitchError::internal(format!(
            "store '{}' already exists",
            store_name
        )));
    }
    if store_dir.symlink_metadata().is_ok() {
        return Err(StitchError::internal(format!(
            "store path '{}' already exists",
            store_dir.display()
        )));
    }

    // Validate user-supplied fragments before touching the filesystem: a
    // `--file ../x` would otherwise escape the store/target dirs during apply
    // (and leave an orphaned store dir on failure).
    let validation_context = format!("store '{store_name}'");
    config::validate_fragments(files, patterns, &validation_context)?;
    // Match generated-state validation before the dry-run branch so a preview
    // never accepts a pattern that a real add would refuse to persist.
    config::validate_globs(patterns, &[], &validation_context)?;

    let source_exists = source.exists();

    // --files/--patterns only apply when creating an empty store (path doesn't
    // exist). On the adopt path the moved content determines the store layout,
    // so passing them is a user error — silently ignoring them would repeat the
    // "stitch says done, did nothing useful" footgun this command was created to
    // fix.
    if source_exists && (!files.is_empty() || !patterns.is_empty()) {
        return Err(StitchError::usage(format!(
            "{} exists — --files/--patterns only apply when creating a new empty store \
             (the existing content is moved into the repo as-is)",
            source.display()
        )));
    }

    if dry_run {
        if source_exists {
            let is_dir = source.is_dir();
            let (target_str, adopt_files) = if is_dir {
                (collapse_home(&source)?, Vec::new())
            } else {
                let parent = source
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "~".into());
                (
                    collapse_home(&expand_home(&parent)?)?,
                    vec![raw_name.clone()],
                )
            };
            config::validate_target(&target_str, &format!("store '{store_name}'"))?;

            let data = AddData {
                store: store_name.clone(),
                target: target_str,
                mode: "adopt".into(),
                source: Some(collapse_home(&source)?),
                files: adopt_files,
                patterns: Vec::new(),
            };
            if json {
                report::write("add", data, loaded.warnings);
                return Ok(());
            }
            println!("Would add (adopt existing):");
            println!("  {} → {}/", source.display(), store_dir.display());
            let target = expand_home(&data.target)?;
            println!("  then symlink back to {}", target.display());
        } else {
            let target_str = collapse_home(&source)?;
            config::validate_target(&target_str, &format!("store '{store_name}'"))?;

            let data = AddData {
                store: store_name.clone(),
                target: target_str,
                mode: "create".into(),
                source: None,
                files: files.to_vec(),
                patterns: patterns.to_vec(),
            };
            if json {
                report::write("add", data, loaded.warnings);
                return Ok(());
            }
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
            collapse_home(&source)?
        } else {
            match source.parent() {
                Some(p) => collapse_home(p)?,
                None => "~".into(),
            }
        };
        config::validate_target(&target_str, &format!("store '{store_name}'"))?;

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
            std::fs::rename(&source, &store_dir).map_err(|e| {
                StitchError::io_context(
                    format!(
                        "moving {} into store {}",
                        source.display(),
                        store_dir.display()
                    ),
                    e,
                )
            })?;
        } else {
            std::fs::create_dir_all(&store_dir).map_err(|e| {
                StitchError::io_context(
                    format!("creating store directory {}", store_dir.display()),
                    e,
                )
            })?;
            std::fs::rename(&source, store_dir.join(&raw_name)).map_err(|e| {
                StitchError::io_context(
                    format!(
                        "moving {} into store {}",
                        source.display(),
                        store_dir.join(&raw_name).display()
                    ),
                    e,
                )
            })?;
        }

        // Link: create the return symlink using the in-memory store.
        // If this fails, roll back the move so the user's file is back where
        // it was. State was never touched.
        let platform = Platform::detect();
        let mut _warnings = Vec::new();
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
            &mut _warnings,
        );
        if results.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict { .. } | store::ApplyAction::Error(_)
            )
        }) {
            let primary = apply_error_from_actions(&results.actions)
                .unwrap_or_else(|| StitchError::internal("apply reported conflicts or errors"));
            let cleanup_errors = cleanup_uncommitted_add(root, &store_name, &new_store, &platform);
            if let Err(error) = rollback_adopt_move(&source, &store_dir, &raw_name, is_dir) {
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!(" Cleanup also failed: {}.", cleanup_errors.join("; "))
                };
                return Err(StitchError::internal(format!(
                    "ADD FAILED ({primary}) and rollback also failed: {} is stranded in {} ({error}).{cleanup}",
                    source.display(),
                    store_dir.display(),
                )));
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
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
        if let Err(error) = loaded.generated.save(root) {
            // A directory fsync can fail after rename. The state is then
            // already committed, so rolling back its links/store would make
            // that state point at missing data.
            if error.write_committed() {
                return Err(error.into());
            }
            let primary = StitchError::from(error);
            let cleanup_errors = cleanup_uncommitted_add(root, &store_name, &new_store, &platform);
            if let Err(rollback_error) = rollback_adopt_move(&source, &store_dir, &raw_name, is_dir)
            {
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!(" Cleanup also failed: {}.", cleanup_errors.join("; "))
                };
                return Err(StitchError::internal(format!(
                    "state save failed ({primary}) and rollback also failed: {} is stranded in {} ({rollback_error}).{cleanup}",
                    source.display(),
                    store_dir.display(),
                )));
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
        }

        println!(
            "Added store '{}' (adopted from {})",
            store_name,
            source.display()
        );
        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked(_) => println!("  already linked"),
                _ => {}
            }
        }
    } else {
        // --- Create-empty path: fresh store, link to target. ---
        let target_str = collapse_home(&source)?;
        config::validate_target(&target_str, &format!("store '{store_name}'"))?;

        let new_store = config::Store {
            target: Some(target_str.clone()),
            files: files.to_vec(),
            patterns: patterns.to_vec(),
            ignore: vec![],
            when: config::WhenClause::default(),
            hooks: config::Hooks::default(),
            targets: std::collections::BTreeMap::new(),
        };

        std::fs::create_dir_all(&store_dir).map_err(|e| {
            StitchError::io_context(
                format!("creating store directory {}", store_dir.display()),
                e,
            )
        })?;

        let platform = Platform::detect();
        let mut _warnings = Vec::new();
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
            &mut _warnings,
        );

        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked(_) => println!("  already linked"),
                store::ApplyAction::Conflict { target, .. } => {
                    println!("  conflict at {}", target.display())
                }
                store::ApplyAction::Error(e) => println!("  error: {e}"),
                _ => {}
            }
        }

        let failed = results.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict { .. } | store::ApplyAction::Error(_)
            )
        });

        if failed {
            let primary = apply_error_from_actions(&results.actions)
                .unwrap_or_else(|| StitchError::internal("apply reported conflicts or errors"));
            let mut cleanup_errors =
                cleanup_uncommitted_add(root, &store_name, &new_store, &platform);
            if let Some(error) = discard_uncommitted_add(&store_dir) {
                cleanup_errors.push(error);
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
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
        if let Err(error) = loaded.generated.save(root) {
            // See the adopt path above: rename succeeded, so leave the
            // matching links and store in place when only directory fsync
            // failed.
            if error.write_committed() {
                return Err(error.into());
            }
            let primary = StitchError::from(error);
            let mut cleanup_errors =
                cleanup_uncommitted_add(root, &store_name, &new_store, &platform);
            if let Some(error) = discard_uncommitted_add(&store_dir) {
                cleanup_errors.push(error);
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
        }

        println!("Added store '{}'", store_name);
    }

    Ok(())
}

fn cmd_remove(
    root: &std::path::Path,
    name: &str,
    dry_run: bool,
    json: bool,
) -> Result<(), StitchError> {
    // No lock yet: the pre-remove hook runs first and may itself invoke a
    // mutating stitch command (which takes the lock). The lock is acquired
    // after the hook, and the state is reloaded under it.
    let loaded = Config::load(root)?;
    print_warnings(&loaded);
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
        |loaded: &config::Loaded| -> Result<(Vec<store::StatusEntry>, Vec<String>), StitchError> {
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

fn cmd_edit(root: &std::path::Path, entry: Option<&str>) -> Result<(), StitchError> {
    let path = match entry {
        None => {
            let authored_path = root.join("stitch.toml");
            // Use symlink_metadata (not exists()) so a symlinked stitch.toml
            // is detected and rejected before the editor opens the external
            // file. validate_authored_file rejects symlinks, non-regular
            // files, and hard links; a missing file is reported as absent.
            config::validate_authored_file(&authored_path)?;
            match std::fs::symlink_metadata(&authored_path) {
                Ok(_) => authored_path,
                Err(_) => {
                    return Err(StitchError::internal(format!(
                        "{} does not exist — run `stitch init` first",
                        authored_path.display()
                    )));
                }
            }
        }
        Some(e) => {
            let loaded = Config::load(root)?;
            print_warnings(&loaded);
            render::resolve_edit_source(root, &loaded.config, e).map_err(StitchError::internal)?
        }
    };

    let editor = resolve_editor()?;
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|e| StitchError::internal(format!("could not run editor '{editor}': {e}")))?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(StitchError::internal(format!(
            "editor '{editor}' exited with status {code}"
        )));
    }
    Ok(())
}

fn resolve_editor() -> Result<String, StitchError> {
    for var in ["VISUAL", "EDITOR"] {
        if let Some(value) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
            return Ok(value);
        }
    }
    Ok("vi".into())
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
        let target_str = collapse_home(&fl.link)?;

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
            let Some(target_dir) = target_dir_for_file_link(&fl.link, &rest) else {
                continue;
            };
            let parent = collapse_home(&target_dir)?;
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

        let imported_store = if let Some(ref whole) = bucket.whole_dir_target {
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
            if json {
                ImportedStore {
                    store: store_name.clone(),
                    target: whole.clone(),
                    mode: "whole-dir".into(),
                    files: Vec::new(),
                }
            } else {
                println!("  import '{store_name}' → {whole} (whole-dir)");
                ImportedStore {
                    store: store_name.clone(),
                    target: whole.clone(),
                    mode: "whole-dir".into(),
                    files: Vec::new(),
                }
            }
        } else if !bucket.files.is_empty() {
            // All file links must share the same target parent.
            let parents: std::collections::BTreeSet<_> = bucket.files.values().cloned().collect();
            if parents.len() != 1 {
                let msg = format!(
                    "store '{store_name}': file links point at multiple target \
                     dirs ({}); skipping",
                    parents.into_iter().collect::<Vec<_>>().join(", ")
                );
                if json {
                    warnings.push(msg);
                } else {
                    eprintln!("warning: {msg}");
                }
                continue;
            }
            let target = parents.into_iter().next().unwrap();
            let files: Vec<String> = bucket.files.keys().cloned().collect();
            if !json {
                println!(
                    "  import '{store_name}' → {target} (files: {})",
                    files.join(", ")
                );
            }
            ImportedStore {
                store: store_name.clone(),
                target,
                mode: "file-mode".into(),
                files,
            }
        } else {
            continue;
        };

        if !dry_run {
            let entry = config::GeneratedStore {
                target: Some(imported_store.target.clone()),
                files: imported_store.files.clone(),
                patterns: vec![],
                targets: std::collections::BTreeMap::new(),
            };
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

/// True if two paths refer to the same location (canonical when possible).
fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// For a file-mode symlink, return the target directory by stripping the
/// repo-relative source path from the end of the symlink's location.
///
/// `link` is where the symlink lives (e.g. `~/.config/nvim/lua/plugin.lua`);
/// `source_rel` is its path inside the store (e.g. `lua/plugin.lua`). The
/// result is the common directory the store is linked into
/// (e.g. `~/.config/nvim`).
fn target_dir_for_file_link(
    link: &std::path::Path,
    source_rel: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let link_comps: Vec<_> = link.components().collect();
    let source_comps: Vec<_> = source_rel.components().collect();
    if link_comps.len() < source_comps.len() {
        return None;
    }
    let split = link_comps.len() - source_comps.len();
    if link_comps[split..] != source_comps[..] {
        return None;
    }
    let mut target = std::path::PathBuf::new();
    for c in &link_comps[..split] {
        target.push(c.as_os_str());
    }
    Some(target)
}

/// Resolve `.` and `..` components lexically, without touching the
/// filesystem or following symlinks.
fn lexically_normalize(path: &std::path::Path) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = normalized.components().next_back() {
                    normalized.pop();
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                normalized = std::path::PathBuf::new();
                normalized.push(component.as_os_str());
            }
            Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// Collapse `$HOME` prefix to `~` for state.toml target strings.
fn collapse_home(path: &std::path::Path) -> Result<String, ConfigError> {
    let home = config::expand_home("~")?;
    if let Ok(rel) = path.strip_prefix(&home) {
        if rel.as_os_str().is_empty() {
            return Ok("~".into());
        }
        return Ok(format!("~/{}", rel.display()));
    }
    Ok(path.display().to_string())
}

fn cmd_doctor(root: &std::path::Path, json: bool) -> Result<(), StitchError> {
    if json {
        return report::run_json("doctor", || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            let platform = Platform::detect();
            let result = store::doctor(root, &loaded, &platform);
            let data = report::doctor(&result);
            let warnings = loaded.warnings;
            if data.summary.errors > 0 {
                let error = StitchError::doctor(data.summary.errors);
                report::write_data_error("doctor", data, &error, warnings);
            }
            Ok((data, warnings))
        });
    }

    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    println!("Checking stitch health...\n");

    let result = store::doctor(root, &loaded, &platform);

    for finding in &result.findings {
        let label = match finding.severity {
            store::Severity::Info => "[info] ",
            store::Severity::Warning => "[warn] ",
            store::Severity::Error => "[error]",
        };
        println!("  {label} {}", finding.message);
    }

    let (errors, warnings, info) =
        result
            .findings
            .iter()
            .fold((0, 0, 0), |acc, f| match f.severity {
                store::Severity::Error => (acc.0 + 1, acc.1, acc.2),
                store::Severity::Warning => (acc.0, acc.1 + 1, acc.2),
                store::Severity::Info => (acc.0, acc.1, acc.2 + 1),
            });
    let total = errors + warnings + info;
    if total == 0 {
        println!("  All checks passed ✓");
    } else {
        println!(
            "\n  {} issues ({} errors, {} warnings, {} info)",
            total, errors, warnings, info
        );
    }

    if errors > 0 {
        Err(StitchError::doctor(errors))
    } else {
        Ok(())
    }
}

fn cmd_migrate(root: &std::path::Path, dry_run: bool, json: bool) -> Result<(), StitchError> {
    let stitch_dir = root.join(".stitch");
    let stitch_meta = std::fs::symlink_metadata(&stitch_dir).map_err(|e| {
        StitchError::internal(format!("could not inspect {}: {e}", stitch_dir.display()))
    })?;
    if stitch_meta.file_type().is_symlink() || !stitch_meta.is_dir() {
        return Err(StitchError::internal(format!(
            "{} is symlinked or not a directory — refusing migration before writing anything",
            stitch_dir.display()
        )));
    }
    // Serialize state mutations.
    let _state_lock = if dry_run {
        None
    } else {
        Some(config::StateLock::exclusive(root).map_err(StitchError::from)?)
    };
    let legacy_path = stitch_dir.join("config.toml");
    let authored_path = root.join("stitch.toml");
    let state_path = root.join(".stitch").join("state.toml");

    if !legacy_path.exists() {
        if authored_path.exists() {
            let msg = format!(
                "nothing to migrate: {} exists (already converted)",
                authored_path.display()
            );
            if json {
                report::write(
                    "migrate",
                    MigrateData {
                        authored_path: None,
                        authored: None,
                        state_path: None,
                        state: None,
                    },
                    vec![msg],
                );
            } else {
                println!("{msg}");
            }
            return Ok(());
        }
        return Err(StitchError::internal(format!(
            "nothing to migrate: {} not found",
            legacy_path.display()
        )));
    }
    // Refuse to overwrite an existing stitch.toml — a half-finished migrate
    // should not clobber the user's authored file.
    if std::fs::symlink_metadata(&authored_path).is_ok() {
        return Err(StitchError::internal(format!(
            "{} already exists — refusing to overwrite; remove it if you want to re-migrate",
            authored_path.display()
        )));
    }
    // Refuse if the .bak backup target already exists — we'd have nowhere to
    // preserve the original. Checked up front (before parse, before any write)
    // so a .bak collision fails before touching anything, matching the
    // fail-before-mutate invariant the other writers uphold.
    let backup_path = legacy_path.with_extension("toml.bak");
    if std::fs::symlink_metadata(&backup_path).is_ok() {
        return Err(StitchError::internal(format!(
            "{} already exists — move it aside first (it's where the original \
             .stitch/config.toml would be backed up during migration)",
            backup_path.display()
        )));
    }
    // Refuse to overwrite an existing state.toml — a half-finished migrate
    // should not clobber the generated state file.
    if std::fs::symlink_metadata(&state_path).is_ok() {
        return Err(StitchError::internal(format!(
            "{} already exists — refusing to overwrite; remove it if you want to re-migrate",
            state_path.display()
        )));
    }

    // Parse the v0.2 file into the frozen LegacyConfig shape (not the
    // post-split types, which no longer carry the v0.2 layout).
    let contents = std::fs::read_to_string(&legacy_path).map_err(|e| {
        StitchError::io_context(
            format!("reading legacy config {}", legacy_path.display()),
            e,
        )
    })?;
    let legacy: config::LegacyConfig = toml::from_str(&contents)
        .map_err(|e| StitchError::config(ConfigError::Parse(e, legacy_path.clone())))?;
    legacy.validate()?;

    let (authored, generated) = config::split_legacy(&legacy);

    // Validate the split inventory before rendering, previewing, or writing.
    // v0.2 accepted entries the new validator rejects (e.g. `./bashrc`); we
    // must fail fast so migration does not create state that cannot load.
    generated.validate()?;
    config::validate_merged(&authored, &generated)?;

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
        if json {
            let data = MigrateData {
                authored_path: Some(authored_path.to_string_lossy().into_owned()),
                authored: Some(authored_str),
                state_path: Some(state_path.to_string_lossy().into_owned()),
                state: Some(state_str),
            };
            report::write("migrate", data, Vec::new());
        } else {
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
        }
        return Ok(());
    }

    // Write both new files first; only after both succeed do we move the legacy
    // file aside. A crash during writes leaves the original intact. The .bak
    // target was pre-checked above, so this rename can't clobber.
    //
    // Parent-directory fsync can fail after a successful rename. Continue a
    // completed migration in that case: retrying after returning early would
    // refuse the visible authored/state files and strand the legacy config.
    let mut durability_warnings = Vec::new();
    match config::atomic_write(&authored_path, &authored_str) {
        Ok(()) => {}
        Err(error) if error.write_committed() => durability_warnings.push(error.to_string()),
        Err(error) => return Err(error.into()),
    }
    match config::atomic_write(&state_path, &state_str) {
        Ok(()) => {}
        Err(error) if error.write_committed() => durability_warnings.push(error.to_string()),
        Err(error) => return Err(error.into()),
    }

    // Preserve the original as a .bak rather than delete — the user's comments
    // and formatting are the recovery path (migrate is comment-lossy by design).
    std::fs::rename(&legacy_path, &backup_path).map_err(|e| {
        StitchError::io_context(
            format!(
                "moving legacy config {} to {}",
                legacy_path.display(),
                backup_path.display()
            ),
            e,
        )
    })?;

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
    if !durability_warnings.is_empty() {
        return Err(StitchError::internal(format!(
            "migration completed, but its config directory could not be synced: {}",
            durability_warnings.join("; ")
        )));
    }
    Ok(())
}

fn cmd_prune(
    root: &std::path::Path,
    scan_dirs: &[String],
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), StitchError> {
    if json {
        return report::run_json("prune", || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            let warnings = loaded.warnings;
            let platform = Platform::detect();
            let roots = prune_roots(scan_dirs)
                .map_err(|e| Box::new((StitchError::from(e), warnings.clone())))?;

            // Pin $HOME identity across the scan-to-removal window, matching
            // the non-JSON path.
            let home_identity = safety::HomeIdentity::capture()
                .map_err(|e| Box::new((StitchError::internal(e.to_string()), warnings.clone())))?;

            let found = scan::scan_for_repo_links(root, &roots);
            let orphan_refs = scan::orphan_links(root, &found, &loaded.config, &platform);
            let orphans: Vec<scan::FoundLink> = orphan_refs.iter().map(|&fl| fl.clone()).collect();

            if !yes || dry_run {
                let data = report::prune(&orphans, 0, 0);
                return Ok((data, warnings));
            }

            // Removal mutates links: serialize with other mutating commands
            // and re-scan under the lock, so a concurrent add/apply cannot
            // have its state or links change between classification and
            // removal.
            let _state_lock = config::StateLock::exclusive_if_present(root)
                .map_err(|e| Box::new((StitchError::from(e), warnings.clone())))?;
            // Revalidate $HOME identity under the lock before any removal.
            home_identity
                .revalidate()
                .map_err(|e| Box::new((StitchError::internal(e.to_string()), warnings.clone())))?;
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            let found = scan::scan_for_repo_links(root, &roots);
            let orphan_refs = scan::orphan_links(root, &found, &loaded.config, &platform);
            let orphans: Vec<scan::FoundLink> = orphan_refs.iter().map(|&fl| fl.clone()).collect();

            let mut removed = 0;
            let mut failed = 0;
            let mut statuses = Vec::with_capacity(orphans.len());
            for fl in &orphans {
                match linker::remove_link(&fl.link, root) {
                    Ok(true) => {
                        removed += 1;
                        statuses.push("removed".to_string());
                    }
                    Ok(false) => {
                        failed += 1;
                        statuses.push("failed".to_string());
                    }
                    Err(_) => {
                        failed += 1;
                        statuses.push("failed".to_string());
                    }
                }
            }

            let data = report::prune_with_status(&orphans, &statuses, removed, failed);
            if failed > 0 {
                let error =
                    StitchError::internal(format!("prune could not remove {failed} link(s)"));
                report::write_data_error("prune", data, &error, warnings);
            }
            Ok((data, warnings))
        });
    }

    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    let platform = Platform::detect();

    let roots = prune_roots(scan_dirs)?;

    // Pin $HOME identity across the scan-to-removal window. A symlinked $HOME
    // whose backing directory is replaced between scan and removal would
    // otherwise cause prune to remove links from the wrong directory.
    let home_identity =
        safety::HomeIdentity::capture().map_err(|e| StitchError::internal(e.to_string()))?;

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

    // Removal mutates links: serialize with other mutating commands and
    // re-scan under the lock, so a concurrent add/apply cannot have its state
    // or links change between classification and removal.
    let _state_lock = config::StateLock::exclusive_if_present(root).map_err(StitchError::from)?;
    // Revalidate $HOME identity under the lock: detect a replaced backing
    // directory before any removal.
    home_identity
        .revalidate()
        .map_err(|e| StitchError::internal(e.to_string()))?;
    let loaded = Config::load(root)?;
    let found = scan::scan_for_repo_links(root, &roots);
    let orphans = scan::orphan_links(root, &found, &loaded.config, &platform);

    if orphans.is_empty() {
        println!("No orphaned links found.");
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
        return Err(StitchError::internal(
            "prune could not remove some links — see warnings above",
        ));
    }
    Ok(())
}

fn prune_roots(scan_dirs: &[String]) -> Result<Vec<scan::ScanRoot>, ConfigError> {
    if scan_dirs.is_empty() {
        scan::default_scan_dirs()
    } else {
        scan_dirs
            .iter()
            .map(|s| Ok(scan::ScanRoot::from(expand_home(s)?)))
            .collect()
    }
}

fn validate_render_spec(
    loaded: &Loaded,
    store_name: &str,
    source_rel: &str,
) -> Result<(), StitchError> {
    if !config::is_safe_fragment(store_name) {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{store_name}': must be relative and contain no '.', '..' or leading '/'"
        )));
    }
    if !config::is_safe_fragment(source_rel) {
        return Err(StitchError::path_validation(format!(
            "invalid source path '{source_rel}': must be relative and contain no '.', '..' or leading '/'"
        )));
    }
    if !loaded.config.stores.contains_key(store_name) {
        let valid: Vec<_> = loaded.config.stores.keys().cloned().collect();
        return Err(StitchError::unknown_store(
            vec![store_name.to_string()],
            valid,
        ));
    }
    Ok(())
}

fn cmd_render(root: &std::path::Path, spec: &str, json: bool) -> Result<(), StitchError> {
    let (store_name, source_rel) = spec.split_once('/').ok_or_else(|| {
        StitchError::usage("render: expected <store>/<file>, e.g. git/gitconfig.tmpl")
    })?;
    if source_rel.is_empty() {
        return Err(StitchError::usage("render: missing file name"));
    }
    if !render::is_template(source_rel) {
        return Err(StitchError::usage(
            "render: only .tmpl files can be rendered",
        ));
    }

    if json {
        return report::run_json("render", || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            validate_render_spec(&loaded, store_name, source_rel)
                .map_err(|e| Box::new((e, loaded.warnings.clone())))?;
            let warnings = loaded.warnings;
            let store_dir = root.join(store_name);
            let source_path = store_dir.join(source_rel);
            if !source_path.is_file() {
                return Err(Box::new((
                    StitchError::internal(format!(
                        "source does not exist: {}",
                        source_path.display()
                    )),
                    warnings,
                )));
            }
            let platform = Platform::detect();
            let content =
                render::render_file(&source_path, source_rel, &platform, &loaded.config.vars)
                    .map_err(|e| {
                        Box::new((StitchError::render(&source_path, e), warnings.clone()))
                    })?;
            let data = report::render(&source_path, source_rel, &content);
            Ok((data, warnings))
        });
    }

    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    validate_render_spec(&loaded, store_name, source_rel)?;
    let store_dir = root.join(store_name);
    let source_path = store_dir.join(source_rel);
    if !source_path.is_file() {
        return Err(StitchError::internal(format!(
            "source does not exist: {}",
            source_path.display()
        )));
    }
    let platform = Platform::detect();
    let content = render::render_file(&source_path, source_rel, &platform, &loaded.config.vars)
        .map_err(|e| StitchError::render(&source_path, e))?;
    print!("{content}");
    Ok(())
}

// ===========================================================================
// Test-only seam for the direct-apply parse-then-restore TOCTOU regression.
//
// `test_pause_after_snapshot` runs inside `cmd_apply` immediately after
// `ConfigSnapshot::load`, before the global pre-apply hook revalidation and
// the per-store loop. A test installs a callback that swaps the on-disk
// config (e.g. from malicious B to benign A), then calls `cmd_apply`. This
// deterministically reproduces the race where a config is captured as B and
// restored to A before revalidation — the exact invariant that
// `ConfigSnapshot` + `pinned_hash` fixes. Both text and JSON paths go
// through `cmd_apply`, so a single seam covers both.
// ===========================================================================
#[cfg(test)]
thread_local! {
    static TEST_PAUSE_AFTER_SNAPSHOT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_pause_after_snapshot() {
    TEST_PAUSE_AFTER_SNAPSHOT.with(|p| {
        if let Some(f) = p.borrow_mut().take() {
            f();
        }
    });
}

/// Test-only setter for the post-snapshot seam. Install a callback that
/// runs inside `cmd_apply` immediately after `ConfigSnapshot::load`.
#[cfg(test)]
fn set_test_pause_after_snapshot(f: Option<Box<dyn FnOnce()>>) {
    TEST_PAUSE_AFTER_SNAPSHOT.with(|p| *p.borrow_mut() = f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::platform::Platform;
    use crate::store::ApplyOpts;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn add_rejects_nested_store_name_before_creating_a_store() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir_all(repo.join(".stitch")).unwrap();
        fs::write(repo.join("stitch.toml"), "").unwrap();
        fs::write(repo.join(".stitch/state.toml"), "").unwrap();

        let err = cmd_add(
            repo,
            &repo.join("target").to_string_lossy(),
            &Some("nested/name".into()),
            &[],
            &[],
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 9);
        assert!(!repo.join("nested").exists());
    }

    #[test]
    fn rollback_adopt_move_refuses_to_overwrite_repointed_target() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("original");
        let store_dir = tmp.path().join("store");
        let adopted = store_dir.join("original");
        let foreign = tmp.path().join("foreign");
        fs::create_dir(&store_dir).unwrap();
        fs::write(&adopted, "adopted data").unwrap();
        fs::write(&foreign, "foreign data").unwrap();
        symlink(&foreign, &source).unwrap();

        let error = rollback_adopt_move(&source, &store_dir, "original", false).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_link(&source).unwrap(), foreign);
        assert_eq!(fs::read_to_string(&source).unwrap(), "foreign data");
        assert_eq!(fs::read_to_string(adopted).unwrap(), "adopted data");
    }

    #[test]
    fn migrate_rejects_invalid_store_name_before_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let stitch = repo.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(
            stitch.join("config.toml"),
            "[stores.\"nested/name\"]\ntarget = \"~\"\n",
        )
        .unwrap();

        let err = cmd_migrate(repo, false, false).unwrap_err();
        assert_eq!(err.exit_code(), 9);
        assert!(!repo.join("stitch.toml").exists());
        assert!(!stitch.join("state.toml").exists());
        assert!(!stitch.join("config.toml.bak").exists());
    }

    #[test]
    fn remove_store_with_external_source_symlink_cleans_link_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let _home_guard = config::test_home_guard(tmp.path().to_path_buf());
        let repo = tmp.path();
        let stitch = repo.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(repo.join("stitch.toml"), "").unwrap();
        fs::write(stitch.join("state.toml"), "").unwrap();
        fs::write(repo.join(".gitignore"), ".stitch/render/\n").unwrap();

        // External file that the repo source symlink will resolve to.
        let external = tmp.path().join("external").join("real");
        fs::create_dir_all(external.parent().unwrap()).unwrap();
        fs::write(&external, "outside").unwrap();

        // Store with one regular file and one source symlink to the external path.
        let store_dir = repo.join("app");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("regular"), "regular").unwrap();
        let source_alias = store_dir.join("alias");
        symlink(&external, &source_alias).unwrap();

        let target_dir = tmp.path().join("home").join("app");

        let state = format!(
            r#"
[stores.app]
target = "{}"
files = ["regular", "alias"]
"#,
            target_dir.to_string_lossy()
        );
        fs::write(stitch.join("state.toml"), &state).unwrap();

        // Apply creates the target links. `alias` points directly at the repo
        // source entry, not through the source symlink.
        let platform = Platform::detect();
        let loaded = Config::load(repo).unwrap();
        let store = loaded.config.stores.get("app").unwrap();
        crate::store::apply_store(
            repo,
            "app",
            store,
            &platform,
            &loaded.config.vars,
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut Vec::new(),
        );

        assert!(target_dir.join("regular").is_symlink());
        assert!(target_dir.join("alias").is_symlink());
        assert_eq!(
            fs::read_link(target_dir.join("alias")).unwrap(),
            source_alias
        );
        assert_eq!(
            fs::read_to_string(target_dir.join("alias")).unwrap(),
            "outside"
        );

        // `remove` must remove the target link and the state, then exit 0.
        cmd_remove(repo, "app", false, false).unwrap();

        assert!(
            !target_dir.join("alias").exists(),
            "target link must be gone"
        );
        let state_text = fs::read_to_string(stitch.join("state.toml")).unwrap();
        assert!(
            !state_text.contains("[stores.app]"),
            "state entry must be removed"
        );

        // The repo source symlink is user config and must be untouched.
        assert!(source_alias.is_symlink());
    }

    #[test]
    fn remove_store_succeeds_when_pre_remove_hook_already_removed_link() {
        let tmp = tempfile::tempdir().unwrap();
        let _home_guard = config::test_home_guard(tmp.path().to_path_buf());
        let repo = tmp.path();
        let stitch = repo.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(repo.join("stitch.toml"), "").unwrap();
        fs::write(stitch.join("state.toml"), "").unwrap();
        fs::write(repo.join(".gitignore"), ".stitch/render/\n").unwrap();

        let store_dir = repo.join("app");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("regular"), "regular").unwrap();

        let target_dir = tmp.path().join("home").join("app");

        let state = format!(
            r#"
[stores.app]
target = "{}"
files = ["regular"]
"#,
            target_dir.to_string_lossy()
        );
        fs::write(stitch.join("state.toml"), &state).unwrap();

        let platform = Platform::detect();
        let loaded = Config::load(repo).unwrap();
        let store = loaded.config.stores.get("app").unwrap();
        crate::store::apply_store(
            repo,
            "app",
            store,
            &platform,
            &loaded.config.vars,
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut Vec::new(),
        );

        assert!(target_dir.join("regular").is_symlink());

        // Simulate a pre-remove hook (or an external process) that removes the
        // target symlink before the remove loop runs.
        fs::remove_file(target_dir.join("regular")).unwrap();

        // `remove` must still succeed and delete the state, not error with a
        // foreign-symlink conflict.
        cmd_remove(repo, "app", false, false).unwrap();

        let state_text = fs::read_to_string(stitch.join("state.toml")).unwrap();
        assert!(
            !state_text.contains("[stores.app]"),
            "state entry must be removed"
        );
    }

    // --- Direct-apply parse-then-restore TOCTOU regression ---
    //
    // These tests exercise the actual `cmd_apply` production handler (both
    // text and JSON paths) via a test-only seam that runs immediately after
    // `ConfigSnapshot::load`. The seam swaps the on-disk config from
    // malicious B (captured by the snapshot) to benign A before any
    // revalidation, proving that the pinned hash catches the swap and the
    // malicious per-store hook never runs.

    /// Shared setup: creates a repo with a store, a malicious per-store
    /// pre-hook in `stitch.toml` (config B), and state pointing at `~/.app`.
    /// Returns the paths needed by the test.
    struct SnapshotSwapSetup {
        root: std::path::PathBuf,
        authored_path: std::path::PathBuf,
        marker: std::path::PathBuf,
        home: std::path::PathBuf,
        _guard: crate::config::TestHomeGuard,
        _tmp: tempfile::TempDir,
    }

    fn setup_snapshot_swap_repo() -> SnapshotSwapSetup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let stitch = root.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(root.join(".gitignore"), ".stitch/render/\n").unwrap();

        let store_dir = root.join("app");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("file"), "contents").unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        // Keep the guard in the returned struct so it outlives the test body —
        // dropping it here would restore the real $HOME mid-test.
        let guard = crate::config::test_home_guard(home.clone());

        let marker = root.join("pwned");
        let malicious_authored = format!(
            "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
            marker.display()
        );
        let authored_path = root.join("stitch.toml");
        fs::write(&authored_path, &malicious_authored).unwrap();
        fs::write(
            stitch.join("state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n",
        )
        .unwrap();

        SnapshotSwapSetup {
            root,
            authored_path,
            marker,
            home,
            _guard: guard,
            _tmp: tmp,
        }
    }

    /// **Text path:** `cmd_apply` (json=false) must reject when the config is
    /// swapped from malicious B to benign A after snapshot capture. The
    /// malicious per-store hook must not run, and no target link must be
    /// created.
    #[test]
    fn cmd_apply_text_rejects_malicious_config_captured_then_restored() {
        let setup = setup_snapshot_swap_repo();
        let authored_path = setup.authored_path.clone();
        let marker = setup.marker.clone();
        let home = setup.home.clone();

        // Swap stitch.toml from B (malicious) to A (benign, empty) immediately
        // after the snapshot is captured.
        set_test_pause_after_snapshot(Some(Box::new(move || {
            fs::write(&authored_path, "").unwrap();
        })));

        let result = cmd_apply(
            &setup.root,
            &[],
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            false,
        );
        set_test_pause_after_snapshot(None);

        assert!(
            result.is_err(),
            "text apply must reject when config was swapped after snapshot capture"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("config changed during pre-apply hook"),
            "expected 'config changed during pre-apply hook', got: {err}"
        );

        // The malicious per-store hook must NOT have run.
        assert!(
            !marker.exists(),
            "malicious per-store hook must not run when config was captured as B \
             and restored to A before revalidation"
        );
        // No target link must have been created.
        assert!(
            !home.join(".app").exists(),
            "no target link should be created when apply rejects due to config hash mismatch"
        );
    }

    /// **JSON path:** `cmd_apply` (json=true) must reject when the config is
    /// swapped from malicious B to benign A after snapshot capture. The
    /// malicious per-store hook must not run, and no target link must be
    /// created.
    #[test]
    fn cmd_apply_json_rejects_malicious_config_captured_then_restored() {
        let setup = setup_snapshot_swap_repo();
        let authored_path = setup.authored_path.clone();
        let marker = setup.marker.clone();
        let home = setup.home.clone();

        set_test_pause_after_snapshot(Some(Box::new(move || {
            fs::write(&authored_path, "").unwrap();
        })));

        let result = cmd_apply(
            &setup.root,
            &[],
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            true,
        );
        set_test_pause_after_snapshot(None);

        assert!(
            result.is_err(),
            "json apply must reject when config was swapped after snapshot capture"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("config changed during pre-apply hook"),
            "expected 'config changed during pre-apply hook', got: {err}"
        );

        assert!(
            !marker.exists(),
            "malicious per-store hook must not run when config was captured as B \
             and restored to A before revalidation (json path)"
        );
        assert!(
            !home.join(".app").exists(),
            "no target link should be created when apply rejects due to config hash mismatch (json path)"
        );
    }

    /// **Positive counterpart (text):** when the config is stable (no swap),
    /// the per-store pre-hook must run and the target link must be created.
    #[test]
    fn cmd_apply_text_per_store_hook_runs_when_config_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let stitch = root.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(root.join(".gitignore"), ".stitch/render/\n").unwrap();

        let store_dir = root.join("app");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("file"), "contents").unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _guard = crate::config::test_home_guard(home.clone());

        let marker = root.join("hook_ran");
        fs::write(
            root.join("stitch.toml"),
            format!(
                "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::write(
            stitch.join("state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n",
        )
        .unwrap();

        // No seam callback — config stays stable.
        let result = cmd_apply(
            &root,
            &[],
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            false,
        );
        assert!(result.is_ok(), "stable apply should succeed: {:?}", result);

        assert!(marker.exists(), "per-store hook should have run");
        assert!(
            home.join(".app").is_dir(),
            "target directory should be created when config is stable"
        );
        assert!(
            home.join(".app").join("file").is_symlink(),
            "file symlink should be created when config is stable"
        );
    }

    /// **Positive counterpart (JSON):** when the config is stable (no swap),
    /// the per-store pre-hook must run and the target link must be created.
    #[test]
    fn cmd_apply_json_per_store_hook_runs_when_config_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let stitch = root.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(root.join(".gitignore"), ".stitch/render/\n").unwrap();

        let store_dir = root.join("app");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("file"), "contents").unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _guard = crate::config::test_home_guard(home.clone());

        let marker = root.join("hook_ran");
        fs::write(
            root.join("stitch.toml"),
            format!(
                "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::write(
            stitch.join("state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n",
        )
        .unwrap();

        let result = cmd_apply(
            &root,
            &[],
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            true,
        );
        assert!(
            result.is_ok(),
            "stable json apply should succeed: {:?}",
            result
        );

        assert!(
            marker.exists(),
            "per-store hook should have run (json path)"
        );
        assert!(
            home.join(".app").is_dir(),
            "target directory should be created when config is stable (json path)"
        );
        assert!(
            home.join(".app").join("file").is_symlink(),
            "file symlink should be created when config is stable (json path)"
        );
    }
}
