use super::common::print_warnings;
use crate::config::{self, Config, ConfigError, expand_home};
use crate::error::StitchError;
use crate::linker;
use crate::platform::Platform;
use crate::report;
use crate::safety;
use crate::scan;

pub(crate) fn cmd_prune(
    root: &std::path::Path,
    scan_dirs: &[String],
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), StitchError> {
    if json {
        // --dry-run is non-mutating even with --yes: exclude it from audit
        // logging so dry runs don't produce audit entries.
        let audit_root = if yes && !dry_run { Some(root) } else { None };
        return report::run_json("prune", audit_root, || {
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
                crate::audit::append_command_result(root, "prune", Err(&error));
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

pub(crate) fn prune_roots(scan_dirs: &[String]) -> Result<Vec<scan::ScanRoot>, ConfigError> {
    if scan_dirs.is_empty() {
        scan::default_scan_dirs()
    } else {
        scan_dirs
            .iter()
            .map(|s| Ok(scan::ScanRoot::from(expand_home(s)?)))
            .collect()
    }
}
