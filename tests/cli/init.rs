//! Initialization and migration (`stitch init` / `stitch migrate`) (split from `tests/cli.rs`).
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
fn init_creates_config_in_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("Initialized stitch config"));

    // Post-split: stitch.toml + .stitch/state.toml are created; the v0.2
    // .stitch/config.toml is not.
    let authored = dir.path().join("stitch.toml");
    let state = dir.path().join(".stitch").join("state.toml");
    let legacy = dir.path().join(".stitch").join("config.toml");
    assert!(authored.exists(), "stitch.toml must be created");
    assert!(state.exists(), "state.toml must be created");
    assert!(!legacy.exists(), ".stitch/config.toml must not be created");

    // The authored header documents that the tool never rewrites it.
    let authored_text = fs::read_to_string(&authored).unwrap();
    assert!(
        authored_text.contains("the tool never rewrites this"),
        "stitch.toml should carry the read-only header"
    );

    // Trust foundation: .gitignore covers staging; render root is 0700.
    let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(
        gi.lines().any(|l| l.trim() == ".stitch/render/"),
        ".gitignore must contain .stitch/render/"
    );
    let render = dir.path().join(".stitch").join("render");
    assert!(render.is_dir(), ".stitch/render/ must be created");
    let mode = fs::metadata(&render).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, ".stitch/render must be 0700, got {mode:04o}");
}

#[test]
fn init_fails_when_config_already_exists() {
    let repo = Repo::new();
    repo.cmd()
        .arg("init")
        .assert()
        .failure()
        .stderr(contains("config already exists"));
}

#[test]
fn init_fails_on_v02_repo() {
    // A v0.2-only repo: init must point at `migrate`, not silently re-init.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "vars = {}\n",
    )
    .unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(contains("stitch migrate"));
}

#[test]
fn init_fails_when_state_already_exists() {
    // An existing .stitch/state.toml (e.g. from a script or partial migration)
    // must not be silently overwritten.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    let state = dir.path().join(".stitch").join("state.toml");
    let original = "[stores.bash]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n";
    fs::write(&state, original).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(contains("state already exists"));

    // The pre-existing state must be preserved byte-for-byte.
    let after = fs::read_to_string(&state).unwrap();
    assert_eq!(
        after, original,
        "pre-existing state.toml must not be overwritten"
    );
}

#[test]
fn init_refuses_dangling_state_symlink() {
    // A dangling .stitch/state.toml symlink must not be silently replaced
    // by a regular file.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    let state = dir.path().join(".stitch").join("state.toml");
    std::os::unix::fs::symlink("some/nonexistent", &state).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(contains("state already exists"));

    // The symlink is still a dangling symlink, not replaced by a regular file.
    assert!(state.is_symlink(), "state.toml must remain a symlink");
    assert_eq!(
        fs::read_link(&state).unwrap(),
        Path::new("some/nonexistent")
    );
    assert!(!state.exists(), "state.toml must still be dangling");
}

#[test]
fn init_refuses_dangling_stitch_toml_symlink() {
    // A dangling stitch.toml symlink must not be overwritten by `init`.
    let dir = tempfile::tempdir().unwrap();
    let authored = dir.path().join("stitch.toml");
    std::os::unix::fs::symlink("some/nonexistent", &authored).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(contains("config already exists"));

    assert!(authored.is_symlink(), "stitch.toml must remain a symlink");
    assert_eq!(
        fs::read_link(&authored).unwrap(),
        Path::new("some/nonexistent")
    );
}

/// `init` is cwd-anchored and ignores `--repo` — it creates the repo in the
/// current directory, not at the --repo path.
#[test]
fn init_ignores_repo_flag() {
    let elsewhere = tempfile::tempdir().unwrap();
    let bogus = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(elsewhere.path())
        .env_remove("STITCH_REPO")
        .arg("--repo")
        .arg(bogus.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("Initialized stitch config"));

    // Created in `elsewhere` (cwd), not in `bogus` (--repo).
    assert!(elsewhere.path().join(".stitch").is_dir());
    assert!(!bogus.path().join(".stitch").is_dir());
}

/// v0.2-only repos get an actionable error pointing at migrate (item 5): every
/// read command errors, not just apply.
#[test]
fn v02_repo_errors_on_apply_with_migrate_hint() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "vars = {}\n\n[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("stitch migrate"));
}

#[test]
fn v02_repo_errors_on_list_with_migrate_hint() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "vars = {}\n",
    )
    .unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(contains("stitch migrate"));
}

/// Stale-config warning (item 5): both files present → new format wins, and a
/// warning is printed to stderr.
#[test]
fn both_files_present_uses_new_format_and_warns() {
    let repo = Repo::new();
    repo.write_state("[stores.nvim]\ntarget = \"~/.config/nvim\"\n");
    // Stale v0.2 file alongside.
    fs::write(repo.path().join(".stitch").join("config.toml"), "# stale\n").unwrap();

    repo.cmd()
        .arg("list")
        .assert()
        .success()
        .stderr(contains("stale v0.2"));
}

/// `stitch migrate` converts a v0.2 repo deterministically: authored half →
/// stitch.toml, inventory half → state.toml, original preserved as .bak.
/// migrate is comment-lossy by design (structural conversion); the note is
/// printed so the user can re-add comments.
#[test]
fn migrate_splits_v02_repo_and_backs_up_original() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    // A representative v0.2 config: flat target, behavior, and a comment that
    // anchors the comment-lossy assertion.
    let original = "\
# my dotfiles — this comment must NOT survive into stitch.toml
vars = { editor = \"nvim\" }

[stores.nvim]
target = \"~/.config/nvim\"

[stores.shells]
target = \"~\"
files = [\".bashrc\"]
ignore = [\"*.bak\"]
when = { os = \"linux\" }
";
    fs::write(dir.path().join(".stitch").join("config.toml"), original).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .success()
        .stderr(contains("comments"));

    // stitch.toml + state.toml now exist.
    let authored = fs::read_to_string(dir.path().join("stitch.toml")).unwrap();
    let state = fs::read_to_string(dir.path().join(".stitch").join("state.toml")).unwrap();

    // Authored half carries vars + behavior (ignore, when), NOT inventory.
    assert!(authored.contains("editor = \"nvim\""));
    assert!(authored.contains("[stores.shells]"));
    assert!(authored.contains("ignore"));
    assert!(authored.contains("when"));
    // migrate is comment-lossy: the v0.2 comment must not appear.
    assert!(!authored.contains("my dotfiles"));

    // Generated half carries inventory (target, files), NOT behavior.
    assert!(state.contains("[stores.nvim]"));
    assert!(state.contains("~/.config/nvim"));
    assert!(state.contains("[stores.shells]"));
    assert!(state.contains(".bashrc"));
    // The generated file starts with the tool-owned header.
    assert!(state.starts_with("# Generated by stitch"));

    // Original preserved as .bak (comments intact — the recovery path).
    let backup = fs::read_to_string(dir.path().join(".stitch").join("config.toml.bak")).unwrap();
    assert!(backup.contains("my dotfiles"));
    // The old config.toml is gone (renamed to .bak).
    assert!(!dir.path().join(".stitch").join("config.toml").exists());

    // The migrated repo now works with the new commands.
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(contains("nvim"));
}

/// `stitch migrate --json` performs the real migration and emits a post-op
/// envelope with the authored/state paths and contents, so an agent can verify
/// the write without re-reading the files.
#[test]
fn migrate_json_emits_post_op_envelope() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    let original = "\
vars = { editor = \"nvim\" }

[stores.nvim]
target = \"~/.config/nvim\"
";
    fs::write(dir.path().join(".stitch").join("config.toml"), original).unwrap();

    let output = Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .args(["--json", "migrate"])
        .output()
        .unwrap();
    assert!(output.status.success(), "migrate --json must succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON envelope");
    assert_eq!(value["schema"], 1);
    assert_eq!(value["command"], "migrate");
    assert_eq!(value["ok"], true);
    assert!(value["data"]["authored_path"].is_string());
    assert!(value["data"]["state_path"].is_string());
    assert!(value["data"]["authored"].is_string());
    assert!(value["data"]["state"].is_string());
    // The warning about comments being dropped is carried in the envelope.
    let warnings = value["warnings"].as_array().expect("warnings array");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.contains("comments"))),
        "warnings should mention comments: {warnings:?}"
    );
    // The files were actually written.
    assert!(dir.path().join("stitch.toml").exists());
    assert!(dir.path().join(".stitch").join("state.toml").exists());
    assert!(dir.path().join(".stitch").join("config.toml.bak").exists());
}

/// `stitch migrate --dry-run` previews without writing.
#[test]
fn migrate_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .args(["migrate", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("stitch.toml"));

    // Nothing written.
    assert!(!dir.path().join("stitch.toml").exists());
    assert!(dir.path().join(".stitch").join("config.toml").exists());
    assert!(!dir.path().join(".stitch").join("config.toml.bak").exists());
}

/// migrate refuses to overwrite an existing stitch.toml.
#[test]
fn migrate_refuses_when_stitch_toml_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "vars = {}\n",
    )
    .unwrap();
    fs::write(dir.path().join("stitch.toml"), "").unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("refusing to overwrite"));
}

/// migrate fails *before* writing anything when the .bak backup target already
/// exists — the fail-before-mutate invariant the other writers uphold. A
/// pre-existing .bak must not leave the repo half-migrated with the original
/// stranded.
#[test]
fn migrate_fails_before_writing_when_bak_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();
    // Plant a prior backup that would collide.
    fs::write(dir.path().join(".stitch").join("config.toml.bak"), "old").unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("config.toml.bak already exists"));

    // Nothing was written: no stitch.toml, no state.toml, original intact.
    assert!(
        !dir.path().join("stitch.toml").exists(),
        "must not write stitch.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("state.toml").exists(),
        "must not write state.toml"
    );
    assert!(
        dir.path().join(".stitch").join("config.toml").exists(),
        "original intact"
    );
}

/// migrate refuses to overwrite an existing state.toml and fails before
/// writing anything — the fail-before-mutate invariant the other writers
/// uphold. A pre-existing state.toml must be preserved and the legacy
/// config must not be renamed.
#[test]
fn migrate_refuses_when_state_toml_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();
    // Plant an existing state.toml that would be silently overwritten.
    let state_path = dir.path().join(".stitch").join("state.toml");
    fs::write(&state_path, "pre-existing state content\n").unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("state.toml"))
        .stderr(contains("already exists"))
        .stderr(contains("refusing to overwrite"));

    // The pre-existing state.toml is preserved, not overwritten.
    let state = fs::read_to_string(&state_path).unwrap();
    assert_eq!(state, "pre-existing state content\n");

    // No partial migration happened.
    assert!(
        !dir.path().join("stitch.toml").exists(),
        "must not write stitch.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("config.toml.bak").exists(),
        "must not create backup"
    );
    assert!(
        dir.path().join(".stitch").join("config.toml").exists(),
        "legacy config intact"
    );
}

/// migrate refuses to overwrite a dangling .stitch/state.toml symlink.
#[test]
fn migrate_refuses_dangling_state_symlink() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();
    let state_path = dir.path().join(".stitch").join("state.toml");
    std::os::unix::fs::symlink("some/nonexistent", &state_path).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("state.toml"))
        .stderr(contains("already exists"))
        .stderr(contains("refusing to overwrite"));

    // The dangling symlink is preserved.
    assert!(state_path.is_symlink(), "state.toml must remain a symlink");
    assert_eq!(
        fs::read_link(&state_path).unwrap(),
        Path::new("some/nonexistent")
    );
    assert!(!state_path.exists(), "state.toml must still be dangling");

    // No partial migration happened.
    assert!(
        !dir.path().join("stitch.toml").exists(),
        "must not write stitch.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("config.toml.bak").exists(),
        "must not create backup"
    );
    assert!(
        dir.path().join(".stitch").join("config.toml").exists(),
        "legacy config intact"
    );
}

/// migrate refuses to overwrite a dangling .stitch/config.toml.bak symlink.
#[test]
fn migrate_refuses_dangling_backup_symlink() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();
    let backup = dir.path().join(".stitch").join("config.toml.bak");
    std::os::unix::fs::symlink("some/nonexistent", &backup).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("config.toml.bak"))
        .stderr(contains("already exists"));

    // The dangling backup symlink is preserved.
    assert!(backup.is_symlink(), "backup must remain a symlink");
    assert_eq!(
        fs::read_link(&backup).unwrap(),
        Path::new("some/nonexistent")
    );
    assert!(!backup.exists(), "backup must still be dangling");

    // No partial migration happened.
    assert!(
        !dir.path().join("stitch.toml").exists(),
        "must not write stitch.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("state.toml").exists(),
        "must not write state.toml"
    );
    assert!(
        dir.path().join(".stitch").join("config.toml").exists(),
        "legacy config intact"
    );
}

/// migrate refuses to overwrite a dangling stitch.toml symlink.
#[test]
fn migrate_refuses_dangling_authored_symlink() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch").join("config.toml"),
        "[stores.nvim]\ntarget = \"~/.config/nvim\"\n",
    )
    .unwrap();
    let authored = dir.path().join("stitch.toml");
    std::os::unix::fs::symlink("some/nonexistent", &authored).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("stitch.toml"))
        .stderr(contains("already exists"))
        .stderr(contains("refusing to overwrite"));

    // The dangling stitch.toml symlink is preserved.
    assert!(authored.is_symlink(), "stitch.toml must remain a symlink");
    assert_eq!(
        fs::read_link(&authored).unwrap(),
        Path::new("some/nonexistent")
    );
    assert!(!authored.exists(), "stitch.toml must still be dangling");

    // No partial migration happened.
    assert!(
        !dir.path().join(".stitch").join("state.toml").exists(),
        "must not write state.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("config.toml.bak").exists(),
        "must not create backup"
    );
    assert!(
        dir.path().join(".stitch").join("config.toml").exists(),
        "legacy config intact"
    );
}

/// migrate with nothing to migrate reports so.
#[test]
fn migrate_nothing_to_do() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("nothing to migrate"));
}

/// migrate on an already-converted repo exits 0 with a non-error message.
/// The message and exit code must agree: success must not be paired with
/// "error:".
#[test]
fn migrate_message_and_exit_code_agree() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(dir.path().join("stitch.toml"), "").unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .success()
        .stdout(contains("nothing to migrate"))
        .stdout(contains("stitch.toml"))
        .stderr(contains("error:").not());
}

/// migrate rejects v0.2 entries that the new validator would refuse (e.g.
/// `files = ["../escape"]`) and fails *before* writing or backing up.
#[test]
fn migrate_rejects_invalid_file_fragment_before_mutating() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    let original = "[stores.shells]\ntarget = \"~\"\nfiles = [\"../escape\"]\n";
    fs::write(dir.path().join(".stitch").join("config.toml"), original).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("invalid file entry"))
        .stderr(contains("../escape"));

    // No partial migration: legacy is untouched, no new files written.
    assert!(
        !dir.path().join("stitch.toml").exists(),
        "must not write stitch.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("state.toml").exists(),
        "must not write state.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("config.toml.bak").exists(),
        "must not create backup"
    );
    let legacy = fs::read_to_string(dir.path().join(".stitch").join("config.toml")).unwrap();
    assert_eq!(legacy, original, "legacy config must be unchanged");
}

#[test]
fn migrate_rejects_unknown_keys_before_writing() {
    for (args, original) in [
        (&["migrate"][..], "unexpected = true\n"),
        (
            &["migrate", "--dry-run"][..],
            "[stores.shells]\ntarget = \"~\"\nignroe = [\"secret\"]\n",
        ),
        (
            &["migrate"][..],
            "[[stores.shells.targets]]\ntarget = \"~\"\nignroe = [\"secret\"]\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".stitch")).unwrap();
        let legacy_path = dir.path().join(".stitch/config.toml");
        fs::write(&legacy_path, original).unwrap();

        Command::cargo_bin("stitch")
            .unwrap()
            .current_dir(dir.path())
            .args(args)
            .assert()
            .failure()
            .code(3)
            .stderr(contains("unknown field"));

        assert!(!dir.path().join("stitch.toml").exists());
        assert!(!dir.path().join(".stitch/state.toml").exists());
        assert!(!dir.path().join(".stitch/config.toml.bak").exists());
        assert_eq!(fs::read_to_string(&legacy_path).unwrap(), original);
    }
}

/// migrate --dry-run also rejects invalid v0.2 fragments before previewing.
#[test]
fn migrate_dry_run_rejects_invalid_file_fragment() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    let original = "[stores.shells]\ntarget = \"~\"\nfiles = [\"../escape\"]\n";
    fs::write(dir.path().join(".stitch").join("config.toml"), original).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .args(["migrate", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains("invalid file entry"))
        .stderr(contains("../escape"));

    // Nothing is written in dry-run, and the legacy config is untouched.
    assert!(
        !dir.path().join("stitch.toml").exists(),
        "must not write stitch.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("state.toml").exists(),
        "must not write state.toml"
    );
    assert!(
        !dir.path().join(".stitch").join("config.toml.bak").exists(),
        "must not create backup"
    );
    let legacy = fs::read_to_string(dir.path().join(".stitch").join("config.toml")).unwrap();
    assert_eq!(legacy, original, "legacy config must be unchanged");
}

/// v0.7.1 regressed and rejected harmless `./` file entries. They must migrate
/// successfully and produce a loadable state.
#[test]
fn migrate_accepts_dot_slash_file_fragment() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    let original = "[stores.shells]\ntarget = \"~\"\nfiles = [\"./bashrc\"]\n";
    fs::write(dir.path().join(".stitch").join("config.toml"), original).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .success();

    let state = fs::read_to_string(dir.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.starts_with("# Generated by stitch"));
    assert!(state.contains("[stores.shells]"));
    assert!(state.contains("./bashrc"));

    let backup = fs::read_to_string(dir.path().join(".stitch").join("config.toml.bak")).unwrap();
    assert_eq!(backup, original, "legacy config must be preserved");

    // The migrated repo must load (not repeat the v0.7.1 "invalid file entry" error).
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(contains("shells"));
}

#[test]
fn init_rejects_gitignore_symlink_before_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let external = tempfile::NamedTempFile::new().unwrap();
    fs::write(external.path(), "keep\n").unwrap();
    std::os::unix::fs::symlink(external.path(), dir.path().join(".gitignore")).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure();
    assert_eq!(fs::read_to_string(external.path()).unwrap(), "keep\n");
    assert!(!dir.path().join("stitch.toml").exists());
    assert!(!dir.path().join(".stitch").exists());
}

#[test]
fn migrate_rejects_invalid_authored_ignore_before_writes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".stitch")).unwrap();
    fs::write(
        dir.path().join(".stitch/config.toml"),
        "[stores.app]\ntarget = \"~\"\nignore = [\"[unterminated\"]\n",
    )
    .unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("invalid glob pattern"));
    assert!(!dir.path().join("stitch.toml").exists());
    assert!(!dir.path().join(".stitch/state.toml").exists());
}

#[test]
fn migrate_rejects_symlinked_stitch_dir_before_writes() {
    let dir = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("config.toml"), "[stores]\n").unwrap();
    std::os::unix::fs::symlink(external.path(), dir.path().join(".stitch")).unwrap();

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("migrate")
        .assert()
        .failure()
        .stderr(contains("refusing migration before writing anything"));
    assert!(!dir.path().join("stitch.toml").exists());
    assert!(!external.path().join("state.toml").exists());
}

#[test]
fn init_io_error_includes_path() {
    // An unwritable cwd should not produce a bare "I/O error" when init tries
    // to create .stitch/.
    if is_root() {
        eprintln!("note: init_io_error_includes_path skipped under root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = RestoreMode {
        path: dir.path(),
        mode: 0o755,
    };

    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(contains(".stitch"))
        .stderr(contains("Permission denied"))
        .stderr(contains("I/O error").not());
}
