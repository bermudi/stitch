use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub vars: HashMap<String, String>,
    pub stores: HashMap<String, Store>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    pub target: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub when: WhenClause,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub targets: Vec<TargetEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetEntry {
    pub target: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub when: WhenClause,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WhenClause {
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub distro: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hooks {
    pub pre: Option<String>,
    pub post: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    WholeDir,
    File,
}

impl Store {
    pub fn mode(&self) -> StoreMode {
        if self.files.is_empty() && self.patterns.is_empty() {
            StoreMode::WholeDir
        } else {
            StoreMode::File
        }
    }

    pub fn is_multi_target(&self) -> bool {
        !self.targets.is_empty()
    }
}

impl Config {
    pub fn empty() -> Self {
        Self {
            vars: HashMap::new(),
            stores: HashMap::new(),
        }
    }

    pub fn load(repo_root: &Path) -> Result<Self, ConfigError> {
        let config_path = repo_root.join(".stitch").join("config.toml");
        if !config_path.exists() {
            return Err(ConfigError::NotFound(config_path));
        }
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| ConfigError::Read(e, config_path.clone()))?;
        let config: Config =
            toml::from_str(&contents).map_err(|e| ConfigError::Parse(e, config_path))?;
        Ok(config)
    }

    pub fn save(&self, repo_root: &Path) -> Result<(), ConfigError> {
        let config_dir = repo_root.join(".stitch");
        std::fs::create_dir_all(&config_dir)
            .map_err(|e| ConfigError::Write(e, config_dir.clone()))?;
        let config_path = config_dir.join("config.toml");
        let contents = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        std::fs::write(&config_path, contents).map_err(|e| ConfigError::Write(e, config_path))?;
        Ok(())
    }
}

/// Walk upward from `start` to find a directory containing `.stitch/`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };

    loop {
        if current.join(".stitch").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Expand `~` at the start of a path.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(rest)
    } else if path == "~" {
        dirs::home_dir().expect("could not determine home directory")
    } else {
        PathBuf::from(path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config not found at {0}")]
    NotFound(PathBuf),
    #[error("could not read config at {1}: {0}")]
    Read(std::io::Error, PathBuf),
    #[error("could not parse config at {1}: {0}")]
    Parse(toml::de::Error, PathBuf),
    #[error("could not serialize config: {0}")]
    Serialize(toml::ser::Error),
    #[error("could not write config: {0}")]
    Write(std::io::Error, PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(
            expand_home("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn test_find_root() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        std::fs::create_dir_all(&stitch_dir).unwrap();

        assert_eq!(find_root(tmp.path()), Some(tmp.path().to_path_buf()));

        let sub = tmp.path().join("some").join("nested").join("dir");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_root(&sub), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn test_config_roundtrip() {
        let config = Config {
            vars: HashMap::from([
                ("editor".into(), "nvim".into()),
                ("email".into(), "test@example.com".into()),
            ]),
            stores: HashMap::from([
                (
                    "nvim".into(),
                    Store {
                        target: Some("~/.config/nvim".into()),
                        files: vec![],
                        patterns: vec![],
                        ignore: vec![],
                        when: WhenClause::default(),
                        hooks: Hooks::default(),
                        targets: vec![],
                    },
                ),
                (
                    "shells".into(),
                    Store {
                        target: Some("~".into()),
                        files: vec![".bashrc".into(), ".zshrc".into()],
                        patterns: vec![],
                        ignore: vec![],
                        when: WhenClause {
                            os: Some("linux".into()),
                            ..Default::default()
                        },
                        hooks: Hooks::default(),
                        targets: vec![],
                    },
                ),
            ]),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.vars, parsed.vars);
        assert_eq!(config.stores.len(), parsed.stores.len());
        assert_eq!(parsed.stores["shells"].files, vec![".bashrc", ".zshrc"]);
        assert_eq!(parsed.stores["shells"].when.os.as_deref(), Some("linux"));
    }

    #[test]
    fn test_store_mode() {
        let whole = Store {
            target: Some("~/.config/nvim".into()),
            files: vec![],
            patterns: vec![],
            ignore: vec![],
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets: vec![],
        };
        assert_eq!(whole.mode(), StoreMode::WholeDir);

        let file_mode = Store {
            target: Some("~".into()),
            files: vec![".bashrc".into()],
            patterns: vec![],
            ignore: vec![],
            when: WhenClause::default(),
            hooks: Hooks::default(),
            targets: vec![],
        };
        assert_eq!(file_mode.mode(), StoreMode::File);
    }
}
