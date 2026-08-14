mod ancestor;
mod cli;
mod commands;
mod config;
mod error;
mod fsutil;
mod hooks;
mod linker;
mod plan;
mod plan_exec;
mod plan_file;
mod plan_validate;
mod platform;
mod render;
mod report;
mod safety;
mod scan;
mod store;

use clap::Parser;
use commands::{add_error_from_action, apply_error_from_actions, print_warnings};
use config::{Config, ConfigError, Loaded, expand_home};
use error::StitchError;
use fsutil::{CreatedDirectory, InodeIdentity, ensure_inode_identity, inode_identity};
use platform::Platform;
use std::os::unix::fs::MetadataExt;
use std::path::Component;

fn main() {
    let cli = cli::Cli::parse();
    let json = cli.json;
    let command_name = commands::command_name(&cli.command);
    if let Err(e) = commands::run(cli) {
        if json {
            report::write_error(command_name, &e, Vec::new());
        } else {
            eprintln!("error: {e}");
            if let Some(hint) = e.hint() {
                eprintln!("hint: {hint}");
            }
        }
        std::process::exit(e.exit_code());
    }
}

/// Reverse the move step of adopt: restore the user's file/dir to its
/// original path and clean up the store dir created for file mode.
///
/// The destination is revalidated immediately before rename. If the return
/// link was repointed (or any other entry appeared), leave both that entry and
/// the adopted data untouched rather than letting `rename` overwrite it.
/// Restore the adopted path only while the pinned `$HOME` still resolves to
/// the same directory. If that boundary moved, refusing cleanup is safer than
/// restoring data through a changed pathname.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rollback_adopt_move(
    source: &std::path::Path,
    store_dir: &std::path::Path,
    raw_name: &str,
    is_dir: bool,
    expected_identity: InodeIdentity,
    expected_store_identity: Option<InodeIdentity>,
    home_identity: Option<&safety::HomeIdentity>,
    target_parents: &[CreatedDirectory],
) -> Result<(), std::io::Error> {
    if let Some(home_identity) = home_identity {
        home_identity
            .revalidate()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    match std::fs::symlink_metadata(source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to restore over entry that appeared at {}",
                    source.display()
                ),
            ));
        }
        Err(error) => return Err(error),
    }

    let moved_path = if is_dir {
        store_dir.to_path_buf()
    } else {
        store_dir.join(raw_name)
    };
    let actual = std::fs::symlink_metadata(&moved_path)?;
    if (actual.dev(), actual.ino()) != (expected_identity.dev, expected_identity.ino) {
        return Err(std::io::Error::other(format!(
            "refusing to restore {} because its inode changed",
            moved_path.display()
        )));
    }

    // Dir mode: store_dir is the moved directory itself. File mode moves the
    // file back, then removes only the empty store directory we created.
    if let Some(home_identity) = home_identity {
        home_identity
            .revalidate()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    std::fs::rename(&moved_path, source)?;
    if !is_dir {
        if let Some(expected_store_identity) = expected_store_identity {
            let actual_store_identity = std::fs::symlink_metadata(store_dir)?;
            if (actual_store_identity.dev(), actual_store_identity.ino())
                != (expected_store_identity.dev, expected_store_identity.ino)
            {
                return Err(std::io::Error::other(format!(
                    "refusing to remove store directory {} because its inode changed",
                    store_dir.display()
                )));
            }
        }
        std::fs::remove_dir(store_dir)?;
    }
    for parent in target_parents.iter().rev() {
        if let Some(home_identity) = home_identity {
            home_identity
                .revalidate()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        let actual = std::fs::symlink_metadata(&parent.path)?;
        if (actual.dev(), actual.ino()) != (parent.identity.dev, parent.identity.ino) {
            return Err(std::io::Error::other(format!(
                "refusing to remove target parent {} because its inode changed",
                parent.path.display()
            )));
        }
        match std::fs::remove_dir(&parent.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Remove links and staged renders created by an `add` attempt using their
/// exact desired sources. A link repointed meanwhile remains untouched.
///
/// Cleanup is best effort only in the sense that every independent step is
/// attempted. Its failures are returned to the user: a failed `add` must not
/// quietly leave an unrecorded link or rendered output behind.
fn cleanup_uncommitted_add(
    repo_root: &std::path::Path,
    store_name: &str,
    new_store: &config::Store,
    platform: &Platform,
    home_identity: Option<&safety::HomeIdentity>,
    target_parents: &[CreatedDirectory],
) -> Vec<String> {
    if let Some(home_identity) = home_identity
        && let Err(error) = home_identity.revalidate()
    {
        return vec![format!(
            "could not clean up uncommitted add because $HOME changed: {error}"
        )];
    }

    let mut config = Config {
        vars: std::collections::BTreeMap::new(),
        stores: std::collections::BTreeMap::new(),
    };
    config
        .stores
        .insert(store_name.to_string(), new_store.clone());
    let mut errors = Vec::new();
    for entry in store::status_all(repo_root, &config, platform) {
        match entry.status {
            linker::LinkStatus::Linked | linker::LinkStatus::Broken(_) => {
                match linker::remove_link_to(&entry.target, &entry.link_source, repo_root) {
                    Ok(true) => {}
                    Ok(false) => errors.push(format!(
                        "could not remove uncommitted link {} because it was repointed",
                        entry.target.display()
                    )),
                    Err(error) => errors.push(format!(
                        "could not remove uncommitted link {}: {error}",
                        entry.target.display()
                    )),
                }
            }
            linker::LinkStatus::Foreign(_) | linker::LinkStatus::Conflict(_) => {
                errors.push(format!(
                    "could not remove uncommitted link {} because it was replaced",
                    entry.target.display()
                ));
            }
            linker::LinkStatus::Missing => {}
            linker::LinkStatus::StoreError(_) | linker::LinkStatus::ConfigError(_) => {
                errors.push(format!(
                    "could not inspect uncommitted link {} during cleanup",
                    entry.target.display()
                ));
            }
        }
    }
    if let Err(error) = render::remove_store_staging(repo_root, store_name) {
        errors.push(error);
    }
    errors.extend(remove_created_parents(target_parents));
    errors
}

fn discard_uncommitted_empty_file(
    path: &std::path::Path,
    expected_identity: InodeIdentity,
) -> Option<String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(format!(
                "could not inspect uncommitted file {}: {error}",
                path.display()
            ));
        }
    };
    let actual = InodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if actual != expected_identity {
        return Some(format!(
            "retained uncommitted file {} because its inode changed (now device {}, inode {})",
            path.display(),
            actual.dev,
            actual.ino
        ));
    }
    if !metadata.file_type().is_file() || metadata.len() != 0 || metadata.nlink() != 1 {
        return Some(format!(
            "retained uncommitted file {} because it is no longer an empty regular file",
            path.display()
        ));
    }
    std::fs::remove_file(path).err().map(|error| {
        format!(
            "could not remove uncommitted file {}: {error}",
            path.display()
        )
    })
}

fn discard_uncommitted_add(
    store_dir: &std::path::Path,
    expected_identity: InodeIdentity,
) -> Option<String> {
    let metadata = match std::fs::symlink_metadata(store_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(format!(
                "could not inspect uncommitted store directory {}: {error}",
                store_dir.display()
            ));
        }
    };
    let actual = InodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if actual != expected_identity {
        return Some(format!(
            "retained uncommitted store directory {} because its inode changed (now device {}, inode {})",
            store_dir.display(),
            actual.dev,
            actual.ino
        ));
    }
    std::fs::remove_dir(store_dir).err().map(|error| {
        format!(
            "could not remove uncommitted store directory {}: {error}",
            store_dir.display()
        )
    })
}

fn add_cleanup_error(primary: StitchError, errors: Vec<String>) -> StitchError {
    if errors.is_empty() {
        primary
    } else {
        StitchError::internal(format!(
            "add failed ({primary}); cleanup also failed: {}. Inspect the listed paths before retrying.",
            errors.join("; ")
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn rollback_add_to_store(
    root: &std::path::Path,
    source: &std::path::Path,
    destination: &std::path::Path,
    destination_identity: InodeIdentity,
    created_parents: &[CreatedDirectory],
    home_identity: Option<&safety::HomeIdentity>,
    target_parents: &[CreatedDirectory],
    link_created: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(home_identity) = home_identity
        && let Err(error) = home_identity.revalidate()
    {
        return vec![format!(
            "could not roll back adopted file because $HOME changed: {error}"
        )];
    }
    if link_created {
        match linker::remove_link_to(source, destination, root) {
            Ok(true) => {}
            Ok(false) => {
                errors.push(format!(
                    "could not remove uncommitted link {} because it no longer points at {}",
                    source.display(),
                    destination.display()
                ));
                return errors;
            }
            Err(error) => {
                errors.push(format!(
                    "could not remove uncommitted link {}: {error}",
                    source.display()
                ));
                return errors;
            }
        }
    } else if source.symlink_metadata().is_ok() {
        errors.push(format!(
            "refusing to restore over entry that appeared at {}",
            source.display()
        ));
        return errors;
    }

    if link_created && source.symlink_metadata().is_ok() {
        errors.push(format!(
            "refusing to restore over entry that appeared at {}",
            source.display()
        ));
        return errors;
    }

    match inode_identity(destination) {
        Ok(identity) if identity == destination_identity => {}
        Ok(identity) => {
            errors.push(format!(
                "refusing to restore {} because its inode changed (was device {}, inode {}; now device {}, inode {})",
                source.display(),
                destination_identity.dev,
                destination_identity.ino,
                identity.dev,
                identity.ino
            ));
            return errors;
        }
        Err(error) => {
            errors.push(format!(
                "could not verify adopted file {} before restore: {error}",
                destination.display()
            ));
            return errors;
        }
    }

    if let Some(home_identity) = home_identity
        && let Err(error) = home_identity.revalidate()
    {
        errors.push(format!(
            "could not roll back adopted file because $HOME changed: {error}"
        ));
        return errors;
    }
    if let Err(error) = std::fs::rename(destination, source) {
        errors.push(format!(
            "could not restore {} from {}: {error}",
            source.display(),
            destination.display()
        ));
        return errors;
    }

    if let Some(home_identity) = home_identity
        && let Err(error) = home_identity.revalidate()
    {
        errors.push(format!(
            "could not clean up adopted file parents because $HOME changed: {error}"
        ));
        return errors;
    }
    errors.extend(remove_created_parents(created_parents));
    errors.extend(remove_created_parents(target_parents));
    errors
}

fn remove_created_parents(created: &[CreatedDirectory]) -> Vec<String> {
    let mut errors = Vec::new();
    for parent in created.iter().rev() {
        match inode_identity(&parent.path) {
            Ok(identity) if identity == parent.identity => {}
            Ok(identity) => {
                errors.push(format!(
                    "retained transaction-created directory {} because its inode changed (was device {}, inode {}; now device {}, inode {})",
                    parent.path.display(),
                    parent.identity.dev,
                    parent.identity.ino,
                    identity.dev,
                    identity.ino
                ));
                continue;
            }
            Err(error) => {
                errors.push(format!(
                    "could not verify transaction-created directory {} before cleanup: {error}",
                    parent.path.display()
                ));
                continue;
            }
        }
        match std::fs::remove_dir(&parent.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!(
                "could not remove transaction-created directory {}: {error}",
                parent.path.display()
            )),
        }
    }
    errors
}

fn validate_store_destination_parent(
    store_dir: &std::path::Path,
    parent: &std::path::Path,
) -> Result<(), StitchError> {
    let relative = parent.strip_prefix(store_dir).map_err(|_| {
        StitchError::path_validation(format!(
            "store destination parent {} escapes store {}",
            parent.display(),
            store_dir.display()
        ))
    })?;
    let mut current = store_dir.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(StitchError::path_validation(format!(
                "store destination parent {} contains an unsafe path component",
                parent.display()
            )));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(StitchError::conflict_foreign(
                    current.clone(),
                    std::fs::read_link(&current).ok(),
                ));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(StitchError::conflict_real(current));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(StitchError::io_context(
                    format!("inspecting store destination parent {}", current.display()),
                    error,
                ));
            }
        }
    }
    Ok(())
}

fn prepare_store_destination_parent(
    store_dir: &std::path::Path,
    parent: &std::path::Path,
) -> Result<Vec<CreatedDirectory>, StitchError> {
    let relative = parent.strip_prefix(store_dir).map_err(|_| {
        StitchError::path_validation(format!(
            "store destination parent {} escapes store {}",
            parent.display(),
            store_dir.display()
        ))
    })?;
    let mut current = store_dir.to_path_buf();
    let mut created = Vec::new();

    // Any failure after creating a parent must remove only the directories
    // created by this transaction. In particular, never leave a nested empty
    // path behind after a config/home revalidation or ancestry conflict.
    macro_rules! fail {
        ($primary:expr) => {
            return Err(add_cleanup_error(
                $primary,
                remove_created_parents(&created),
            ));
        };
    }

    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            fail!(StitchError::path_validation(format!(
                "store destination parent {} contains an unsafe path component",
                parent.display()
            )));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fail!(StitchError::conflict_foreign(
                    current.clone(),
                    std::fs::read_link(&current).ok(),
                ));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                fail!(StitchError::internal(format!(
                    "store destination parent {} is not a directory",
                    current.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Do not use create_dir_all: an ancestor can be replaced by a
                // symlink between these checks and the syscall.
                if let Err(error) = std::fs::create_dir(&current) {
                    fail!(StitchError::io_context(
                        format!("creating store destination parent {}", current.display()),
                        error,
                    ));
                }
                match std::fs::symlink_metadata(&current) {
                    Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                        created.push(CreatedDirectory {
                            path: current.clone(),
                            identity: InodeIdentity {
                                dev: meta.dev(),
                                ino: meta.ino(),
                            },
                        });
                    }
                    Ok(meta) if meta.file_type().is_symlink() => {
                        fail!(StitchError::conflict_foreign(
                            current.clone(),
                            std::fs::read_link(&current).ok(),
                        ));
                    }
                    Ok(_) => {
                        fail!(StitchError::internal(format!(
                            "store destination parent {} is not a directory after creation",
                            current.display()
                        )));
                    }
                    Err(error) => {
                        fail!(StitchError::io_context(
                            format!("rechecking store destination parent {}", current.display()),
                            error,
                        ));
                    }
                }
            }
            Err(error) => {
                fail!(StitchError::io_context(
                    format!("inspecting store destination parent {}", current.display()),
                    error,
                ));
            }
        }
    }
    Ok(created)
}

fn target_parent_candidates(target: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut missing = Vec::new();
    let mut current = target.parent();
    while let Some(path) = current {
        match std::fs::symlink_metadata(path) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(path.to_path_buf());
                current = path.parent();
            }
            Err(_) => break,
        }
    }
    missing
}

fn prepare_target_parents(
    target: &std::path::Path,
    root: &std::path::Path,
    pinned_hash: &str,
    home_identity: &safety::HomeIdentity,
) -> Result<Vec<CreatedDirectory>, StitchError> {
    let mut candidates = target_parent_candidates(target);
    candidates.sort_by_key(|path| path.components().count());
    let mut created = Vec::new();

    for path in candidates {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(add_cleanup_error(
                    StitchError::conflict_foreign(path.clone(), std::fs::read_link(&path).ok()),
                    remove_created_parents(&created),
                ));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(add_cleanup_error(
                    StitchError::conflict_real(path.clone()),
                    remove_created_parents(&created),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = revalidate_add_boundaries(root, pinned_hash, home_identity) {
                    return Err(add_cleanup_error(error, remove_created_parents(&created)));
                }
                if let Err(error) = std::fs::create_dir(&path) {
                    return Err(add_cleanup_error(
                        StitchError::io_context(
                            format!("creating target parent {}", path.display()),
                            error,
                        ),
                        remove_created_parents(&created),
                    ));
                }
                match inode_identity(&path) {
                    Ok(identity) => created.push(CreatedDirectory { path, identity }),
                    Err(error) => {
                        return Err(add_cleanup_error(error, remove_created_parents(&created)));
                    }
                }
            }
            Err(error) => {
                return Err(add_cleanup_error(
                    StitchError::io_context(
                        format!("inspecting target parent {}", path.display()),
                        error,
                    ),
                    remove_created_parents(&created),
                ));
            }
        }
    }
    Ok(created)
}

fn revalidate_add_boundaries(
    root: &std::path::Path,
    pinned_hash: &str,
    home_identity: &safety::HomeIdentity,
) -> Result<(), StitchError> {
    let found = config::revalidate_config_hash(root)?;
    if found != pinned_hash {
        return Err(StitchError::plan_stale(format!(
            "config changed while preparing add (pinned {pinned_hash}, found {found})"
        )));
    }
    home_identity
        .revalidate()
        .map_err(|error| StitchError::internal(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn cmd_add_to_store(
    root: &std::path::Path,
    loaded: &mut Loaded,
    source: &std::path::Path,
    raw_name: &str,
    store_name: &str,
    pinned_hash: &str,
    home_identity: Option<&safety::HomeIdentity>,
    dry_run: bool,
    json: bool,
) -> Result<(), StitchError> {
    let valid: Vec<String> = loaded.config.stores.keys().cloned().collect();
    let store = loaded
        .config
        .stores
        .get(store_name)
        .ok_or_else(|| StitchError::unknown_store(vec![store_name.to_string()], valid))?;
    let generated = loaded.generated.stores.get(store_name).ok_or_else(|| {
        StitchError::usage(format!(
            "store '{store_name}' has no generated inventory to extend"
        ))
    })?;
    if store.is_multi_target() || !generated.targets.is_empty() {
        return Err(StitchError::usage(format!(
            "store '{store_name}' has named targets; --to currently supports single-target stores only"
        )));
    }
    if generated.target.is_none() || (generated.files.is_empty() && generated.patterns.is_empty()) {
        return Err(StitchError::usage(format!(
            "store '{store_name}' is not an explicit file-mode store"
        )));
    }
    let platform = Platform::detect();
    if !platform.matches_when(&store.when) {
        return Err(StitchError::usage(format!(
            "store '{store_name}' is skipped on this platform"
        )));
    }

    let metadata = std::fs::symlink_metadata(source).map_err(|error| {
        StitchError::io_context(format!("inspecting {}", source.display()), error)
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(StitchError::usage(format!(
            "{} must be an existing regular file for --to",
            source.display()
        )));
    }
    if metadata.nlink() > 1 {
        return Err(StitchError::usage(format!(
            "{} is hard-linked; refusing to leave another path able to modify repo content",
            source.display()
        )));
    }
    let source_identity = InodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };

    let target_str = store
        .target
        .as_deref()
        .ok_or_else(|| StitchError::usage(format!("store '{store_name}' has no target")))?;
    let target_root = config::normalized_target_path(target_str)?;
    let source_resolved = source.canonicalize().map_err(|error| {
        StitchError::io_context(format!("resolving {}", source.display()), error)
    })?;
    let repo_resolved = root.canonicalize().map_err(|error| {
        StitchError::io_context(format!("resolving repository {}", root.display()), error)
    })?;
    if source_resolved.starts_with(&repo_resolved) {
        return Err(StitchError::usage(format!(
            "{} is inside the stitch repository; --to only adopts files from outside the repository",
            source.display()
        )));
    }
    let relative = source_resolved.strip_prefix(&target_root).map_err(|_| {
        StitchError::usage(format!(
            "{} is not inside store '{store_name}' target {}",
            source.display(),
            target_root.display()
        ))
    })?;
    let relative = relative.to_str().ok_or_else(|| {
        StitchError::path_validation(format!("{} is not valid UTF-8", relative.display()))
    })?;
    if relative.is_empty() {
        return Err(StitchError::usage(
            "--to requires a file below the store target",
        ));
    }
    config::validate_fragments(
        &[relative.to_string()],
        &[],
        &format!("store '{store_name}'"),
    )?;

    let store_dir = root.join(store_name);
    if !linker::is_real_directory(&store_dir) {
        return Err(StitchError::internal(format!(
            "store directory '{}' is missing, symlinked, or not a directory",
            store_dir.display()
        )));
    }
    let target_path = config::expand_home(target_str)?;
    let entry = render::resolve_entry(relative);
    let target = target_path.join(&entry.link_rel);
    if let Err(action) = store::preflight_add_target(root, &target_path, &target) {
        return Err(add_error_from_action(&action));
    }
    let destination = store_dir.join(relative);
    if destination.symlink_metadata().is_ok() {
        return Err(StitchError::internal(format!(
            "store entry '{}' already exists",
            destination.display()
        )));
    }
    let mut template_peer_os = destination.as_os_str().to_os_string();
    template_peer_os.push(".tmpl");
    let template_peer = std::path::PathBuf::from(template_peer_os);
    if template_peer.symlink_metadata().is_ok() {
        return Err(StitchError::path_validation(format!(
            "adding '{relative}' would collide with template source '{}'",
            template_peer.display()
        )));
    }

    let mut candidate_generated = loaded.generated.clone();
    let candidate_entry = candidate_generated
        .stores
        .get_mut(store_name)
        .expect("generated store checked above");
    if !candidate_entry.files.iter().any(|file| file == relative) {
        candidate_entry.files.push(relative.to_string());
    }
    config::validate_merged(&loaded.authored, &candidate_generated)?;
    let mut candidate_store = store.clone();
    if !candidate_store.files.iter().any(|file| file == relative) {
        candidate_store.files.push(relative.to_string());
    }

    if dry_run {
        if let Some(parent) = destination.parent() {
            validate_store_destination_parent(&store_dir, parent)?;
        }
        let data = report::AddData {
            store: store_name.to_string(),
            target: target_str.to_string(),
            mode: "add-to-store".into(),
            source: Some(collapse_home(source)?),
            files: vec![relative.to_string()],
            patterns: Vec::new(),
        };
        if json {
            report::write("add", data, loaded.warnings.clone());
        } else {
            println!("Would add to store '{store_name}':");
            println!("  {} → {}", source.display(), destination.display());
            println!("  then symlink back to {}", source.display());
        }
        return Ok(());
    }

    let inventory_errors = safety::validate_inventory(root, &loaded.config);
    if let Some(error) = inventory_errors.first() {
        return Err(StitchError::path_validation(error.to_string()));
    }
    revalidate_add_boundaries(
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    )?;
    let created_parents = destination
        .parent()
        .map(|parent| prepare_store_destination_parent(&store_dir, parent))
        .transpose()?
        .unwrap_or_default();
    if let Err(error) = revalidate_add_boundaries(
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    ) {
        return Err(add_cleanup_error(
            error,
            remove_created_parents(&created_parents),
        ));
    }
    let target_parents = match prepare_target_parents(
        &target,
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    ) {
        Ok(parents) => parents,
        Err(error) => {
            let cleanup_errors = remove_created_parents(&created_parents);
            return Err(add_cleanup_error(error, cleanup_errors));
        }
    };
    if let Err(error) = revalidate_add_boundaries(
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    ) {
        let mut cleanup_errors = remove_created_parents(&created_parents);
        cleanup_errors.extend(remove_created_parents(&target_parents));
        return Err(add_cleanup_error(error, cleanup_errors));
    }
    if let Err(error) =
        ensure_inode_identity(source, source_identity, "source changed before adoption")
    {
        let mut cleanup_errors = remove_created_parents(&created_parents);
        cleanup_errors.extend(remove_created_parents(&target_parents));
        return Err(add_cleanup_error(error, cleanup_errors));
    }
    if let Err(error) = std::fs::rename(source, &destination) {
        let mut cleanup_errors = remove_created_parents(&created_parents);
        cleanup_errors.extend(remove_created_parents(&target_parents));
        return Err(add_cleanup_error(
            StitchError::io_context(
                format!("moving {} to {}", source.display(), destination.display()),
                error,
            ),
            cleanup_errors,
        ));
    }

    let destination_identity = match inode_identity(&destination) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(add_cleanup_error(
                error,
                rollback_add_to_store(
                    root,
                    source,
                    &destination,
                    source_identity,
                    &created_parents,
                    home_identity,
                    &target_parents,
                    false,
                ),
            ));
        }
    };
    if destination_identity != source_identity {
        let primary = StitchError::internal(format!(
            "adopted file {} changed identity during the move",
            source.display()
        ));
        return Err(add_cleanup_error(
            primary,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                false,
            ),
        ));
    }
    if let Err(error) = revalidate_add_boundaries(
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    ) {
        return Err(add_cleanup_error(
            error,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                false,
            ),
        ));
    }
    if !store::store_resolves_source(&store_dir, &candidate_store, relative) {
        let primary = StitchError::path_validation(format!(
            "adopted source '{relative}' is ignored or otherwise does not resolve in store '{store_name}'"
        ));
        return Err(add_cleanup_error(
            primary,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                false,
            ),
        ));
    }

    if let Err(error) = revalidate_add_boundaries(
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    ) {
        return Err(add_cleanup_error(
            error,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                false,
            ),
        ));
    }
    let action = store::apply_added_plain_file(
        root,
        store_name,
        &candidate_store,
        relative,
        &platform,
        store::ApplyOpts {
            dry_run: false,
            force: false,
        },
    );
    if matches!(
        action,
        store::ApplyAction::Conflict { .. }
            | store::ApplyAction::Error(_)
            | store::ApplyAction::SkippedPlatform
    ) {
        let primary = apply_error_from_actions(std::slice::from_ref(&action))
            .unwrap_or_else(|| StitchError::internal("could not link adopted file"));
        return Err(add_cleanup_error(
            primary,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                false,
            ),
        ));
    }

    if let Err(error) = revalidate_add_boundaries(
        root,
        pinned_hash,
        home_identity.expect("real add captured $HOME identity"),
    ) {
        return Err(add_cleanup_error(
            error,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                true,
            ),
        ));
    }
    loaded.generated = candidate_generated;
    if let Err(error) = loaded.generated.save(root) {
        if error.write_committed() {
            return Err(error.into());
        }
        let primary = StitchError::from(error);
        return Err(add_cleanup_error(
            primary,
            rollback_add_to_store(
                root,
                source,
                &destination,
                source_identity,
                &created_parents,
                home_identity,
                &target_parents,
                true,
            ),
        ));
    }

    println!("Added {} to store '{}'", raw_name, store_name);
    println!("  linked {}", source.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_add_json(
    root: &std::path::Path,
    path: &str,
    name: &Option<String>,
    files: &[String],
    patterns: &[String],
    create_file: bool,
    to: Option<&str>,
) -> Result<(), StitchError> {
    let warnings = match config::ConfigSnapshot::load(root) {
        Ok(snapshot) => snapshot.loaded.warnings,
        Err(error) => {
            let error = StitchError::from(error);
            report::write_error("add", &error, Vec::new());
            std::process::exit(error.exit_code());
        }
    };
    match cmd_add(
        root,
        path,
        name,
        files,
        patterns,
        create_file,
        to,
        true,
        true,
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            report::write_error("add", &error, warnings);
            std::process::exit(error.exit_code());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_add(
    root: &std::path::Path,
    path: &str,
    name: &Option<String>,
    files: &[String],
    patterns: &[String],
    create_file: bool,
    to: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<(), StitchError> {
    // Serialize state mutations: load must see latest state and save must not
    // race. Hold the exclusive lock for the entire non-dry-run operation.
    let _state_lock = if dry_run {
        None
    } else {
        Some(config::StateLock::exclusive(root).map_err(StitchError::from)?)
    };
    let snapshot = config::ConfigSnapshot::load(root)?;
    let pinned_hash = snapshot.hash().to_owned();
    let mut loaded = snapshot.loaded;
    if !json {
        print_warnings(&loaded);
    }

    if json && !dry_run {
        return Err(StitchError::usage(
            "--json is not supported for add without --dry-run",
        ));
    }
    if let Some(name) = name
        && !config::is_store_name(name)
    {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{name}': store names must be exactly one normal path component"
        )));
    }
    if create_file && (!files.is_empty() || !patterns.is_empty() || to.is_some()) {
        return Err(StitchError::usage(
            "--file cannot be combined with --files, --patterns, or --to",
        ));
    }
    if to.is_some() && (name.is_some() || !files.is_empty() || !patterns.is_empty()) {
        return Err(StitchError::usage(
            "--to cannot be combined with --name, --files, or --patterns",
        ));
    }
    if let Some(store_name) = to
        && !config::is_store_name(store_name)
    {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{store_name}': store names must be exactly one normal path component"
        )));
    }

    let expanded_source = expand_home(path)?;
    let raw_source = if expanded_source.is_absolute() {
        expanded_source
    } else {
        std::env::current_dir()
            .map_err(|e| StitchError::io_context("getting current working directory", e))?
            .join(expanded_source)
    };
    // Symlink-aware normalization: gateway/../victim must resolve through
    // the gateway symlink (POSIX: symlink target spliced before ..), not
    // collapse lexically to ~/victim. Ancestors resolve fully, but the
    // terminal component is never followed — a terminal symlink must be
    // rejected below, not silently adopted (its referent would be moved and
    // the original link repointed during reconciliation). Only apply
    // canonical resolution when the path contains ".." — otherwise preserve
    // lexical HOME spelling so a symlinked $HOME (home_link -> real_home)
    // doesn't canonicalize ~/.bashrc to /real_home/.bashrc and break
    // collapse_home. Resolution failure is a hard error: falling back to
    // lexical normalization could silently pick a different file.
    let source = if raw_source
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        crate::linker::resolve_ancestors_with_missing(&raw_source).ok_or_else(|| {
            StitchError::internal(format!(
                "could not resolve {} through symlinks — refusing to guess at the path",
                raw_source.display()
            ))
        })?
    } else {
        lexically_normalize(&raw_source)
    };

    // A symlink at the target is always an error — we never silently clobber
    // or repoint a foreign symlink.
    if source.is_symlink() {
        return Err(StitchError::internal(format!(
            "{} is already a symlink — add expects a real file or directory \
             (remove the symlink first if you want stitch to manage it)",
            source.display()
        )));
    }

    // Derive the final entry name before either fresh-store or --to handling.
    let raw_name = source
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    if raw_name.ends_with(".tmpl") && (create_file || to.is_some()) {
        return Err(StitchError::usage(
            "--file and --to accept plain files only; create template sources in the repo",
        ));
    }

    if let Some(existing_store) = to {
        let home_identity = if dry_run {
            None
        } else {
            Some(
                safety::HomeIdentity::capture()
                    .map_err(|error| StitchError::internal(error.to_string()))?,
            )
        };
        return cmd_add_to_store(
            root,
            &mut loaded,
            &source,
            &raw_name,
            existing_store,
            &pinned_hash,
            home_identity.as_ref(),
            dry_run,
            json,
        );
    }

    let store_name = name
        .clone()
        .unwrap_or_else(|| raw_name.trim_start_matches('.').to_string());
    if !config::is_store_name(&store_name) {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{store_name}': store names must be exactly one normal path component"
        )));
    }
    let store_dir = root.join(&store_name);

    // Pre-checks: reject any collision BEFORE mutating anything.
    if loaded.config.stores.contains_key(&store_name) {
        return Err(StitchError::internal(format!(
            "store '{}' already exists",
            store_name
        )));
    }
    if store_dir.symlink_metadata().is_ok() {
        return Err(StitchError::internal(format!(
            "store path '{}' already exists",
            store_dir.display()
        )));
    }

    // Validate user-supplied fragments before touching the filesystem: a
    // `--file ../x` would otherwise escape the store/target dirs during apply
    // (and leave an orphaned store dir on failure).
    let validation_context = format!("store '{store_name}'");
    config::validate_fragments(files, patterns, &validation_context)?;
    // Match generated-state validation before the dry-run branch so a preview
    // never accepts a pattern that a real add would refuse to persist.
    config::validate_globs(patterns, &[], &validation_context)?;

    let source_exists = source.exists();
    if source_exists {
        let source_resolved = source.canonicalize().map_err(|error| {
            StitchError::io_context(format!("resolving {}", source.display()), error)
        })?;
        let repo_resolved = root.canonicalize().map_err(|error| {
            StitchError::io_context(format!("resolving repository {}", root.display()), error)
        })?;
        if source_resolved.starts_with(&repo_resolved) {
            return Err(StitchError::usage(format!(
                "{} is inside the stitch repository; add only adopts paths outside the repository",
                source.display()
            )));
        }
    }
    if create_file && source_exists {
        return Err(StitchError::usage(format!(
            "{} already exists — --file is only for creating a missing file",
            source.display()
        )));
    }

    // --files/--patterns only apply when creating an empty store (path doesn't
    // exist). On the adopt path the moved content determines the store layout,
    // so passing them is a user error — silently ignoring them would repeat the
    // "stitch says done, did nothing useful" footgun this command was created to
    // fix.
    if source_exists && (!files.is_empty() || !patterns.is_empty()) {
        return Err(StitchError::usage(format!(
            "{} exists — --files/--patterns only apply when creating a new empty store \
             (the existing content is moved into the repo as-is)",
            source.display()
        )));
    }

    if dry_run {
        if source_exists {
            let is_dir = source.is_dir();
            let (target_str, adopt_files) = if is_dir {
                (collapse_home(&source)?, Vec::new())
            } else {
                let parent = source
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "~".into());
                (
                    collapse_home(&expand_home(&parent)?)?,
                    vec![raw_name.clone()],
                )
            };
            config::validate_target(&target_str, &format!("store '{store_name}'"))?;
            let target_path = config::expand_home(&target_str)?;
            let target_link = if is_dir {
                target_path.clone()
            } else {
                target_path.join(&raw_name)
            };
            if let Err(action) = store::preflight_add_target(root, &target_path, &target_link) {
                return Err(add_error_from_action(&action));
            }

            let data = report::AddData {
                store: store_name.clone(),
                target: target_str,
                mode: "adopt".into(),
                source: Some(collapse_home(&source)?),
                files: adopt_files,
                patterns: Vec::new(),
            };
            if json {
                report::write("add", data, loaded.warnings);
                return Ok(());
            }
            println!("Would add (adopt existing):");
            println!("  {} → {}/", source.display(), store_dir.display());
            println!("  then symlink back to {}", target_path.display());
        } else {
            let (target_str, create_files) = if create_file {
                let parent = source.parent().ok_or_else(|| {
                    StitchError::path_validation(format!(
                        "{} has no parent directory",
                        source.display()
                    ))
                })?;
                (collapse_home(parent)?, vec![raw_name.clone()])
            } else {
                (collapse_home(&source)?, files.to_vec())
            };
            config::validate_target(&target_str, &format!("store '{store_name}'"))?;

            let data = report::AddData {
                store: store_name.clone(),
                target: target_str,
                mode: if create_file { "create-file" } else { "create" }.into(),
                source: None,
                files: create_files,
                patterns: patterns.to_vec(),
            };
            // Dry-run must validate the same target ancestry as the real
            // operation, while still leaving the filesystem untouched.
            let target_path = config::expand_home(&data.target)?;
            let target_link = if create_file {
                target_path.join(&raw_name)
            } else {
                target_path.clone()
            };
            if let Err(action) = store::preflight_add_target(root, &target_path, &target_link) {
                return Err(add_error_from_action(&action));
            }
            if json {
                report::write("add", data, loaded.warnings);
                return Ok(());
            }
            if create_file {
                println!("Would add (create empty file):");
                println!(
                    "  {} → {} (empty file, linked to {})",
                    store_name,
                    store_dir.join(&raw_name).display(),
                    source.display()
                );
            } else {
                println!("Would add (create empty store):");
                println!(
                    "  {} → {} (empty store, linked to {})",
                    store_name,
                    store_dir.display(),
                    source.display()
                );
            }
        }
        return Ok(());
    }

    let home_identity = if dry_run {
        None
    } else {
        Some(
            safety::HomeIdentity::capture()
                .map_err(|error| StitchError::internal(error.to_string()))?,
        )
    };
    if !dry_run {
        revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        )?;
    }

    if source_exists {
        // --- Adopt path: move existing content into the repo, link back. ---
        // --files/--patterns are not used here; the moved content determines
        // the store layout (whole-dir for dirs, single-file for files).
        let is_dir = source.is_dir();
        let target_str = if is_dir {
            collapse_home(&source)?
        } else {
            match source.parent() {
                Some(p) => collapse_home(p)?,
                None => "~".into(),
            }
        };
        config::validate_target(&target_str, &format!("store '{store_name}'"))?;

        let adopt_files = if is_dir {
            vec![]
        } else {
            vec![raw_name.clone()]
        };

        let new_store = config::Store {
            target: Some(target_str.clone()),
            files: adopt_files.clone(),
            patterns: vec![],
            ignore: vec![],
            when: config::WhenClause::default(),
            hooks: config::Hooks::default(),
            targets: std::collections::BTreeMap::new(),
        };
        let target_path = config::expand_home(&target_str)?;
        let target_link = if is_dir {
            target_path.clone()
        } else {
            target_path.join(&raw_name)
        };
        if let Err(action) = store::preflight_add_target(root, &target_path, &target_link) {
            return Err(add_error_from_action(&action));
        }
        let target_parents = prepare_target_parents(
            &target_link,
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        )?;
        let source_metadata = match std::fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(add_cleanup_error(
                    StitchError::io_context(format!("inspecting {}", source.display()), error),
                    remove_created_parents(&target_parents),
                ));
            }
        };
        let source_identity = InodeIdentity {
            dev: source_metadata.dev(),
            ino: source_metadata.ino(),
        };

        // Revalidate immediately before moving user data. Target parents were
        // created above and are identity-pinned for rollback.
        if let Err(error) = revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            return Err(add_cleanup_error(
                error,
                remove_created_parents(&target_parents),
            ));
        }

        // Move: relocate the file/dir into the repo. If the source changed
        // while target parents were being prepared, remove only those parents
        // before returning the error.
        if let Err(error) =
            ensure_inode_identity(&source, source_identity, "source changed before adoption")
        {
            return Err(add_cleanup_error(
                error,
                remove_created_parents(&target_parents),
            ));
        }
        let mut store_identity = None;
        if is_dir {
            if let Err(error) = std::fs::rename(&source, &store_dir) {
                return Err(add_cleanup_error(
                    StitchError::io_context(
                        format!(
                            "moving {} into store {}",
                            source.display(),
                            store_dir.display()
                        ),
                        error,
                    ),
                    remove_created_parents(&target_parents),
                ));
            }
        } else {
            // `store_dir` was checked absent above and is a direct child of
            // the existing repo root. Create it exclusively and retain its
            // inode so a failed cross-filesystem move cannot leave an
            // unowned empty directory behind (or remove a replacement).
            if let Err(error) = std::fs::create_dir(&store_dir) {
                return Err(add_cleanup_error(
                    StitchError::io_context(
                        format!("creating store directory {}", store_dir.display()),
                        error,
                    ),
                    remove_created_parents(&target_parents),
                ));
            }
            let created_store_dir = match inode_identity(&store_dir) {
                Ok(identity) => CreatedDirectory {
                    path: store_dir.clone(),
                    identity,
                },
                Err(error) => {
                    return Err(add_cleanup_error(
                        StitchError::internal(format!(
                            "could not verify newly created store directory {}: {error}",
                            store_dir.display()
                        )),
                        remove_created_parents(&target_parents),
                    ));
                }
            };
            store_identity = Some(created_store_dir.identity);
            if let Err(error) = std::fs::rename(&source, store_dir.join(&raw_name)) {
                let mut cleanup_errors =
                    remove_created_parents(std::slice::from_ref(&created_store_dir));
                cleanup_errors.extend(remove_created_parents(&target_parents));
                return Err(add_cleanup_error(
                    StitchError::io_context(
                        format!(
                            "moving {} into store {}",
                            source.display(),
                            store_dir.join(&raw_name).display()
                        ),
                        error,
                    ),
                    cleanup_errors,
                ));
            }
        }

        // Link: create the return symlink using the in-memory store.
        // If this fails, roll back the move so the user's file is back where
        // it was. State was never touched.
        if let Err(error) = revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            let cleanup_errors = match rollback_adopt_move(
                &source,
                &store_dir,
                &raw_name,
                is_dir,
                source_identity,
                store_identity,
                home_identity.as_ref(),
                &target_parents,
            ) {
                Ok(()) => Vec::new(),
                Err(error) => vec![format!("could not roll back adopted path: {error}")],
            };
            return Err(add_cleanup_error(error, cleanup_errors));
        }
        let platform = Platform::detect();
        let mut _warnings = Vec::new();
        let results = store::apply_store(
            root,
            &store_name,
            &new_store,
            &platform,
            &loaded.config.vars,
            store::ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut _warnings,
        );
        if results.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict { .. } | store::ApplyAction::Error(_)
            )
        }) {
            let primary = apply_error_from_actions(&results.actions)
                .unwrap_or_else(|| StitchError::internal("apply reported conflicts or errors"));
            let cleanup_errors = cleanup_uncommitted_add(
                root,
                &store_name,
                &new_store,
                &platform,
                home_identity.as_ref(),
                &target_parents,
            );
            if let Err(error) = rollback_adopt_move(
                &source,
                &store_dir,
                &raw_name,
                is_dir,
                source_identity,
                store_identity,
                home_identity.as_ref(),
                &target_parents,
            ) {
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!(" Cleanup also failed: {}.", cleanup_errors.join("; "))
                };
                return Err(StitchError::internal(format!(
                    "ADD FAILED ({primary}) and rollback also failed: {} is stranded in {} ({error}).{cleanup}",
                    source.display(),
                    store_dir.display(),
                )));
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
        }

        // Record: persist state.toml (generated half only). stitch.toml is
        // never rewritten by the tool after init, so comments/formatting survive.
        loaded.generated.stores.insert(
            store_name.clone(),
            config::GeneratedStore {
                target: Some(target_str.clone()),
                files: adopt_files,
                patterns: vec![],
                targets: std::collections::BTreeMap::new(),
            },
        );
        if let Err(error) = revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            let cleanup_errors = cleanup_uncommitted_add(
                root,
                &store_name,
                &new_store,
                &platform,
                home_identity.as_ref(),
                &target_parents,
            );
            if let Err(rollback_error) = rollback_adopt_move(
                &source,
                &store_dir,
                &raw_name,
                is_dir,
                source_identity,
                store_identity,
                home_identity.as_ref(),
                &target_parents,
            ) {
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!(" Cleanup also failed: {}.", cleanup_errors.join("; "))
                };
                return Err(StitchError::internal(format!(
                    "add revalidation failed ({error}) and rollback also failed: {rollback_error}.{cleanup}"
                )));
            }
            return Err(add_cleanup_error(error, cleanup_errors));
        }
        if let Err(error) = loaded.generated.save(root) {
            // A directory fsync can fail after rename. The state is then
            // already committed, so rolling back its links/store would make
            // that state point at missing data.
            if error.write_committed() {
                return Err(error.into());
            }
            let primary = StitchError::from(error);
            let cleanup_errors = cleanup_uncommitted_add(
                root,
                &store_name,
                &new_store,
                &platform,
                home_identity.as_ref(),
                &target_parents,
            );
            if let Err(rollback_error) = rollback_adopt_move(
                &source,
                &store_dir,
                &raw_name,
                is_dir,
                source_identity,
                store_identity,
                home_identity.as_ref(),
                &target_parents,
            ) {
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!(" Cleanup also failed: {}.", cleanup_errors.join("; "))
                };
                return Err(StitchError::internal(format!(
                    "state save failed ({primary}) and rollback also failed: {} is stranded in {} ({rollback_error}).{cleanup}",
                    source.display(),
                    store_dir.display(),
                )));
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
        }

        println!(
            "Added store '{}' (adopted from {})",
            store_name,
            source.display()
        );
        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked(_) => println!("  already linked"),
                _ => {}
            }
        }
    } else {
        // --- Create-empty path: fresh directory store, or one empty file. ---
        let (target_str, create_files) = if create_file {
            let parent = source.parent().ok_or_else(|| {
                StitchError::path_validation(format!(
                    "{} has no parent directory",
                    source.display()
                ))
            })?;
            (collapse_home(parent)?, vec![raw_name.clone()])
        } else {
            (collapse_home(&source)?, files.to_vec())
        };
        config::validate_target(&target_str, &format!("store '{store_name}'"))?;

        let new_store = config::Store {
            target: Some(target_str.clone()),
            files: create_files.clone(),
            patterns: patterns.to_vec(),
            ignore: vec![],
            when: config::WhenClause::default(),
            hooks: config::Hooks::default(),
            targets: std::collections::BTreeMap::new(),
        };
        let target_path = config::expand_home(&target_str)?;
        let target_link = if create_file {
            target_path.join(&raw_name)
        } else {
            target_path.clone()
        };
        if let Err(action) = store::preflight_add_target(root, &target_path, &target_link) {
            return Err(add_error_from_action(&action));
        }
        revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        )?;
        std::fs::create_dir(&store_dir).map_err(|e| {
            StitchError::io_context(
                format!("creating store directory {}", store_dir.display()),
                e,
            )
        })?;
        let store_identity = match inode_identity(&store_dir) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(add_cleanup_error(
                    error,
                    vec![format!(
                        "new store directory {} could not be identity-checked; inspect it before retrying",
                        store_dir.display()
                    )],
                ));
            }
        };
        let empty_file_identity = if create_file {
            if let Err(error) = revalidate_add_boundaries(
                root,
                &pinned_hash,
                home_identity
                    .as_ref()
                    .expect("real add captured $HOME identity"),
            ) {
                let cleanup = discard_uncommitted_add(&store_dir, store_identity);
                return Err(add_cleanup_error(error, cleanup.into_iter().collect()));
            }
            let file_path = store_dir.join(&raw_name);
            if let Err(error) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)
            {
                let cleanup = discard_uncommitted_add(&store_dir, store_identity);
                return Err(add_cleanup_error(
                    StitchError::io_context(
                        format!("creating empty file {}", file_path.display()),
                        error,
                    ),
                    cleanup.into_iter().collect(),
                ));
            }
            Some(match inode_identity(&file_path) {
                Ok(identity) => identity,
                Err(error) => {
                    let cleanup = discard_uncommitted_add(&store_dir, store_identity);
                    return Err(add_cleanup_error(error, cleanup.into_iter().collect()));
                }
            })
        } else {
            None
        };
        let target_parents = match prepare_target_parents(
            &target_link,
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            Ok(parents) => parents,
            Err(error) => {
                let mut cleanup_errors = Vec::new();
                if let Some(identity) = empty_file_identity
                    && let Some(cleanup_error) =
                        discard_uncommitted_empty_file(&store_dir.join(&raw_name), identity)
                {
                    cleanup_errors.push(cleanup_error);
                }
                if let Some(cleanup_error) = discard_uncommitted_add(&store_dir, store_identity) {
                    cleanup_errors.push(cleanup_error);
                }
                return Err(add_cleanup_error(error, cleanup_errors));
            }
        };
        if let Err(error) = revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            let mut cleanup_errors = remove_created_parents(&target_parents);
            if let Some(identity) = empty_file_identity
                && let Some(cleanup_error) =
                    discard_uncommitted_empty_file(&store_dir.join(&raw_name), identity)
            {
                cleanup_errors.push(cleanup_error);
            }
            if let Some(cleanup_error) = discard_uncommitted_add(&store_dir, store_identity) {
                cleanup_errors.push(cleanup_error);
            }
            return Err(add_cleanup_error(error, cleanup_errors));
        }

        if let Err(error) = revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            let mut cleanup_errors = remove_created_parents(&target_parents);
            if let Some(identity) = empty_file_identity
                && let Some(cleanup_error) =
                    discard_uncommitted_empty_file(&store_dir.join(&raw_name), identity)
            {
                cleanup_errors.push(cleanup_error);
            }
            if let Some(cleanup_error) = discard_uncommitted_add(&store_dir, store_identity) {
                cleanup_errors.push(cleanup_error);
            }
            return Err(add_cleanup_error(error, cleanup_errors));
        }
        let platform = Platform::detect();
        let mut _warnings = Vec::new();
        let results = store::apply_store(
            root,
            &store_name,
            &new_store,
            &platform,
            &loaded.config.vars,
            store::ApplyOpts {
                dry_run: false,
                force: false,
            },
            &mut _warnings,
        );
        // Target ancestors were created and identity-pinned before the store
        // mutation, so cleanup can never claim a directory created by another
        // process.

        for action in &results.actions {
            match action {
                store::ApplyAction::Created(p) => println!("  linked {}", p.display()),
                store::ApplyAction::AlreadyLinked(_) => println!("  already linked"),
                store::ApplyAction::Conflict { target, .. } => {
                    println!("  conflict at {}", target.display())
                }
                store::ApplyAction::Error(e) => println!("  error: {e}"),
                _ => {}
            }
        }

        let failed = results.actions.iter().any(|a| {
            matches!(
                a,
                store::ApplyAction::Conflict { .. } | store::ApplyAction::Error(_)
            )
        });

        if failed {
            let primary = apply_error_from_actions(&results.actions)
                .unwrap_or_else(|| StitchError::internal("apply reported conflicts or errors"));
            let mut cleanup_errors = cleanup_uncommitted_add(
                root,
                &store_name,
                &new_store,
                &platform,
                home_identity.as_ref(),
                &target_parents,
            );
            if create_file
                && let Some(identity) = empty_file_identity
                && let Some(error) =
                    discard_uncommitted_empty_file(&store_dir.join(&raw_name), identity)
            {
                cleanup_errors.push(error);
            }
            if let Some(error) = discard_uncommitted_add(&store_dir, store_identity) {
                cleanup_errors.push(error);
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
        }

        // Persist state.toml (generated half only). If save fails after apply
        // already created links, undo them and the empty store dir so no
        // half-applied store is left without a state entry.
        loaded.generated.stores.insert(
            store_name.clone(),
            config::GeneratedStore {
                target: Some(target_str.clone()),
                files: create_files,
                patterns: patterns.to_vec(),
                targets: std::collections::BTreeMap::new(),
            },
        );
        if let Err(error) = revalidate_add_boundaries(
            root,
            &pinned_hash,
            home_identity
                .as_ref()
                .expect("real add captured $HOME identity"),
        ) {
            let mut cleanup_errors = cleanup_uncommitted_add(
                root,
                &store_name,
                &new_store,
                &platform,
                home_identity.as_ref(),
                &target_parents,
            );
            if create_file
                && let Some(identity) = empty_file_identity
                && let Some(cleanup_error) =
                    discard_uncommitted_empty_file(&store_dir.join(&raw_name), identity)
            {
                cleanup_errors.push(cleanup_error);
            }
            if let Some(cleanup_error) = discard_uncommitted_add(&store_dir, store_identity) {
                cleanup_errors.push(cleanup_error);
            }
            return Err(add_cleanup_error(error, cleanup_errors));
        }
        if let Err(error) = loaded.generated.save(root) {
            // See the adopt path above: rename succeeded, so leave the
            // matching links and store in place when only directory fsync
            // failed.
            if error.write_committed() {
                return Err(error.into());
            }
            let primary = StitchError::from(error);
            let mut cleanup_errors = cleanup_uncommitted_add(
                root,
                &store_name,
                &new_store,
                &platform,
                home_identity.as_ref(),
                &target_parents,
            );
            if create_file
                && let Some(identity) = empty_file_identity
                && let Some(error) =
                    discard_uncommitted_empty_file(&store_dir.join(&raw_name), identity)
            {
                cleanup_errors.push(error);
            }
            if let Some(error) = discard_uncommitted_add(&store_dir, store_identity) {
                cleanup_errors.push(error);
            }
            return Err(add_cleanup_error(primary, cleanup_errors));
        }

        println!("Added store '{}'", store_name);
    }

    Ok(())
}

/// True if two paths refer to the same location (canonical when possible).
pub(crate) fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// For a file-mode symlink, return the target directory by stripping the
/// repo-relative source path from the end of the symlink's location.
///
/// `link` is where the symlink lives (e.g. `~/.config/nvim/lua/plugin.lua`);
/// `source_rel` is its path inside the store (e.g. `lua/plugin.lua`). The
/// result is the common directory the store is linked into
/// (e.g. `~/.config/nvim`).
pub(crate) fn target_dir_for_file_link(
    link: &std::path::Path,
    source_rel: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let link_comps: Vec<_> = link.components().collect();
    let source_comps: Vec<_> = source_rel.components().collect();
    if link_comps.len() < source_comps.len() {
        return None;
    }
    let split = link_comps.len() - source_comps.len();
    if link_comps[split..] != source_comps[..] {
        return None;
    }
    let mut target = std::path::PathBuf::new();
    for c in &link_comps[..split] {
        target.push(c.as_os_str());
    }
    Some(target)
}

/// Resolve `.` and `..` components lexically, without touching the
/// filesystem or following symlinks.
fn lexically_normalize(path: &std::path::Path) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = normalized.components().next_back() {
                    normalized.pop();
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                normalized = std::path::PathBuf::new();
                normalized.push(component.as_os_str());
            }
            Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// Collapse `$HOME` prefix to `~` for state.toml target strings.
pub(crate) fn collapse_home(path: &std::path::Path) -> Result<String, ConfigError> {
    let home = config::expand_home("~")?;
    if let Ok(rel) = path.strip_prefix(&home) {
        if rel.as_os_str().is_empty() {
            return Ok("~".into());
        }
        return Ok(format!("~/{}", rel.display()));
    }
    Ok(path.display().to_string())
}
