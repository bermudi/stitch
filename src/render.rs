//! Template rendering and staging for v0.6.
//!
//! Detection is by `.tmpl` suffix only (no content sniffing). Rendered output
//! lands at `.stitch/render/<store>/...` inside the repo so `points_into_repo`,
//! `remove_link`, and `prune` keep working. All rendering is in-memory; staged
//! files are written atomically at mode `0600` under a `0700` directory.

use crate::config;
use crate::linker;
use crate::platform::Platform;
use minijinja::{AutoEscape, Environment, Error as MjError, ErrorKind as MjErrorKind};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Template source suffix. Sole detection signal — deterministic from the
/// directory entry alone.
pub const TMPL_SUFFIX: &str = ".tmpl";

/// Path fragment under the repo for staged renders.
pub const RENDER_DIR: &str = ".stitch/render";

/// Line that `init` appends to `.gitignore`; rendering requires it.
pub const RENDER_GITIGNORE_ENTRY: &str = ".stitch/render/";

const RENDER_DIR_MODE: u32 = 0o700;
const RENDER_FILE_MODE: u32 = 0o600;

/// Whether `name` (a path relative to a store, or a bare file name) is a
/// template source.
pub fn is_template(name: &str) -> bool {
    // Require a non-empty stem so a lone ".tmpl" is not treated as a template.
    name.len() > TMPL_SUFFIX.len() && name.ends_with(TMPL_SUFFIX)
}

/// Link name for a store entry: strip a trailing `.tmpl` if present.
///
/// Invariant: a store file name is never used directly as a link target — always
/// go through this (or [`resolve_entry`]).
pub fn link_name(source_name: &str) -> &str {
    source_name
        .strip_suffix(TMPL_SUFFIX)
        .filter(|stem| !stem.is_empty())
        .unwrap_or(source_name)
}

/// A resolved store entry: source path in the store + link name under the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntry {
    /// Path relative to the store directory (may end in `.tmpl`).
    pub source_rel: String,
    /// Path relative to the target directory (`.tmpl` stripped).
    pub link_rel: String,
    pub is_template: bool,
}

/// Shared resolution path used by apply/status/remove/diff/edit.
pub fn resolve_entry(source_rel: &str) -> ResolvedEntry {
    if is_template(source_rel) {
        ResolvedEntry {
            source_rel: source_rel.to_string(),
            link_rel: link_name(source_rel).to_string(),
            is_template: true,
        }
    } else {
        ResolvedEntry {
            source_rel: source_rel.to_string(),
            link_rel: source_rel.to_string(),
            is_template: false,
        }
    }
}

/// Reject stores that contain both `foo` and `foo.tmpl` (same link name).
pub fn check_name_collisions(source_names: &[String]) -> Result<(), String> {
    let mut by_link: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in source_names {
        by_link
            .entry(link_name(name).to_string())
            .or_default()
            .push(name.clone());
    }
    for (link, sources) in by_link {
        if sources.len() > 1 {
            return Err(format!(
                "name collision for '{link}': {} — remove or rename one",
                sources.join(", ")
            ));
        }
    }
    Ok(())
}

/// True if any file under `store_dir` has a `.tmpl` suffix (full-tree walk).
pub fn store_has_templates(store_dir: &Path) -> bool {
    walkdir::WalkDir::new(store_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .any(|e| e.file_name().to_str().map(is_template).unwrap_or(false))
}

/// Absolute path of the staged render for `store_name` / `link_rel`.
pub fn staging_path(repo_root: &Path, store_name: &str, link_rel: &str) -> PathBuf {
    repo_root.join(RENDER_DIR).join(store_name).join(link_rel)
}

/// Root of all staged renders: `repo/.stitch/render`.
pub fn render_root(repo_root: &Path) -> PathBuf {
    repo_root.join(RENDER_DIR)
}

/// Per-store staging directory.
pub fn store_render_dir(repo_root: &Path, store_name: &str) -> PathBuf {
    render_root(repo_root).join(store_name)
}

// ---------------------------------------------------------------------------
// .gitignore enforcement
// ---------------------------------------------------------------------------

/// Whether `.gitignore` text already ignores the render staging dir.
pub fn gitignore_has_render_entry(contents: &str) -> bool {
    contents.lines().any(|line| {
        let t = line.trim();
        // Exact entry, or broader ignores that already cover it.
        t == RENDER_GITIGNORE_ENTRY
            || t == ".stitch/render"
            || t == ".stitch/"
            || t == ".stitch"
            || t == "**/.stitch/render/"
            || t == "**/.stitch/render"
    })
}

/// Read the repo `.gitignore` (if any) and report whether the render entry is present.
pub fn repo_gitignore_covers_render(repo_root: &Path) -> bool {
    let path = repo_root.join(".gitignore");
    match std::fs::read_to_string(path) {
        Ok(contents) => gitignore_has_render_entry(&contents),
        // No .gitignore at all → not covered.
        Err(_) => false,
    }
}

/// Whether staging contains rendered output. The top-level render directory is
/// pre-created by `init`, so its existence alone does not imply template use.
pub fn has_staged_output(repo_root: &Path) -> bool {
    let root = render_root(repo_root);
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| entry.file_type().is_file())
}

/// Append `.stitch/render/` to the repo `.gitignore`, creating the file if needed.
/// Idempotent: a no-op when the entry is already present.
pub fn ensure_render_gitignore(repo_root: &Path) -> Result<(), std::io::Error> {
    let path = repo_root.join(".gitignore");
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        if gitignore_has_render_entry(&contents) {
            return Ok(());
        }
        let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
        use std::io::Write;
        if !contents.is_empty() && !contents.ends_with('\n') {
            writeln!(file)?;
        }
        writeln!(file, "{RENDER_GITIGNORE_ENTRY}")?;
    } else {
        std::fs::write(&path, format!("{RENDER_GITIGNORE_ENTRY}\n"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Ensure `dir` exists with mode `0700`. Creates parents as needed; chmods an
/// existing directory if its mode is wider than required.
pub fn ensure_render_dir(dir: &Path) -> Result<(), std::io::Error> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(RENDER_DIR_MODE)
            .create(dir)?;
    }
    // Always enforce mode — an existing dir may have been created with a
    // looser umask by a hand-mkdir or older stitch.
    let meta = std::fs::metadata(dir)?;
    let mut perms = meta.permissions();
    if perms.mode() & 0o777 != RENDER_DIR_MODE {
        perms.set_mode(RENDER_DIR_MODE);
        std::fs::set_permissions(dir, perms)?;
    }
    // Also lock down the top-level `.stitch/render` when we created a nested path.
    if let Some(parent) = dir.parent() {
        let root_name = parent.file_name().and_then(|n| n.to_str());
        if root_name == Some("render") {
            let meta = std::fs::metadata(parent)?;
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != RENDER_DIR_MODE {
                perms.set_mode(RENDER_DIR_MODE);
                std::fs::set_permissions(parent, perms)?;
            }
        }
    }
    Ok(())
}

/// Mode bits (low 9) of a path, if it exists.
pub fn path_mode(path: &Path) -> Option<u32> {
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

// ---------------------------------------------------------------------------
// Engine + context
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RenderCtx<'a> {
    os: &'a str,
    arch: &'a str,
    distro: Option<&'a str>,
    hostname: &'a str,
    shell: &'a str,
    vars: &'a BTreeMap<String, String>,
}

/// `{{ env("VAR") }}` / `{{ env("VAR", "default") }}`.
///
/// One-arg form hard-fails when unset (red line: no silent empty substitution).
/// Two-arg form supplies the default.
fn env_fn(name: String, default: Option<String>) -> Result<String, MjError> {
    match std::env::var(&name) {
        Ok(v) => Ok(v),
        Err(_) => match default {
            Some(d) => Ok(d),
            None => Err(MjError::new(
                MjErrorKind::InvalidOperation,
                format!("environment variable `{name}` is not set"),
            )),
        },
    }
}

fn make_env() -> Environment<'static> {
    let mut env = Environment::new();
    // Dotfiles are plaintext; never HTML-escape `&` → `&amp;`.
    env.set_auto_escape_callback(|_| AutoEscape::None);
    // Jinja's default strips one trailing newline from the template source.
    // Dotfiles almost always end in `\n`; stripping it rewrites the user's
    // file on every render. Keep it.
    env.set_keep_trailing_newline(true);
    env.add_function("env", env_fn);
    env
}

/// Render `source` (template text) with the given context.
///
/// `name` is the template identity used in error messages (typically the
/// store-relative path). Returns the rendered string; never writes to disk.
pub fn render_string(
    name: &str,
    source: &str,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut env = make_env();
    env.add_template(name, source)
        .map_err(|e| format_mj_error(name, &e))?;
    let tmpl = env
        .get_template(name)
        .map_err(|e| format_mj_error(name, &e))?;
    let ctx = RenderCtx {
        os: &platform.os,
        arch: &platform.arch,
        distro: platform.distro.as_deref(),
        hostname: &platform.hostname,
        shell: &platform.shell,
        vars,
    };
    tmpl.render(&ctx).map_err(|e| format_mj_error(name, &e))
}

fn format_mj_error(name: &str, err: &MjError) -> String {
    // Prefer the primary error's line; fall back to the whole Display.
    match err.line() {
        Some(line) => format!("template {name}:{line}: {err}"),
        None => format!("template {name}: {err}"),
    }
}

/// Read a `.tmpl` source file and render it in memory.
pub fn render_file(
    source_path: &Path,
    template_name: &str,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
) -> Result<String, String> {
    let source = std::fs::read_to_string(source_path)
        .map_err(|e| format!("could not read template {}: {e}", source_path.display()))?;
    render_string(template_name, &source, platform, vars)
}

// ---------------------------------------------------------------------------
// Staging writes + reconciliation
// ---------------------------------------------------------------------------

/// Atomically write `contents` to `path` at mode `0600` (tempfile + fsync + rename).
///
/// Unlike [`config::atomic_write`], the resulting file is always `0600` regardless
/// of umask — staged renders may hold secrets pulled via `env()` even in v0.6.
fn atomic_write_secure(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    ensure_render_dir(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let prefix = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stitch".into());
    let tmp_path = dir.join(format!(".{prefix}.{}.tmp", std::process::id()));

    let result = (|| -> Result<(), String> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true)
            .create(true)
            .truncate(true)
            .mode(RENDER_FILE_MODE);
        let mut f = opts
            .open(&tmp_path)
            .map_err(|e| format!("could not create {}: {e}", tmp_path.display()))?;
        use std::io::Write;
        f.write_all(contents.as_bytes())
            .map_err(|e| format!("could not write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("could not fsync {}: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| format!("could not rename into {}: {e}", path.display()))?;
        // rename(2) preserves the temp file's mode; re-chmod in case the
        // destination existed with different bits and the fs did something odd.
        let mut perms = std::fs::metadata(path)
            .map_err(|e| format!("could not stat {}: {e}", path.display()))?
            .permissions();
        if perms.mode() & 0o777 != RENDER_FILE_MODE {
            perms.set_mode(RENDER_FILE_MODE);
            std::fs::set_permissions(path, perms)
                .map_err(|e| format!("could not chmod {}: {e}", path.display()))?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Outcome of staging a rendered template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// Staged file was written (new or content changed).
    Written(PathBuf),
    /// Staged file already matched the fresh render — mtime preserved.
    Unchanged(PathBuf),
}

/// Render `source_path` in memory and stage it at the canonical path.
///
/// Hash-gated: skips the write when the existing staged content is identical,
/// so re-apply is cheap and does not bust mtimes. On render failure nothing is
/// written (and any prior staged file is left alone — the caller skips the link).
pub fn stage_template(
    repo_root: &Path,
    store_name: &str,
    source_rel: &str,
    source_path: &Path,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
) -> Result<StageOutcome, String> {
    let entry = resolve_entry(source_rel);
    if !entry.is_template {
        return Err(format!(
            "internal: stage_template called on non-template '{source_rel}'"
        ));
    }
    let rendered = render_file(source_path, source_rel, platform, vars)?;
    let dest = staging_path(repo_root, store_name, &entry.link_rel);

    if let Ok(existing) = std::fs::read_to_string(&dest)
        && existing == rendered
    {
        // Still enforce mode in case a hand-edit loosened it.
        if let Some(mode) = path_mode(&dest)
            && mode != RENDER_FILE_MODE
        {
            let mut perms = std::fs::metadata(&dest)
                .map_err(|e| format!("could not stat {}: {e}", dest.display()))?
                .permissions();
            perms.set_mode(RENDER_FILE_MODE);
            std::fs::set_permissions(&dest, perms)
                .map_err(|e| format!("could not chmod {}: {e}", dest.display()))?;
        }
        return Ok(StageOutcome::Unchanged(dest));
    }

    atomic_write_secure(&dest, &rendered)?;
    Ok(StageOutcome::Written(dest))
}

/// Fresh in-memory render compared against the staged file.
///
/// Used by `diff` (content dimension) and `doctor` (drift flag). Returns
/// `true` when the staged file is missing or differs from the fresh render.
pub fn staged_differs(
    repo_root: &Path,
    store_name: &str,
    source_rel: &str,
    source_path: &Path,
    platform: &Platform,
    vars: &BTreeMap<String, String>,
) -> Result<bool, String> {
    let entry = resolve_entry(source_rel);
    let rendered = render_file(source_path, source_rel, platform, vars)?;
    let dest = staging_path(repo_root, store_name, &entry.link_rel);
    match std::fs::read_to_string(&dest) {
        Ok(existing) => Ok(existing != rendered),
        Err(_) => Ok(true),
    }
}

/// Remove stale links beneath a file-mode target.
///
/// A whole-dir store containing a `.tmpl` is promoted to file mode. If a source
/// is later deleted or renamed, it disappears from the resolved entry set, but
/// its old target symlink is otherwise invisible to the next apply. Reconcile
/// those links before staging cleanup. Only links that point into this store or
/// its staging tree are candidates; `remove_link` performs the final
/// repo-ownership check immediately before unlinking, so foreign links are not
/// clobbered.
pub fn reconcile_store_links(
    target_path: &Path,
    repo_root: &Path,
    store_dir: &Path,
    store_name: &str,
    keep_link_rels: &BTreeSet<String>,
    dry_run: bool,
) -> Result<Vec<PathBuf>, String> {
    // This helper owns file-mode children, never the target root itself. Do
    // not follow a whole-directory link while a mode transition is pending.
    if target_path.is_symlink() || !target_path.is_dir() {
        return Ok(Vec::new());
    }

    let staging_dir = store_render_dir(repo_root, store_name);
    let mut stale = Vec::new();
    for entry in walkdir::WalkDir::new(target_path)
        .follow_links(false)
        .into_iter()
    {
        let entry = entry.map_err(|e| {
            format!(
                "could not scan target {} for stale links: {e}",
                target_path.display()
            )
        })?;
        if entry.depth() == 0 || !entry.file_type().is_symlink() {
            continue;
        }

        let path = entry.path();
        let Ok(rel) = path.strip_prefix(target_path) else {
            continue;
        };
        let rel = rel.to_string_lossy();
        if keep_link_rels.contains(rel.as_ref()) {
            continue;
        }

        // The first check is the invariant used by remove_link. The narrower
        // checks prevent this store from removing a link into another store
        // merely because it also lives under repo_root.
        if !linker::points_into_repo(path, repo_root)
            || (!linker::points_into(path, store_dir) && !linker::points_into(path, &staging_dir))
        {
            continue;
        }
        stale.push(path.to_path_buf());
    }

    let mut removed = Vec::new();
    for path in stale {
        if dry_run {
            removed.push(path);
            continue;
        }
        match linker::remove_link(&path, repo_root) {
            Ok(true) => removed.push(path),
            // A link may have been repointed between the scan and unlink. The
            // ownership guard intentionally turns that race into a no-op.
            Ok(false) => {}
            Err(e) => {
                return Err(format!(
                    "could not remove stale link {}: {e}",
                    path.display()
                ));
            }
        }
    }

    Ok(removed)
}

/// Remove staged renders under `store_name` whose link names are not in
/// `keep_link_rels`. Also drops empty parent directories left behind.
///
/// "Config is truth" applied to `.stitch/render/`: a deleted/renamed `.tmpl`
/// must not leave a frozen artifact that silently never updates.
pub fn reconcile_store_staging(
    repo_root: &Path,
    store_name: &str,
    keep_link_rels: &BTreeSet<String>,
) -> Result<(), String> {
    let dir = store_render_dir(repo_root, store_name);
    if !dir.exists() {
        return Ok(());
    }

    let mut to_remove: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(&dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        if !keep_link_rels.contains(rel_str.as_ref()) {
            to_remove.push(entry.path().to_path_buf());
        }
    }

    for path in to_remove {
        std::fs::remove_file(&path)
            .map_err(|e| format!("could not remove stale render {}: {e}", path.display()))?;
        // Best-effort: prune empty parents up to the store render dir.
        let mut parent = path.parent().map(|p| p.to_path_buf());
        while let Some(p) = parent {
            if p == dir {
                break;
            }
            match std::fs::remove_dir(&p) {
                Ok(()) => parent = p.parent().map(|x| x.to_path_buf()),
                Err(_) => break,
            }
        }
    }

    // If the store dir is now empty, remove it too.
    if dir
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(&dir);
    }

    Ok(())
}

/// Delete the entire staging tree for a store (used by `remove`).
pub fn remove_store_staging(repo_root: &Path, store_name: &str) -> Result<(), String> {
    let dir = store_render_dir(repo_root, store_name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("could not remove staging {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Resolve the repo source path for `edit <entry>`: never the staged render.
///
/// Addressing is config-based (works pre-apply):
/// 1. Exact store name → the store directory in the repo.
/// 2. Otherwise home-expand and reverse-match against configured targets.
pub fn resolve_edit_source(
    repo_root: &Path,
    config: &config::Config,
    entry: &str,
) -> Result<PathBuf, String> {
    // 1. Store name.
    if config.stores.contains_key(entry) {
        let store_dir = repo_root.join(entry);
        if !store_dir.exists() {
            return Err(format!(
                "store '{entry}' has no directory at {}",
                store_dir.display()
            ));
        }
        return Ok(store_dir);
    }

    // 2. Target path → owning store + file.
    let expanded = config::expand_home(entry);

    // If the path is a symlink, keep its own path for target-prefix matching.
    // Resolving through a repo-owned symlink (e.g. an already-linked dotfile)
    // would break the prefix match against the configured target path and land
    // us in the repo or the staging tree. For foreign symlinks we must not
    // silently resolve to a repo source.
    let expanded = match std::fs::symlink_metadata(&expanded) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if !linker::points_into_repo(&expanded, repo_root) {
                return Err(format!(
                    "'{entry}' is a foreign symlink and does not point into this repo"
                ));
            }
            expanded
        }
        _ if expanded.exists() => expanded.canonicalize().unwrap_or(expanded),
        _ => expanded,
    };

    for (name, store) in &config.stores {
        let store_dir = repo_root.join(name);
        if store.is_multi_target() {
            for te in store.targets.values() {
                if let Some(path) =
                    match_target_to_source(&store_dir, &config::expand_home(&te.target), &expanded)
                {
                    return Ok(path);
                }
            }
        } else if let Some(ref target_str) = store.target {
            let target_path = config::expand_home(target_str);
            if let Some(path) = match_target_to_source(&store_dir, &target_path, &expanded) {
                return Ok(path);
            }
        }
    }

    Err(format!(
        "could not resolve '{entry}' to a store or configured target — \
         pass a store name or a target path (e.g. ~/.gitconfig)"
    ))
}

/// If `expanded` is `target` or a path under it, return the corresponding repo
/// source. Prefer the `.tmpl` source when present; a plain-file/template pair
/// with the same link name is rejected during entry resolution.
fn match_target_to_source(
    store_dir: &Path,
    target_path: &Path,
    expanded: &Path,
) -> Option<PathBuf> {
    let target_norm = if target_path.exists() {
        target_path
            .canonicalize()
            .unwrap_or_else(|_| target_path.to_path_buf())
    } else {
        target_path.to_path_buf()
    };

    if expanded == target_norm || expanded == target_path {
        // Whole-dir target: open the store directory.
        return Some(store_dir.to_path_buf());
    }

    let rel = expanded
        .strip_prefix(&target_norm)
        .or_else(|_| expanded.strip_prefix(target_path))
        .ok()?;
    if rel.as_os_str().is_empty() {
        return Some(store_dir.to_path_buf());
    }
    let rel_str = rel.to_string_lossy();
    if !config::is_safe_fragment(&rel_str) {
        return None;
    }

    // Prefer the template source when present — that's what the user edits.
    let tmpl = store_dir.join(format!("{rel_str}{TMPL_SUFFIX}"));
    if tmpl.is_file() {
        return Some(tmpl);
    }
    let plain = store_dir.join(rel.as_os_str());
    if plain.exists() {
        return Some(plain);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_platform() -> Platform {
        Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: Some("arch".into()),
            hostname: "testhost".into(),
            shell: "zsh".into(),
        }
    }

    #[test]
    fn is_template_detects_suffix() {
        assert!(is_template("gitconfig.tmpl"));
        assert!(is_template("hooks/pre-commit.tmpl"));
        assert!(!is_template("gitconfig"));
        assert!(!is_template(".tmpl"));
        assert!(!is_template("tmpl"));
    }

    #[test]
    fn link_name_strips_suffix() {
        assert_eq!(link_name("gitconfig.tmpl"), "gitconfig");
        assert_eq!(link_name("hooks/pre-commit.tmpl"), "hooks/pre-commit");
        assert_eq!(link_name("gitconfig"), "gitconfig");
    }

    #[test]
    fn resolve_entry_template() {
        let e = resolve_entry("foo.tmpl");
        assert_eq!(e.source_rel, "foo.tmpl");
        assert_eq!(e.link_rel, "foo");
        assert!(e.is_template);
    }

    #[test]
    fn collision_detected() {
        let names = vec!["foo".into(), "foo.tmpl".into()];
        assert!(check_name_collisions(&names).is_err());
        assert!(check_name_collisions(&["foo.tmpl".into(), "bar".into()]).is_ok());
    }

    #[test]
    fn render_context_fields() {
        let p = test_platform();
        let vars = BTreeMap::from([("editor".into(), "nvim".into())]);
        let out = render_string(
            "t.tmpl",
            "os={{ os }} host={{ hostname }} ed={{ vars.editor }}",
            &p,
            &vars,
        )
        .unwrap();
        assert_eq!(out, "os=linux host=testhost ed=nvim");
    }

    #[test]
    fn absent_distro_renders_as_none() {
        let mut p = test_platform();
        p.distro = None;
        let out = render_string("t.tmpl", "{{ distro }}", &p, &BTreeMap::new()).unwrap();
        assert_eq!(out, "none");
        let fallback = render_string(
            "t.tmpl",
            r#"{{ distro or "unknown" }}"#,
            &p,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(fallback, "unknown");
    }

    #[test]
    fn env_one_arg_hard_fails() {
        // Pick a name vanishingly unlikely to be set.
        let p = test_platform();
        let vars = BTreeMap::new();
        let err = render_string(
            "t.tmpl",
            r#"{{ env("STITCH_TEST_UNSET_VAR_XYZ_999") }}"#,
            &p,
            &vars,
        )
        .unwrap_err();
        assert!(
            err.contains("STITCH_TEST_UNSET_VAR_XYZ_999"),
            "error should name the key: {err}"
        );
    }

    #[test]
    fn env_two_arg_default() {
        let p = test_platform();
        let vars = BTreeMap::new();
        let out = render_string(
            "t.tmpl",
            r#"{{ env("STITCH_TEST_UNSET_VAR_XYZ_999", "fallback") }}"#,
            &p,
            &vars,
        )
        .unwrap();
        assert_eq!(out, "fallback");
    }

    #[test]
    fn no_html_autoescape() {
        let p = test_platform();
        let vars = BTreeMap::from([("x".into(), "a & b".into())]);
        let out = render_string("t.tmpl", "{{ vars.x }}", &p, &vars).unwrap();
        assert_eq!(out, "a & b");
    }

    #[test]
    fn gitignore_detection() {
        assert!(gitignore_has_render_entry(".stitch/render/\n"));
        assert!(gitignore_has_render_entry("# comment\n.stitch/\n"));
        assert!(gitignore_has_render_entry(".stitch\n"));
        assert!(!gitignore_has_render_entry("target/\n*.bak\n"));
    }

    #[test]
    fn stage_is_hash_gated_and_secure() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("git");
        std::fs::create_dir_all(&store).unwrap();
        let src = store.join("gitconfig.tmpl");
        std::fs::write(&src, "host={{ hostname }}\n").unwrap();

        let p = test_platform();
        let vars = BTreeMap::new();

        let r1 = stage_template(repo, "git", "gitconfig.tmpl", &src, &p, &vars).unwrap();
        assert!(matches!(r1, StageOutcome::Written(_)));
        let dest = staging_path(repo, "git", "gitconfig");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "host=testhost\n");
        assert_eq!(path_mode(&dest), Some(0o600));
        assert_eq!(path_mode(&store_render_dir(repo, "git")), Some(0o700));

        // Second stage with same content → Unchanged.
        let r2 = stage_template(repo, "git", "gitconfig.tmpl", &src, &p, &vars).unwrap();
        assert!(matches!(r2, StageOutcome::Unchanged(_)));

        // Content change → Written again.
        std::fs::write(&src, "host={{ hostname }}!\n").unwrap();
        let r3 = stage_template(repo, "git", "gitconfig.tmpl", &src, &p, &vars).unwrap();
        assert!(matches!(r3, StageOutcome::Written(_)));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "host=testhost!\n");
    }

    #[test]
    fn reconcile_removes_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let keep = staging_path(repo, "git", "keep");
        let drop = staging_path(repo, "git", "drop");
        ensure_render_dir(keep.parent().unwrap()).unwrap();
        std::fs::write(&keep, "k").unwrap();
        std::fs::write(&drop, "d").unwrap();

        let mut keep_set = BTreeSet::new();
        keep_set.insert("keep".into());
        reconcile_store_staging(repo, "git", &keep_set).unwrap();

        assert!(keep.exists());
        assert!(!drop.exists());
    }

    #[test]
    fn ensure_gitignore_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        ensure_render_gitignore(repo).unwrap();
        let contents = std::fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert!(gitignore_has_render_entry(&contents));
        // Idempotent.
        ensure_render_gitignore(repo).unwrap();
        let contents2 = std::fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert_eq!(
            contents2.matches(RENDER_GITIGNORE_ENTRY).count(),
            1,
            "must not duplicate the entry"
        );
    }

    #[test]
    fn resolve_edit_source_finds_repo_owned_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("git");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("gitconfig"), "hello\n").unwrap();

        let target = repo.join("home");
        std::fs::create_dir_all(&target).unwrap();
        let link = target.join("gitconfig");
        std::os::unix::fs::symlink(store.join("gitconfig"), &link).unwrap();

        let config = config::Config {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "git".into(),
                config::Store {
                    target: Some(target.to_string_lossy().into_owned()),
                    files: vec!["gitconfig".into()],
                    patterns: vec![],
                    ignore: vec![],
                    when: config::WhenClause::default(),
                    hooks: config::Hooks::default(),
                    targets: BTreeMap::new(),
                },
            )]),
        };

        let resolved = resolve_edit_source(repo, &config, &link.to_string_lossy()).unwrap();
        assert_eq!(resolved, store.join("gitconfig"));
    }

    #[test]
    fn resolve_edit_source_rejects_foreign_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("git");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("gitconfig"), "hello\n").unwrap();

        let target = repo.join("home");
        std::fs::create_dir_all(&target).unwrap();

        // Place the foreign target outside the repo so the symlink is not repo-owned.
        let foreign_tmp = tempfile::tempdir().unwrap();
        let foreign = foreign_tmp.path().join("foreign");
        std::fs::write(&foreign, "not ours\n").unwrap();

        let link = target.join("gitconfig");
        std::os::unix::fs::symlink(&foreign, &link).unwrap();

        let config = config::Config {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "git".into(),
                config::Store {
                    target: Some(target.to_string_lossy().into_owned()),
                    files: vec!["gitconfig".into()],
                    patterns: vec![],
                    ignore: vec![],
                    when: config::WhenClause::default(),
                    hooks: config::Hooks::default(),
                    targets: BTreeMap::new(),
                },
            )]),
        };

        let err = resolve_edit_source(repo, &config, &link.to_string_lossy()).unwrap_err();
        assert!(
            err.contains("foreign"),
            "expected foreign-symlink error, got {err}"
        );
    }

    #[test]
    fn resolve_edit_source_rejects_target_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("git");
        std::fs::create_dir_all(&store).unwrap();

        let target = repo.join("home");
        std::fs::create_dir_all(&target).unwrap();

        let config = config::Config {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "git".into(),
                config::Store {
                    target: Some(target.to_string_lossy().into_owned()),
                    files: vec![],
                    patterns: vec![],
                    ignore: vec![],
                    when: config::WhenClause::default(),
                    hooks: config::Hooks::default(),
                    targets: BTreeMap::new(),
                },
            )]),
        };

        let entry = format!("{}/../../../outside", target.display());
        let result = resolve_edit_source(repo, &config, &entry);
        assert!(
            result.is_err(),
            "expected traversal to be rejected, got {result:?}"
        );
    }

    #[test]
    fn resolve_edit_source_rejects_dotdot_in_repo_owned_symlink_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("git");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("gitconfig"), "hello\n").unwrap();

        let target = repo.join("home");
        std::fs::create_dir_all(&target).unwrap();
        let sub = target.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let link = target.join("gitconfig");
        std::os::unix::fs::symlink(store.join("gitconfig"), &link).unwrap();

        let config = config::Config {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "git".into(),
                config::Store {
                    target: Some(target.to_string_lossy().into_owned()),
                    files: vec!["gitconfig".into()],
                    patterns: vec![],
                    ignore: vec![],
                    when: config::WhenClause::default(),
                    hooks: config::Hooks::default(),
                    targets: BTreeMap::new(),
                },
            )]),
        };

        // The path resolves to a repo-owned symlink, but the unnormalized form
        // contains parent-dir components and must be rejected.
        let entry = format!("{}/sub/../gitconfig", target.display());
        let result = resolve_edit_source(repo, &config, &entry);
        assert!(
            result.is_err(),
            "expected `..` in symlink path to be rejected, got {result:?}"
        );
    }

    #[test]
    fn resolve_edit_source_resolves_nested_file_under_target() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("nvim");
        let lua_dir = store.join("lua");
        std::fs::create_dir_all(&lua_dir).unwrap();
        std::fs::write(lua_dir.join("plugin.lua"), "--\n").unwrap();

        let target = repo.join("home").join(".config").join("nvim");
        std::fs::create_dir_all(&target).unwrap();

        let config = config::Config {
            vars: BTreeMap::new(),
            stores: BTreeMap::from([(
                "nvim".into(),
                config::Store {
                    target: Some(target.to_string_lossy().into_owned()),
                    files: vec!["lua/plugin.lua".into()],
                    patterns: vec![],
                    ignore: vec![],
                    when: config::WhenClause::default(),
                    hooks: config::Hooks::default(),
                    targets: BTreeMap::new(),
                },
            )]),
        };

        let entry = target.join("lua").join("plugin.lua");
        let resolved = resolve_edit_source(repo, &config, &entry.to_string_lossy()).unwrap();
        assert_eq!(resolved, lua_dir.join("plugin.lua"));
    }
}
