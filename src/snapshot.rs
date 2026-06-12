use std::path::{Path, PathBuf};

/// File to snapshot: path on disk + the name to use inside the gist.
pub struct SnapshotFile {
    pub path: PathBuf,
    pub gist_name: String,
}

/// Ensure the repo has a snapshot gist. Reads the stored ID from
/// `.stitch/snapshot_gist`, or creates a new gist if none exists.
fn gist_id(root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let marker = root.join(".stitch").join("snapshot_gist");
    if marker.exists() {
        return Ok(std::fs::read_to_string(&marker)?.trim().to_string());
    }

    // Create a temp file with a placeholder so gh doesn't complain about empty content.
    let tmp_path = std::env::temp_dir().join(format!("stitch-placeholder-{}", std::process::id()));
    std::fs::write(&tmp_path, "stitch snapshot gist\n")?;

    let output = std::process::Command::new("gh")
        .args([
            "gist",
            "create",
            "--desc",
            "stitch snapshots (auto-created)",
            "-f",
            ".stitch-placeholder",
        ])
        .arg(&tmp_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_remove("GH_EDITOR")
        .output()?;

    let _ = std::fs::remove_file(&tmp_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh gist create failed: {stderr}").into());
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // URL is like https://gist.github.com/OWNER/ID or just the ID on some versions
    let id = url
        .rsplit('/')
        .next()
        .ok_or_else(|| format!("unexpected gist URL: {url}"))?
        .to_string();

    std::fs::write(&marker, &id)?;
    Ok(id)
}

/// Check that `gh` is available. Returns an error with install instructions if not.
pub fn ensure_gh() -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(
            "gh CLI not found. Install it: https://cli.github.com\n\
             Stitch requires gh to snapshot files before making changes."
                .into(),
        ),
    }
}

/// Snapshot files to the repo's gist. Each file is added under a unique name
/// derived from the operation tag and the original path.
///
/// Returns the gist URL.
pub fn snapshot(
    root: &Path,
    files: &[SnapshotFile],
) -> Result<String, Box<dyn std::error::Error>> {
    ensure_gh()?;
    let id = gist_id(root)?;

    for file in files {
        let status = std::process::Command::new("gh")
            .args([
                "gist",
                "edit",
                &id,
                "--add",
                &file.gist_name,
            ])
            .arg(&file.path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .env_remove("GH_EDITOR")
            .status()?;

        if !status.success() {
            return Err(format!(
                "failed to snapshot {} to gist {}",
                file.path.display(),
                id
            )
            .into());
        }
    }

    // Remove the placeholder on first real snapshot.
    let placeholder_key = ".stitch-placeholder";
    let _ = std::process::Command::new("gh")
        .args(["gist", "edit", &id, "--remove", placeholder_key])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env_remove("GH_EDITOR")
        .status();

    Ok(format!("https://gist.github.com/{id}"))
}

/// Build a gist-safe filename from a tag and an absolute path.
/// e.g. tag="adopt", path="/home/user/.bashrc" → "adopt/home/user/.bashrc"
/// Gist filenames can contain `/`, so we use the full path for uniqueness.
pub fn gist_filename(tag: &str, abs_path: &Path) -> String {
    // Strip leading / to avoid double-slash (e.g. "adopt//home/..." → "adopt/home/...")
    let path_str = abs_path.display().to_string();
    let trimmed = path_str.strip_prefix('/').unwrap_or(&path_str);
    format!("{}/{}", tag, trimmed)
}

/// Return the stored gist URL for this repo, if any.
pub fn gist_url(root: &Path) -> Option<String> {
    let marker = root.join(".stitch").join("snapshot_gist");
    let id = std::fs::read_to_string(marker).ok()?.trim().to_string();
    Some(format!("https://gist.github.com/{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gist_filename_basic() {
        let path = Path::new("/home/user/.bashrc");
        assert_eq!(gist_filename("adopt", path), "adopt/home/user/.bashrc");
    }

    #[test]
    fn gist_filename_uniqueness() {
        let a = Path::new("/home/user/.bashrc");
        let b = Path::new("/home/user/.config/nvim/init.lua");
        assert_ne!(
            gist_filename("apply", a),
            gist_filename("apply", b)
        );
    }
}
