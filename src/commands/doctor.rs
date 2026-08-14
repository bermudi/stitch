use super::common::print_warnings;
use crate::config::Config;
use crate::error::StitchError;
use crate::platform::Platform;
use crate::report;
use crate::store;

pub(crate) fn cmd_doctor(root: &std::path::Path, json: bool) -> Result<(), StitchError> {
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
