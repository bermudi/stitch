use crate::error::StitchError;
use crate::{report, self_update};

pub(crate) fn cmd_self_update(check_only: bool, json: bool) -> Result<(), StitchError> {
    let data = self_update::run(check_only, !json).map_err(StitchError::self_update)?;
    if json {
        report::write("self-update", &data, Vec::new());
    } else {
        match data.status {
            "up-to-date" => println!("stitch v{} is already up to date.", data.current_version),
            "newer" => println!(
                "stitch v{} is newer than the latest published release (v{}); not downgrading.",
                data.current_version, data.latest_version
            ),
            "update-available" => println!(
                "Update available: stitch v{} → v{}",
                data.current_version, data.latest_version
            ),
            "updated" => println!(
                "Updated stitch v{} → v{} at {}",
                data.current_version,
                data.latest_version,
                data.installed_path.as_deref().unwrap_or("<unknown path>")
            ),
            status => {
                return Err(StitchError::internal(format!(
                    "unknown update status {status}"
                )));
            }
        }
    }
    Ok(())
}
