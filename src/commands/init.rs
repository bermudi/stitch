use crate::config::{self, ConfigError};
use crate::error::StitchError;
use crate::render;

pub(crate) fn cmd_init() -> Result<(), StitchError> {
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

    // The per-host `flock` lock (`.stitch/state.lock`) is meaningless shared
    // across machines; ignore it from the start so a fresh repo never commits it.
    render::ensure_lock_gitignore(&cwd).map_err(|e| {
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
