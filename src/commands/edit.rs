use super::common::print_warnings;
use crate::config::{self, Config};
use crate::error::StitchError;
use crate::render;

pub(crate) fn cmd_edit(
    root: &std::path::Path,
    entry: Option<&str>,
    print_path: bool,
) -> Result<(), StitchError> {
    let path = match entry {
        None => {
            let authored_path = root.join("stitch.toml");
            // Use symlink_metadata (not exists()) so a symlinked stitch.toml
            // is detected and rejected before the editor opens the external
            // file. validate_authored_file rejects symlinks, non-regular
            // files, and hard links; a missing file is reported as absent.
            config::validate_authored_file(&authored_path)?;
            match std::fs::symlink_metadata(&authored_path) {
                Ok(_) => authored_path,
                Err(_) => {
                    return Err(StitchError::internal(format!(
                        "{} does not exist — run `stitch init` first",
                        authored_path.display()
                    )));
                }
            }
        }
        Some(e) => {
            let loaded = Config::load(root)?;
            // Warnings go to stderr, so they don't corrupt the stdout path
            // emitted by --print-path. Keep them in both modes.
            print_warnings(&loaded);
            render::resolve_edit_source(root, &loaded.config, e).map_err(StitchError::internal)?
        }
    };

    if print_path {
        // Print the resolved repo source path and exit — no editor launched.
        // This is the agent-friendly path: the agent opens the file with its
        // own tools.
        println!("{}", path.display());
        return Ok(());
    }

    let editor = resolve_editor()?;
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|e| StitchError::internal(format!("could not run editor '{editor}': {e}")))?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(StitchError::internal(format!(
            "editor '{editor}' exited with status {code}"
        )));
    }
    Ok(())
}

fn resolve_editor() -> Result<String, StitchError> {
    for var in ["VISUAL", "EDITOR"] {
        if let Some(value) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
            return Ok(value);
        }
    }
    Ok("vi".into())
}
