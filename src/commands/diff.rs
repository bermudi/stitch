use super::common::{check_unknown_names, plan_error, print_warnings};
use crate::config::Config;
use crate::error::StitchError;
use crate::platform::Platform;
use crate::report;
use crate::store;

pub(crate) fn cmd_diff(
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
            let changes = crate::commands::apply::pending_change_count(&plan);
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

    crate::commands::apply::render_plan(&plan, true);

    if plan.summary.errors > 0 || plan.summary.conflicts > 0 {
        Err(plan_error(&plan))
    } else {
        let changes = crate::commands::apply::pending_change_count(&plan);
        if exit_code && changes > 0 {
            Err(StitchError::drift(changes))
        } else {
            Ok(())
        }
    }
}
