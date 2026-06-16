use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

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

impl Store {
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
        config.validate()?;
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

    /// Validate that no `files`/`patterns` fragment can escape its store or target dir.
    ///
    /// Every entry across all stores and target entries must be a relative path
    /// with no `..` component and no leading `/`. Entries are joined directly
    /// onto `store_dir`/`target_path` at apply time, so a `../` or absolute
    /// fragment would symlink outside the intended dirs. Checked once at load
    /// so a malformed (or malicious, shared) config fails loudly before any
    /// filesystem mutation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, store) in &self.stores {
            validate_fragments(&store.files, &store.patterns, &format!("store '{name}'"))?;
            for te in &store.targets {
                validate_fragments(
                    &te.files,
                    &te.patterns,
                    &format!("store '{name}' (target '{}')", te.target),
                )?;
            }
        }
        Ok(())
    }
}

/// Whether `fragment` is safe to join onto a store or target directory.
///
/// Safe means: non-empty, relative (no leading `/`), and containing no `..`
/// component. Nested paths like `config/app.conf` are allowed; a leading `./`
/// is harmless and accepted. The check is lexical — it inspects
/// [`Path::components`] without touching the filesystem, so it is TOCTOU-free
/// and accepts entries for files that do not exist yet.
pub fn is_safe_fragment(fragment: &str) -> bool {
    !fragment.is_empty()
        && Path::new(fragment)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Reject any `files`/`patterns` entry that is not a safe fragment.
///
/// `context` names where the entries came from (e.g. `store 'shells'`) so the
/// error points at the offending config section. Shared by [`Config::validate`]
/// (load-time, whole config) and `cmd_add` (before mutating the filesystem).
pub fn validate_fragments(
    files: &[String],
    patterns: &[String],
    context: &str,
) -> Result<(), ConfigError> {
    for f in files {
        if !is_safe_fragment(f) {
            return Err(ConfigError::InvalidPath(format!(
                "invalid file entry '{f}' in {context}: paths must be relative to the store and contain no '..' or leading '/'"
            )));
        }
    }
    for p in patterns {
        if !is_safe_fragment(p) {
            return Err(ConfigError::InvalidPath(format!(
                "invalid pattern '{p}' in {context}: patterns must be relative to the store and contain no '..' or leading '/'"
            )));
        }
    }
    Ok(())
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
    #[error("{0}")]
    InvalidPath(String),
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

    // --- path-fragment validation (P1#6) ---

    #[test]
    fn test_is_safe_fragment() {
        // Allowed: flat names, nested paths, and a harmless leading './'.
        assert!(is_safe_fragment(".bashrc"));
        assert!(is_safe_fragment(".zshrc"));
        assert!(is_safe_fragment("config/app.conf"));
        assert!(is_safe_fragment("deep/nested/path.conf"));
        assert!(is_safe_fragment("./bashrc"));

        // Rejected: empty, absolute, and any '..' component in any position.
        assert!(!is_safe_fragment(""));
        assert!(!is_safe_fragment("/"));
        assert!(!is_safe_fragment("/etc/passwd"));
        assert!(!is_safe_fragment(".."));
        assert!(!is_safe_fragment("../escape"));
        assert!(!is_safe_fragment("foo/../bar"));
        assert!(!is_safe_fragment("foo/../.."));
        assert!(!is_safe_fragment("ok/../../escape"));
    }

    #[test]
    fn test_validate_rejects_traversal_in_store_files() {
        let config = config_with_files(vec!["../escape".into()]);
        let err = config.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid file entry"), "got: {msg}");
        assert!(msg.contains("'../escape'"), "got: {msg}");
        assert!(msg.contains("store 's'"), "got: {msg}");
    }

    #[test]
    fn test_validate_rejects_absolute_in_store_files() {
        let config = config_with_files(vec!["/etc/passwd".into()]);
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("'/etc/passwd'"));
    }

    #[test]
    fn test_validate_rejects_bad_patterns() {
        let config = config_with_patterns(vec!["../**".into()]);
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("invalid pattern"));
        assert!(err.to_string().contains("'../**'"));
    }

    #[test]
    fn test_validate_rejects_target_entry_files() {
        let mut config = Config::empty();
        config.stores.insert(
            "s".into(),
            Store {
                target: None,
                files: vec![],
                patterns: vec![],
                ignore: vec![],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets: vec![TargetEntry {
                    target: "~/.config/x".into(),
                    files: vec!["../escape".into()],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause::default(),
                }],
            },
        );
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("store 's' (target '~/.config/x')"));
    }

    #[test]
    fn test_validate_allows_nested_and_flat() {
        // Nested relative paths are legitimate (SPEC's ignore examples use them).
        let config = config_with_files(vec!["config/app.conf".into(), ".bashrc".into()]);
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_empty_config_ok() {
        Config::empty().validate().unwrap();
    }

    /// Helper: a single-store config with the given file entries.
    fn config_with_files(files: Vec<String>) -> Config {
        let mut config = Config::empty();
        config.stores.insert(
            "s".into(),
            Store {
                target: Some("~".into()),
                files,
                patterns: vec![],
                ignore: vec![],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets: vec![],
            },
        );
        config
    }

    fn config_with_patterns(patterns: Vec<String>) -> Config {
        let mut config = Config::empty();
        config.stores.insert(
            "s".into(),
            Store {
                target: Some("~".into()),
                files: vec![],
                patterns,
                ignore: vec![],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets: vec![],
            },
        );
        config
    }
}
