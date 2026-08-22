//! v0.2 migration: frozen v0.2 layout and the split into authored + generated
//! halves.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::error::ConfigError;
use super::paths::validate_store_names;
use super::types::{
    AuthoredConfig, AuthoredStore, AuthoredTarget, GeneratedState, GeneratedStore, GeneratedTarget,
    Hooks, WhenClause,
};

/// Frozen v0.2 layout, used only by `migrate` (parse-only, never serialized).
/// Mirrors the pre-split `Config`/`Store`/`TargetEntry` shapes, including the
/// array-form `targets`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyConfig {
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub stores: BTreeMap<String, LegacyStore>,
}

impl LegacyConfig {
    /// Validate legacy keys before splitting so migration never writes an
    /// invalid authored or generated config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_store_names(self.stores.keys(), "legacy config")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
                    sources: BTreeMap::new(),
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
                sources: BTreeMap::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
