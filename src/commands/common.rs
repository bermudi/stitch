use crate::ancestor::TargetAncestorRedirect;
use crate::config::{Config, Loaded, expand_home, find_root};
use crate::error::{FailureClass, StitchError};
use crate::plan;
use crate::store;
use std::collections::BTreeSet;

pub(crate) fn global_redirect_to_error(redirect: TargetAncestorRedirect) -> StitchError {
    match redirect {
        TargetAncestorRedirect::Symlinked { path, resolves_to } => {
            StitchError::conflict_foreign(path, resolves_to)
        }
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: Some(resolves_to),
        } => StitchError::conflict_foreign(path, Some(resolves_to)),
        TargetAncestorRedirect::Removed { path } => StitchError::internal(format!(
            "target ancestor {} was removed by the pre-apply hook",
            path.display()
        )),
        TargetAncestorRedirect::Redirected {
            path,
            resolves_to: None,
        } => StitchError::internal(format!(
            "target ancestor {} changed identity during the pre-apply hook",
            path.display()
        )),
    }
}

/// Print non-fatal load-time warnings (e.g. a stale v0.2 file alongside the new
/// format) to stderr. Each command calls this once after `Config::load`.
pub(crate) fn print_warnings(loaded: &Loaded) {
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
}

/// Clone the config and retain only the named stores. Used by commands that
/// need a filtered view for pre-apply checks (template gitignore, global hook
/// ancestor capture) without splitting the snapshot passed to the executor.
pub(crate) fn filter_config(config: &Config, only: &[String]) -> Config {
    let mut filtered = config.clone();
    if !only.is_empty() {
        filtered.stores.retain(|name, _| only.contains(name));
    }
    filtered
}

/// Validate that every name in `only` exists in the config. Returns an error
/// listing unknown names so a typo can't silently do nothing.
pub(crate) fn check_unknown_names(
    only: impl IntoIterator<Item = impl AsRef<str>>,
    config: &Config,
) -> Result<(), StitchError> {
    let unknown: Vec<_> = only
        .into_iter()
        .filter(|n| !config.stores.contains_key(n.as_ref()))
        .map(|n| n.as_ref().to_string())
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        let valid: Vec<_> = config.stores.keys().cloned().collect();
        Err(StitchError::unknown_store(unknown, valid))
    }
}

/// Build an apply error from the failure actions in a single store result.
pub(crate) fn apply_error_from_actions(actions: &[store::ApplyAction]) -> Option<StitchError> {
    let mut classes = BTreeSet::new();
    for action in actions {
        match action {
            store::ApplyAction::Conflict {
                resolves_to: Some(_),
                ..
            } => {
                classes.insert(FailureClass::ConflictForeign);
            }
            store::ApplyAction::Conflict {
                resolves_to: None, ..
            } => {
                classes.insert(FailureClass::ConflictReal);
            }
            store::ApplyAction::Error(e) => {
                classes.insert(e.class());
            }
            _ => {}
        }
    }
    if classes.is_empty() {
        None
    } else {
        Some(StitchError::apply(
            classes.into_iter().collect(),
            "apply reported conflicts or errors",
        ))
    }
}

pub(crate) fn add_error_from_action(action: &store::ApplyAction) -> StitchError {
    match action {
        store::ApplyAction::Conflict {
            target,
            resolves_to: Some(resolves_to),
        } => StitchError::conflict_foreign(target.clone(), Some(resolves_to.clone())),
        store::ApplyAction::Conflict {
            target,
            resolves_to: None,
        } => StitchError::conflict_real(target.clone()),
        store::ApplyAction::Error(error) => StitchError::internal(error.to_string()),
        _ => StitchError::internal("add target preflight failed"),
    }
}

/// Resolve the repo root.
///
/// Precedence: an explicit `--repo` override > the `STITCH_REPO` env var > an
/// upward walk from cwd looking for `.stitch/`. `init` is cwd-anchored and
/// does not call this. An override (flag or env) must point at a directory
/// that actually contains `.stitch/` — we don't trust a bare path, so a typo
/// can't silently operate on the wrong directory.
pub(crate) fn resolve_root(override_path: Option<&str>) -> Result<std::path::PathBuf, StitchError> {
    if let Some(p) = override_path {
        return resolve_override(p, "--repo");
    }
    if let Ok(p) = std::env::var("STITCH_REPO")
        && !p.is_empty()
    {
        return resolve_override(&p, "STITCH_REPO");
    }
    let cwd = std::env::current_dir()
        .map_err(|e| StitchError::io_context("getting current working directory", e))?;
    find_root(&cwd).ok_or_else(|| StitchError::repo_resolution("cwd", cwd))
}

/// Validate an explicit repo override (from `--repo` or `STITCH_REPO`):
/// expand `~`, require a `.stitch/` dir so a typo can't silently operate on
/// the wrong directory, and canonicalize when possible. `label` prefixes the
/// error so the user knows which override was bad.
fn resolve_override(path: &str, label: &str) -> Result<std::path::PathBuf, StitchError> {
    let root = expand_home(path).map_err(StitchError::from)?;
    if !root.join(".stitch").is_dir() {
        return Err(StitchError::repo_resolution(label, root));
    }
    Ok(root.canonicalize().unwrap_or(root))
}

pub(crate) fn plan_error(plan: &plan::Plan) -> StitchError {
    let mut classes = BTreeSet::new();
    for store in &plan.stores {
        for op in &store.ops {
            match op {
                plan::PlanOp::Conflict { resolves_to, .. } => {
                    if resolves_to.is_some() {
                        classes.insert(FailureClass::ConflictForeign);
                    } else {
                        classes.insert(FailureClass::ConflictReal);
                    }
                }
                plan::PlanOp::Error { class, .. } => {
                    if let Some(c) = FailureClass::from_id(class) {
                        classes.insert(c);
                    }
                }
                _ => {}
            }
        }
    }
    let conflicts = plan.summary.conflicts;
    let errors = plan.summary.errors;
    StitchError::apply(
        classes.into_iter().collect(),
        format!("{conflicts} conflict(s), {errors} error(s)"),
    )
}
