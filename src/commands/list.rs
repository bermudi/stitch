use super::common::print_warnings;
use crate::config::Config;
use crate::error::StitchError;
use crate::report;

pub(crate) fn cmd_list(root: &std::path::Path, json: bool) -> Result<(), StitchError> {
    if json {
        return report::run_json("list", None, || {
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
