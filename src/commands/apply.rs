use super::common::{
    check_unknown_names, filter_config, global_redirect_to_error, plan_error, print_warnings,
};
use crate::ancestor::TargetAncestorSnapshot;
use crate::config::{self, Config, expand_home};
use crate::error::StitchError;
use crate::fsutil::{ensure_filesystem_identity, filesystem_identity};
use crate::hooks;
use crate::plan;
use crate::plan_exec::{self, PlanExecError, PlanFile, PlanFileOp};
use crate::platform::Platform;
use crate::render;
use crate::report;
use crate::safety;
use crate::store;
use std::collections::BTreeSet;

pub(crate) fn render_plan(plan: &plan::Plan, dry_run: bool) {
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

pub(crate) fn cmd_apply(
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

    // Reject two active stores claiming the same link path before any plan is
    // built: that config is self-contradictory, and `apply` would otherwise
    // report success while the filesystem never converges (`diff --exit-code`
    // alarms forever). Mirrors `doctor`'s duplicate-target check, but keyed on
    // the actual link paths so file-mode stores sharing a target directory with
    // disjoint files remain legitimate.
    let platform = Platform::detect();
    let collision_config = filter_config(&snapshot.loaded.config, only);
    store::check_link_path_collisions(root, &collision_config, &platform)
        .map_err(StitchError::config)?;

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
        return Err(plan_error(&plan, "apply"));
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

pub(crate) fn cmd_apply_plan(
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
                crate::audit::append_command_result(root, "apply", Err(&e.error));
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

    // Build the `desired` half of the composite envelope from the pre-apply
    // config. This is the host-evaluated merge — what the world should look
    // like — so the agent doesn't need a separate `explain` call. The same
    // `--only` filter that applies to the plan also applies here so composite
    // output is consistent.
    let desired = super::explain::build_explain_data(root, &filtered_config, &platform, false);

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

    // Build `result`: per-op execution outcome. On dry-run, `result` is null.
    let result = if opts.dry_run {
        None
    } else {
        Some(build_apply_result(&plan))
    };

    // Build `post_status`: re-run status for the applied stores after
    // execution. On dry-run this reflects pre-apply state (still useful —
    // it shows the agent what's already converged). Filtered to `--only`
    // so the composite output is consistent with the plan.
    let post_status = build_post_status(root, &filtered_config, &platform);

    let data = report::ApplyData {
        desired,
        plan: plan.clone(),
        result,
        post_status,
    };

    if plan.summary.errors > 0 || plan.summary.conflicts > 0 {
        let error = plan_error(&plan, command);
        crate::audit::append_command_result(root, command, Err(&error));
        report::write_data_error(command, data, &error, warnings);
    }

    report::write(command, data, warnings);
    Ok(())
}

/// Build the per-store execution result summary from the plan.
fn build_apply_result(plan: &plan::Plan) -> report::ApplyResult {
    let stores = plan
        .stores
        .iter()
        .map(|s| {
            let mut ok = 0;
            let mut conflicts = 0;
            let mut errors = 0;
            let mut skipped = 0;
            for op in &s.ops {
                match op {
                    plan::PlanOp::Conflict { .. } => conflicts += 1,
                    plan::PlanOp::Error { .. } => errors += 1,
                    plan::PlanOp::SkippedPlatform => skipped += 1,
                    _ => ok += 1,
                }
            }
            report::ApplyResultStore {
                store: s.store_name.clone(),
                ok,
                conflicts,
                errors,
                skipped,
            }
        })
        .collect();
    report::ApplyResult {
        ops_executed: plan.summary.created
            + plan.summary.replaced
            + plan.summary.backed_up
            + plan.summary.removed
            + plan.summary.content_changed
            + plan.summary.already_linked,
        ops_total: plan.summary.created
            + plan.summary.replaced
            + plan.summary.backed_up
            + plan.summary.removed
            + plan.summary.content_changed
            + plan.summary.already_linked
            + plan.summary.conflicts
            + plan.summary.errors
            + plan.summary.skipped,
        conflicts: plan.summary.conflicts,
        errors: plan.summary.errors,
        stores,
    }
}

/// Build `post_status` by re-running status for all stores after apply.
fn build_post_status(
    root: &std::path::Path,
    config: &Config,
    platform: &Platform,
) -> Vec<report::StatusRow> {
    let entries = store::status_all(root, config, platform);
    report::status(root, &entries)
}

pub(crate) fn pending_change_count(plan: &plan::Plan) -> usize {
    let summary = &plan.summary;
    summary.created
        + summary.replaced
        + summary.backed_up
        + summary.removed
        + summary.content_changed
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
    use crate::commands::add::cmd_add;
    use crate::commands::add::rollback_adopt_move;
    use crate::commands::migrate::cmd_migrate;
    use crate::commands::remove::cmd_remove;
    use crate::config::Config;
    use crate::fsutil::InodeIdentity;
    use crate::platform::Platform;
    use crate::store::ApplyOpts;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

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
            None,
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

        let metadata = fs::symlink_metadata(&adopted).unwrap();
        let error = rollback_adopt_move(
            &source,
            &store_dir,
            "original",
            false,
            InodeIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            },
            None,
            None,
            &[],
        )
        .unwrap_err();

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

    // --- Skipped stores must not run hooks (regression for v0.10.0 bug) ---
    //
    // `when` is the "leave this machine alone" switch. A store excluded by its
    // `when` clause used to still fire its pre/post hooks, running commands the
    // user deliberately gated off (e.g. `git config --global`, `systemctl`) with
    // no sign in the summary (which reports the store as skipped). The store is
    // gated on a hostname that can never match, so the test is deterministic on
    // any host platform.
    #[test]
    fn apply_skipped_store_does_not_run_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let stitch = root.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(root.join(".gitignore"), ".stitch/render/\n").unwrap();

        let store_dir = root.join("bashrc");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join(".bashrc"), "set -o vi\n").unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _guard = crate::config::test_home_guard(home.clone());

        let pre_marker = tmp.path().join("PRE_RAN");
        let post_marker = tmp.path().join("POST_RAN");
        fs::write(
            root.join("stitch.toml"),
            format!(
                "[stores.bashrc]\n\
                 hooks = {{ pre = \"touch {pre}\", post = \"touch {post}\" }}\n\
                 [stores.bashrc.when]\n\
                 hostname = \"__stitch_skip_never_matches__\"\n",
                pre = pre_marker.display(),
                post = post_marker.display(),
            ),
        )
        .unwrap();
        fs::write(
            stitch.join("state.toml"),
            "[stores.bashrc]\ntarget = \"~/.bashrc\"\nfiles = [\".bashrc\"]\n",
        )
        .unwrap();

        // Sanity: the store really is classified as skipped on this host, so
        // the marker assertions below prove the hooks were suppressed rather
        // than the store being a no-op for some other reason.
        let snap = crate::config::ConfigSnapshot::load(&root).unwrap();
        let platform = Platform::detect();
        let (plan, _) = store::apply_all(
            &root,
            &snap.loaded.config,
            None,
            &[],
            &platform,
            ApplyOpts {
                dry_run: true,
                force: false,
            },
        );
        assert_eq!(
            plan.summary.skipped, 1,
            "store must be classified as skipped for this assertion to be meaningful"
        );

        // Real apply through the production handler: hooks must not fire.
        cmd_apply(
            &root,
            &[],
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            false,
        )
        .expect("apply should succeed");
        assert!(
            !pre_marker.exists(),
            "pre hook must not run for a skipped store"
        );
        assert!(
            !post_marker.exists(),
            "post hook must not run for a skipped store"
        );
        // And the target link was never created — the skip suppressed linking too.
        assert!(
            !home.join(".bashrc").exists(),
            "skipped store must not link"
        );
    }

    #[test]
    fn skipped_store_hooks_still_run_when_store_is_active() {
        // Negative control for the fix: a store that IS active on this host
        // (no `when` gate) must still run its post hook. Guards against an
        // over-broad fix that suppresses hooks for all stores.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let stitch = root.join(".stitch");
        fs::create_dir_all(&stitch).unwrap();
        fs::write(root.join(".gitignore"), ".stitch/render/\n").unwrap();

        let store_dir = root.join("app");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("file"), "contents\n").unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _guard = crate::config::test_home_guard(home.clone());

        let marker = tmp.path().join("POST_RAN");
        fs::write(
            root.join("stitch.toml"),
            format!(
                "[stores.app]\nhooks = {{ post = \"touch {}\" }}\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::write(
            stitch.join("state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n",
        )
        .unwrap();

        cmd_apply(
            &root,
            &[],
            ApplyOpts {
                dry_run: false,
                force: false,
            },
            false,
        )
        .expect("apply should succeed");
        assert!(marker.exists(), "active store must still run its post hook");
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
