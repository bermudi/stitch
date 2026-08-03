//! Config types: authored (`stitch.toml`), generated (`.stitch/state.toml`),
//! and the load-time merged view.
//!
//! v0.3 splits human-authored config from tool-generated desired state so that
//! mutations to the link inventory never clobber the user's comments and
//! formatting. Authored content lives in `stitch.toml` (repo root); generated
//! content lives in `.stitch/state.toml`. After `init`, the tool never rewrites
//! the authored file — every mutation (`add`/`remove`) writes
//! `state.toml` only.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

// ===========================================================================
// Authored — from stitch.toml. Read-only to the tool after `init`.
// ===========================================================================

/// Human-authored config: user variables and per-store behavior (filters,
/// hooks, ignore rules). Written once by `init` (static) or `migrate` (split
/// from v0.2); thereafter the tool never rewrites it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthoredConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stores: BTreeMap<String, AuthoredStore>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthoredStore {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "skip_if_default")]
    pub when: WhenClause,
    #[serde(default, skip_serializing_if = "skip_if_default")]
    pub hooks: Hooks,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, AuthoredTarget>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthoredTarget {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "skip_if_default")]
    pub when: WhenClause,
}

// ===========================================================================
// Generated — from .stitch/state.toml. Tool-owned.
// ===========================================================================

/// Tool-generated desired state: the concrete link inventory. `add`/
/// `remove` are the only writers; `init`/`migrate` seed it. Serialized
/// deterministically (BTreeMap key order + sorted `files`/`patterns`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedState {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stores: BTreeMap<String, GeneratedStore>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedStore {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, GeneratedTarget>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedTarget {
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
}

// ===========================================================================
// Merged view — built at load, never serialized as one unit.
// ===========================================================================

/// The merged view of authored + generated halves, keyed by store name.
/// Read-only: callers (apply/status/doctor) read it; writers mutate
/// [`Loaded::generated`] then call [`GeneratedState::save`].
#[derive(Debug, Clone)]
pub struct Config {
    /// User variables from `stitch.toml`, carried through for the merged view.
    /// Consumed by the template engine (`{{ vars.key }}`) at apply/diff time.
    pub vars: BTreeMap<String, String>,
    pub stores: BTreeMap<String, Store>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub target: Option<String>,
    pub files: Vec<String>,
    pub patterns: Vec<String>,
    pub ignore: Vec<String>,
    pub when: WhenClause,
    pub hooks: Hooks,
    /// Name-keyed: the cross-file join key (target paths can collide across
    /// hosts, so the path cannot be the key).
    pub targets: BTreeMap<String, TargetEntry>,
}

#[derive(Debug, Clone)]
pub struct TargetEntry {
    pub target: String,
    pub files: Vec<String>,
    pub patterns: Vec<String>,
    pub ignore: Vec<String>,
    pub when: WhenClause,
}

/// The result of [`Config::load`]: both halves alongside the merged view.
///
/// Writers mutate `generated` then `save()`; readers use `config`; `warnings`
/// carries non-fatal load-time notices (e.g. a stale v0.2 file alongside the
/// new format).
#[derive(Debug)]
pub struct Loaded {
    /// Read-only to callers; carried for `doctor`'s orphaned-behavior check
    /// and future tooling. Never saved by the running commands.
    pub authored: AuthoredConfig,
    pub generated: GeneratedState,
    pub config: Config,
    pub warnings: Vec<String>,
}

// ===========================================================================
// Shared clause types
// ===========================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hooks {
    pub pre: Option<String>,
    pub post: Option<String>,
}

/// Header prepended to every `state.toml`. Injected/stripped outside the TOML
/// data model because the `toml` crate does not round-trip comments.
const STATE_HEADER: &str = "# Generated by stitch — do not hand-edit; use stitch commands.\n";

/// The static authored file written by `init`. Hand-written, never reserialized
/// — the tool does not rewrite `stitch.toml` after this.
pub const AUTHORED_TEMPLATE: &str = "\
# stitch — authored config. Edit freely; the tool never rewrites this.
# Fields: vars, and per-store behavior (when, hooks, ignore, targets).
# Link inventory (target, files, patterns) is tool-managed in .stitch/state.toml.
";

impl Store {
    pub fn is_multi_target(&self) -> bool {
        !self.targets.is_empty()
    }
}

impl Config {
    /// An empty merged config (for tests).
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            vars: BTreeMap::new(),
            stores: BTreeMap::new(),
        }
    }

    /// Load authored (`stitch.toml`) + generated (`.stitch/state.toml`), merge
    /// them by store name, and return both halves alongside the merged view.
    ///
    /// A missing authored file with a v0.2 `.stitch/config.toml` present is a
    /// hard error pointing at `migrate`. Missing individual files are legal
    /// (empty halves) — `init` may not have written state yet, or a store may
    /// have behavior but no links.
    pub fn load(repo_root: &Path) -> Result<Loaded, ConfigError> {
        let stitch_path = repo_root.join("stitch.toml");
        let state_path = repo_root.join(".stitch").join("state.toml");
        let legacy_path = repo_root.join(".stitch").join("config.toml");
        let mut warnings = Vec::new();

        // Item 5: a v0.2-only repo (legacy present, new format absent) is a
        // hard, actionable error. Do not parse the old file.
        if !stitch_path.exists() && legacy_path.exists() {
            return Err(ConfigError::LegacyV02(legacy_path));
        }
        // Both present (partial/aborted migrate): new format wins, but warn so
        // the user knows the legacy file is stale and removable.
        if stitch_path.exists() && legacy_path.exists() {
            warnings.push(format!(
                "found stale v0.2 config at {} — stitch.toml is in use; \
                 you can remove the old file",
                legacy_path.display()
            ));
        }

        // Authored half: missing file = empty authored.
        let authored = if stitch_path.exists() {
            let contents = std::fs::read_to_string(&stitch_path)
                .map_err(|e| ConfigError::Read(e, stitch_path.clone()))?;
            toml::from_str::<AuthoredConfig>(&contents)
                .map_err(|e| ConfigError::Parse(e, stitch_path))?
        } else {
            AuthoredConfig::default()
        };

        // Generated half: missing file = empty state.
        let generated = if state_path.exists() {
            let raw = std::fs::read_to_string(&state_path)
                .map_err(|e| ConfigError::Read(e, state_path.clone()))?;
            // Strip the known tool-owned header before parsing (it is not part
            // of the TOML data model). Absent/differing header → parse verbatim.
            let contents = raw.strip_prefix(STATE_HEADER).unwrap_or(&raw);
            toml::from_str::<GeneratedState>(contents)
                .map_err(|e| ConfigError::Parse(e, state_path))?
        } else {
            GeneratedState::default()
        };

        let (config, merge_warnings) = merge(&authored, &generated);
        warnings.extend(merge_warnings);
        config.validate()?;

        Ok(Loaded {
            authored,
            generated,
            config,
            warnings,
        })
    }

    /// Validate that no `files`/`patterns` fragment can escape its store or
    /// target dir. Operates on the merged view. Logic-unchanged from v0.2; the
    /// only mechanical delta is iterating a name-keyed map via `.values()`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, store) in &self.stores {
            validate_fragments(&store.files, &store.patterns, &format!("store '{name}'"))?;
            for te in store.targets.values() {
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

impl GeneratedState {
    /// Render the canonical `state.toml` content: sorted `files`/`patterns`
    /// (for stable git diffs) plus the tool-owned header.
    ///
    /// Pure: sorts on cloned collections so the caller's `self` is untouched.
    /// `state.toml` is tool-owned, so imposing a canonical order on write is
    /// the tool's prerogative and never destroys authored content — but the
    /// sort is a serialization concern, not a mutation of in-memory state, so
    /// it does not bleed into the caller's view (e.g. a `--dry-run` preview
    /// must not mutate the state that a subsequent real write would persist).
    fn render(&self) -> Result<String, ConfigError> {
        // Serialize through a sorted clone so ordering is canonical without
        // mutating self. toml::to_string_pretty over a BTreeMap emits keys in
        // sorted order; the Vec fields need an explicit sort.
        let mut sorted = self.clone();
        for store in sorted.stores.values_mut() {
            store.files.sort();
            store.patterns.sort();
            for target in store.targets.values_mut() {
                target.files.sort();
                target.patterns.sort();
            }
        }
        let body = toml::to_string_pretty(&sorted).map_err(ConfigError::Serialize)?;
        Ok(format!("{STATE_HEADER}{body}"))
    }

    /// Write `.stitch/state.toml` atomically via temp-file + rename + fsync.
    /// On interruption the original file is left intact.
    pub fn save(&self, repo_root: &Path) -> Result<(), ConfigError> {
        let contents = self.render()?;
        let state_path = repo_root.join(".stitch").join("state.toml");
        atomic_write(&state_path, &contents)
    }

    /// Render the canonical `state.toml` content to a string without writing.
    /// Used by `migrate --dry-run` to preview the planned file. Propagates
    /// serialization errors rather than silently printing an empty preview.
    pub fn render_for_display(&self) -> Result<String, ConfigError> {
        self.render()
    }
}

/// Atomically write `contents` to `path` via a temp file in the same directory
/// then rename. On Linux `rename(2)` is atomic for same-filesystem paths, so
/// the destination is never truncated or partially written, and the temp file
/// is fsynced before the rename so a crash leaves either the old file or the
/// fully-durable new one. The PID-suffixed temp name avoids collisions between
/// concurrent stitch processes (stitch is single-threaded, so within one
/// process there are no parallel writers). The temp file is cleaned up on any
/// error.
pub fn atomic_write(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e, dir.clone()))?;
    let prefix = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stitch".into());
    let tmp_path = dir.join(format!(".{prefix}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| ConfigError::Write(e, tmp_path.clone()))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| ConfigError::Write(e, tmp_path.clone()))?;
        f.sync_all()
            .map_err(|e| ConfigError::Write(e, tmp_path.clone()))?;
        std::fs::rename(&tmp_path, path).map_err(|e| ConfigError::Write(e, path.to_path_buf()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Merge authored + generated halves into the read-only view, returning any
/// non-fatal warnings (authored-only targets — behavior declared, no link).
fn merge(authored: &AuthoredConfig, generated: &GeneratedState) -> (Config, Vec<String>) {
    let mut warnings = Vec::new();
    let mut stores = BTreeMap::new();

    let names: BTreeSet<&String> = authored
        .stores
        .keys()
        .chain(generated.stores.keys())
        .collect();

    for name in names {
        let (store, store_warnings) =
            merge_store(name, authored.stores.get(name), generated.stores.get(name));
        warnings.extend(store_warnings);
        stores.insert(name.clone(), store);
    }

    let config = Config {
        vars: authored.vars.clone(),
        stores,
    };
    (config, warnings)
}

fn merge_store(
    name: &str,
    a: Option<&AuthoredStore>,
    g: Option<&GeneratedStore>,
) -> (Store, Vec<String>) {
    let mut warnings = Vec::new();
    let targets = merge_targets(
        name,
        a.map(|a| &a.targets),
        g.map(|g| &g.targets),
        &mut warnings,
    );

    let store = Store {
        target: g.and_then(|g| g.target.clone()),
        files: g.map(|g| g.files.clone()).unwrap_or_default(),
        patterns: g.map(|g| g.patterns.clone()).unwrap_or_default(),
        ignore: a.map(|a| a.ignore.clone()).unwrap_or_default(),
        when: a.map(|a| a.when.clone()).unwrap_or_default(),
        hooks: a.map(|a| a.hooks.clone()).unwrap_or_default(),
        targets,
    };
    (store, warnings)
}

/// Merge per-target maps keyed by name. An entry appears iff it has a
/// generated half (a link inventory). An authored-only target (behavior
/// declared, no inventory) is load-OK but contributes no link — it is skipped
/// and a warning is appended so `doctor` can surface it. A generated-only
/// target is legal with default behavior.
fn merge_targets(
    store_name: &str,
    authored: Option<&BTreeMap<String, AuthoredTarget>>,
    generated: Option<&BTreeMap<String, GeneratedTarget>>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, TargetEntry> {
    let mut result = BTreeMap::new();
    let a = authored.cloned().unwrap_or_default();
    let g = generated.cloned().unwrap_or_default();

    let names: BTreeSet<String> = a.keys().chain(g.keys()).cloned().collect();
    for tname in names {
        let at = a.get(&tname);
        match g.get(&tname) {
            Some(gt) => {
                result.insert(
                    tname.clone(),
                    TargetEntry {
                        target: gt.target.clone(),
                        files: gt.files.clone(),
                        patterns: gt.patterns.clone(),
                        ignore: at.map(|a| a.ignore.clone()).unwrap_or_default(),
                        when: at.map(|a| a.when.clone()).unwrap_or_default(),
                    },
                );
            }
            None => {
                // Authored-only target: behavior declared, no link inventory.
                warnings.push(format!(
                    "store '{store_name}' target '{tname}': behavior in stitch.toml but no \
                     target in state.toml (orphaned after rename?)"
                ));
                // Skip — contributes no link.
            }
        }
    }
    result
}

// ===========================================================================
// v0.2 migration
// ===========================================================================

/// Frozen v0.2 layout, used only by `migrate` (parse-only, never serialized).
/// Mirrors the pre-split `Config`/`Store`/`TargetEntry` shapes, including the
/// array-form `targets`.
#[derive(Debug, Deserialize)]
pub struct LegacyConfig {
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub stores: BTreeMap<String, LegacyStore>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyStore {
    #[serde(default)]
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
    pub targets: Vec<LegacyTargetEntry>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyTargetEntry {
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

/// Split a parsed v0.2 config into authored + generated halves per the
/// field-ownership table. Multi-target array entries get deterministic names
/// (hostname-first, else positional, with a collision suffix). A store/target
/// with no authored content is omitted from the authored half (keeps
/// `stitch.toml` signal, not noise); one with no inventory is omitted from the
/// generated half.
pub fn split_legacy(legacy: &LegacyConfig) -> (AuthoredConfig, GeneratedState) {
    let mut authored = AuthoredConfig {
        vars: legacy.vars.clone(),
        stores: BTreeMap::new(),
    };
    let mut generated = GeneratedState {
        stores: BTreeMap::new(),
    };

    for (name, lstore) in &legacy.stores {
        let (a_targets, g_targets) = split_legacy_targets(&lstore.targets);

        // Authored half: only stores with non-default behavior.
        let has_behavior = !lstore.ignore.is_empty()
            || lstore.when != WhenClause::default()
            || lstore.hooks != Hooks::default()
            || !a_targets.is_empty();
        if has_behavior {
            authored.stores.insert(
                name.clone(),
                AuthoredStore {
                    ignore: lstore.ignore.clone(),
                    when: lstore.when.clone(),
                    hooks: lstore.hooks.clone(),
                    targets: a_targets,
                },
            );
        }

        // Generated half: only stores with link inventory.
        let has_inventory = lstore.target.is_some()
            || !lstore.files.is_empty()
            || !lstore.patterns.is_empty()
            || !g_targets.is_empty();
        if has_inventory {
            generated.stores.insert(
                name.clone(),
                GeneratedStore {
                    target: lstore.target.clone(),
                    files: lstore.files.clone(),
                    patterns: lstore.patterns.clone(),
                    targets: g_targets,
                },
            );
        }
    }

    (authored, generated)
}

/// Name v0.2 array-form target entries and split into authored/generated maps.
/// Deterministic: hostname-first (meaningful to the user), else `target-{i}`
/// positional, with a `-N` suffix on collision so the result is always unique.
fn split_legacy_targets(
    legacy_targets: &[LegacyTargetEntry],
) -> (
    BTreeMap<String, AuthoredTarget>,
    BTreeMap<String, GeneratedTarget>,
) {
    let mut a_targets = BTreeMap::new();
    let mut g_targets = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for (i, lte) in legacy_targets.iter().enumerate() {
        let base = lte
            .when
            .hostname
            .clone()
            .unwrap_or_else(|| format!("target-{}", i + 1));
        let mut tname = base.clone();
        let mut n = 1;
        while seen.contains(&tname) {
            tname = format!("{base}-{n}");
            n += 1;
        }
        seen.insert(tname.clone());

        // Generated side always gets the entry (it carries the target path).
        g_targets.insert(
            tname.clone(),
            GeneratedTarget {
                target: lte.target.clone(),
                files: lte.files.clone(),
                patterns: lte.patterns.clone(),
            },
        );

        // Authored side only if this target declares behavior.
        let has_behavior = !lte.ignore.is_empty() || lte.when != WhenClause::default();
        if has_behavior {
            a_targets.insert(
                tname,
                AuthoredTarget {
                    ignore: lte.ignore.clone(),
                    when: lte.when.clone(),
                },
            );
        }
    }

    (a_targets, g_targets)
}

// ===========================================================================
// Shared helpers (byte-unchanged from v0.2)
// ===========================================================================

/// `skip_serializing_if` helper: skip a field when it equals its default.
fn skip_if_default<T: Default + PartialEq>(t: &T) -> bool {
    t == &T::default()
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
    let raw = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(rest)
    } else if path == "~" {
        dirs::home_dir().expect("could not determine home directory")
    } else {
        PathBuf::from(path)
    };
    // Strip trailing slashes: symlink(2) fails with ENOENT when the linkpath
    // has a trailing slash (the kernel treats it as "must resolve to a
    // directory", but the path doesn't exist yet when we're creating a link).
    // User input like `stitch add ~/.config/alacritty/` would otherwise fail
    // at the link step with a confusing rollback error.
    let mut s = raw.to_string_lossy().into_owned();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    PathBuf::from(s)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
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
    /// A v0.2 single-file repo that has not been migrated. The message tells
    /// the user exactly how to upgrade.
    #[error(
        "v0.2 config found at {0} — run `stitch migrate` to split into stitch.toml + .stitch/state.toml"
    )]
    LegacyV02(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- unchanged helpers ---

    #[test]
    fn test_expand_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(
            expand_home("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        // Trailing slashes are stripped — symlink(2) fails on a linkpath
        // with a trailing slash, so `stitch add ~/.config/foo/` must not
        // carry the slash through to the linker.
        assert_eq!(expand_home("~/foo/"), home.join("foo"));
        assert_eq!(expand_home("~/foo///"), home.join("foo"));
        assert_eq!(
            expand_home("/absolute/path/"),
            PathBuf::from("/absolute/path")
        );
        // Root stays root — the `len() > 1` guard prevents stripping "/" to "".
        assert_eq!(expand_home("/"), PathBuf::from("/"));
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

    // --- serde roundtrips (split halves, independently) ---

    #[test]
    fn test_authored_roundtrip() {
        let authored = AuthoredConfig {
            vars: BTreeMap::from([("editor".into(), "nvim".into())]),
            stores: BTreeMap::from([(
                "shells".into(),
                AuthoredStore {
                    ignore: vec!["*.bak".into()],
                    when: WhenClause {
                        os: Some("linux".into()),
                        ..Default::default()
                    },
                    hooks: Hooks::default(),
                    targets: BTreeMap::new(),
                },
            )]),
        };
        let toml_str = toml::to_string_pretty(&authored).unwrap();
        let parsed: AuthoredConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.vars, authored.vars);
        assert_eq!(parsed.stores["shells"].when.os.as_deref(), Some("linux"));
        assert_eq!(parsed.stores["shells"].ignore, vec!["*.bak"]);
    }

    #[test]
    fn test_generated_state_rejects_unknown_root_key() {
        let err = toml::from_str::<GeneratedState>("unexpected = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn test_generated_state_rejects_unknown_store_key() {
        let err =
            toml::from_str::<GeneratedState>("[stores.nvim]\nunexpected = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn test_generated_state_rejects_unknown_target_key() {
        let err = toml::from_str::<GeneratedState>(
            "[stores.nvim.targets.laptop]\ntarget = \"~/.config/nvim\"\nunexpected = true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn test_generated_roundtrip() {
        let generated = GeneratedState {
            stores: BTreeMap::from([(
                "nvim".into(),
                GeneratedStore {
                    target: Some("~/.config/nvim".into()),
                    files: vec!["init.lua".into()],
                    patterns: vec![],
                    targets: BTreeMap::new(),
                },
            )]),
        };
        let toml_str = toml::to_string_pretty(&generated).unwrap();
        let parsed: GeneratedState = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.stores["nvim"].target.as_deref(),
            Some("~/.config/nvim")
        );
        assert_eq!(parsed.stores["nvim"].files, vec!["init.lua"]);
    }

    #[test]
    fn test_state_header_roundtrip() {
        // save prepends the header; load strips it. A roundtrip preserves the
        // header presence and the content.
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        std::fs::create_dir_all(&stitch_dir).unwrap();
        let state = GeneratedState {
            stores: BTreeMap::from([(
                "nvim".into(),
                GeneratedStore {
                    target: Some("~/.config/nvim".into()),
                    files: vec![],
                    patterns: vec![],
                    targets: BTreeMap::new(),
                },
            )]),
        };
        state.save(tmp.path()).unwrap();

        let raw = std::fs::read_to_string(stitch_dir.join("state.toml")).unwrap();
        assert!(
            raw.starts_with(STATE_HEADER),
            "state.toml must start with the tool-owned header"
        );
        // Re-parsing via the same strip logic recovers the store.
        let body = raw.strip_prefix(STATE_HEADER).unwrap();
        let parsed: GeneratedState = toml::from_str(body).unwrap();
        assert_eq!(
            parsed.stores["nvim"].target.as_deref(),
            Some("~/.config/nvim")
        );
    }

    // --- merge semantics ---

    /// Helper: build a merged [`Store`] from authored + generated halves.
    fn merged_store(a: AuthoredStore, g: GeneratedStore) -> (Store, Vec<String>) {
        let at: BTreeMap<String, AuthoredTarget> = a.targets.clone();
        let gt: BTreeMap<String, GeneratedTarget> = g.targets.clone();
        let mut warnings = Vec::new();
        let targets = merge_targets("s", Some(&at), Some(&gt), &mut warnings);
        let store = Store {
            target: g.target,
            files: g.files,
            patterns: g.patterns,
            ignore: a.ignore,
            when: a.when,
            hooks: a.hooks,
            targets,
        };
        (store, warnings)
    }

    #[test]
    fn test_merge_both_halves() {
        let (store, warnings) = merged_store(
            AuthoredStore {
                ignore: vec!["*.bak".into()],
                when: WhenClause {
                    os: Some("linux".into()),
                    ..Default::default()
                },
                hooks: Hooks::default(),
                targets: BTreeMap::new(),
            },
            GeneratedStore {
                target: Some("~/.config/nvim".into()),
                files: vec!["init.lua".into()],
                patterns: vec![],
                targets: BTreeMap::new(),
            },
        );
        assert!(warnings.is_empty());
        assert_eq!(store.target.as_deref(), Some("~/.config/nvim"));
        assert_eq!(store.files, vec!["init.lua"]);
        assert_eq!(store.ignore, vec!["*.bak"]);
        assert_eq!(store.when.os.as_deref(), Some("linux"));
    }

    #[test]
    fn test_merge_generated_only_store_uses_default_behavior() {
        // Store in state.toml only → default when/ignore (legal per SPEC).
        let (store, warnings) = merged_store(
            AuthoredStore::default(),
            GeneratedStore {
                target: Some("~".into()),
                files: vec![".bashrc".into()],
                patterns: vec![],
                targets: BTreeMap::new(),
            },
        );
        assert!(warnings.is_empty());
        assert_eq!(store.when, WhenClause::default());
        assert!(store.ignore.is_empty());
    }

    #[test]
    fn test_merge_authored_only_store_has_no_target() {
        // Behavior declared, no inventory → target None (no link contributed).
        let (store, warnings) = merged_store(
            AuthoredStore {
                ignore: vec!["*.bak".into()],
                when: WhenClause::default(),
                hooks: Hooks::default(),
                targets: BTreeMap::new(),
            },
            GeneratedStore::default(),
        );
        assert!(warnings.is_empty());
        assert!(store.target.is_none());
        assert_eq!(store.ignore, vec!["*.bak"]);
    }

    #[test]
    fn test_merge_authored_only_target_is_skipped_with_warning() {
        // A target with behavior but no inventory: load-OK, contributes no
        // link, appended to warnings (doctor surfaces it).
        let at = BTreeMap::from([(
            "laptop".into(),
            AuthoredTarget {
                ignore: vec![],
                when: WhenClause {
                    hostname: Some("laptop".into()),
                    ..Default::default()
                },
            },
        )]);
        let gt: BTreeMap<String, GeneratedTarget> = BTreeMap::new();
        let mut warnings = Vec::new();
        let targets = merge_targets("helix", Some(&at), Some(&gt), &mut warnings);
        assert!(
            targets.is_empty(),
            "authored-only target contributes no link"
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("helix"));
        assert!(warnings[0].contains("laptop"));
    }

    #[test]
    fn test_merge_generated_only_target_uses_default_behavior() {
        // Name in state.toml, not in stitch.toml → legal, default when.
        let at: BTreeMap<String, AuthoredTarget> = BTreeMap::new();
        let gt = BTreeMap::from([(
            "server".into(),
            GeneratedTarget {
                target: "~/.config/h".into(),
                files: vec![],
                patterns: vec![],
            },
        )]);
        let mut warnings = Vec::new();
        let targets = merge_targets("helix", Some(&at), Some(&gt), &mut warnings);
        assert!(warnings.is_empty());
        assert_eq!(targets["server"].target, "~/.config/h");
        assert_eq!(targets["server"].when, WhenClause::default());
    }

    // --- load: v0.2 rejection + stale-config warning ---

    #[test]
    fn test_load_rejects_v02_only_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        std::fs::create_dir_all(&stitch_dir).unwrap();
        // v0.2 layout: only .stitch/config.toml, no stitch.toml.
        std::fs::write(stitch_dir.join("config.toml"), "vars = {}\n\n[stores]\n").unwrap();

        let err = Config::load(tmp.path()).unwrap_err();
        match err {
            ConfigError::LegacyV02(_) => {}
            other => panic!("expected LegacyV02, got {other:?}"),
        }
        assert!(err.to_string().contains("stitch migrate"));
    }

    #[test]
    fn test_load_both_present_uses_new_format_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        std::fs::create_dir_all(&stitch_dir).unwrap();
        std::fs::write(tmp.path().join("stitch.toml"), "").unwrap();
        std::fs::write(
            stitch_dir.join("state.toml"),
            "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
        )
        .unwrap();
        // Stale legacy file present alongside.
        std::fs::write(stitch_dir.join("config.toml"), "# old\n").unwrap();

        let loaded = Config::load(tmp.path()).unwrap();
        assert_eq!(
            loaded.config.stores["nvim"].target.as_deref(),
            Some("~/.config/nvim")
        );
        assert!(
            loaded.warnings.iter().any(|w| w.contains("stale v0.2")),
            "expected stale-config warning, got {:?}",
            loaded.warnings
        );
    }

    // --- migrate split ---

    #[test]
    fn test_split_legacy_flat_target() {
        let legacy = LegacyConfig {
            vars: BTreeMap::from([("editor".into(), "nvim".into())]),
            stores: BTreeMap::from([(
                "nvim".into(),
                LegacyStore {
                    target: Some("~/.config/nvim".into()),
                    files: vec![],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause::default(),
                    hooks: Hooks::default(),
                    targets: vec![],
                },
            )]),
        };
        let (authored, generated) = split_legacy(&legacy);
        // No behavior → authored store omitted; inventory → generated store present.
        assert!(authored.stores.is_empty());
        assert_eq!(authored.vars["editor"], "nvim");
        assert_eq!(
            generated.stores["nvim"].target.as_deref(),
            Some("~/.config/nvim")
        );
    }

    #[test]
    fn test_split_legacy_names_multi_target_by_hostname() {
        let legacy = LegacyConfig {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "helix".into(),
                LegacyStore {
                    target: None,
                    files: vec![],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause::default(),
                    hooks: Hooks::default(),
                    targets: vec![
                        LegacyTargetEntry {
                            target: "~/.config/h".into(),
                            files: vec![],
                            patterns: vec![],
                            ignore: vec![],
                            when: WhenClause {
                                hostname: Some("laptop".into()),
                                ..Default::default()
                            },
                        },
                        LegacyTargetEntry {
                            target: "~/.config/h".into(),
                            files: vec![],
                            patterns: vec![],
                            ignore: vec![],
                            when: WhenClause {
                                hostname: Some("server".into()),
                                ..Default::default()
                            },
                        },
                    ],
                },
            )]),
        };
        let (authored, generated) = split_legacy(&legacy);
        let names: Vec<&String> = generated.stores["helix"].targets.keys().collect();
        assert_eq!(names, vec![&"laptop".to_string(), &"server".to_string()]);
        assert_eq!(
            authored.stores["helix"].targets["laptop"]
                .when
                .hostname
                .as_deref(),
            Some("laptop")
        );
    }

    #[test]
    fn test_split_legacy_positional_name_fallback() {
        // No hostname → positional target-1; no behavior → authored side empty.
        let legacy = LegacyConfig {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "helix".into(),
                LegacyStore {
                    target: None,
                    files: vec![],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause::default(),
                    hooks: Hooks::default(),
                    targets: vec![LegacyTargetEntry {
                        target: "~/.config/h".into(),
                        files: vec![],
                        patterns: vec![],
                        ignore: vec![],
                        when: WhenClause::default(),
                    }],
                },
            )]),
        };
        let (authored, generated) = split_legacy(&legacy);
        assert!(authored.stores.is_empty());
        assert!(generated.stores["helix"].targets.contains_key("target-1"));
    }

    #[test]
    fn test_split_legacy_collision_suffix() {
        // Two entries with the same hostname must not collide; second gets -1.
        let legacy = LegacyConfig {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "helix".into(),
                LegacyStore {
                    target: None,
                    files: vec![],
                    patterns: vec![],
                    ignore: vec![],
                    when: WhenClause::default(),
                    hooks: Hooks::default(),
                    targets: vec![
                        LegacyTargetEntry {
                            target: "~/.config/h".into(),
                            files: vec![],
                            patterns: vec![],
                            ignore: vec![],
                            when: WhenClause {
                                hostname: Some("box".into()),
                                ..Default::default()
                            },
                        },
                        LegacyTargetEntry {
                            target: "~/.config/h2".into(),
                            files: vec![],
                            patterns: vec![],
                            ignore: vec![],
                            when: WhenClause {
                                hostname: Some("box".into()),
                                ..Default::default()
                            },
                        },
                    ],
                },
            )]),
        };
        let (_, generated) = split_legacy(&legacy);
        let keys: BTreeSet<&String> = generated.stores["helix"].targets.keys().collect();
        assert!(keys.contains(&&"box".to_string()));
        assert!(keys.contains(&&"box-1".to_string()));
    }

    // --- path-fragment validation (P1#6) — unchanged semantics ---

    #[test]
    fn test_is_safe_fragment() {
        assert!(is_safe_fragment(".bashrc"));
        assert!(is_safe_fragment("config/app.conf"));
        assert!(is_safe_fragment("./bashrc"));
        assert!(!is_safe_fragment(""));
        assert!(!is_safe_fragment("/"));
        assert!(!is_safe_fragment("/etc/passwd"));
        assert!(!is_safe_fragment(".."));
        assert!(!is_safe_fragment("../escape"));
        assert!(!is_safe_fragment("foo/../bar"));
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
                targets: BTreeMap::from([(
                    "t".into(),
                    TargetEntry {
                        target: "~/.config/x".into(),
                        files: vec!["../escape".into()],
                        patterns: vec![],
                        ignore: vec![],
                        when: WhenClause::default(),
                    },
                )]),
            },
        );
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("store 's' (target '~/.config/x')"));
    }

    #[test]
    fn test_validate_allows_nested_and_flat() {
        let config = config_with_files(vec!["config/app.conf".into(), ".bashrc".into()]);
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_empty_config_ok() {
        Config::empty().validate().unwrap();
    }

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
                targets: BTreeMap::new(),
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
                targets: BTreeMap::new(),
            },
        );
        config
    }
}
