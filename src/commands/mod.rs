mod common;

pub(crate) use common::{
    add_error_from_action, apply_error_from_actions, check_unknown_names, filter_config,
    global_redirect_to_error, plan_error, print_warnings, resolve_root,
};

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
    match command {
        cli::Commands::Init => {
            if json {
                return Err(StitchError::usage("--json is not supported for init"));
            }
            crate::cmd_init()
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
                crate::cmd_apply_plan(&root, &plan_file, dry_run, force, json)
            } else {
                crate::cmd_apply(&root, &only, store::ApplyOpts { dry_run, force }, json)
            }
        }
        cli::Commands::Plan { only, force } => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_plan(&root, &only, force, json)
        }
        cli::Commands::Status { name } => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_status(&root, &name, json)
        }
        cli::Commands::Diff {
            only,
            force,
            exit_code,
        } => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_diff(&root, &only, force, exit_code, json)
        }
        cli::Commands::List => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_list(&root, json)
        }
        cli::Commands::Add {
            path,
            name,
            files,
            patterns,
            file,
            to,
            dry_run,
        } => {
            if json && !dry_run {
                return Err(StitchError::usage(
                    "--json is not supported for add without --dry-run",
                ));
            }
            let root = match resolve_root(repo.as_deref()) {
                Ok(root) => root,
                Err(error) if json && dry_run => {
                    report::write_error("add", &error, Vec::new());
                    std::process::exit(error.exit_code());
                }
                Err(error) => return Err(error),
            };
            if json && dry_run {
                return crate::cmd_add_json(
                    &root,
                    &path,
                    &name,
                    &files,
                    &patterns,
                    file,
                    to.as_deref(),
                );
            }
            crate::cmd_add(
                &root,
                &path,
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
            if json && !dry_run {
                return Err(StitchError::usage(
                    "--json is not supported for remove without --dry-run",
                ));
            }
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_remove(&root, &name, dry_run, json)
        }
        cli::Commands::Edit { entry } => {
            if json {
                return Err(StitchError::usage("--json is not supported for edit"));
            }
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_edit(&root, entry.as_deref())
        }
        cli::Commands::Import { scan_dirs, dry_run } => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_import(&root, &scan_dirs, dry_run, json)
        }
        cli::Commands::Doctor => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_doctor(&root, json)
        }
        cli::Commands::Migrate { dry_run } => {
            if json && !dry_run {
                return Err(StitchError::usage(
                    "--json is not supported for migrate without --dry-run",
                ));
            }
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_migrate(&root, dry_run, json)
        }
        cli::Commands::Prune {
            scan_dirs,
            dry_run,
            yes,
        } => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_prune(&root, &scan_dirs, dry_run, yes, json)
        }
        cli::Commands::Render { spec } => {
            let root = resolve_root(repo.as_deref())?;
            crate::cmd_render(&root, &spec, json)
        }
    }
}
