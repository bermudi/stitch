//! `stitch add` — adopting existing paths, creating empty stores, and path normalisation (split from `tests/cli.rs`).
//!
//! See `support.rs` for shared fixtures (`Repo`, `SymlinkedHomeRepo`, etc.).

#![allow(unused_imports)]
#![allow(clippy::all)]
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::Value;

use crate::support::{
    MatrixHomeEnv, Repo, RestoreMode, SymlinkedHomeRepo, assert_envelope_shape, assert_error_shape,
    assert_plan_summary_fields, is_root, json_output, make_executable, prune_fixture,
    repo_with_bashrc_store,
};

#[allow(unused_imports)]
use super::support as _;

#[test]
fn add_rejects_traversal_in_files() {
    // `add --files ../escape` must fail before the store dir is created, so no
    // orphaned directory is left behind and nothing escapes the target.
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shells");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args([
            "add",
            &target_str,
            "--name",
            "shells",
            "--files",
            "../escape",
        ])
        .assert()
        .failure()
        .stderr(contains("invalid file entry"));

    // Validation ran before create_dir_all — no orphaned store dir, no link.
    assert!(!repo.path().join("shells").exists());
    assert!(!target.exists());
}

#[test]
fn add_dry_run_rejects_invalid_glob_before_previewing() {
    let repo = Repo::new();
    let target = repo.path().join("home").join("shells");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args([
            "add",
            &target_str,
            "--name",
            "shells",
            "--patterns",
            "[",
            "--dry-run",
        ])
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid glob pattern"));
    assert!(!repo.path().join("shells").exists());
    assert!(!target.exists());
}

#[test]
fn add_dry_run_adopt_existing_makes_no_changes() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join(".myrc");
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap(), "--dry-run"])
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("Would add (adopt existing)"));

    // Nothing was moved.
    assert!(src.exists());
    assert!(!repo.path().join("myrc").exists());
}

#[test]
fn add_adopt_file_moves_and_links_back() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join(".myrc");
    fs::write(&src, "data").unwrap();
    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("Added store"));

    // The file should now live at <repo>/myrc/.myrc (File mode layout).
    let in_repo = repo.path().join("myrc").join(".myrc");
    assert!(in_repo.exists());
    assert_eq!(fs::read_to_string(&in_repo).unwrap(), "data");

    // The symlink should be back where the original was, pointing into the repo.
    assert!(src.is_symlink());
    let resolved = fs::read_link(&src).unwrap();
    assert!(resolved.starts_with(repo.path()));
}

#[test]
fn add_adopt_dir_moves_and_links_back() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("Added store"));

    // The directory should now be inside the repo.
    assert!(repo.path().join("myconfig").is_dir());
    // And the original location should be a symlink back.
    assert!(src.is_symlink());
}

#[test]
fn add_adopt_dir_with_trailing_slash() {
    // Regression: `stitch add ~/.config/alacritty/` (trailing slash) used to
    // fail at the link step because symlink(2) rejects a linkpath with a
    // trailing slash. expand_home now strips trailing slashes.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    // Pass the path with a trailing slash.
    let src_str = format!("{}/", src.to_str().unwrap());
    repo.cmd()
        .args(["add", &src_str])
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("Added store"));

    assert!(repo.path().join("myconfig").is_dir());
    assert!(src.is_symlink());
}

#[test]
fn add_adopt_dir_collapses_home_target() {
    // state.toml must record the portable ~-collapsed target, not the raw
    // machine-specific absolute path.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let src = home_path.join(".config").join("nvim");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("init.lua"), "vim config").unwrap();
    let home_str = home_path.to_str().unwrap();

    repo.cmd()
        .args(["add", "~/.config/nvim"])
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("Added store"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains(r#"target = "~/.config/nvim""#),
        "state.toml must record ~-collapsed target:\n{state}"
    );

    // The symlink still resolves into the repo.
    let link = home_path.join(".config").join("nvim");
    assert!(link.is_symlink());
    let resolved = fs::read_link(&link).unwrap();
    assert!(resolved.starts_with(repo.path()));
}

#[test]
fn add_adopt_file_collapses_home_target() {
    // File-mode adopt must collapse the parent directory, not the file itself.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let parent = home_path.join(".config").join("myapp");
    let src = parent.join(".myrc");
    fs::create_dir_all(&parent).unwrap();
    fs::write(&src, "data").unwrap();
    let home_str = home_path.to_str().unwrap();

    repo.cmd()
        .args(["add", "~/.config/myapp/.myrc"])
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("Added store"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains(r#"target = "~/.config/myapp""#),
        "state.toml must record ~-collapsed parent target:\n{state}"
    );
    assert!(
        state.contains(r#"".myrc""#),
        "state.toml must record the adopted file:\n{state}"
    );

    let link = parent.join(".myrc");
    assert!(link.is_symlink());
    let resolved = fs::read_link(&link).unwrap();
    assert!(resolved.starts_with(repo.path()));
    assert_eq!(fs::read_to_string(&link).unwrap(), "data");
}

#[test]
fn add_adopt_file_with_symlinked_home() {
    // Issue #3: when $HOME itself is a symlink, `stitch add ~/.bashrc` must
    // treat the file at the canonical home as the source, not as a foreign
    // whole-dir symlink.
    let env = SymlinkedHomeRepo::new();
    let real_bashrc = env.real_home().join(".bashrc");
    fs::write(&real_bashrc, "my bashrc").unwrap();

    env.cmd()
        .args(["add", "~/.bashrc"])
        .assert()
        .success()
        .stdout(contains("Added store"));

    // The file is now in the repo.
    let in_repo = env.repo().join("bashrc").join(".bashrc");
    assert!(in_repo.exists());
    assert_eq!(fs::read_to_string(&in_repo).unwrap(), "my bashrc");

    // state.toml records the ~-collapsed target, not the literal symlink path.
    let state = fs::read_to_string(env.repo().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains(r#"target = "~""#),
        "state.toml must record ~-collapsed target:\n{state}"
    );
    assert!(
        state.contains(r#"".bashrc""#),
        "state.toml must record the adopted file:\n{state}"
    );

    // The original location is now a symlink back to the repo, reachable
    // through the symlinked HOME.
    let link = env.home_link().join(".bashrc");
    assert!(link.is_symlink(), "home link must be a symlink");
    let resolved = fs::read_link(&link).unwrap();
    assert!(
        resolved.starts_with(env.repo()),
        "link must point into repo: {resolved:?}"
    );

    // Regression: status reports linked, and apply is a no-op.
    env.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("linked"));

    env.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("ok"));
}

#[test]
fn add_rejects_existing_symlink_at_target() {
    let repo = Repo::new();
    let src = repo.path().join("external").join("myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/elsewhere", &src).unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("already a symlink"));

    // Foreign symlinks are conflicts, not replacements: the source must remain
    // an untouched symlink pointing at the original foreign target.
    assert!(src.is_symlink(), "foreign symlink must not be clobbered");
    assert_eq!(
        fs::read_link(&src).unwrap(),
        PathBuf::from("/elsewhere"),
        "foreign symlink must still point where it pointed"
    );
}

#[test]
fn add_rejects_store_name_already_in_config() {
    // Pre-existing state entry for "bashrc" must block adding .bashrc,
    // which would derive the same store name. Nothing should be moved.
    let repo = Repo::new();
    repo.write_state("[stores.bashrc]\ntarget = \"~/.bashrc\"\n");

    let src = repo.path().join("external").join(".bashrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("already exists"));

    // File untouched.
    assert!(src.exists());
    assert_eq!(fs::read_to_string(&src).unwrap(), "data");
}

#[test]
fn add_rejects_when_store_dir_already_exists() {
    // A directory for the derived store name already sits in the repo.
    let repo = Repo::new();
    repo.make_store("myrc", &["stale"]); // creates <repo>/myrc/

    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("already exists"));

    // File untouched; the existing store dir not overwritten.
    assert!(src.exists());
    assert_eq!(fs::read_to_string(&src).unwrap(), "data");
    assert_eq!(
        fs::read_to_string(repo.path().join("myrc").join("stale")).unwrap(),
        "contents of stale"
    );
}

#[test]
fn add_rolls_back_adopt_file_when_record_fails() {
    // Force the state-save step to fail (after move + link succeed) by making
    // the .stitch/ directory unwritable. add must roll back: file restored
    // to its original path, the store dir removed, no partial state left.
    // Skipped under root: root ignores file mode bits, so the failure path
    // can't be triggered and the test would give false confidence.
    if is_root() {
        eprintln!("note: add_rolls_back_adopt_file_when_record_fails skipped under root");
        return;
    }
    let repo = Repo::new();
    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    let stitch_dir = repo.path().join(".stitch");
    let mut perms = fs::metadata(&stitch_dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&stitch_dir, perms).unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .assert()
        .failure();

    // The file is back where it started, intact.
    assert!(src.exists(), "file must be restored on rollback");
    assert_eq!(fs::read_to_string(&src).unwrap(), "data");
    // No orphaned store dir or symlink left in the repo.
    assert!(!repo.path().join("myrc").exists());
    assert!(!src.is_symlink());
}

#[test]
fn add_rolls_back_adopt_dir_when_record_fails() {
    // Symmetric to the file-mode rollback test, but exercising the dir branch
    // of rollback_adopt_move (rename(store_dir, source) directly).
    if is_root() {
        eprintln!("note: add_rolls_back_adopt_dir_when_record_fails skipped under root");
        return;
    }
    let repo = Repo::new();
    let src = repo.path().join("external").join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    let stitch_dir = repo.path().join(".stitch");
    let mut perms = fs::metadata(&stitch_dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&stitch_dir, perms).unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
        .assert()
        .failure();

    // The directory is back where it started, intact.
    assert!(src.exists(), "dir must be restored on rollback");
    assert_eq!(fs::read_to_string(src.join("a.conf")).unwrap(), "a");
    // No orphaned store dir or symlink.
    assert!(!repo.path().join("myconfig").exists());
    assert!(!src.is_symlink());
}

#[test]
fn add_creates_empty_store_and_links() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shells");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &target_str])
        .assert()
        .success()
        .stdout(contains("Added store 'shells'"));

    // Store directory should be created.
    assert!(repo.path().join("shells").is_dir());
    // State should have the entry.
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state_text.contains("shells"));
    // Target symlinked to the empty store.
    assert!(target.is_symlink());
}

#[test]
fn add_file_creates_empty_regular_file_and_link() {
    let repo = Repo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join(".bashrc");

    repo.cmd()
        .args(["add", target.to_str().unwrap(), "--file"])
        .env("HOME", &home)
        .assert()
        .success()
        .stdout(contains("Added store 'bashrc'"));

    let source = repo.path().join("bashrc").join(".bashrc");
    assert!(source.is_file());
    assert_eq!(fs::metadata(&source).unwrap().len(), 0);
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), source);
    let state = fs::read_to_string(repo.path().join(".stitch/state.toml")).unwrap();
    assert!(state.contains("target = \"~\""));
    assert!(state.contains("files = [\".bashrc\"]"));
}

#[test]
fn add_file_dry_run_changes_nothing() {
    let repo = Repo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join(".bashrc");

    repo.cmd()
        .args(["add", target.to_str().unwrap(), "--file", "--dry-run"])
        .env("HOME", &home)
        .assert()
        .success()
        .stdout(contains("Would add (create empty file)"));

    assert!(!target.exists());
    assert!(!repo.path().join("bashrc").exists());
}

#[test]
fn add_file_rejects_symlinked_target_ancestor_before_creating_store() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let gateway = repo.path().join("gateway");
    fs::create_dir_all(&gateway).unwrap();
    std::os::unix::fs::symlink(&gateway, home.path().join(".config")).unwrap();
    let target = home.path().join(".config/.bashrc");

    repo.cmd()
        .args(["add", target.to_str().unwrap(), "--file"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("conflict"));
    assert!(!repo.path().join("bashrc").exists());
    assert!(!target.exists());
}

#[test]
fn add_file_rejects_existing_path_and_incompatible_flags() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join(".bashrc");
    fs::write(&target, "existing").unwrap();

    repo.cmd()
        .args(["add", target.to_str().unwrap(), "--file"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .code(2)
        .stderr(contains("already exists"));
    repo.cmd()
        .args(["add", "~/.zshrc", "--file", "--files", ".zshrc"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .code(2)
        .stderr(contains("cannot be combined"));
    assert_eq!(fs::read_to_string(&target).unwrap(), "existing");
}

#[test]
fn add_to_existing_file_mode_store_adopts_and_records_file() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let store = repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    repo.cmd()
        .arg("apply")
        .env("HOME", home_path)
        .assert()
        .success();
    let zshrc = home_path.join(".zshrc");
    fs::write(&zshrc, "zsh config").unwrap();

    repo.cmd()
        .args(["add", zshrc.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home_path)
        .assert()
        .success()
        .stdout(contains("Added .zshrc to store 'shells'"));

    let adopted = store.join(".zshrc");
    assert_eq!(fs::read_to_string(&adopted).unwrap(), "zsh config");
    assert!(zshrc.is_symlink());
    assert_eq!(fs::read_link(&zshrc).unwrap(), adopted);
    assert!(home_path.join(".bashrc").is_symlink());
    let state = fs::read_to_string(repo.path().join(".stitch/state.toml")).unwrap();
    assert!(state.contains(".bashrc"));
    assert!(state.contains(".zshrc"));
}

#[test]
fn add_to_existing_store_supports_nested_file() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let target_root = home.path().join(".config/app");
    fs::create_dir_all(target_root.join("sub")).unwrap();
    repo.make_store("app", &["base"]);
    repo.write_state("[stores.app]\ntarget = \"~/.config/app\"\nfiles = [\"base\"]\n");
    let source = target_root.join("sub/config");
    fs::write(&source, "nested").unwrap();

    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "app"])
        .env("HOME", home.path())
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(repo.path().join("app/sub/config")).unwrap(),
        "nested"
    );
    assert!(source.is_symlink());
    let state = fs::read_to_string(repo.path().join(".stitch/state.toml")).unwrap();
    assert!(state.contains("sub/config"));
}

#[test]
fn add_to_explicit_entry_is_not_removed_by_authored_ignore() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let store = repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    repo.write_authored("[stores.shells]\nignore = [\".zshrc\"]\n");
    let source = home.path().join(".zshrc");
    fs::write(&source, "keep me").unwrap();

    // Explicit inventory wins over ignore patterns, matching ordinary file
    // mode. --to records the adopted path explicitly in generated state.
    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home.path())
        .assert()
        .success();

    assert_eq!(fs::read_to_string(store.join(".zshrc")).unwrap(), "keep me");
    assert!(source.is_symlink());
}

#[test]
fn add_to_rejects_symlinked_store_parent_without_moving_file() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let store = repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~/.config\"\nfiles = [\".bashrc\"]\n");
    let outside = repo.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, store.join("nested")).unwrap();
    let source = home.path().join(".config/nested/zshrc");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "keep me").unwrap();

    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home.path())
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(&source).unwrap(), "keep me");
    assert!(!outside.join("zshrc").exists());
    assert!(!source.is_symlink());
}

#[test]
fn add_to_rejects_whole_dir_multi_target_and_wrong_target() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    repo.make_store("whole", &["x"]);
    repo.write_state("[stores.whole]\ntarget = \"~/.whole\"\n");
    let source = home.path().join("file");
    fs::write(&source, "data").unwrap();

    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "whole"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .code(2)
        .stderr(contains("not an explicit file-mode store"));
    assert_eq!(fs::read_to_string(&source).unwrap(), "data");
}

#[test]
fn add_to_rejects_hard_linked_file_without_moving() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    let source = home.path().join(".zshrc");
    fs::write(&source, "data").unwrap();
    fs::hard_link(&source, home.path().join(".zshrc-alias")).unwrap();

    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .code(2)
        .stderr(contains("hard-linked"));
    assert_eq!(fs::read_to_string(&source).unwrap(), "data");
    assert!(!repo.path().join("shells/.zshrc").exists());
}

#[test]
fn add_to_rejects_template_peer_without_moving() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let store = repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    fs::write(store.join(".zshrc.tmpl"), "template").unwrap();
    let source = home.path().join(".zshrc");
    fs::write(&source, "data").unwrap();

    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("template source"));
    assert_eq!(fs::read_to_string(&source).unwrap(), "data");
    assert!(!source.is_symlink());
}

#[test]
fn add_to_rejects_source_inside_repository_without_moving() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    let source = repo.path().join("unmanaged").join(".zshrc");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "repo content").unwrap();

    repo.cmd()
        .args(["add", source.to_str().unwrap(), "--to", "shells"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("inside the stitch repository"));
    assert_eq!(fs::read_to_string(&source).unwrap(), "repo content");
    assert!(!repo.path().join("shells/unmanaged/.zshrc").exists());
}

#[test]
fn add_rejects_the_repository_itself_with_a_clear_message() {
    // `add <repo>` must fail with a message naming the repo itself, not a
    // generic "inside the repository" message and not a raw OS error.
    let repo = Repo::new();
    repo.cmd()
        .args(["add", repo.path().to_str().unwrap()])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("cannot add the repository itself"));
}

#[test]
fn add_strips_setuid_bit_from_adopted_file() {
    // A setuid bit on a dotfile is almost always unintentional and git would
    // drop it on clone anyway. `add` must strip setuid/setgid/sticky bits when
    // adopting a file into the repo, and warn the user.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".local").join("bin").join("helper");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "#!/bin/sh\necho hi\n").unwrap();
    // Make it executable + setuid. Any user can set setuid on a file they own.
    let mut perms = fs::metadata(&source).unwrap().permissions();
    perms.set_mode(0o4755);
    fs::set_permissions(&source, perms).unwrap();
    assert!(
        fs::metadata(&source).unwrap().mode() & 0o4000 != 0,
        "setuid must be set before add"
    );

    repo.cmd()
        .args(["add", source.to_str().unwrap()])
        .env("HOME", home.path())
        .assert()
        .success()
        .stderr(contains("stripped privileged bits"));

    // The adopted file in the repo must NOT have setuid. A single-file adopt
    // creates a store directory named after the file, with the file inside it.
    let adopted = repo.path().join("helper").join("helper");
    assert!(
        adopted.is_file(),
        "adopted file must exist at {}",
        adopted.display()
    );
    let mode = fs::metadata(&adopted).unwrap().mode();
    assert_eq!(
        mode & 0o7000,
        0,
        "setuid/setgid/sticky bits must be stripped from adopted file (mode=0o{mode:o})"
    );
    // The executable bits are preserved.
    assert_eq!(mode & 0o111, 0o111, "executable bits must be preserved");
}

#[test]
fn add_to_dry_run_changes_nothing() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    let source = home.path().join(".zshrc");
    fs::write(&source, "data").unwrap();
    let before = fs::read_to_string(repo.path().join(".stitch/state.toml")).unwrap();

    repo.cmd()
        .args([
            "add",
            source.to_str().unwrap(),
            "--to",
            "shells",
            "--dry-run",
        ])
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("Would add to store 'shells'"));

    assert_eq!(fs::read_to_string(&source).unwrap(), "data");
    assert!(!repo.path().join("shells/.zshrc").exists());
    assert_eq!(
        fs::read_to_string(repo.path().join(".stitch/state.toml")).unwrap(),
        before
    );
}

#[test]
fn add_creates_store_with_explicit_name() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &target_str, "--name", "editor"])
        .assert()
        .success()
        .stdout(contains("Added store 'editor'"));

    assert!(repo.path().join("editor").is_dir());
    assert!(target.is_symlink());
}

#[test]
fn add_duplicate_store_errors() {
    // Two different paths that derive the same store name: the second must
    // fail because the store name already exists in config.
    let repo = Repo::new();
    let target1 = repo.path().join("home").join(".config").join("shells");
    let target2 = repo.path().join("home").join(".local").join("shells");
    let t1 = target1.to_string_lossy().into_owned();
    let t2 = target2.to_string_lossy().into_owned();

    repo.cmd().args(["add", &t1]).assert().success();

    repo.cmd()
        .args(["add", &t2])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn add_rejects_existing_store_directory() {
    let repo = Repo::new();
    // Create a directory that collides with the derived store name.
    fs::create_dir_all(repo.path().join("shells")).unwrap();
    let target = repo.path().join("home").join(".config").join("shells");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &target_str])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn add_rejects_existing_file_at_store_path() {
    let repo = Repo::new();
    // A regular file at the store path should also be rejected.
    fs::write(repo.path().join("shells"), "not a dir").unwrap();
    let target = repo.path().join("home").join(".config").join("shells");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &target_str])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn add_rejects_existing_symlink_at_store_path() {
    let repo = Repo::new();
    // A symlink at the store path should also be rejected.
    std::os::unix::fs::symlink("/tmp", repo.path().join("shells")).unwrap();
    let target = repo.path().join("home").join(".config").join("shells");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &target_str])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn add_rolls_back_when_config_save_fails() {
    // apply succeeds (link created) but state.save fails: adopt-style
    // all-or-nothing must undo the link and the empty store dir so no
    // half-applied store is left without a state entry.
    // Skipped under root: root ignores file mode bits, so the failure path
    // can't be triggered and the test would give false confidence.
    if is_root() {
        eprintln!("note: add_rolls_back_when_config_save_fails skipped under root");
        return;
    }
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();

    let stitch_dir = repo.path().join(".stitch");
    let state = stitch_dir.join("state.toml");
    let mut perms = fs::metadata(&stitch_dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&stitch_dir, perms).unwrap();

    repo.cmd().args(["add", &target_str]).assert().failure();

    // Link undone, store dir removed, state has no entry.
    assert!(!target.is_symlink(), "symlink must be removed on rollback");
    assert!(
        !repo.path().join("nvim").exists(),
        "store dir must be removed on rollback"
    );
    let state_text = fs::read_to_string(&state).unwrap();
    assert!(!state_text.contains("nvim"));
}

#[test]
fn add_create_empty_dry_run_makes_no_changes() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &target_str, "--dry-run"])
        .assert()
        .success()
        .stdout(contains("Would add (create empty store)"));

    // Nothing created on disk.
    assert!(!repo.path().join("nvim").exists());
    assert!(!target.exists());
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(!state_text.contains("nvim"));
}

#[test]
fn add_rejects_files_on_existing_path() {
    // --files on an existing path is a user error: the moved content determines
    // the store layout, so --files would be silently ignored. Must error rather
    // than surprise.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join(".myrc");
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap(), "--files", "x"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("only apply when creating a new empty store"));

    // File untouched, no store created.
    assert!(src.exists());
    assert_eq!(fs::read_to_string(&src).unwrap(), "data");
    assert!(!repo.path().join("myrc").exists());
}

#[test]
fn add_rejects_patterns_on_existing_path() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap(), "--patterns", "*"])
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("only apply when creating a new empty store"));

    assert!(src.exists());
    assert!(!repo.path().join("myconfig").exists());
}

#[test]
fn add_normalizes_dotdot_in_path() {
    // A `..` that stays inside $HOME must be normalized away before the
    // target is stored in state.toml.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let home_str = home_path.to_str().unwrap();

    repo.cmd()
        .args(["add", "~/sub/../myconfig"])
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("Added store 'myconfig'"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains(r#"target = "~/myconfig""#),
        "state.toml must record the normalized target:\n{state}"
    );
    assert!(
        !state.contains("sub/.."),
        "state.toml must not contain un-normalized '..':\n{state}"
    );
    assert!(
        !state.contains("sub"),
        "state.toml must not contain the redundant 'sub' component:\n{state}"
    );

    let link = home_path.join("myconfig");
    assert!(link.is_symlink(), "target link must be created");
    let resolved = fs::read_link(&link).unwrap();
    assert!(
        resolved.starts_with(repo.path()),
        "link must point into repo"
    );
}

#[test]
fn add_rejects_dotdot_escaping_home() {
    // A `..` that escapes $HOME must still be rejected after normalization.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();

    repo.cmd()
        .args(["add", "~/../outside"])
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"))
        .stderr(contains("inside $HOME"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state.contains("outside"),
        "state must not record an escaped target"
    );
}

#[test]
fn add_dotdot_target_resolves_correctly() {
    // Regression guard: an existing target path containing `..` is adopted,
    // normalized, linked, and remains correct on re-apply.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let home_str = home_path.to_str().unwrap();

    let real = home_path.join(".config").join("nvim");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("init.lua"), "vim config").unwrap();
    let placeholder = home_path.join(".config").join("placeholder");
    fs::create_dir_all(&placeholder).unwrap();

    repo.cmd()
        .args(["add", "~/.config/placeholder/../nvim"])
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("Added store 'nvim'"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains(r#"target = "~/.config/nvim""#),
        "state.toml must record the normalized target:\n{state}"
    );
    assert!(
        !state.contains("placeholder/.."),
        "state.toml must not contain un-normalized '..':\n{state}"
    );

    let link = home_path.join(".config").join("nvim");
    assert!(link.is_symlink(), "target must be a symlink");
    let resolved = fs::read_link(&link).unwrap();
    assert!(
        resolved.starts_with(repo.path()),
        "link must point into repo"
    );
    assert_eq!(
        fs::read_to_string(link.join("init.lua")).unwrap(),
        "vim config"
    );

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("ok"))
        .stdout(contains("0 conflict"));
}

#[test]
fn adding_template_promotes_existing_whole_directory_link_safely() {
    let repo = Repo::new();
    let store = repo.make_store("git", &["config"]);
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));
    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink(), "initial mode is a directory link");

    fs::write(store.join("new.tmpl"), "new = {{ os }}\n").unwrap();
    repo.cmd().arg("apply").assert().success();

    assert!(target.is_dir());
    assert!(
        !target.is_symlink(),
        "file-mode promotion must replace the root link with a real directory"
    );
    assert!(target.join("config").is_symlink());
    assert!(target.join("new").is_symlink());
    assert_eq!(
        fs::read_to_string(target.join("config")).unwrap(),
        "contents of config"
    );
    assert!(
        store.join("new").symlink_metadata().is_err(),
        "per-file link must not be written through the old root link into the store"
    );
}

#[test]
fn add_rejects_target_outside_home() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();

    let outside = tempfile::tempdir().unwrap();
    let src = outside.path().join("myconfig");
    fs::create_dir_all(&src).unwrap();
    let src_str = src.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", &src_str])
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"));

    assert!(!repo.path().join("myconfig").exists());
}

#[test]
fn add_io_error_includes_path() {
    // Make the repo root unwritable. The adopt path's create_dir_all for the
    // store directory should fail with context naming the store path.
    if is_root() {
        eprintln!("note: add_io_error_includes_path skipped under root");
        return;
    }
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".myrc");
    fs::write(&source, "data").unwrap();

    fs::set_permissions(repo.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = RestoreMode {
        path: repo.path(),
        mode: 0o755,
    };

    repo.cmd()
        .args(["add", source.to_str().unwrap()])
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("myrc"))
        .stderr(contains("Permission denied"))
        .stderr(contains("I/O error").not());
}

#[test]
fn add_rejects_terminal_symlink_behind_dotdot() {
    // `add ~/sub/../link` where `link` is a terminal symlink must reject the
    // link, never adopt its referent (which would then be moved into the repo
    // and the original link repointed during reconciliation).
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    let real = home.path().join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("data"), "inside").unwrap();
    let link = home.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    repo.cmd()
        .args(["add", "~/sub/../link"])
        .env("HOME", home_str)
        .assert()
        .failure()
        .stderr(contains("already a symlink"));

    assert!(link.is_symlink(), "original link must be untouched");
    assert_eq!(fs::read_link(&link).unwrap(), real);
    assert!(real.join("data").exists(), "referent must be untouched");
    assert!(
        !repo.path().join("link").exists(),
        "no store may be created from the referent"
    );
    assert!(
        !repo.path().join("real").exists(),
        "referent must not be adopted into the repo"
    );
}

// ===========================================================================
// Phase 0 characterization tests (2026-08-13 module refactor).
//
// These pin the `add` rollback machinery's current behavior before the
// mechanical module move. They target the gaps identified in the plan's
// coverage audit: `--to` state-save rollback, and the cleanup/discard
// branches that fire when `apply_store` fails at link creation on the
// create-empty paths.
//
// All three are deterministic: the failure condition (read-only directory)
// is set up *before* the command starts, and the failure point is naturally
// later in the sequence than preflight (preflight_add_target checks for
// symlink conflicts, not parent writability; atomic_write creates a temp
// file in .stitch/ which requires .stitch/ to be writable). No filesystem
// race is involved.
//
// Skipped under root: root ignores file mode bits, so the read-only
// permission can't trigger the failure path and the test would give false
// confidence.
// ===========================================================================

#[test]
fn add_to_rolls_back_when_state_save_fails() {
    // `add --to` moves the file into the store, creates the symlink, then
    // saves state. If state save fails (`.stitch/` read-only), rollback must
    // remove the symlink and restore the file to its original path so no
    // half-adopted entry is left without a state record.
    if is_root() {
        eprintln!("note: add_to_rolls_back_when_state_save_fails skipped under root");
        return;
    }
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    // Set up an existing file-mode store and apply it so the target root
    // exists as a real directory with an existing link.
    repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    repo.cmd()
        .arg("apply")
        .env("HOME", home_path)
        .assert()
        .success();
    assert!(home_path.join(".bashrc").is_symlink());

    // A new file to adopt into the store via --to.
    let zshrc = home_path.join(".zshrc");
    fs::write(&zshrc, "zsh config").unwrap();

    // Make .stitch/ read-only: the lock file (already present, opened by fd)
    // and state.toml (read) are fine, but atomic_write's temp-file creation
    // in .stitch/ will fail with EACCES.
    let stitch_dir = repo.path().join(".stitch");
    let mut perms = fs::metadata(&stitch_dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&stitch_dir, perms).unwrap();

    repo.cmd()
        .args(["add", zshrc.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home_path)
        .assert()
        .failure();

    // The file is back where it started, intact, not a symlink.
    assert!(zshrc.exists(), "file must be restored on rollback");
    assert!(
        !zshrc.is_symlink(),
        "file must not be a symlink after rollback"
    );
    assert_eq!(fs::read_to_string(&zshrc).unwrap(), "zsh config");

    // No orphaned entry in the store dir.
    assert!(
        !repo.path().join("shells").join(".zshrc").exists(),
        "store dir must not retain the moved file after rollback"
    );

    // State must not record .zshrc.
    let state = fs::read_to_string(stitch_dir.join("state.toml")).unwrap();
    assert!(
        !state.contains(".zshrc"),
        "state must not record the rolled-back file"
    );
}

#[test]
fn add_file_rolls_back_when_link_creation_fails() {
    // `add --file` creates an empty file in the store dir, then `apply_store`
    // creates the symlink at the target. If the target parent ($HOME) is
    // read-only, link creation fails and the cleanup/discard branches must
    // remove both the empty file and the store dir so no orphaned content
    // is left behind.
    //
    // Deterministic: $HOME is made read-only *before* the command. The store
    // dir and empty file are created in the repo (writable); the target
    // parent already exists so prepare_target_parents is a no-op; the first
    // step that writes to $HOME is the symlink creation, which fails.
    if is_root() {
        eprintln!("note: add_file_rolls_back_when_link_creation_fails skipped under root");
        return;
    }
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let target = home_path.join(".bashrc");

    // Make $HOME read-only: the symlink at ~/.bashrc cannot be created.
    let mut perms = fs::metadata(home_path).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(home_path, perms).unwrap();
    let _restore = RestoreMode {
        path: home_path,
        mode: 0o755,
    };

    repo.cmd()
        .args(["add", target.to_str().unwrap(), "--file"])
        .env("HOME", home_path)
        .assert()
        .failure();

    // No orphaned store dir or empty file left in the repo.
    assert!(
        !repo.path().join("bashrc").exists(),
        "store dir must be removed on rollback"
    );
    // Target was never created.
    assert!(
        !target.exists(),
        "target link must not exist after rollback"
    );
    // State must not record the store.
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state.contains("bashrc"),
        "state must not record the rolled-back store"
    );
}

#[test]
fn add_create_empty_rolls_back_when_link_creation_fails() {
    // `add` (create empty store) creates the store dir, then `apply_store`
    // creates the symlink at the target. If the target parent is read-only,
    // link creation fails and the discard branch must remove the store dir.
    //
    // Deterministic: the target parent ($HOME/.config) is made read-only
    // *before* the command. The store dir is in the repo (writable); the
    // target parent already exists so prepare_target_parents is a no-op; the
    // first step that writes to the target parent is the symlink creation.
    if is_root() {
        eprintln!("note: add_create_empty_rolls_back_when_link_creation_fails skipped under root");
        return;
    }
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let config_dir = home_path.join(".config");
    fs::create_dir_all(&config_dir).unwrap();
    let target = config_dir.join("nvim");

    // Make $HOME/.config read-only: the symlink at ~/.config/nvim cannot be
    // created, but the store dir in the repo can.
    let mut perms = fs::metadata(&config_dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&config_dir, perms).unwrap();
    let _restore = RestoreMode {
        path: &config_dir,
        mode: 0o755,
    };

    repo.cmd()
        .args(["add", target.to_str().unwrap()])
        .env("HOME", home_path)
        .assert()
        .failure();

    // No orphaned store dir left in the repo.
    assert!(
        !repo.path().join("nvim").exists(),
        "store dir must be removed on rollback"
    );
    // Target was never created.
    assert!(
        !target.exists(),
        "target link must not exist after rollback"
    );
    // State must not record the store.
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state.contains("nvim"),
        "state must not record the rolled-back store"
    );
}

#[test]
fn bulk_add_partial_failure_json() {
    // Two paths derive the same store name ("bashrc"). The duplicate is
    // detected before any apply, so neither path is committed — no partial
    // bulk add despite the "validate all paths first" contract.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let bashrc = home.path().join(".bashrc");
    let config_bashrc = home.path().join(".config").join(".bashrc");
    fs::write(&bashrc, "bashrc").unwrap();
    fs::create_dir_all(config_bashrc.parent().unwrap()).unwrap();
    fs::write(&config_bashrc, "config bashrc").unwrap();

    let output = repo
        .cmd()
        .env("HOME", home.path())
        .args([
            "--json",
            "add",
            bashrc.to_str().unwrap(),
            config_bashrc.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "duplicate store name in bulk add must exit non-zero"
    );
    assert_eq!(output.status.code(), Some(2), "exit code must be 2 (usage)");

    let value = json_output(&output);
    assert_envelope_shape(&value, "add", false);
    assert_error_shape(&value, "usage", 2);
    assert!(value["data"].is_object(), "data must not be null");
    assert_eq!(value["data"]["all_ok"], false);
    let results = value["data"]["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    // Both paths are rejected — no partial commit.
    assert_eq!(results[0]["ok"], false);
    assert_eq!(results[1]["ok"], false);
    let err = value["error"]["message"].as_str().expect("error message");
    assert!(
        err.contains("both derive store name"),
        "error should describe the duplicate store name conflict: {err}"
    );

    // Neither path was committed.
    assert!(!repo.path().join("bashrc").exists());
    assert!(!bashrc.is_symlink());
    assert!(!config_bashrc.is_symlink());
}

#[test]
fn bulk_add_validation_error_json() {
    // Validation fails for a path inside the repo. The error envelope must
    // carry the bulk data so the agent can see per-path status.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let good = home.path().join(".good");
    fs::write(&good, "good").unwrap();

    let output = repo
        .cmd()
        .env("HOME", home.path())
        .args([
            "--json",
            "add",
            good.to_str().unwrap(),
            ".stitch/state.toml",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "validation failure must exit non-zero"
    );

    let value = json_output(&output);
    assert_envelope_shape(&value, "add", false);
    assert_error_shape(&value, "usage", 2);
    assert!(value["data"].is_object(), "data must not be null");
    assert_eq!(value["data"]["all_ok"], false);
    let results = value["data"]["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["store"], "good");
    assert!(results[0]["error"].is_null());
    assert_eq!(results[1]["ok"], false);
    assert_eq!(results[1]["store"], "state.toml");
    let err = results[1]["error"].as_str().expect("error string");
    assert!(
        err.contains("inside the stitch repository"),
        "per-path error should describe the repo containment violation: {err}"
    );

    // No filesystem mutation should have occurred: validation failed before
    // Phase 2 (apply), so the valid path must NOT have been committed.
    assert!(
        !repo.path().join("good").exists(),
        "valid path must not be committed when another path fails validation"
    );
    assert!(
        !good.is_symlink(),
        "valid path must not be symlinked when another path fails validation"
    );
}

#[test]
fn bulk_add_text_suppresses_validation_output() {
    // Real bulk add in text mode must not print dry-run "Would add" lines from
    // the validation phase; only the real "Added store" output should appear.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let bashrc = home.path().join(".bashrc");
    let nvim = home.path().join(".config").join("nvim");
    fs::write(&bashrc, "bashrc").unwrap();
    fs::create_dir_all(&nvim).unwrap();
    fs::write(nvim.join("init.lua"), "nvim").unwrap();

    repo.cmd()
        .env("HOME", home.path())
        .args(["add", bashrc.to_str().unwrap(), nvim.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Added store").and(contains("Would add").not()));

    assert!(repo.path().join("bashrc").join(".bashrc").exists());
    assert!(repo.path().join("nvim").join("init.lua").exists());
    assert!(bashrc.is_symlink());
    assert!(nvim.is_symlink());
}

#[test]
fn bulk_add_dry_run_text() {
    // Bulk dry-run in text mode should show the would-add previews and not
    // print real add output or mutate the filesystem.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let bashrc = home.path().join(".bashrc");
    let nvim = home.path().join(".config").join("nvim");
    fs::write(&bashrc, "bashrc").unwrap();
    fs::create_dir_all(&nvim).unwrap();
    fs::write(nvim.join("init.lua"), "nvim").unwrap();

    repo.cmd()
        .env("HOME", home.path())
        .args([
            "add",
            bashrc.to_str().unwrap(),
            nvim.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(contains("Would add").and(contains("Added store").not()));

    assert!(!repo.path().join("bashrc").exists());
    assert!(!repo.path().join("nvim").exists());
    assert!(!bashrc.is_symlink());
    assert!(!nvim.is_symlink());
}

/// `add --to <store> --json` adopts a file into an existing file-mode store
/// and reports the created symlink in the post-op envelope.
#[test]
fn add_to_store_json_post_op() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    repo.make_store("shells", &[".bashrc"]);
    repo.write_state("[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n");
    repo.cmd()
        .arg("apply")
        .env("HOME", home_path)
        .assert()
        .success();
    assert!(home_path.join(".bashrc").is_symlink());

    let zshrc = home_path.join(".zshrc");
    fs::write(&zshrc, "zsh config").unwrap();

    let output = repo
        .cmd()
        .args(["--json", "add", zshrc.to_str().unwrap(), "--to", "shells"])
        .env("HOME", home_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "add --to --json must succeed");
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);

    let data = &value["data"];
    assert_eq!(data["store"], "shells");
    assert_eq!(data["target"], "~");
    assert_eq!(data["mode"], "add-to-store");
    assert_eq!(data["source"], "~/.zshrc");
    let files = data["files"].as_array().expect("files array");
    assert_eq!(files.as_slice(), &[Value::String(".zshrc".into())]);
    assert!(data["patterns"].as_array().map_or(true, |a| a.is_empty()));

    let link_str = data["link_created"].as_str().expect("link_created string");
    let link = Path::new(link_str);
    assert!(link.is_symlink(), "link_created must be a symlink");
    let adopted = repo.path().join("shells").join(".zshrc");
    assert_eq!(fs::read_to_string(&adopted).unwrap(), "zsh config");
    assert_eq!(fs::read_link(link).unwrap(), adopted);

    assert!(zshrc.is_symlink());
    assert_eq!(fs::read_link(&zshrc).unwrap(), adopted);

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.contains(".bashrc"));
    assert!(state.contains(".zshrc"));
}

/// `add --file --json` creates an empty file in a new store, links it, and
/// reports the post-op shape.
#[test]
fn add_file_json_post_op() {
    let repo = Repo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join(".bashrc");

    let output = repo
        .cmd()
        .args(["--json", "add", target.to_str().unwrap(), "--file"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success(), "add --file --json must succeed");
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);

    let data = &value["data"];
    assert_eq!(data["store"], "bashrc");
    assert_eq!(data["target"], "~");
    assert_eq!(data["mode"], "create-file");
    assert!(data["source"].is_null());
    let files = data["files"].as_array().expect("files array");
    assert_eq!(files.as_slice(), &[Value::String(".bashrc".into())]);
    assert!(data["patterns"].as_array().map_or(true, |a| a.is_empty()));

    let source_file = repo.path().join("bashrc").join(".bashrc");
    assert!(source_file.is_file());
    assert_eq!(fs::metadata(&source_file).unwrap().len(), 0);
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), source_file);

    let link_str = data["link_created"].as_str().expect("link_created string");
    assert_eq!(Path::new(link_str), target);

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.contains(r#"target = "~""#));
    assert!(state.contains(r#"".bashrc""#));
}

#[test]
fn bulk_add_phase2_partial_failure_json() {
    // Adding a directory and then a file inside it in the same bulk invocation:
    // the first path succeeds and moves the directory into the repo + symlinks
    // it back, so the second path now resolves inside the repo and fails in
    // Phase 2 (apply). The earlier path is kept, the bulk error is "mixed",
    // and the per-path results show one ok and one not ok.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".config");
    let init = config_dir.join("nvim").join("init.lua");
    fs::create_dir_all(init.parent().unwrap()).unwrap();
    fs::write(&init, "vim").unwrap();

    let output = repo
        .cmd()
        .env("HOME", home.path())
        .args([
            "--json",
            "add",
            config_dir.to_str().unwrap(),
            init.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "bulk add must fail");
    assert_eq!(
        output.status.code(),
        Some(11),
        "exit code must be 11 (mixed)"
    );

    let value = json_output(&output);
    assert_envelope_shape(&value, "add", false);
    assert_error_shape(&value, "mixed", 11);
    assert!(value["data"].is_object(), "data must not be null");
    assert_eq!(value["data"]["all_ok"], false);
    let results = value["data"]["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["store"], "config");
    assert_eq!(results[1]["ok"], false);
    assert_eq!(results[1]["store"], "init.lua");
    let err = results[1]["error"].as_str().expect("error string");
    assert!(
        err.contains("inside the stitch repository"),
        "per-path error should describe repo containment: {err}"
    );

    // The first path was committed (store + symlink), the second was not.
    assert!(
        repo.path()
            .join("config")
            .join("nvim")
            .join("init.lua")
            .exists()
    );
    assert!(config_dir.is_symlink());
    assert!(!repo.path().join("init.lua").exists());
}

/// Red line: `add` must never rewrite the authored `stitch.toml`.
/// Mutations write `.stitch/state.toml` only, preserving user comments and
/// hand-formatting byte-for-byte.
#[test]
fn add_preserves_authored_stitch_toml_bytes() {
    let repo = Repo::new();
    let authored = r#"# Authored config for stitch.
# This file is hand-edited and must never be rewritten by the tool.

[stores.legacy]
# A store I configured by hand.
ignore = ["*.bak"]
"#;
    repo.write_authored(authored);
    let before = fs::read_to_string(repo.path().join("stitch.toml")).unwrap();

    // Add a different store. The target is inside the test $HOME.
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join(".myapp");

    repo.cmd()
        .args(["add", target.to_str().unwrap(), "--name", "myapp"])
        .assert()
        .success()
        .stdout(contains("Added store 'myapp'"));

    // The new store should be recorded in generated state only.
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.myapp]"),
        "state must record the new store:\n{state}"
    );

    // Authored config must be byte-for-byte identical.
    let after = fs::read_to_string(repo.path().join("stitch.toml")).unwrap();
    assert_eq!(before, after, "add must not rewrite stitch.toml");
}
