//! Shared safety invariants for every stitch command that mutates `$HOME` or
//! repository state.
//!
//! Prior fixes addressed individual command paths one at a time, which left
//! adjacent paths uncovered: a guard added to `apply` was missing from
//! `remove`, a guard for active stores was missing for platform-skipped ones,
//! and a hash computed from a re-read was not bound to the bytes actually
//! parsed for hook selection.
//!
//! This module defines invariants that every mutating command must uphold:
//!
//! - **[`HomeIdentity`]**: `$HOME` is a pinned location, not a live pathname.
//!   Both the entry itself (lstat) and the directory it resolves to (stat) are
//!   captured and revalidated after any hook. A hook that replaces the
//!   directory *behind* a symlinked `$HOME` — without changing the symlink —
//!   is detected.
//!
//! - **Config snapshot** (in [`config::ConfigSnapshot`]): configuration is one
//!   trusted snapshot, not a sequence of independent reads. The parsed
//!   [`config::Loaded`] config and the hash of the exact bytes it was parsed
//!   from are bound together. Hook selection reads from the parsed config;
//!   hash checks compare the snapshot's hash. There is no re-read between
//!   parse and hash, so a config that changes for `Config::load` and is
//!   restored before a separate `compute_config_hash` call cannot install a
//!   wrong hook that passes the hash check.
//!
//! - **[`InventoryCheck`]**: inventory validity (symlinked source roots,
//!   source-name collisions, unreadable store dirs) is enforced for *all*
//!   stores regardless of platform match. "Skipped" changes whether a command
//!   acts on a store, not whether it validates the store. A platform-skipped
//!   store with a symlinked source root or colliding sources is still invalid
//!   and must not be silently removed.
//!
//! ## Documented race boundary
//!
//! These invariants reject hostile state present at capture time and
//! revalidate after hooks immediately before mutation. A malicious same-UID
//! process racing the final filesystem syscall is out of scope (it can already
//! mutate the same files directly). No claim stronger than that is made here
//! or in comments.

use crate::config::{Config, expand_home};
use crate::render;
use crate::store::{self, LinkTargets};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// ===========================================================================
// HomeIdentity — $HOME is a pinned location, not a live pathname.
// ===========================================================================

/// The pre-hook identity of `$HOME`, captured in two complementary ways.
///
/// `lstat_identity` is the identity of the `$HOME` entry itself — the symlink
/// or directory that `$HOME` names. It detects the entry being replaced,
/// removed, or repointed.
///
/// `resolved_identity` is the identity of the directory `$HOME` resolves to
/// (following symlinks). It detects the directory *behind* a symlinked `$HOME`
/// being replaced with a different directory, even when the symlink itself is
/// unchanged.
///
/// A hook that replaces either must be rejected before any target mutation.
#[derive(Debug, Clone)]
pub struct HomeIdentity {
    /// The `$HOME` path as resolved from the environment (may be a symlink).
    home_path: PathBuf,
    /// lstat identity of the `$HOME` entry: `(dev, ino)` of the symlink or
    /// directory at that path. `None` if `$HOME` did not exist at capture
    /// (legal for some test setups; revalidation treats appearance as a
    /// redirect).
    lstat_identity: Option<(u64, u64)>,
    /// stat identity of the directory `$HOME` resolves to: `(dev, ino)` via
    /// `std::fs::metadata` (follows symlinks). This is the directory that
    /// target paths like `~/.config/app` resolve *through*.
    resolved_identity: (u64, u64),
}

/// A change to `$HOME` detected by [`HomeIdentity::revalidate`].
#[derive(Debug, Clone)]
pub enum HomeIdentityError {
    /// The `$HOME` entry itself changed (replaced, removed, or repointed).
    EntryChanged { message: String },
    /// The directory `$HOME` resolves to changed identity, even though the
    /// `$HOME` entry (e.g. a symlink) is unchanged. This is the
    /// "replace the directory behind the symlink" attack.
    ResolvedDirChanged { message: String },
    /// `$HOME` could not be inspected.
    Inspect { message: String },
}

impl std::fmt::Display for HomeIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntryChanged { message } | Self::ResolvedDirChanged { message } => {
                write!(f, "{message}")
            }
            Self::Inspect { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for HomeIdentityError {}

impl HomeIdentity {
    /// Capture the current identity of `$HOME`.
    ///
    /// `$HOME` must resolve to an existing directory (this is enforced by
    /// [`config::home_dir`] before any command runs, so this is a defense-
    /// in-depth check). Both the lstat and resolved identities are captured.
    pub fn capture() -> Result<Self, HomeIdentityError> {
        let home_path = expand_home("~").map_err(|e| HomeIdentityError::Inspect {
            message: e.to_string(),
        })?;

        let lstat_identity = match std::fs::symlink_metadata(&home_path) {
            Ok(meta) => Some((meta.dev(), meta.ino())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(HomeIdentityError::Inspect {
                    message: format!("could not inspect $HOME {}: {e}", home_path.display()),
                });
            }
        };

        let resolved_meta =
            std::fs::metadata(&home_path).map_err(|e| HomeIdentityError::Inspect {
                message: format!("could not resolve $HOME {}: {e}", home_path.display()),
            })?;
        if !resolved_meta.is_dir() {
            return Err(HomeIdentityError::Inspect {
                message: format!(
                    "$HOME {} does not resolve to a directory",
                    home_path.display()
                ),
            });
        }
        let resolved_identity = (resolved_meta.dev(), resolved_meta.ino());

        Ok(Self {
            home_path,
            lstat_identity,
            resolved_identity,
        })
    }

    /// Revalidate both the lstat and resolved identities of `$HOME`.
    ///
    /// Returns an error if either changed: the entry itself was replaced, or
    /// the directory it resolves to was swapped out.
    pub fn revalidate(&self) -> Result<(), HomeIdentityError> {
        // Check the lstat identity: the $HOME entry itself.
        let current_lstat = match std::fs::symlink_metadata(&self.home_path) {
            Ok(meta) => Some((meta.dev(), meta.ino())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(HomeIdentityError::Inspect {
                    message: format!(
                        "could not re-inspect $HOME {}: {e}",
                        self.home_path.display()
                    ),
                });
            }
        };
        if current_lstat != self.lstat_identity {
            return Err(HomeIdentityError::EntryChanged {
                message: format!(
                    "$HOME {} changed identity during the hook (entry replaced or repointed)",
                    self.home_path.display()
                ),
            });
        }

        // Check the resolved identity: the directory $HOME resolves to.
        // This catches the "replace the directory behind the symlink" attack
        // where the symlink itself is unchanged but its target directory is
        // swapped to a different inode.
        let current_resolved =
            std::fs::metadata(&self.home_path).map_err(|e| HomeIdentityError::Inspect {
                message: format!(
                    "could not re-resolve $HOME {}: {e}",
                    self.home_path.display()
                ),
            })?;
        if !current_resolved.is_dir() {
            return Err(HomeIdentityError::ResolvedDirChanged {
                message: format!(
                    "$HOME {} no longer resolves to a directory",
                    self.home_path.display()
                ),
            });
        }
        let current_id = (current_resolved.dev(), current_resolved.ino());
        if current_id != self.resolved_identity {
            return Err(HomeIdentityError::ResolvedDirChanged {
                message: format!(
                    "$HOME {} resolves to a different directory than it did before the hook \
                     (the directory behind the symlink was replaced)",
                    self.home_path.display()
                ),
            });
        }

        Ok(())
    }
}

// ===========================================================================
// InventoryCheck — validate all stores, regardless of platform match.
// ===========================================================================

/// The kind of inventory error found by [`validate_inventory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryErrorKind {
    /// The store's source root directory is a symlink or non-directory.
    /// A symlinked source root can resolve outside the repo and must never
    /// be treated as a valid store, even for platform-skipped stores.
    SymlinkedSourceRoot,
    /// Two source entries resolve to the same link name (e.g. `foo` and
    /// `foo.tmpl`). This makes the link inventory ambiguous.
    SourceNameCollision,
    /// A template source (`.tmpl`) is in an unsupported location (e.g. a
    /// directory rather than a regular file).
    UnsupportedTemplateSource,
    /// The store directory could not be read (permission denied, I/O error).
    StoreDirUnreadable,
}

impl std::fmt::Display for InventoryErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SymlinkedSourceRoot => write!(f, "symlinked source root"),
            Self::SourceNameCollision => write!(f, "source-name collision"),
            Self::UnsupportedTemplateSource => write!(f, "unsupported template source"),
            Self::StoreDirUnreadable => write!(f, "store directory unreadable"),
        }
    }
}

/// An inventory validation error for one store.
#[derive(Debug, Clone)]
pub struct InventoryError {
    pub store: String,
    pub kind: InventoryErrorKind,
    pub message: String,
    /// The target name within a multi-target store, if applicable.
    pub target_name: Option<String>,
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.target_name {
            Some(tn) => write!(
                f,
                "store '{}' (target '{}'): {}: {}",
                self.store, tn, self.kind, self.message
            ),
            None => write!(f, "store '{}': {}: {}", self.store, self.kind, self.message),
        }
    }
}

/// Validate the inventory for every store in `config`, regardless of platform
/// match.
///
/// "Skipped" changes *whether* a command acts on a store, not *whether* it
/// validates the store. A platform-skipped store with a symlinked source root
/// or colliding sources is still invalid and must not be silently removed or
/// have its state dropped.
///
/// This function checks:
/// - Store source roots are real directories (not symlinks).
/// - Source-name collisions (e.g. `foo` and `foo.tmpl`) within each target.
/// - Unsupported template sources (e.g. a `.tmpl` directory).
///
/// It does **not** re-check path fragment validation or target validation —
/// those are already enforced by `Config::validate` at load time for all
/// stores.
pub fn validate_inventory(repo_root: &Path, config: &Config) -> Vec<InventoryError> {
    let mut errors = Vec::new();
    let sorted: std::collections::BTreeMap<_, _> = config.stores.iter().collect();

    for (name, store) in sorted {
        let store_dir = repo_root.join(name);

        // Check the source root: must be a real directory, not a symlink.
        // This applies to ALL stores, including platform-skipped ones.
        match std::fs::symlink_metadata(&store_dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                errors.push(InventoryError {
                    store: name.clone(),
                    kind: InventoryErrorKind::SymlinkedSourceRoot,
                    message: format!(
                        "store directory '{}' is a symlink, not a real directory",
                        name
                    ),
                    target_name: None,
                });
                // A symlinked root means we cannot safely scan its contents;
                // skip further checks for this store.
                continue;
            }
            Ok(meta) if !meta.is_dir() => {
                errors.push(InventoryError {
                    store: name.clone(),
                    kind: InventoryErrorKind::SymlinkedSourceRoot,
                    message: format!("store directory '{}' exists but is not a directory", name),
                    target_name: None,
                });
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing store dir is not an inventory error by itself —
                // it may be an authored-only store with no generated links, or
                // a store that hasn't been created yet. Apply/remove handle
                // missing dirs at execution time. Skip source-name checks
                // since there's nothing to scan.
                continue;
            }
            Err(e) => {
                errors.push(InventoryError {
                    store: name.clone(),
                    kind: InventoryErrorKind::StoreDirUnreadable,
                    message: format!("could not read store directory '{}': {e}", name),
                    target_name: None,
                });
                continue;
            }
            Ok(_) => {}
        }

        // Check source-name collisions and unsupported template sources for
        // each target. These checks run regardless of platform match.
        if store.is_multi_target() {
            for (target_name, target_entry) in &store.targets {
                check_target_inventory(
                    &store_dir,
                    &target_entry.files,
                    &target_entry.patterns,
                    &target_entry.ignore,
                    name,
                    Some(target_name),
                    &mut errors,
                );
            }
        } else if !store.files.is_empty() || !store.patterns.is_empty() {
            check_target_inventory(
                &store_dir,
                &store.files,
                &store.patterns,
                &store.ignore,
                name,
                None,
                &mut errors,
            );
        } else {
            // Whole-dir mode with no explicit files/patterns: check for
            // unsupported template sources and collisions in the implicit
            // expansion.
            check_target_inventory(&store_dir, &[], &[], &store.ignore, name, None, &mut errors);
        }
    }

    errors
}

/// Check one target's source inventory for collisions and unsupported
/// templates. Appends errors to `out`.
fn check_target_inventory(
    store_dir: &Path,
    files: &[String],
    patterns: &[String],
    ignore: &[String],
    store_name: &str,
    target_name: Option<&str>,
    out: &mut Vec<InventoryError>,
) {
    // Unsupported template source (e.g. a .tmpl directory or symlink).
    // unsupported_template_source returns Ok(Some(path)) when one is found,
    // Ok(None) when none is found, and Err(msg) when the dir can't be scanned.
    match render::unsupported_template_source(store_dir) {
        Ok(Some(path)) => {
            out.push(InventoryError {
                store: store_name.to_string(),
                kind: InventoryErrorKind::UnsupportedTemplateSource,
                message: format!(
                    "template source {} must be a direct regular file",
                    path.display()
                ),
                target_name: target_name.map(str::to_owned),
            });
            return;
        }
        Ok(None) => {}
        Err(msg) => {
            out.push(InventoryError {
                store: store_name.to_string(),
                kind: InventoryErrorKind::StoreDirUnreadable,
                message: msg,
                target_name: target_name.map(str::to_owned),
            });
            return;
        }
    }

    // Resolve source names and check for collisions.
    let targets = store::resolve_target_names(store_dir, files, patterns, ignore);
    if let LinkTargets::Files(names) = targets
        && let Err(msg) = render::check_name_collisions(&names)
    {
        out.push(InventoryError {
            store: store_name.to_string(),
            kind: InventoryErrorKind::SourceNameCollision,
            message: msg,
            target_name: target_name.map(str::to_owned),
        });
    }
}

/// Check whether any inventory error affects the given store. Used by commands
/// that operate on a single store (e.g. `remove <name>`) to decide whether to
/// abort before mutation.
pub fn store_has_inventory_error(errors: &[InventoryError], store_name: &str) -> bool {
    errors.iter().any(|e| e.store == store_name)
}

// ===========================================================================
// Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    // --- HomeIdentity tests ---

    fn test_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        // set_test_home is thread-local; we use the real HOME env via the
        // test harness. For unit tests we call expand_home("~") which reads
        // HOME, so set it.
        // SAFETY: tests run in parallel but each sets its own HOME via the
        // thread-local test override.
        config::set_test_home(Some(home.clone()));
        (tmp, home)
    }

    #[test]
    fn home_identity_captures_real_directory() {
        let (_tmp, _home) = test_home();
        let identity = HomeIdentity::capture().expect("capture");
        identity.revalidate().expect("unchanged");
    }

    #[test]
    fn home_identity_detects_resolved_dir_replacement() {
        // The P0 attack: $HOME is a symlink to real_home. A hook replaces
        // real_home with a different directory. The symlink is unchanged, but
        // the resolved directory's identity changes.
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_home = tmp.path().join("real_home");
        let home_link = tmp.path().join("home_link");
        std::fs::create_dir_all(&real_home).expect("mkdir real_home");
        std::os::unix::fs::symlink(&real_home, &home_link).expect("symlink home");
        config::set_test_home(Some(home_link.clone()));

        let identity = HomeIdentity::capture().expect("capture");

        // Simulate the hook: replace real_home with a different directory.
        std::fs::remove_dir_all(&real_home).expect("rm real_home");
        std::fs::create_dir_all(&real_home).expect("mkdir new real_home");

        let err = identity.revalidate().expect_err("must detect change");
        assert!(
            matches!(err, HomeIdentityError::ResolvedDirChanged { .. }),
            "expected ResolvedDirChanged, got {err:?}"
        );
    }

    #[test]
    fn home_identity_detects_symlink_replacement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_home_a = tmp.path().join("real_a");
        let real_home_b = tmp.path().join("real_b");
        let home_link = tmp.path().join("home_link");
        std::fs::create_dir_all(&real_home_a).expect("mkdir a");
        std::fs::create_dir_all(&real_home_b).expect("mkdir b");
        std::os::unix::fs::symlink(&real_home_a, &home_link).expect("symlink");
        config::set_test_home(Some(home_link.clone()));

        let identity = HomeIdentity::capture().expect("capture");

        // Replace the symlink itself.
        std::fs::remove_file(&home_link).expect("rm symlink");
        std::os::unix::fs::symlink(&real_home_b, &home_link).expect("new symlink");

        let err = identity.revalidate().expect_err("must detect change");
        assert!(
            matches!(err, HomeIdentityError::EntryChanged { .. }),
            "expected EntryChanged, got {err:?}"
        );
    }

    #[test]
    fn home_identity_accepts_unchanged_symlinked_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_home = tmp.path().join("real_home");
        let home_link = tmp.path().join("home_link");
        std::fs::create_dir_all(&real_home).expect("mkdir");
        std::os::unix::fs::symlink(&real_home, &home_link).expect("symlink");
        config::set_test_home(Some(home_link.clone()));

        let identity = HomeIdentity::capture().expect("capture");
        identity
            .revalidate()
            .expect("unchanged symlinked home is fine");
    }

    // --- ConfigSnapshot tests ---

    fn test_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("repo");
        let stitch = root.join(".stitch");
        std::fs::create_dir_all(&stitch).expect("mkdir .stitch");
        std::fs::write(root.join("stitch.toml"), "").expect("write stitch.toml");
        std::fs::write(stitch.join("state.toml"), "").expect("write state.toml");
        std::fs::write(root.join(".gitignore"), ".stitch/render/\n").expect("gitignore");
        (tmp, root)
    }

    #[test]
    fn config_snapshot_loads_and_hashes_consistently() {
        let (_tmp, root) = test_repo();
        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let hash = snap.hash().to_string();

        // The hash must match a fresh load.
        let snap2 = crate::config::ConfigSnapshot::load(&root).expect("load 2");
        assert_eq!(snap2.hash(), hash);
    }

    #[test]
    fn config_snapshot_detects_state_change() {
        let (_tmp, root) = test_repo();
        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let hash = snap.hash().to_string();

        // Simulate a hook changing state.
        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.evil]\ntarget = \"~/.evil\"\n",
        )
        .expect("write evil state");

        // A fresh load must produce a different hash.
        let snap2 = crate::config::ConfigSnapshot::load(&root).expect("load 2");
        assert_ne!(snap2.hash(), hash, "hash must change when state changes");
    }

    #[test]
    fn config_snapshot_detects_authored_change() {
        let (_tmp, root) = test_repo();
        std::fs::write(
            root.join("stitch.toml"),
            "[stores.app]\nhooks = { pre = \"echo safe\" }\n",
        )
        .expect("write authored");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let hash = snap.hash().to_string();

        // Simulate a hook changing authored config.
        std::fs::write(
            root.join("stitch.toml"),
            "[stores.app]\nhooks = { pre = \"touch pwned\" }\n",
        )
        .expect("write evil authored");

        let snap2 = crate::config::ConfigSnapshot::load(&root).expect("load 2");
        assert_ne!(snap2.hash(), hash, "hash must change when authored changes");
    }

    #[test]
    fn config_snapshot_rejects_symlinked_state() {
        let (_tmp, root) = test_repo();
        let external = tempfile::tempdir().expect("tempdir");
        let external_state = external.path().join("state.toml");
        std::fs::write(&external_state, "[stores.evil]\ntarget = \"~/.evil\"\n").expect("write");

        let state = root.join(".stitch/state.toml");
        std::fs::remove_file(&state).expect("rm state");
        std::os::unix::fs::symlink(&external_state, &state).expect("symlink state");

        let err =
            crate::config::ConfigSnapshot::load(&root).expect_err("must reject symlinked state");
        let msg = err.to_string();
        assert!(msg.contains("symlink"), "expected symlink in error: {msg}");
    }

    #[test]
    fn config_snapshot_rejects_hard_linked_state() {
        let (_tmp, root) = test_repo();
        let external = tempfile::tempdir().expect("tempdir");
        let external_state = external.path().join("state.toml");
        std::fs::write(&external_state, "[stores.evil]\ntarget = \"~/.evil\"\n").expect("write");

        let state = root.join(".stitch/state.toml");
        std::fs::remove_file(&state).expect("rm state");
        std::fs::hard_link(&external_state, &state).expect("hard link state");

        let err =
            crate::config::ConfigSnapshot::load(&root).expect_err("must reject hard-linked state");
        let msg = err.to_string();
        assert!(msg.contains("hard-linked"), "expected hard-linked: {msg}");
    }

    #[test]
    fn config_snapshot_rejects_symlinked_authored() {
        let (_tmp, root) = test_repo();
        let external = tempfile::tempdir().expect("tempdir");
        let external_authored = external.path().join("stitch.toml");
        std::fs::write(
            &external_authored,
            "[stores.app]\nhooks = { pre = \"touch pwned\" }\n",
        )
        .expect("write");

        let authored = root.join("stitch.toml");
        std::fs::remove_file(&authored).expect("rm authored");
        std::os::unix::fs::symlink(&external_authored, &authored).expect("symlink authored");

        let err =
            crate::config::ConfigSnapshot::load(&root).expect_err("must reject symlinked authored");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") && msg.contains("authored"),
            "expected symlink+authored in error: {msg}"
        );
    }

    // --- InventoryCheck tests ---

    fn make_store_dir(root: &Path, name: &str, files: &[&str]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir store");
        for f in files {
            std::fs::write(dir.join(f), format!("contents of {f}")).expect("write file");
        }
    }

    #[test]
    fn inventory_check_passes_for_valid_store() {
        let (_tmp, root) = test_repo();
        make_store_dir(&root, "app", &["file"]);
        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let errors = validate_inventory(&root, &snap.loaded.config);
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    #[test]
    fn inventory_check_detects_symlinked_source_root() {
        let (_tmp, root) = test_repo();
        let external = tempfile::tempdir().expect("tempdir");
        let external_store = external.path().join("evil");
        std::fs::create_dir_all(&external_store).expect("mkdir external");

        let store = root.join("app");
        std::os::unix::fs::symlink(&external_store, &store).expect("symlink store");

        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let errors = validate_inventory(&root, &snap.loaded.config);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, InventoryErrorKind::SymlinkedSourceRoot);
        assert_eq!(errors[0].store, "app");
    }

    #[test]
    fn inventory_check_detects_symlinked_source_root_for_skipped_store() {
        // The key invariant: a platform-skipped store with a symlinked source
        // root is still invalid. "Skipped" changes whether we act, not whether
        // we validate.
        let (_tmp, root) = test_repo();
        let external = tempfile::tempdir().expect("tempdir");
        let external_store = external.path().join("evil");
        std::fs::create_dir_all(&external_store).expect("mkdir external");

        let store = root.join("app");
        std::os::unix::fs::symlink(&external_store, &store).expect("symlink store");

        // Authored config with a when clause that never matches this platform.
        std::fs::write(
            root.join("stitch.toml"),
            "[stores.app]\nwhen = { os = \"nonexistent\" }\n",
        )
        .expect("write authored");
        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let errors = validate_inventory(&root, &snap.loaded.config);
        assert_eq!(errors.len(), 1, "skipped store must still be validated");
        assert_eq!(errors[0].kind, InventoryErrorKind::SymlinkedSourceRoot);
        assert_eq!(errors[0].store, "app");
    }

    #[test]
    fn inventory_check_detects_source_name_collision() {
        let (_tmp, root) = test_repo();
        make_store_dir(&root, "app", &["foo", "foo.tmpl"]);

        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"foo\", \"foo.tmpl\"]\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let errors = validate_inventory(&root, &snap.loaded.config);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, InventoryErrorKind::SourceNameCollision);
        assert_eq!(errors[0].store, "app");
    }

    #[test]
    fn inventory_check_detects_collision_for_skipped_store() {
        let (_tmp, root) = test_repo();
        make_store_dir(&root, "app", &["foo", "foo.tmpl"]);

        std::fs::write(
            root.join("stitch.toml"),
            "[stores.app]\nwhen = { os = \"nonexistent\" }\n",
        )
        .expect("write authored");
        std::fs::write(
            root.join(".stitch/state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"foo\", \"foo.tmpl\"]\n",
        )
        .expect("write state");

        let snap = crate::config::ConfigSnapshot::load(&root).expect("load");
        let errors = validate_inventory(&root, &snap.loaded.config);
        assert_eq!(errors.len(), 1, "skipped store must still be validated");
        assert_eq!(errors[0].kind, InventoryErrorKind::SourceNameCollision);
    }
}
