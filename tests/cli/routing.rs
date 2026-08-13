//! Repository discovery (`--repo` / `STITCH_REPO`) and `$HOME` validation (split from `tests/cli.rs`).
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
fn apply_outside_repo_errors() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("STITCH_REPO")
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("does not point at a stitch repo"));
}

#[test]
fn list_outside_repo_errors() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("STITCH_REPO")
        .arg("list")
        .assert()
        .failure()
        .stderr(contains("does not point at a stitch repo"));
}

/// Run `stitch --repo <path> list` from an unrelated cwd and confirm it
/// operates on the referenced repo.
#[test]
fn repo_flag_works_from_outside() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.write_state(
        r#"
[stores.nvim]
target = "~/.config/nvim"
"#,
    );

    // Run from a completely different tempdir.
    let elsewhere = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(elsewhere.path())
        .env_remove("STITCH_REPO")
        .arg("--repo")
        .arg(repo.path())
        .arg("list")
        .assert()
        .success()
        .stdout(contains("nvim"));
}

/// `STITCH_REPO` env var alone is enough to operate from outside the repo.
#[test]
fn stitch_repo_env_works_from_outside() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let elsewhere = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(elsewhere.path())
        .env("STITCH_REPO", repo.path())
        .arg("list")
        .assert()
        .success()
        .stdout(contains("bashrc"));
}

/// `--repo` takes precedence over `STITCH_REPO` when both are set.
#[test]
fn repo_flag_overrides_env() {
    let repo = Repo::new();
    repo.make_store("real", &["f"]);
    repo.write_state(
        r#"
[stores.real]
target = "~/.config/real"
"#,
    );

    // A second repo that would produce different output.
    let decoy = Repo::new();
    decoy.make_store("decoy", &["f"]);
    decoy.write_state(
        r#"
[stores.decoy]
target = "~/.config/decoy"
"#,
    );

    let elsewhere = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(elsewhere.path())
        .env("STITCH_REPO", decoy.path())
        .arg("--repo")
        .arg(repo.path())
        .arg("list")
        .assert()
        .success()
        .stdout(contains("real"))
        .stdout(contains("decoy").not());
}

/// `--repo` pointing at a directory without `.stitch/` is rejected — a typo
/// can't silently operate on the wrong directory.
#[test]
fn repo_flag_rejects_non_repo_path() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("STITCH_REPO")
        .arg("--repo")
        .arg(dir.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(contains("does not point at a stitch repo"));
}

/// `STITCH_REPO` pointing at a non-repo is rejected with a clear message.
#[test]
fn stitch_repo_env_rejects_non_repo_path() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .env("STITCH_REPO", dir.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(contains("does not point at a stitch repo"));
}

/// `--repo` accepts a relative path, resolved against cwd.
#[test]
fn repo_flag_accepts_relative_path() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.write_state(
        r#"
[stores.nvim]
target = "~/.config/nvim"
"#,
    );

    // Run from a sibling tempdir, referencing the repo by a relative path.
    // We can't easily construct a relative path between two tempdirs, so run
    // from the repo's parent and use the basename.
    let parent = repo.path().parent().unwrap();
    let basename = repo.path().file_name().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(parent)
        .env_remove("STITCH_REPO")
        .arg("--repo")
        .arg(basename)
        .arg("list")
        .assert()
        .success()
        .stdout(contains("nvim"));
}

/// `STITCH_REPO` expands `~` to the user's home directory.
#[test]
fn stitch_repo_env_expands_tilde() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    // Set HOME to the repo's parent so `~/basename` resolves to the repo.
    let parent = repo.path().parent().unwrap();
    let basename = repo.path().file_name().unwrap();
    let tilde_path = format!("~/{}", basename.to_string_lossy());

    let elsewhere = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(elsewhere.path())
        .env("HOME", parent)
        .env("STITCH_REPO", &tilde_path)
        .arg("list")
        .assert()
        .success()
        .stdout(contains("bashrc"));
}

#[test]
fn status_fails_when_home_unset() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    repo.cmd()
        .env_remove("HOME")
        .arg("status")
        .assert()
        .failure()
        .stderr(contains("$HOME"))
        .stderr(contains("not set"));
}

#[test]
fn status_fails_when_home_empty() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    repo.cmd()
        .env("HOME", "")
        .arg("status")
        .assert()
        .failure()
        .stderr(contains("$HOME"))
        .stderr(contains("empty"));
}

#[test]
fn apply_fails_when_home_does_not_exist() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.write_state(
        r#"
[stores.nvim]
target = "~/.config/nvim"
"#,
    );

    let home_parent = tempfile::tempdir().unwrap();
    let bogus_home = home_parent.path().join("ghosthome");
    let bogus = bogus_home.to_string_lossy();

    repo.cmd()
        .env("HOME", bogus.as_ref())
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("$HOME"))
        .stderr(contains("does not exist"));

    assert!(
        !bogus_home.exists(),
        "stitch must not create a bogus $HOME directory"
    );
}

#[test]
fn status_works_with_existing_home() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let home = tempfile::tempdir().unwrap();
    repo.cmd()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(contains("bashrc"));
}

#[test]
fn apply_creates_subdir_under_existing_home() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.write_state(
        r#"
[stores.nvim]
target = "~/.config/nvim"
"#,
    );

    let home = tempfile::tempdir().unwrap();
    repo.cmd()
        .env("HOME", home.path())
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("created"));

    assert!(home.path().join(".config").is_dir());
    assert!(home.path().join(".config/nvim").is_symlink());
}

#[test]
fn apply_noop_with_symlinked_home_target_root() {
    // Issue #3 regression: an already-correct file-mode link through a
    // symlinked $HOME must report "ok" on apply, not "conflict".
    let env = SymlinkedHomeRepo::new();
    let real_bashrc = env.real_home().join(".bashrc");
    fs::write(&real_bashrc, "my bashrc").unwrap();

    env.cmd().args(["add", "~/.bashrc"]).assert().success();

    // Re-apply should be a clean no-op.
    env.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("ok"))
        .stdout(contains("0 conflict"));

    // The link is still correct.
    let link = env.home_link().join(".bashrc");
    assert!(link.is_symlink());
    assert_eq!(fs::read_to_string(&link).unwrap(), "my bashrc");
}
