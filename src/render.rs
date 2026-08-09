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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

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

/// Validated paths below `.stitch/render/<store>`.
///
/// `dirs` holds every directory from `.stitch` through the leaf's parent. It
/// lets each operation reject a pre-existing symlink or non-directory before
/// it reads or mutates a staged file.
struct StagedPaths {
    store_dir: PathBuf,
    dest: PathBuf,
    dirs: Vec<PathBuf>,
}

struct StoreStagingPaths {
    store_dir: PathBuf,
    dirs: Vec<PathBuf>,
}

fn check_staging_fragment(fragment: &str, kind: &str) -> Result<(), String> {
    if config::is_safe_fragment(fragment) {
        Ok(())
    } else {
        Err(format!("invalid staged {kind} '{fragment}'"))
    }
}

/// Append a checked relative fragment to a directory chain. Fragments have
/// already passed `is_safe_fragment`; matching components again keeps this
/// helper safe when it is reused outside config loading.
fn append_staging_dirs(
    current: &mut PathBuf,
    dirs: &mut Vec<PathBuf>,
    fragment: &str,
) -> Result<(), String> {
    for component in Path::new(fragment).components() {
        match component {
            Component::Normal(part) => {
                current.push(part);
                dirs.push(current.clone());
            }
            Component::CurDir => {}
            _ => return Err(format!("invalid staged path fragment '{fragment}'")),
        }
    }
    Ok(())
}

fn store_staging_paths(repo_root: &Path, store_name: &str) -> Result<StoreStagingPaths, String> {
    check_staging_fragment(store_name, "store name")?;

    let root = render_root(repo_root);
    let mut dirs = vec![repo_root.join(".stitch"), root.clone()];
    let mut store_dir = root;
    append_staging_dirs(&mut store_dir, &mut dirs, store_name)?;

    Ok(StoreStagingPaths { store_dir, dirs })
}

fn staged_paths(repo_root: &Path, store_name: &str, link_rel: &str) -> Result<StagedPaths, String> {
    check_staging_fragment(link_rel, "path")?;
    let store = store_staging_paths(repo_root, store_name)?;
    let mut dirs = store.dirs;
    let mut parent = store.store_dir.clone();
    let mut parts: Vec<_> = Path::new(link_rel)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_os_string()),
            Component::CurDir => None,
            _ => None,
        })
        .collect();
    let leaf = parts
        .pop()
        .ok_or_else(|| format!("invalid staged path '{link_rel}'"))?;
    for part in parts {
        parent.push(part);
        dirs.push(parent.clone());
    }
    let dest = parent.join(leaf);
    if !dest.starts_with(&store.store_dir) {
        return Err(format!(
            "staged path escapes render tree: {}",
            dest.display()
        ));
    }

    Ok(StagedPaths {
        store_dir: store.store_dir,
        dest,
        dirs,
    })
}

/// Inspect one staging directory without following it. `None` means it is
/// absent and the caller chose not to create it.
fn checked_render_dir(path: &Path, create: bool) -> Result<Option<std::fs::Metadata>, String> {
    loop {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "refusing symlinked render ancestor {}",
                    path.display()
                ));
            }
            Ok(meta) if !meta.file_type().is_dir() => {
                return Err(format!(
                    "render ancestor {} is not a directory",
                    path.display()
                ));
            }
            Ok(meta) => return Ok(Some(meta)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(format!(
                        "could not create render directory {}: {e}",
                        path.display()
                    ));
                }
            },
            Err(e) => {
                return Err(format!(
                    "could not inspect render directory {}: {e}",
                    path.display()
                ));
            }
        }
    }
}

/// Validate every existing render ancestor, optionally creating missing ones.
/// Directories at and below `.stitch/render` are tightened to `0700`; `.stitch`
/// itself belongs to the rest of stitch's state and is only type-checked here.
fn checked_render_dirs(dirs: &[PathBuf], create: bool) -> Result<bool, String> {
    for (index, dir) in dirs.iter().enumerate() {
        let Some(meta) = checked_render_dir(dir, create)? else {
            return Ok(false);
        };
        if create && index != 0 && meta.permissions().mode() & 0o777 != RENDER_DIR_MODE {
            let mut perms = meta.permissions();
            perms.set_mode(RENDER_DIR_MODE);
            std::fs::set_permissions(dir, perms)
                .map_err(|e| format!("could not chmod render directory {}: {e}", dir.display()))?;
        }
    }
    Ok(true)
}

/// Pre-create the render root safely for `init`.
pub fn ensure_render_root(repo_root: &Path) -> Result<(), String> {
    let dirs = vec![repo_root.join(".stitch"), render_root(repo_root)];
    let _ = checked_render_dirs(&dirs, true)?;
    Ok(())
}

/// Return a staged leaf's metadata without following a symlink. A staged
/// render is always a regular file; rejecting every other type ensures reads
/// can never block on a FIFO or device.
fn staged_leaf_metadata(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(format!("refusing symlinked staged file {}", path.display()))
        }
        Ok(meta) if !meta.file_type().is_file() => Err(format!(
            "staged file {} is not a regular file",
            path.display()
        )),
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "could not inspect staged file {}: {e}",
            path.display()
        )),
    }
}

/// Read a staged regular file after validating its confined directory chain.
fn read_staged_file(paths: &StagedPaths) -> Result<Option<(String, std::fs::Metadata)>, String> {
    if !checked_render_dirs(&paths.dirs, false)? {
        return Ok(None);
    }
    let Some(meta) = staged_leaf_metadata(&paths.dest)? else {
        return Ok(None);
    };
    let contents = std::fs::read_to_string(&paths.dest)
        .map_err(|e| format!("could not read staged file {}: {e}", paths.dest.display()))?;
    Ok(Some((contents, meta)))
}

const TEMP_NAME_ATTEMPTS: usize = 16;

/// A short, unpredictable file name generated from Linux's kernel RNG. The
/// fixed prefix and 128-bit suffix keep names well under `NAME_MAX`.
fn random_temp_name() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: `bytes[filled..]` is a writable buffer of the exact length
        // passed to the Linux `getrandom(2)` syscall.
        let count = unsafe {
            libc::getrandom(
                bytes[filled..].as_mut_ptr().cast(),
                bytes.len() - filled,
                libc::GRND_NONBLOCK,
            )
        };
        if count > 0 {
            filled += count as usize;
            continue;
        }
        if count == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        return Err(format!(
            "could not obtain random name for staged render: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(format!(
        ".stitch-render-{:032x}.tmp",
        u128::from_le_bytes(bytes)
    ))
}

fn create_secure_temp(dir: &Path) -> Result<(std::fs::File, PathBuf), String> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let path = dir.join(random_temp_name()?);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true)
            // `create_new` is O_CREAT | O_EXCL. O_NOFOLLOW rejects a pre-existing
            // symlink even on platforms where an implementation changes first.
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(RENDER_FILE_MODE);
        match opts.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create {}: {e}", path.display())),
        }
    }
    Err(format!(
        "could not create a unique staged-render temporary file in {}",
        dir.display()
    ))
}

/// Atomically write `contents` at mode `0600` after validating all staging
/// ancestors and the existing leaf. The temporary is fsynced before rename.
fn atomic_write_secure(paths: &StagedPaths, contents: &str) -> Result<(), String> {
    if !checked_render_dirs(&paths.dirs, true)? {
        return Err(format!(
            "render directory disappeared before writing {}",
            paths.dest.display()
        ));
    }
    // Do not replace a symlink, FIFO, or other unexpected leaf. A newly
    // missing leaf is the only non-regular state a stage write accepts.
    let _ = staged_leaf_metadata(&paths.dest)?;

    let dir = paths
        .dest
        .parent()
        .ok_or_else(|| format!("staged path has no parent: {}", paths.dest.display()))?;
    let (mut file, tmp_path) = create_secure_temp(dir)?;
    let result = (|| -> Result<(), String> {
        // `mode` on open is filtered by umask, so set the exact private mode
        // on our just-created descriptor rather than chmodding the destination.
        file.set_permissions(std::fs::Permissions::from_mode(RENDER_FILE_MODE))
            .map_err(|e| format!("could not chmod {}: {e}", tmp_path.display()))?;
        use std::io::Write;
        file.write_all(contents.as_bytes())
            .map_err(|e| format!("could not write {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("could not fsync {}: {e}", tmp_path.display()))?;

        // Revalidate immediately before the ordinary path rename. The declared
        // threat model deliberately excludes a same-UID replacement after this
        // check and before rename(2).
        if !checked_render_dirs(&paths.dirs, false)? {
            return Err(format!(
                "render directory disappeared before renaming into {}",
                paths.dest.display()
            ));
        }
        let _ = staged_leaf_metadata(&paths.dest)?;
        std::fs::rename(&tmp_path, &paths.dest)
            .map_err(|e| format!("could not rename into {}: {e}", paths.dest.display()))?;
        Ok(())
    })();

    if result.is_err() {
        // The temp name was random and created exclusively by us. Cleanup is
        // intentionally best-effort so the original error is preserved.
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
    let paths = staged_paths(repo_root, store_name, &entry.link_rel)?;

    if let Some((existing, meta)) = read_staged_file(&paths)?
        && existing == rendered
        && meta.nlink() == 1
        && meta.permissions().mode() & 0o777 == RENDER_FILE_MODE
    {
        // Do not chmod an equal file in place: it could be hard-linked to an
        // external inode. Any mode or link-count drift is repaired by replacing
        // the leaf atomically below.
        return Ok(StageOutcome::Unchanged(paths.dest));
    }

    atomic_write_secure(&paths, &rendered)?;
    Ok(StageOutcome::Written(paths.dest))
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
    let paths = staged_paths(repo_root, store_name, &entry.link_rel)?;
    match read_staged_file(&paths)? {
        Some((existing, _)) => Ok(existing != rendered),
        None => Ok(true),
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
    // A missing target has no stale children; all other inspection failures
    // must reach apply so staging is not cleaned after an incomplete scan.
    match std::fs::symlink_metadata(target_path) {
        Ok(meta) if meta.file_type().is_symlink() => return Ok(Vec::new()),
        Ok(meta) if !meta.is_dir() => {
            return Err(format!(
                "could not reconcile target {}: it is not a directory",
                target_path.display()
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "could not inspect target {} for stale links: {e}",
                target_path.display()
            ));
        }
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

/// Remove empty parents from `parent` up to, but not including, `stop`.
/// `DirectoryNotEmpty` is never hidden: we only attempt an unlink after a
/// successful empty read, so a non-NotFound failure means the cleanup did not
/// complete as expected.
fn remove_empty_staging_parents(mut parent: Option<PathBuf>, stop: &Path) -> Result<(), String> {
    while let Some(dir) = parent {
        if dir == stop {
            break;
        }
        let mut entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(format!(
                    "could not read render directory {}: {e}",
                    dir.display()
                ));
            }
        };
        if entries.next().is_some() {
            break;
        }
        match std::fs::remove_dir(&dir) {
            Ok(()) => parent = dir.parent().map(Path::to_path_buf),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(format!(
                    "could not remove render directory {}: {e}",
                    dir.display()
                ));
            }
        }
    }
    Ok(())
}

fn remove_empty_store_staging_dir(dir: &Path) -> Result<(), String> {
    let mut entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "could not read render directory {}: {e}",
                dir.display()
            ));
        }
    };
    if entries.next().is_none() {
        match std::fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "could not remove render directory {}: {e}",
                    dir.display()
                ));
            }
        }
    }
    Ok(())
}

/// Remove one stale staged render. Missing files are harmless, but every
/// other inspection or removal error is reported to the caller. This retains
/// ordinary path deletion after the immediate validation described above; the
/// threat model excludes a hostile same-UID replacement after that validation.
pub fn remove_staged(repo_root: &Path, store_name: &str, link_rel: &str) -> Result<(), String> {
    let paths = staged_paths(repo_root, store_name, link_rel)?;
    if !checked_render_dirs(&paths.dirs, false)? {
        return Ok(());
    }
    if staged_leaf_metadata(&paths.dest)?.is_none() {
        return Ok(());
    }

    match std::fs::remove_file(&paths.dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "could not remove stale render {}: {e}",
                paths.dest.display()
            ));
        }
    }
    remove_empty_staging_parents(paths.dest.parent().map(Path::to_path_buf), &paths.store_dir)
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
    let store = store_staging_paths(repo_root, store_name)?;
    if !checked_render_dirs(&store.dirs, false)? {
        return Ok(());
    }

    let mut to_remove = Vec::new();
    for entry in walkdir::WalkDir::new(&store.store_dir)
        .follow_links(false)
        .into_iter()
    {
        let entry = entry.map_err(|e| {
            format!(
                "could not scan staging directory {}: {e}",
                store.store_dir.display()
            )
        })?;
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(format!(
                "refusing symlink in staging tree {}",
                entry.path().display()
            ));
        }
        if !entry.file_type().is_file() {
            return Err(format!(
                "staged file {} is not a regular file",
                entry.path().display()
            ));
        }
        let rel = entry.path().strip_prefix(&store.store_dir).map_err(|_| {
            format!(
                "staged path escapes render tree: {}",
                entry.path().display()
            )
        })?;
        let rel = rel
            .to_str()
            .ok_or_else(|| format!("staged path is not valid UTF-8: {}", entry.path().display()))?;
        if !keep_link_rels.contains(rel) {
            to_remove.push(rel.to_string());
        }
    }

    for rel in to_remove {
        remove_staged(repo_root, store_name, &rel)?;
    }
    remove_empty_store_staging_dir(&store.store_dir)
}

/// Delete the entire staging tree for a store (used by `remove`). Missing
/// staging stays a no-op and never causes directories to be created.
pub fn remove_store_staging(repo_root: &Path, store_name: &str) -> Result<(), String> {
    let store = store_staging_paths(repo_root, store_name)?;
    if !checked_render_dirs(&store.dirs, false)? {
        return Ok(());
    }
    match std::fs::remove_dir_all(&store.store_dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "could not remove staging {}: {e}",
            store.store_dir.display()
        )),
    }
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
    //
    // Uses the narrow immediate-hop `points_into` (not the broad canonical
    // `points_into_repo`): `edit` is read-only source resolution, not a
    // destructive broad operation, and a link pointing directly at a repo
    // source entry that is itself a symlink resolving outside the repo is
    // still stitch-addressable.
    let expanded = match std::fs::symlink_metadata(&expanded) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if !linker::points_into(&expanded, repo_root) {
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
        std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
        std::fs::write(&keep, "k").unwrap();
        std::fs::write(&drop, "d").unwrap();

        let mut keep_set = BTreeSet::new();
        keep_set.insert("keep".into());
        reconcile_store_staging(repo, "git", &keep_set).unwrap();

        assert!(keep.exists());
        assert!(!drop.exists());
    }

    #[test]
    fn stage_rejects_symlinked_render_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let store = repo.join("git");
        std::fs::create_dir_all(&store).unwrap();
        let source = store.join("gitconfig.tmpl");
        std::fs::write(&source, "safe\n").unwrap();
        std::fs::create_dir_all(repo.join(".stitch")).unwrap();
        std::os::unix::fs::symlink(outside.path(), render_root(repo)).unwrap();

        let err = stage_template(
            repo,
            "git",
            "gitconfig.tmpl",
            &source,
            &test_platform(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("symlinked render ancestor"), "got: {err}");
        assert!(
            !outside.path().join("git").exists(),
            "staging must not write through the render-root symlink"
        );
    }

    #[test]
    fn staged_differs_rejects_symlinked_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let source = repo.join("gitconfig.tmpl");
        std::fs::write(&source, "safe\n").unwrap();
        let staged = staging_path(repo, "git", "gitconfig");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        let foreign = outside.path().join("rendered");
        std::fs::write(&foreign, "safe\n").unwrap();
        std::os::unix::fs::symlink(&foreign, &staged).unwrap();

        let err = staged_differs(
            repo,
            "git",
            "gitconfig.tmpl",
            &source,
            &test_platform(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("symlinked staged file"), "got: {err}");
    }

    #[test]
    fn fifo_staged_leaf_is_rejected_without_reading() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let source = repo.join("gitconfig.tmpl");
        std::fs::write(&source, "safe\n").unwrap();
        let staged = staging_path(repo, "git", "gitconfig");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        let path = CString::new(staged.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a NUL-terminated pathname owned by this test.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

        let err = staged_differs(
            repo,
            "git",
            "gitconfig.tmpl",
            &source,
            &test_platform(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");

        let err = stage_template(
            repo,
            "git",
            "gitconfig.tmpl",
            &source,
            &test_platform(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");
    }

    #[test]
    fn equal_hardlinked_staging_is_replaced_not_chmodded() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let source = repo.join("gitconfig.tmpl");
        std::fs::write(&source, "same\n").unwrap();
        let staged = staging_path(repo, "git", "gitconfig");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        let external = outside.path().join("external");
        std::fs::write(&external, "same\n").unwrap();
        std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&external, &staged).unwrap();

        let outcome = stage_template(
            repo,
            "git",
            "gitconfig.tmpl",
            &source,
            &test_platform(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(outcome, StageOutcome::Written(_)));
        assert_eq!(path_mode(&staged), Some(RENDER_FILE_MODE));
        assert_eq!(path_mode(&external), Some(0o644));
        assert_eq!(std::fs::metadata(&staged).unwrap().nlink(), 1);
        assert_eq!(std::fs::metadata(&external).unwrap().nlink(), 1);
    }

    #[test]
    fn cleanup_rejects_symlinked_store_root_and_missing_remove_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        remove_store_staging(repo, "missing").unwrap();
        assert!(
            !repo.join(".stitch").exists(),
            "removing missing staging must not create .stitch"
        );

        std::fs::create_dir_all(render_root(repo)).unwrap();
        std::os::unix::fs::symlink(outside.path(), store_render_dir(repo, "git")).unwrap();
        let err = reconcile_store_staging(repo, "git", &BTreeSet::new()).unwrap_err();
        assert!(err.contains("symlinked render ancestor"), "got: {err}");
        let err = remove_store_staging(repo, "git").unwrap_err();
        assert!(err.contains("symlinked render ancestor"), "got: {err}");
        assert!(
            outside.path().exists(),
            "cleanup must not follow the symlink"
        );
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
