pub(crate) mod add;
pub(crate) mod apply;
mod common;
pub(crate) mod diff;
pub(crate) mod doctor;
pub(crate) mod edit;
pub(crate) mod explain;
pub(crate) mod import;
pub(crate) mod init;
pub(crate) mod list;
pub(crate) mod log;
pub(crate) mod migrate;
pub(crate) mod plan;
pub(crate) mod prune;
pub(crate) mod remove;
pub(crate) mod render;
pub(crate) mod schema;
pub(crate) mod status;
pub(crate) mod why;

pub(crate) use common::resolve_root;

use crate::audit::AuditEntry;
use crate::cli;
use crate::error::StitchError;
use crate::report;
use crate::store;

pub(crate) fn command_name(command: &cli::Commands) -> &'static str {
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
        Commands::Explain { .. } => "explain",
        Commands::Schema => "schema",
        Commands::Why { .. } => "why",
        Commands::Log { .. } => "log",
    }
}

pub(crate) fn run(cli: cli::Cli) -> Result<(), StitchError> {
    // `init` is cwd-anchored: it creates a new repo in the current directory,
    // so it must not honor --repo/STITCH_REPO. Every other command resolves
    // the repo once here (flag > env > cwd walk) and receives `&root`.
    let cli::Cli {
        repo,
        json,
        command,
    } = cli;
    let command_name = command_name(&command).to_string();
    let is_mutation = is_mutation_command(&command);
    let result = dispatch(repo.as_deref(), json, command);

    // Audit-log mutating operations. Best-effort: a log write failure is a
    // warning, not a hard error, so the log never blocks a mutation.
    if is_mutation && let Some(root) = resolved_repo_for_audit(repo.as_deref()) {
        let (outcome, exit_class, exit_code) = match &result {
            Ok(()) => ("ok".to_string(), None, 0),
            Err(e) => (
                "error".to_string(),
                Some(e.class().id().to_string()),
                e.exit_code(),
            ),
        };
        let entry = AuditEntry {
            timestamp: now_iso8601(),
            command: command_name,
            store: None,
            target: None,
            outcome,
            exit_class,
            exit_code,
        };
        crate::audit::append(&root, &entry);
    }

    result
}

/// Whether a command mutates state and should be audit-logged.
fn is_mutation_command(command: &cli::Commands) -> bool {
    use cli::Commands;
    matches!(
        command,
        Commands::Apply { .. }
            | Commands::Add { .. }
            | Commands::Remove { .. }
            | Commands::Migrate { .. }
            | Commands::Import { .. }
            | Commands::Prune { yes: true, .. }
    )
}

/// Resolve the repo root for audit logging. Returns None for `init` (no repo
/// yet) and for resolution failures (the audit log must not block the error
/// path).
fn resolved_repo_for_audit(repo: Option<&str>) -> Option<std::path::PathBuf> {
    resolve_root(repo).ok()
}

fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple ISO 8601-ish timestamp without a chrono dependency. Format:
    // unix-seconds aren't ISO 8601, but a stable machine-readable timestamp
    // is what the audit log needs. Use a clear prefix so it's not mistaken
    // for ISO 8601.
    format!("unix:{secs}")
}

fn dispatch(repo: Option<&str>, json: bool, command: cli::Commands) -> Result<(), StitchError> {
    match command {
        cli::Commands::Init => {
            if json {
                return Err(StitchError::usage("--json is not supported for init"));
            }
            init::cmd_init()
        }
        cli::Commands::Apply {
            only,
            dry_run,
            force,
            plan,
        } => {
            let root = resolve_root(repo)?;
            if let Some(plan_file) = plan {
                if !only.is_empty() {
                    return Err(StitchError::usage("--plan is not compatible with --only"));
                }
                apply::cmd_apply_plan(&root, &plan_file, dry_run, force, json)
            } else {
                apply::cmd_apply(&root, &only, store::ApplyOpts { dry_run, force }, json)
            }
        }
        cli::Commands::Plan { only, force } => {
            let root = resolve_root(repo)?;
            plan::cmd_plan(&root, &only, force, json)
        }
        cli::Commands::Status { name } => {
            let root = resolve_root(repo)?;
            status::cmd_status(&root, &name, json)
        }
        cli::Commands::Diff {
            only,
            force,
            exit_code,
        } => {
            let root = resolve_root(repo)?;
            diff::cmd_diff(&root, &only, force, exit_code, json)
        }
        cli::Commands::List => {
            let root = resolve_root(repo)?;
            list::cmd_list(&root, json)
        }
        cli::Commands::Add {
            paths,
            name,
            files,
            patterns,
            file,
            to,
            dry_run,
        } => {
            let root = match resolve_root(repo) {
                Ok(root) => root,
                Err(error) if json => {
                    report::write_error("add", &error, Vec::new());
                    std::process::exit(error.exit_code());
                }
                Err(error) => return Err(error),
            };
            if paths.len() > 1 {
                // Bulk add: multiple paths, simple adds only. Per-store flags
                // (--name, --files, --patterns, --file, --to) are rejected.
                if name.is_some()
                    || !files.is_empty()
                    || !patterns.is_empty()
                    || file
                    || to.is_some()
                {
                    let error = StitchError::usage(
                        "--name, --files, --patterns, --file, and --to are not supported with multiple paths (bulk mode)",
                    );
                    if json {
                        report::write_error("add", &error, Vec::new());
                        std::process::exit(error.exit_code());
                    }
                    return Err(error);
                }
                return add::cmd_add_bulk(&root, &paths, dry_run, json);
            }
            let path = &paths[0];
            if json {
                return add::cmd_add_json(
                    &root,
                    path,
                    &name,
                    &files,
                    &patterns,
                    file,
                    to.as_deref(),
                    dry_run,
                );
            }
            add::cmd_add(
                &root,
                path,
                &name,
                &files,
                &patterns,
                file,
                to.as_deref(),
                dry_run,
                json,
            )
        }
        cli::Commands::Remove { name, dry_run } => {
            let root = resolve_root(repo)?;
            remove::cmd_remove(&root, &name, dry_run, json)
        }
        cli::Commands::Edit { entry, print_path } => {
            if json {
                return Err(StitchError::usage("--json is not supported for edit"));
            }
            let root = resolve_root(repo)?;
            edit::cmd_edit(&root, entry.as_deref(), print_path)
        }
        cli::Commands::Import { scan_dirs, dry_run } => {
            let root = resolve_root(repo)?;
            import::cmd_import(&root, &scan_dirs, dry_run, json)
        }
        cli::Commands::Doctor => {
            let root = resolve_root(repo)?;
            doctor::cmd_doctor(&root, json)
        }
        cli::Commands::Migrate { dry_run } => {
            let root = resolve_root(repo)?;
            migrate::cmd_migrate(&root, dry_run, json)
        }
        cli::Commands::Prune {
            scan_dirs,
            dry_run,
            yes,
        } => {
            let root = resolve_root(repo)?;
            prune::cmd_prune(&root, &scan_dirs, dry_run, yes, json)
        }
        cli::Commands::Render { spec } => {
            let root = resolve_root(repo)?;
            render::cmd_render(&root, &spec, json)
        }
        cli::Commands::Explain { active_only } => {
            let root = resolve_root(repo)?;
            explain::cmd_explain(&root, active_only, json)
        }
        cli::Commands::Schema => schema::cmd_schema(json),
        cli::Commands::Why { target } => {
            let root = resolve_root(repo)?;
            why::cmd_why(&root, &target, json)
        }
        cli::Commands::Log { limit } => {
            let root = resolve_root(repo)?;
            log::cmd_log(&root, limit, json)
        }
    }
}
