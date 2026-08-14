use super::common::{check_unknown_names, print_warnings};
use crate::config::Config;
use crate::error::StitchError;
use crate::plan_exec;
use crate::platform::Platform;
use crate::report;
use crate::store;

pub(crate) fn cmd_plan(
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
