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
use std::collections::BTreeMap;

// ===========================================================================
// Authored — from stitch.toml. Read-only to the tool after `init`.
// ===========================================================================

/// Human-authored config: user variables and per-store behavior (filters,
/// hooks, ignore rules). Written once by `init` (static) or `migrate` (split
/// from v0.2); thereafter the tool never rewrites it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stores: BTreeMap<String, AuthoredStore>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// Decoupled sources (v0.14): target-relative name → repo-relative source.
    /// `files` stays sugar for "name is both target name and store-relative
    /// source"; a `sources` entry says "this target path's content lives
    /// elsewhere in the repo" — the hub fan-in without repo-internal alias
    /// symlinks. Sources may point into another store's directory or at a
    /// non-store path; they are validated at load (safe fragment, not under
    /// `.stitch/`, no symlinked component).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, String>,
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
    /// Per-target decoupled sources; see [`GeneratedStore::sources`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, String>,
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
    /// Decoupled sources, merged from the generated half; see
    /// [`GeneratedStore::sources`].
    pub sources: BTreeMap<String, String>,
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
    pub sources: BTreeMap<String, String>,
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
#[serde(deny_unknown_fields)]
pub struct WhenClause {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

impl WhenClause {
    pub fn is_default(&self) -> bool {
        self == &WhenClause::default()
    }

    /// Returns `true` if every clause in `whens` could all match a single
    /// platform simultaneously. This is the case iff, for every field, no two
    /// clauses supply distinct `Some` values.
    pub fn are_compatible(whens: &[&WhenClause]) -> bool {
        for i in 0..whens.len() {
            for j in (i + 1)..whens.len() {
                if !whens[i].is_compatible_with(whens[j]) {
                    return false;
                }
            }
        }
        true
    }

    fn is_compatible_with(&self, other: &WhenClause) -> bool {
        Self::field_compatible(self.os.as_deref(), other.os.as_deref())
            && Self::field_compatible(self.arch.as_deref(), other.arch.as_deref())
            && Self::field_compatible(self.distro.as_deref(), other.distro.as_deref())
            && Self::field_compatible(self.hostname.as_deref(), other.hostname.as_deref())
            && Self::field_compatible(self.shell.as_deref(), other.shell.as_deref())
    }

    fn field_compatible(a: Option<&str>, b: Option<&str>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    pub pre: Option<String>,
    pub post: Option<String>,
}

impl Hooks {
    pub fn is_default(&self) -> bool {
        self == &Hooks::default()
    }
}

/// Header prepended to every `state.toml`. Injected/stripped outside the TOML
/// data model because the `toml` crate does not round-trip comments.
pub(super) const STATE_HEADER: &str =
    "# Generated by stitch — do not hand-edit; use stitch commands.\n";

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

/// `skip_serializing_if` helper: skip a field when it equals its default.
fn skip_if_default<T: Default + PartialEq>(t: &T) -> bool {
    t == &T::default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_authored_config_rejects_unknown_root_key() {
        let err = toml::from_str::<AuthoredConfig>("unexpected = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn test_authored_store_rejects_unknown_key() {
        let err =
            toml::from_str::<AuthoredConfig>("[stores.nvim]\nignroe = [\"tmp\"]\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `ignroe`"));
    }

    #[test]
    fn test_authored_target_rejects_unknown_key() {
        let err =
            toml::from_str::<AuthoredConfig>("[stores.nvim.targets.laptop]\nignroe = [\"tmp\"]\n")
                .unwrap_err();
        assert!(err.to_string().contains("unknown field `ignroe`"));
    }

    #[test]
    fn test_hooks_reject_unknown_key() {
        let err = toml::from_str::<AuthoredConfig>("[stores.nvim.hooks]\nprer = \"echo typo\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("unknown field `prer`"));
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
    fn test_when_clause_rejects_unknown_field() {
        let err = toml::from_str::<WhenClause>("bogus_key = \"x\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field `bogus_key`"),
            "unknown WhenClause key must be rejected, got: {msg}"
        );
        assert!(
            msg.contains("expected one of `os`, `arch`, `distro`, `hostname`, `shell`"),
            "error should list the valid WhenClause fields, got: {msg}"
        );
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
                    sources: BTreeMap::new(),
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
}
