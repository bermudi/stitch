use crate::config::{self, ConfigError};
use crate::error::StitchError;
use crate::report::{self, MigrateData};

pub(crate) fn cmd_migrate(
    root: &std::path::Path,
    dry_run: bool,
    json: bool,
) -> Result<(), StitchError> {
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
                        backup_path: None,
                        stores_split: 0,
                        comment_loss_note: None,
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
    // Use the full repo-aware validator so staging collisions and source
    // component checks are caught here, not on the next load.
    config::validate_merged_with_repo(&authored, &generated, root)?;

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

    let stores_split = legacy.stores.len();
    let comment_loss_note = Some(true);

    if dry_run {
        if json {
            let data = MigrateData {
                authored_path: Some(authored_path.to_string_lossy().into_owned()),
                authored: Some(authored_str),
                state_path: Some(state_path.to_string_lossy().into_owned()),
                state: Some(state_str),
                backup_path: None,
                stores_split,
                comment_loss_note,
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

    if json {
        let data = MigrateData {
            authored_path: Some(authored_path.to_string_lossy().into_owned()),
            authored: Some(authored_str),
            state_path: Some(state_path.to_string_lossy().into_owned()),
            state: Some(state_str),
            backup_path: Some(backup_path.to_string_lossy().into_owned()),
            stores_split,
            comment_loss_note,
        };
        let mut warnings = durability_warnings.clone();
        warnings.push(format!(
            "comments in the old config were not carried into stitch.toml \
             (structural conversion drops them). The original is preserved at {}",
            backup_path.display()
        ));
        if !durability_warnings.is_empty() {
            // Emit a single error envelope with the migrated data so JSON
            // consumers see exactly one envelope, not a success followed by
            // a failure.
            let error = StitchError::internal(format!(
                "migration completed, but its config directory could not be synced: {}",
                durability_warnings.join("; ")
            ));
            report::write_data_error("migrate", data, &error, warnings);
        } else {
            report::write("migrate", data, warnings);
        }
    } else {
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
    }
    if !durability_warnings.is_empty() {
        return Err(StitchError::internal(format!(
            "migration completed, but its config directory could not be synced: {}",
            durability_warnings.join("; ")
        )));
    }
    Ok(())
}
