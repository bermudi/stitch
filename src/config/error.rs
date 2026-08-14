//! Config error type.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {1}: {0}")]
    Read(std::io::Error, PathBuf),
    #[error("could not parse config at {1}: {0}")]
    Parse(toml::de::Error, PathBuf),
    #[error("could not serialize config: {0}")]
    Serialize(toml::ser::Error),
    #[error("could not write config: {0}")]
    Write(std::io::Error, PathBuf),
    #[error(
        "replaced config at {1}, but could not sync its parent directory: {0}; the new state is visible but may not survive power loss"
    )]
    CommittedWrite(std::io::Error, PathBuf),
    #[error("{0}")]
    InvalidPath(String),
    /// Two active stores claim the same link path (or nested paths), so the
    /// desired state is self-contradictory and `apply` cannot converge.
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Home(String),
    /// A v0.2 single-file repo that has not been migrated. The message tells
    /// the user exactly how to upgrade.
    #[error(
        "v0.2 config found at {0} — run `stitch migrate` to split into stitch.toml + .stitch/state.toml"
    )]
    LegacyV02(PathBuf),
}

impl ConfigError {
    /// True when the rename completed and callers must retain the filesystem
    /// work described by the newly written config.
    pub fn write_committed(&self) -> bool {
        matches!(self, Self::CommittedWrite(_, _))
    }
}
