//! The v0.7 `Plan` type: an executable upgrade of `ApplyAction`.
//!
//! Each planned op carries the source path and the preconditions that must hold
//! for the op to be safe (M4: `apply --plan` uses these to replay verbatim).
//! For M3 the `Plan` is the shared struct consumed by both text and JSON
//! rendering so the two views cannot drift apart.

use crate::error::FailureClass;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The result of `store::compute_plan`: a list of per-store operations.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub stores: Vec<PlanStore>,
    pub summary: PlanSummary,
}

impl Plan {
    pub fn from_stores(stores: Vec<PlanStore>) -> Self {
        let summary = compute_summary(&stores);
        Self { stores, summary }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PlanSummary {
    pub created: usize,
    pub replaced: usize,
    pub backed_up: usize,
    pub removed: usize,
    pub content_changed: usize,
    pub already_linked: usize,
    pub conflicts: usize,
    pub errors: usize,
    pub skipped: usize,
}

fn compute_summary(stores: &[PlanStore]) -> PlanSummary {
    let mut summary = PlanSummary::default();
    for store in stores {
        for op in &store.ops {
            match op {
                PlanOp::CreateLink { .. } => summary.created += 1,
                PlanOp::ReplaceLink { .. } => summary.replaced += 1,
                PlanOp::BackupAndLink { .. } => summary.backed_up += 1,
                PlanOp::RemoveLink { .. } | PlanOp::RemoveStaged { .. } => summary.removed += 1,
                PlanOp::ContentChanged { .. } => summary.content_changed += 1,
                PlanOp::AlreadyLinked { .. } => summary.already_linked += 1,
                PlanOp::Conflict { .. } => summary.conflicts += 1,
                PlanOp::Error { .. } => summary.errors += 1,
                PlanOp::SkippedPlatform => summary.skipped += 1,
                PlanOp::StageRender { .. } => {}
            }
        }
    }
    summary
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanStore {
    pub store_name: String,
    pub ops: Vec<PlanOp>,
}

/// One executable op in a plan. The serialization shape is the JSON that
/// `apply --json` and `diff --json` emit, so text and JSON consume exactly the
/// same struct.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum PlanOp {
    /// Render a template to the staging tree. Not printed in text mode.
    StageRender {
        store: String,
        source_rel: String,
        source: String,
        staged: String,
        sha256: String,
    },

    CreateLink {
        target: String,
        source: String,
        requires: LinkRequires,
    },

    ReplaceLink {
        target: String,
        source: String,
        old_resolves_to: Option<String>,
        requires: LinkRequires,
    },

    BackupAndLink {
        target: String,
        source: String,
        backup: String,
        requires: LinkRequires,
    },

    RemoveLink {
        /// Store that discovered this stale link. This remains explicit even
        /// when `source` is unavailable, because multiple stores can share a
        /// target directory.
        store: String,
        target: String,
        source: Option<String>,
        requires: LinkRequires,
    },

    /// A stale rendered file that apply will remove after target cleanup.
    RemoveStaged {
        path: String,
    },

    ContentChanged {
        target: String,
        source: String,
        requires: LinkRequires,
    },

    AlreadyLinked {
        target: String,
        source: String,
        requires: LinkRequires,
    },

    Conflict {
        target: String,
        resolves_to: Option<String>,
    },

    SkippedPlatform,

    Error {
        message: String,
        class: String,
    },
}

#[allow(dead_code)]
impl PlanOp {
    pub fn target(&self) -> Option<&str> {
        match self {
            PlanOp::StageRender { .. } | PlanOp::RemoveStaged { .. } => None,
            PlanOp::CreateLink { target, .. }
            | PlanOp::ReplaceLink { target, .. }
            | PlanOp::BackupAndLink { target, .. }
            | PlanOp::RemoveLink { target, .. }
            | PlanOp::ContentChanged { target, .. }
            | PlanOp::AlreadyLinked { target, .. }
            | PlanOp::Conflict { target, .. } => Some(target),
            PlanOp::SkippedPlatform | PlanOp::Error { .. } => None,
        }
    }

    /// Human-readable target path for the text summary. `None` for non-target
    /// ops such as `SkippedPlatform` and `Error`.
    pub fn target_path(&self) -> Option<PathBuf> {
        self.target().map(PathBuf::from)
    }
}

/// Preconditions an op needs at exec time. Kept simple and explicit so M4 can
/// preflight and re-check each op.
#[derive(Debug, Clone, Serialize)]
pub struct LinkRequires {
    pub target: TargetState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<TargetState>,
}

impl LinkRequires {
    pub fn new(target: TargetState) -> Self {
        Self {
            target,
            backup: None,
        }
    }

    pub fn with_backup(target: TargetState, backup: TargetState) -> Self {
        Self {
            target,
            backup: Some(backup),
        }
    }
}

/// The expected state of a path when an op is executed. Used as the value of
/// `requires.target` / `requires.backup`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "target", content = "value", rename_all = "snake_case")]
pub enum TargetState {
    Absent,
    RealEntry,
    SymlinkTo(String),
    SymlinkIntoRepo,
}

/// Convert a `Path` to the string form used inside `Plan` (and serialized JSON).
pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Build an `Error` op from a typed `StitchError`, preserving its class id.
#[allow(dead_code)]
pub fn error_op(error: &crate::error::StitchError) -> PlanOp {
    PlanOp::Error {
        message: error.to_string(),
        class: error.class().id().to_string(),
    }
}

/// Build an `Error` op with a specific failure class.
#[allow(dead_code)]
pub fn error_op_class(class: FailureClass, message: impl Into<String>) -> PlanOp {
    PlanOp::Error {
        message: message.into(),
        class: class.id().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{FailureClass, StitchError};
    use serde_json::Value;

    fn requires_absent() -> LinkRequires {
        LinkRequires::new(TargetState::Absent)
    }

    fn store_with_ops(name: &str, ops: Vec<PlanOp>) -> PlanStore {
        PlanStore {
            store_name: name.to_string(),
            ops,
        }
    }

    #[test]
    fn plan_summary_counts_each_op_type() {
        let stores = vec![
            store_with_ops(
                "a",
                vec![
                    PlanOp::CreateLink {
                        target: "/t1".into(),
                        source: "/s1".into(),
                        requires: requires_absent(),
                    },
                    PlanOp::ReplaceLink {
                        target: "/t2".into(),
                        source: "/s2".into(),
                        old_resolves_to: None,
                        requires: requires_absent(),
                    },
                    PlanOp::BackupAndLink {
                        target: "/t3".into(),
                        source: "/s3".into(),
                        backup: "/t3.bak".into(),
                        requires: LinkRequires::with_backup(
                            TargetState::RealEntry,
                            TargetState::Absent,
                        ),
                    },
                    PlanOp::RemoveLink {
                        store: "a".into(),
                        target: "/t4".into(),
                        source: None,
                        requires: requires_absent(),
                    },
                    PlanOp::RemoveStaged {
                        path: "/staged".into(),
                    },
                ],
            ),
            store_with_ops(
                "b",
                vec![
                    PlanOp::ContentChanged {
                        target: "/t5".into(),
                        source: "/s5".into(),
                        requires: requires_absent(),
                    },
                    PlanOp::AlreadyLinked {
                        target: "/t6".into(),
                        source: "/s6".into(),
                        requires: requires_absent(),
                    },
                    PlanOp::Conflict {
                        target: "/t7".into(),
                        resolves_to: None,
                    },
                    PlanOp::Error {
                        message: "oops".into(),
                        class: "internal".into(),
                    },
                    PlanOp::SkippedPlatform,
                    PlanOp::StageRender {
                        store: "b".into(),
                        source_rel: "f.tmpl".into(),
                        source: "/repo/b/f.tmpl".into(),
                        staged: "/repo/.stitch/render/b/f".into(),
                        sha256: "abc".into(),
                    },
                ],
            ),
        ];
        let plan = Plan::from_stores(stores);
        assert_eq!(plan.summary.created, 1);
        assert_eq!(plan.summary.replaced, 1);
        assert_eq!(plan.summary.backed_up, 1);
        // RemoveLink + RemoveStaged both count as removed
        assert_eq!(plan.summary.removed, 2);
        assert_eq!(plan.summary.content_changed, 1);
        assert_eq!(plan.summary.already_linked, 1);
        assert_eq!(plan.summary.conflicts, 1);
        assert_eq!(plan.summary.errors, 1);
        assert_eq!(plan.summary.skipped, 1);
        // StageRender is not counted
        assert_eq!(
            plan.summary.created
                + plan.summary.replaced
                + plan.summary.backed_up
                + plan.summary.removed
                + plan.summary.content_changed
                + plan.summary.already_linked
                + plan.summary.conflicts
                + plan.summary.errors
                + plan.summary.skipped,
            10
        );
    }

    #[test]
    fn plan_summary_empty_is_all_zero() {
        let plan = Plan::from_stores(vec![]);
        assert_eq!(plan.summary.created, 0);
        assert_eq!(plan.summary.conflicts, 0);
        assert_eq!(plan.summary.errors, 0);
    }

    #[test]
    fn plan_summary_default_is_zero() {
        let s = PlanSummary::default();
        assert_eq!(s.created, 0);
        assert_eq!(s.skipped, 0);
    }

    #[test]
    fn plan_op_target_returns_correct_variant() {
        assert_eq!(
            PlanOp::CreateLink {
                target: "/a".into(),
                source: "/s".into(),
                requires: requires_absent(),
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::ReplaceLink {
                target: "/a".into(),
                source: "/s".into(),
                old_resolves_to: None,
                requires: requires_absent(),
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::BackupAndLink {
                target: "/a".into(),
                source: "/s".into(),
                backup: "/a.bak".into(),
                requires: requires_absent(),
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::RemoveLink {
                store: "s".into(),
                target: "/a".into(),
                source: None,
                requires: requires_absent(),
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::ContentChanged {
                target: "/a".into(),
                source: "/s".into(),
                requires: requires_absent(),
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::AlreadyLinked {
                target: "/a".into(),
                source: "/s".into(),
                requires: requires_absent(),
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::Conflict {
                target: "/a".into(),
                resolves_to: None,
            }
            .target(),
            Some("/a")
        );
        assert_eq!(
            PlanOp::StageRender {
                store: "s".into(),
                source_rel: "r".into(),
                source: "/s".into(),
                staged: "/st".into(),
                sha256: "h".into()
            }
            .target(),
            None
        );
        assert_eq!(PlanOp::RemoveStaged { path: "/p".into() }.target(), None);
        assert_eq!(PlanOp::SkippedPlatform.target(), None);
        assert_eq!(
            PlanOp::Error {
                message: "m".into(),
                class: "internal".into()
            }
            .target(),
            None
        );
    }

    #[test]
    fn plan_op_target_path_converts() {
        let op = PlanOp::CreateLink {
            target: "/tmp/a".into(),
            source: "/s".into(),
            requires: requires_absent(),
        };
        assert_eq!(
            op.target_path().unwrap(),
            std::path::PathBuf::from("/tmp/a")
        );
        assert!(PlanOp::SkippedPlatform.target_path().is_none());
    }

    #[test]
    fn link_requires_constructors() {
        let r = LinkRequires::new(TargetState::Absent);
        assert_eq!(r.target, TargetState::Absent);
        assert!(r.backup.is_none());
        let v: Value = serde_json::to_value(&r).unwrap();
        assert!(v.get("backup").is_none(), "backup omitted when None");

        let r2 = LinkRequires::with_backup(TargetState::RealEntry, TargetState::Absent);
        assert_eq!(r2.target, TargetState::RealEntry);
        assert_eq!(r2.backup, Some(TargetState::Absent));
        let v2: Value = serde_json::to_value(&r2).unwrap();
        assert_eq!(v2["backup"]["target"], "absent");
    }

    #[test]
    fn target_state_serializes_with_tag_and_value() {
        let cases = vec![
            (TargetState::Absent, serde_json::json!({"target":"absent"})),
            (
                TargetState::RealEntry,
                serde_json::json!({"target":"real_entry"}),
            ),
            (
                TargetState::SymlinkTo("/repo/a".into()),
                serde_json::json!({"target":"symlink_to","value":"/repo/a"}),
            ),
            (
                TargetState::SymlinkIntoRepo,
                serde_json::json!({"target":"symlink_into_repo"}),
            ),
        ];
        for (state, expected) in cases {
            let v = serde_json::to_value(&state).unwrap();
            assert_eq!(v, expected);
        }
    }

    #[test]
    fn path_to_string_roundtrips() {
        assert_eq!(path_to_string(std::path::Path::new("/a/b")), "/a/b");
        assert_eq!(path_to_string(std::path::Path::new("")), "");
    }

    #[test]
    fn error_op_preserves_class_id() {
        let err = StitchError::internal("boom");
        let op = error_op(&err);
        match op {
            PlanOp::Error { message, class } => {
                assert!(message.contains("boom"));
                assert_eq!(class, "internal");
            }
            _ => panic!("expected Error op"),
        }
    }

    #[test]
    fn error_op_class_uses_given_class() {
        let op = error_op_class(FailureClass::ConflictReal, "msg");
        match op {
            PlanOp::Error { message, class } => {
                assert_eq!(message, "msg");
                assert_eq!(class, "conflict-real");
            }
            _ => panic!("expected Error op"),
        }
    }

    #[test]
    fn plan_op_serializes_with_snake_case_action_tag() {
        let ops = vec![
            PlanOp::CreateLink {
                target: "/t".into(),
                source: "/s".into(),
                requires: requires_absent(),
            },
            PlanOp::Conflict {
                target: "/t".into(),
                resolves_to: None,
            },
            PlanOp::SkippedPlatform,
            PlanOp::Error {
                message: "m".into(),
                class: "internal".into(),
            },
        ];
        let variants = ["create_link", "conflict", "skipped_platform", "error"];
        for (op, expected_tag) in ops.into_iter().zip(variants) {
            let v = serde_json::to_value(&op).unwrap();
            assert_eq!(v["action"], expected_tag);
        }
    }

    fn assert_keys_eq(value: &Value, expected: &[&str]) {
        let obj = value.as_object().expect("expected object");
        let actual: std::collections::BTreeSet<String> = obj.keys().cloned().collect();
        let expected_set: std::collections::BTreeSet<String> =
            expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual, expected_set,
            "keys mismatch: got {actual:?} want {expected_set:?} full value={value}"
        );
    }

    #[test]
    fn plan_json_shape_is_stable() {
        let plan = Plan::from_stores(vec![store_with_ops(
            "git",
            vec![PlanOp::AlreadyLinked {
                target: "/home/.gitconfig".into(),
                source: "/repo/git/gitconfig".into(),
                requires: LinkRequires::new(TargetState::SymlinkTo("/repo/git/gitconfig".into())),
            }],
        )]);
        let v = serde_json::to_value(&plan).unwrap();
        assert_keys_eq(&v, &["stores", "summary"]);
        assert_keys_eq(
            &v["summary"],
            &[
                "already_linked",
                "backed_up",
                "conflicts",
                "content_changed",
                "created",
                "errors",
                "removed",
                "replaced",
                "skipped",
            ],
        );
        let store = &v["stores"][0];
        assert_keys_eq(store, &["ops", "store_name"]);
        let op = &store["ops"][0];
        assert_keys_eq(op, &["action", "requires", "source", "target"]);
        assert_eq!(op["action"], "already_linked");
        assert_eq!(op["requires"]["target"]["target"], "symlink_to");
        // LinkRequires with only target should omit backup
        assert!(op["requires"].get("backup").is_none());
    }

    #[test]
    fn plan_all_op_schemas_are_exact() {
        // Build one of each variant and assert their exact JSON keys.
        let ops = vec![
            (
                PlanOp::StageRender {
                    store: "s".into(),
                    source_rel: "a.tmpl".into(),
                    source: "/repo/s/a.tmpl".into(),
                    staged: "/repo/.stitch/render/s/a".into(),
                    sha256: "abc".into(),
                },
                vec![
                    "action",
                    "sha256",
                    "source",
                    "source_rel",
                    "staged",
                    "store",
                ],
            ),
            (
                PlanOp::CreateLink {
                    target: "/t".into(),
                    source: "/s".into(),
                    requires: LinkRequires::new(TargetState::Absent),
                },
                vec!["action", "requires", "source", "target"],
            ),
            (
                PlanOp::ReplaceLink {
                    target: "/t".into(),
                    source: "/s".into(),
                    old_resolves_to: Some("/old".into()),
                    requires: LinkRequires::new(TargetState::SymlinkIntoRepo),
                },
                vec!["action", "old_resolves_to", "requires", "source", "target"],
            ),
            (
                PlanOp::BackupAndLink {
                    target: "/t".into(),
                    source: "/s".into(),
                    backup: "/t.bak".into(),
                    requires: LinkRequires::with_backup(
                        TargetState::RealEntry,
                        TargetState::Absent,
                    ),
                },
                vec!["action", "backup", "requires", "source", "target"],
            ),
            (
                PlanOp::RemoveLink {
                    store: "s".into(),
                    target: "/t".into(),
                    source: None,
                    requires: LinkRequires::new(TargetState::SymlinkIntoRepo),
                },
                vec!["action", "requires", "source", "store", "target"],
            ),
            (
                PlanOp::RemoveStaged { path: "/p".into() },
                vec!["action", "path"],
            ),
            (
                PlanOp::ContentChanged {
                    target: "/t".into(),
                    source: "/s".into(),
                    requires: LinkRequires::new(TargetState::Absent),
                },
                vec!["action", "requires", "source", "target"],
            ),
            (
                PlanOp::AlreadyLinked {
                    target: "/t".into(),
                    source: "/s".into(),
                    requires: LinkRequires::new(TargetState::SymlinkTo("/s".into())),
                },
                vec!["action", "requires", "source", "target"],
            ),
            (
                PlanOp::Conflict {
                    target: "/t".into(),
                    resolves_to: Some("/r".into()),
                },
                vec!["action", "resolves_to", "target"],
            ),
            (PlanOp::SkippedPlatform, vec!["action"]),
            (
                PlanOp::Error {
                    message: "m".into(),
                    class: "internal".into(),
                },
                vec!["action", "class", "message"],
            ),
        ];
        for (op, expected_keys) in ops {
            let v = serde_json::to_value(&op).unwrap();
            assert_keys_eq(&v, &expected_keys);
        }
        // Variant with None optional should not emit the key
        let v = serde_json::to_value(&PlanOp::Conflict {
            target: "/t".into(),
            resolves_to: None,
        })
        .unwrap();
        // serde serializes Option::None as null for this field (no skip)
        assert!(v.get("resolves_to").is_some());
        assert!(v["resolves_to"].is_null());
        // ReplaceLink with None old_resolves_to serializes as null
        let v = serde_json::to_value(&PlanOp::ReplaceLink {
            target: "/t".into(),
            source: "/s".into(),
            old_resolves_to: None,
            requires: LinkRequires::new(TargetState::Absent),
        })
        .unwrap();
        assert!(v["old_resolves_to"].is_null());
    }
}
