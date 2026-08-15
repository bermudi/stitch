//! Inspection commands — `status`, `diff`, `list`, `doctor`, `prune`, and exit codes (split from `tests/cli.rs`).
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
fn status_rejects_unknown_when_key() {
    let (repo, _target) = repo_with_bashrc_store();
    repo.write_authored(
        r#"
[stores.bashrc.when]
bogus_key = "x"
"#,
    );

    repo.cmd()
        .arg("status")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("unknown field `bogus_key`"));
}

#[test]
fn list_rejects_unknown_when_key() {
    let (repo, _target) = repo_with_bashrc_store();
    repo.write_authored(
        r#"
[stores.bashrc.when]
bogus_key = "x"
"#,
    );

    repo.cmd()
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("unknown field `bogus_key`"));
}

#[test]
fn status_reports_linked_and_missing() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.make_store("shells", &[".bashrc"]);

    let nvim_target = repo.path().join("home").join(".config").join("nvim");
    let shells_target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"

[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        nvim_target.to_string_lossy(),
        shells_target.to_string_lossy(),
    ));

    // Link nvim, leave shells unlinked.
    fs::create_dir_all(nvim_target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(
        repo.path().join("nvim").canonicalize().unwrap(),
        &nvim_target,
    )
    .unwrap();

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("linked"))
        .stdout(contains("missing"));
}

#[test]
fn status_reports_conflict() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "real file").unwrap();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("conflict"));
}

#[test]
fn status_reports_broken_link() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("broken"));
}

#[test]
fn status_labels_live_foreign_symlink_as_foreign() {
    // A symlink at the target that points to an *existing* file outside this
    // repo is a live foreign link, not a broken one.
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join(".bashrc");

    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign_file = foreign_dir.path().join("bashrc");
    fs::write(&foreign_file, "foreign").unwrap();
    std::os::unix::fs::symlink(&foreign_file, &target).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("foreign"))
        .stdout(contains("broken").not());

    let output = repo.cmd().args(["status", "--json"]).assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout).unwrap();
    let json: Value = serde_json::from_str(stdout).unwrap();
    let row = json["data"].as_array().unwrap().first().unwrap();
    assert_eq!(row["state"].as_str().unwrap(), "foreign");
    assert!(row["resolves_to"].as_str().is_some());
}

#[test]
fn status_labels_dangling_symlink_as_broken() {
    // A dangling symlink (target does not exist) must keep the "broken" label,
    // even when it points outside this repo.
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join(".bashrc");

    let foreign_dir = tempfile::tempdir().unwrap();
    let missing = foreign_dir.path().join("missing");
    std::os::unix::fs::symlink(&missing, &target).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("broken"))
        .stdout(contains("foreign").not());

    let output = repo.cmd().args(["status", "--json"]).assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout).unwrap();
    let json: Value = serde_json::from_str(stdout).unwrap();
    let row = json["data"].as_array().unwrap().first().unwrap();
    assert_eq!(row["state"].as_str().unwrap(), "broken");
    assert!(row["resolves_to"].as_str().is_some());
}

#[test]
fn status_and_apply_agree_on_foreign_symlink() {
    // The same on-disk state (live foreign symlink) must be reported as
    // "foreign" by status and as a "conflict" by apply (exit 7).
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join(".bashrc");

    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign_file = foreign_dir.path().join("bashrc");
    fs::write(&foreign_file, "foreign").unwrap();
    std::os::unix::fs::symlink(&foreign_file, &target).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let status = repo.cmd().arg("status").assert().success();
    let status_stdout = std::str::from_utf8(&status.get_output().stdout).unwrap();
    assert!(
        status_stdout.contains("foreign"),
        "status must label a live foreign symlink as foreign"
    );

    let apply = repo.cmd().arg("apply").assert().failure();
    assert_eq!(apply.get_output().status.code().unwrap(), 7);
    let apply_stdout = std::str::from_utf8(&apply.get_output().stdout).unwrap();
    assert!(
        apply_stdout.contains("conflict"),
        "apply must report a foreign symlink as a conflict"
    );
}

#[test]
fn status_name_filter_shows_only_matching_store() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.make_store("shells", &[".bashrc"]);

    let nvim_target = repo.path().join("home").join(".config").join("nvim");
    let shells_target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"

[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        nvim_target.to_string_lossy(),
        shells_target.to_string_lossy(),
    ));

    let output = repo.cmd().args(["status", "nvim"]).assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout).unwrap();
    assert!(stdout.contains("nvim"));
    // `shells` store name should not appear in filtered output.
    assert!(!stdout.contains("shells"));
}

#[test]
fn status_unknown_store_errors() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .args(["status", "nonexistent"])
        .assert()
        .failure()
        .stderr(contains("unknown store"));
}

#[test]
fn source_name_collision_is_reported_by_status_doctor_apply_diff_remove() {
    // A store whose files resolve to the same link name must be flagged as a
    // config error by every command; remove in particular must not silently
    // drop state for a misconfigured store.
    let repo = Repo::new();
    repo.make_store("git", &["gitconfig", "gitconfig.tmpl"]);
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
files = ["gitconfig", "gitconfig.tmpl"]
"#,
    ));

    // status prints an error line.
    let status = repo.cmd().arg("status").assert().success();
    let status_out = std::str::from_utf8(&status.get_output().stdout).unwrap();
    assert!(
        status_out.contains("name collision"),
        "status must mention the collision, got: {status_out}"
    );
    assert!(
        status_out.contains("error:"),
        "status must render an error, got: {status_out}"
    );

    // doctor reports a source-name-collision finding.
    let doctor = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        !doctor.status.success(),
        "doctor must fail on a source-name collision"
    );
    let value: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let findings = value["data"]["findings"].as_array().unwrap();
    let collision = findings
        .iter()
        .find(|f| f["id"] == "source-name-collision")
        .expect("doctor must report a source-name-collision finding");
    assert_eq!(collision["severity"], "error");
    let message = collision["message"].as_str().unwrap();
    assert!(
        message.contains("name collision"),
        "finding message must mention the collision, got: {message}"
    );

    // apply and diff both error.
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("name collision"));
    repo.cmd()
        .arg("diff")
        .assert()
        .failure()
        .stdout(contains("name collision"));

    // remove aborts and preserves state.
    let state_before = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    repo.cmd()
        .args(["remove", "git"])
        .assert()
        .failure()
        .code(9)
        .stderr(contains("configuration error"));
    let state_after = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert_eq!(
        state_before, state_after,
        "remove must preserve state when it aborts"
    );
    assert!(
        state_after.contains("[stores.git]"),
        "remove must preserve the state.toml entry"
    );
}

#[test]
fn status_and_doctor_remain_healthy_without_source_name_collision() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
files = ["init.lua"]
"#,
    ));

    repo.cmd().arg("apply").assert().success();

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("linked"))
        .stdout(contains("name collision").not());

    repo.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("0 errors"));
}

#[test]
fn diff_is_dry_run_apply() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("diff")
        .assert()
        .success()
        .stdout(contains("Dry run"));

    // Nothing should actually have been created.
    assert!(!target.exists());
}

#[test]
fn diff_force_reports_backup_without_changing() {
    // diff --force previews the .bak backup without touching the filesystem.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .args(["diff", "--force"])
        .assert()
        .success()
        .stdout(contains("backed up"));

    // Dry run: nothing moved.
    assert!(target.is_file());
    assert_eq!(fs::read_to_string(&target).unwrap(), "real file");
    assert!(!target.is_symlink());
    assert!(!Path::new(&format!("{}.bak", target.display())).exists());
}

#[test]
fn diff_force_fails_when_bak_already_exists() {
    // diff --force must preview the .bak backup honestly: if a .bak already
    // exists, the operation is a conflict even in dry-run.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let backup = format!("{}.bak", target.display());
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "current").unwrap();
    fs::write(&backup, "previous backup").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .args(["diff", "--force"])
        .assert()
        .failure()
        .stdout(contains("conflict"));

    assert!(target.is_file());
    assert!(!target.is_symlink());
    assert_eq!(fs::read_to_string(&backup).unwrap(), "previous backup");
}

#[test]
fn diff_only_unknown_store_errors() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .args(["diff", "--only", "nonexistent"])
        .assert()
        .failure()
        .stderr(contains("unknown store"));
}

#[test]
fn diff_real_file_conflict_exits_6() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("diff")
        .assert()
        .failure()
        .code(6)
        .stdout(contains("conflict"));

    assert!(target.is_file());
    assert_eq!(fs::read_to_string(&target).unwrap(), "real file");
}

#[test]
fn diff_foreign_symlink_conflict_exits_7() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/etc/foreign", &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("diff")
        .assert()
        .failure()
        .code(7)
        .stdout(contains("conflict"));

    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), Path::new("/etc/foreign"));
}

#[test]
fn diff_no_differences_exits_zero() {
    // After a successful apply, `diff` must exit 0 and clearly report that
    // there is nothing to do. SPEC treats `diff` as a preview of `apply`:
    // it exits non-zero only for conflicts/errors, not simply because
    // differences exist, so an in-sync store is a success.
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.bashrc]
target = "{}"
files = [".bashrc"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();

    repo.cmd()
        .arg("diff")
        .assert()
        .success()
        .stdout(contains("no differences"));
}

#[test]
fn diff_exit_code_is_zero_when_converged() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.bashrc]
target = "{}"
files = [".bashrc"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();
    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .success()
        .stdout(contains("no differences"));
}

#[test]
fn diff_exit_code_reports_safe_drift_without_mutating() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.bashrc]
target = "{}"
files = [".bashrc"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(14)
        .stdout(contains("create:"))
        .stderr(contains("run `stitch apply`"));

    assert!(!target.join(".bashrc").exists());
}

#[test]
fn diff_exit_code_ignores_platform_skipped_store() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.bashrc]\ntarget = \"{}\"\nfiles = [\".bashrc\"]\n",
        target.to_string_lossy()
    ));
    repo.write_authored("[stores.bashrc.when]\nos = \"definitely-not-this-os\"\n");

    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .success()
        .stdout(contains("skipped"));
    assert!(!target.exists());
}

#[test]
fn diff_exit_code_preserves_conflict_code() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "real file").unwrap();
    repo.write_state(&format!(
        "[stores.nvim]\ntarget = \"{}\"\n",
        target.to_string_lossy(),
    ));

    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(6)
        .stdout(contains("conflict"));
}

#[test]
fn diff_with_differences_reports_them() {
    // `diff` is the dry-run mirror of `apply`. When the filesystem is not
    // yet reconciled, it should report the pending operation and still exit
    // 0 (per SPEC, `diff` only exits non-zero on conflicts/errors).
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.bashrc]
target = "{}"
files = [".bashrc"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("diff")
        .assert()
        .success()
        .stdout(contains("create:"));
}

#[test]
fn list_shows_single_target_stores() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(contains("nvim"))
        .stdout(contains("→"));
}

#[test]
fn list_shows_multi_target_stores_with_count() {
    // Multi-target: name-keyed map split across the two files. The target
    // names appear in `list` output so two targets sharing a path are
    // distinguishable.
    let repo = Repo::new();
    let t1 = repo.path().join("home1");
    let t2 = repo.path().join("home2");
    // Authored half: per-target behavior keyed by name.
    repo.write_authored(
        r#"
[stores.shells.targets.laptop]
when = { hostname = "laptop" }

[stores.shells.targets.server]
when = { hostname = "server" }
"#,
    );
    // Generated half: per-target inventory keyed by the same names.
    repo.write_state(&format!(
        r#"
[stores.shells.targets.laptop]
target = "{t1}"

[stores.shells.targets.server]
target = "{t2}"
"#,
        t1 = t1.to_string_lossy(),
        t2 = t2.to_string_lossy(),
    ));

    let output = repo.cmd().arg("list").assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout).unwrap();
    assert!(stdout.contains("shells"), "got: {stdout}");
    assert!(stdout.contains("2 targets"), "got: {stdout}");
    // Names appear alongside targets.
    assert!(stdout.contains("laptop"), "got: {stdout}");
    assert!(stdout.contains("server"), "got: {stdout}");
}

#[test]
fn list_marks_stores_without_target() {
    let repo = Repo::new();
    repo.write_state(
        r#"
[stores.blank]
"#,
    );

    repo.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(contains("(no target)"));
}

#[test]
fn doctor_passes_on_healthy_repo() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));
    repo.cmd().arg("apply").assert().success();

    repo.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("0 errors"));
}

#[test]
fn doctor_flags_missing_store_dir_as_error() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .code(13)
        .stdout(contains("[error]"))
        .stdout(contains("nvim"));
}

#[test]
fn doctor_warns_on_empty_store() {
    let repo = Repo::new();
    // Create an empty store dir, no target means no apply is needed.
    fs::create_dir_all(repo.path().join("nvim")).unwrap();
    repo.write_state(
        r#"
[stores.nvim]
"#,
    );

    repo.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("[warn]"))
        .stdout(contains("empty"));
}

#[test]
fn doctor_warns_on_duplicate_targets() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.make_store("b", &[]);
    repo.write_state(&format!(
        r#"
[stores.a]
target = "{target_str}"

[stores.b]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(contains("both target"));
}

#[test]
fn doctor_warns_on_duplicate_targets_between_multi_target_stores() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.make_store("b", &[]);
    repo.write_state(&format!(
        r#"
[stores.a.targets.main]
target = "{target_str}"

[stores.b.targets.main]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(contains("both target"))
        .stdout(contains("target 'main'"))
        .stdout(contains("store 'a'"))
        .stdout(contains("store 'b'"));
}

#[test]
fn doctor_warns_on_duplicate_targets_single_and_multi_store() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.make_store("b", &[]);
    repo.write_state(&format!(
        r#"
[stores.a]
target = "{target_str}"

[stores.b.targets.main]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(contains("both target"))
        .stdout(contains("store 'a'"))
        .stdout(contains("target 'main' of store 'b'"));
}

#[test]
fn doctor_allows_duplicate_targets_within_same_multi_target_store() {
    // Same-store targets can share a path when their `when` clauses are
    // mutually exclusive — only one applies per machine.
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.write_authored(
        r#"
[stores.a.targets.main]
when = { hostname = "laptop" }

[stores.a.targets.alt]
when = { hostname = "server" }
"#,
    );
    repo.write_state(&format!(
        r#"
[stores.a.targets.main]
target = "{target_str}"

[stores.a.targets.alt]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("both target").not());
}

#[test]
fn doctor_allows_duplicate_target_across_mutually_exclusive_stores() {
    // Different stores can share a target path when their store-level `when`
    // clauses are mutually exclusive.
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.make_store("b", &[]);
    repo.write_authored(
        r#"
[stores.a]
when = { hostname = "laptop" }

[stores.b]
when = { hostname = "server" }
"#,
    );
    repo.write_state(&format!(
        r#"
[stores.a]
target = "{target_str}"

[stores.b]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("both target").not());
}

#[test]
fn doctor_warns_on_duplicate_targets_within_same_store_compatible_when() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.write_state(&format!(
        r#"
[stores.a.targets.main]
target = "{target_str}"

[stores.a.targets.alt]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(contains("both target"))
        .stdout(contains("targets 'main' and 'alt' of store 'a'"));
}

#[test]
fn doctor_warns_on_duplicate_targets_for_platform_filtered_stores() {
    // A duplicate target is a config problem, not a filesystem problem, so
    // `doctor` must report it even when the current platform skips the stores.
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("shared");
    let target_str = target.to_string_lossy().into_owned();
    repo.make_store("a", &[]);
    repo.make_store("b", &[]);
    repo.write_authored(
        r#"
[stores.a]
when = { os = "macos" }

[stores.b]
when = { os = "macos" }
"#,
    );
    repo.write_state(&format!(
        r#"
[stores.a]
target = "{target_str}"

[stores.b]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(contains("both target"))
        .stdout(contains("store 'a'"))
        .stdout(contains("store 'b'"));
}

#[test]
fn doctor_reports_source_name_collision_in_multi_target_store() {
    // A source-name collision in one named target must surface for that
    // specific target, even when another target in the same store is healthy.
    let repo = Repo::new();
    repo.make_store("git", &["gitconfig", "gitconfig.tmpl", "other"]);
    let active_target = repo.path().join("home").join(".config").join("git");
    let other_target = repo.path().join("home2");
    let active_str = active_target.to_string_lossy().into_owned();
    let other_str = other_target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git.targets.active]
target = "{active_str}"
files = ["gitconfig", "gitconfig.tmpl"]

[stores.git.targets.other]
target = "{other_str}"
files = ["other"]
"#,
    ));

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        !output.status.success(),
        "doctor must fail on source-name collision"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let findings = value["data"]["findings"].as_array().unwrap();
    let collision = findings
        .iter()
        .find(|f| f["id"] == "source-name-collision")
        .expect("doctor must report source-name-collision for active target");
    assert_eq!(collision["severity"], "error");
    let message = collision["message"].as_str().unwrap();
    assert!(
        message.contains("name collision"),
        "finding must mention the collision, got: {message}"
    );
    assert!(
        message.contains("target 'active'"),
        "finding must identify the named target, got: {message}"
    );
}

#[test]
fn doctor_reports_unsupported_template_source() {
    // A non-regular `.tmpl` source (e.g. a symlink) must be reported as an
    // `unsupported-template-source` finding, not mislabelled as a name
    // collision.
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig"), "plain\n").unwrap();

    // Create a symlink named `*.tmpl` — that is not a regular template source.
    let real = store.join("real_gitconfig");
    fs::write(&real, "real\n").unwrap();
    std::os::unix::fs::symlink(&real, store.join("gitconfig.tmpl")).unwrap();

    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
"#,
    ));

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        !output.status.success(),
        "doctor must fail on an unsupported template source"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let findings = value["data"]["findings"].as_array().unwrap();

    assert!(
        findings
            .iter()
            .any(|f| f["id"] == "unsupported-template-source"),
        "doctor must report an unsupported-template-source finding"
    );
    assert!(
        !findings.iter().any(|f| f["id"] == "source-name-collision"),
        "a non-regular template source must not be reported as a source-name-collision"
    );

    let finding = findings
        .iter()
        .find(|f| f["id"] == "unsupported-template-source")
        .unwrap();
    assert_eq!(finding["severity"], "error");
    let hint = finding["hint"].as_str().unwrap();
    assert!(
        hint.contains("regular file"),
        "hint must tell the user to use a regular file, got: {hint}"
    );
}

#[test]
fn doctor_flags_orphaned_behavior_store() {
    // A store present in stitch.toml (authored) but not state.toml (generated)
    // — e.g. left behind by `remove`, which never rewrites the authored file.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.write_authored(
        r#"
[stores.nvim]
ignore = ["*.bak"]
"#,
    );
    // No state.toml entry for nvim.

    let output = repo.cmd().arg("doctor").assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout).unwrap();
    assert!(
        stdout.contains("orphaned") && stdout.contains("nvim"),
        "expected orphaned-behavior warning for nvim, got: {stdout}"
    );
}

#[test]
fn doctor_reports_live_foreign_symlink() {
    // A live symlink at the target that points outside this repo (another
    // tool's file) must surface as a `foreign-link` finding, not be silent.
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join(".bashrc");

    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign_file = foreign_dir.path().join("bashrc");
    fs::write(&foreign_file, "foreign").unwrap();
    std::os::unix::fs::symlink(&foreign_file, &target).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        !output.status.success(),
        "doctor must fail on a foreign symlink"
    );
    let value = json_output(&output);
    assert_envelope_shape(&value, "doctor", false);
    assert_error_shape(&value, "doctor", 13);

    let findings = value["data"]["findings"].as_array().unwrap();
    let foreign = findings
        .iter()
        .find(|f| f["id"] == "foreign-link")
        .expect("doctor must report a foreign-link finding");
    assert_eq!(foreign["severity"], "error");

    let message = foreign["message"].as_str().unwrap();
    assert!(
        message.contains(target.to_string_lossy().as_ref()),
        "finding must mention the target path, got: {message}"
    );

    let hint = foreign["hint"].as_str().unwrap();
    assert!(
        hint.contains("foreign") || hint.contains("another tool") || hint.contains("conflict"),
        "hint must describe a foreign/apply-conflict situation, got: {hint}"
    );
    assert!(
        !hint.contains("remove or repoint"),
        "hint must not give broken-link advice, got: {hint}"
    );

    let summary = value["data"]["summary"].as_object().unwrap();
    assert_eq!(summary["errors"], 1);
}

#[test]
fn doctor_reports_broken_link() {
    // A dangling symlink at the target must still be reported as a broken link.
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join(".bashrc");
    std::os::unix::fs::symlink("/nonexistent", &target).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        !output.status.success(),
        "doctor must fail on a broken link"
    );
    let value = json_output(&output);
    let findings = value["data"]["findings"].as_array().unwrap();
    let broken = findings
        .iter()
        .find(|f| f["id"] == "broken-link")
        .expect("doctor must report a broken-link finding");
    assert_eq!(broken["severity"], "error");
    let hint = broken["hint"].as_str().unwrap();
    assert!(
        hint.contains("remove or repoint"),
        "broken-link hint unchanged, got: {hint}"
    );
}

#[test]
fn doctor_reports_missing_link() {
    // A configured store whose target symlink does not exist yet must surface
    // as a `missing-link` warning (apply would create it, so it is not an error).
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        output.status.success(),
        "doctor must succeed on missing link"
    );
    let value = json_output(&output);
    assert_envelope_shape(&value, "doctor", true);

    let findings = value["data"]["findings"].as_array().unwrap();
    let missing = findings
        .iter()
        .find(|f| f["id"] == "missing-link")
        .expect("doctor must report a missing-link finding");
    assert_eq!(missing["severity"], "warning");
    let message = missing["message"].as_str().unwrap();
    assert!(
        message.contains("bashrc") && message.contains("missing"),
        "message must name the store and describe a missing link, got: {message}"
    );
    let hint = missing["hint"].as_str().unwrap();
    assert!(
        hint.contains("apply"),
        "hint must suggest running apply, got: {hint}"
    );

    let summary = value["data"]["summary"].as_object().unwrap();
    assert_eq!(summary["errors"], 0);
    assert_eq!(summary["warnings"], 1);
    assert_eq!(summary["info"], 1); // store-count
}

#[test]
fn doctor_reports_untracked_store_dir() {
    // A directory in the repo root that is not a configured store should be
    // flagged so the user can clean it up or add it to config.
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    fs::create_dir(repo.path().join("ghost")).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
"#,
    );

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        output.status.success(),
        "doctor must succeed on untracked store dir"
    );
    let value = json_output(&output);
    assert_envelope_shape(&value, "doctor", true);

    let findings = value["data"]["findings"].as_array().unwrap();
    let untracked = findings
        .iter()
        .find(|f| f["id"] == "untracked-store-dir")
        .expect("doctor must report an untracked-store-dir finding");
    assert_eq!(untracked["severity"], "info");
    let message = untracked["message"].as_str().unwrap();
    assert!(
        message.contains("ghost") && message.contains("untracked"),
        "message must name the untracked dir, got: {message}"
    );
    let hint = untracked["hint"].as_str().unwrap();
    assert!(
        hint.contains("remove") || hint.contains("config"),
        "hint should suggest cleanup or adding to config, got: {hint}"
    );

    let summary = value["data"]["summary"].as_object().unwrap();
    assert_eq!(summary["errors"], 0);
    assert_eq!(summary["warnings"], 0);
    assert_eq!(summary["info"], 2); // store-count + untracked
}

#[test]
fn doctor_reports_unreadable_source() {
    // A store source directory that is not readable is an error: apply would
    // fail. Skipped under root because root bypasses file mode bits.
    if is_root() {
        eprintln!("note: doctor_reports_unreadable_source skipped under root");
        return;
    }

    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let store_dir = repo.path().join("bashrc");
    let mut perms = fs::metadata(&store_dir).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&store_dir, perms).unwrap();

    repo.write_state(
        r#"
[stores.bashrc]
target = "~"
files = [".bashrc"]
"#,
    );

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(
        !output.status.success(),
        "doctor must fail on unreadable source"
    );
    let value = json_output(&output);
    assert_envelope_shape(&value, "doctor", false);
    assert_error_shape(&value, "doctor", 13);

    let findings = value["data"]["findings"].as_array().unwrap();
    let unreadable = findings
        .iter()
        .find(|f| f["id"] == "unreadable-source")
        .expect("doctor must report an unreadable-source finding");
    assert_eq!(unreadable["severity"], "error");
    let message = unreadable["message"].as_str().unwrap();
    assert!(
        message.contains("bashrc")
            && (message.contains("unreadable") || message.contains("permission")),
        "message must name the store and describe an unreadability, got: {message}"
    );
    let hint = unreadable["hint"].as_str().unwrap();
    assert!(
        hint.contains("permission") || hint.contains("apply will fail"),
        "hint must mention permissions or apply failing, got: {hint}"
    );

    let summary = value["data"]["summary"].as_object().unwrap();
    assert_eq!(summary["errors"], 1);
    assert_eq!(summary["info"], 1); // store-count
}

/// prune with no flags lists orphans without removing anything (the non-
/// destructive default — the core red line).
#[test]
fn prune_default_lists_without_removing() {
    let (repo, _covered, orphan, home) = prune_fixture();

    repo.cmd()
        .arg("prune")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("Found 1 orphaned link(s):"))
        .stdout(contains(orphan.to_string_lossy().as_ref()))
        .stdout(contains("stitch prune --yes"));

    // The orphan symlink is still on disk — default removed nothing.
    assert!(orphan.is_symlink(), "orphan link must survive a bare prune");
}

/// prune --yes removes only the orphan link; the covered link (referenced by a
/// store) is untouched, and the repo store directory is never touched.
#[test]
fn prune_yes_removes_only_orphan() {
    let (repo, covered, orphan, home) = prune_fixture();

    repo.cmd()
        .arg("prune")
        .arg("--yes")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("Removed 1 link(s)."))
        .stdout(contains(format!("removed {}", orphan.display())));

    assert!(!orphan.exists(), "orphan link removed");
    assert!(covered.is_symlink(), "covered link untouched");
    assert!(
        repo.path().join("nvim").exists(),
        "repo store dir untouched"
    );
}

/// prune --dry-run is an explicit alias for the safe default: lists, does not
/// remove, even though `--yes` is what gates removal (no --yes here anyway).
/// Pairs with prune_yes_dry_run_still_lists below for the --yes --dry-run case.
#[test]
fn prune_dry_run_lists_without_removing() {
    let (repo, _covered, orphan, home) = prune_fixture();

    repo.cmd()
        .arg("prune")
        .arg("--dry-run")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("Found 1 orphaned link(s):"));

    assert!(orphan.is_symlink(), "dry run removed nothing");
}

/// --yes --dry-run still removes nothing: dry-run outranks --yes, so the
/// explicit-preview flag can never accidentally mutate $HOME.
#[test]
fn prune_yes_dry_run_still_lists() {
    let (repo, _covered, orphan, home) = prune_fixture();

    repo.cmd()
        .arg("prune")
        .arg("--yes")
        .arg("--dry-run")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("Found 1 orphaned link(s):"));

    assert!(orphan.is_symlink(), "--yes --dry-run removed nothing");
}

/// A foreign symlink (pointing outside the repo) is never listed and never
/// removed — consistent with the points_into_repo guard used elsewhere.
#[test]
fn prune_ignores_foreign_symlink() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);

    let home = tempfile::tempdir().unwrap();
    let foreign_target = tempfile::tempdir().unwrap();
    let foreign_link = home.path().join("stranger");
    std::os::unix::fs::symlink(foreign_target.path(), &foreign_link).unwrap();

    repo.cmd()
        .arg("prune")
        .arg("--yes")
        .arg("--scan-dir")
        .arg(home.path())
        .assert()
        .success()
        .stdout(contains("No orphaned links found."));

    assert!(foreign_link.is_symlink(), "foreign link untouched");
}

#[test]
fn prune_does_not_remove_gateway_foreign_symlink() {
    // P0 regression: a hand-managed link through a repo gateway symlink to an
    // external path is foreign. scan must not classify it as repo-pointing, so
    // prune --yes never lists or removes it — even though the immediate-hop
    // readlink is beneath the repo.
    //
    //   repo/gateway -> /external
    //   home/file    -> repo/gateway/victim
    let repo = Repo::new();
    repo.make_store("app", &["file"]);

    let home = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("victim"), "foreign").unwrap();
    let gateway = repo.path().join("gateway");
    std::os::unix::fs::symlink(external.path(), &gateway).unwrap();

    let link = home.path().join("file");
    std::os::unix::fs::symlink(gateway.join("victim"), &link).unwrap();

    repo.cmd()
        .arg("prune")
        .arg("--yes")
        .arg("--scan-dir")
        .arg(home.path())
        .assert()
        .success()
        .stdout(contains("No orphaned links found."));

    assert!(
        link.is_symlink(),
        "gateway foreign link must not be removed"
    );
    assert_eq!(fs::read_to_string(&link).unwrap(), "foreign");
}

/// No orphans → friendly message, success.
#[test]
fn prune_no_orphans() {
    let (repo, _covered, orphan, home) = prune_fixture();
    // Pre-emptively remove the orphan so only the covered link remains.
    fs::remove_file(&orphan).unwrap();

    repo.cmd()
        .arg("prune")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("No orphaned links found."));
}

/// `gc` is an alias for `prune`.
#[test]
fn prune_gc_alias_works() {
    let (repo, _covered, orphan, home) = prune_fixture();

    repo.cmd()
        .arg("gc")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("Found 1 orphaned link(s):"))
        .stdout(contains(orphan.to_string_lossy().as_ref()));

    assert!(orphan.is_symlink(), "alias honors the list-only default");
}

/// `prune --yes` exits non-zero when a removal fails — the honest-exit-code red
/// line. We force `remove_file` to fail by making the orphan's parent dir
/// non-writable (r-x), which still lets the scan read and traverse it. Skipped
/// when running as root: root bypasses file modes, so removal would succeed and
/// the failure path wouldn't trigger.
#[test]
fn prune_yes_exits_nonzero_on_removal_failure() {
    if is_root() {
        eprintln!("skipping: running as root, file modes won't block removal");
        return;
    }
    let (repo, _covered, orphan, home) = prune_fixture();
    let parent = orphan.parent().expect("orphan has a parent");
    // r-x: readdir/traverse still work, but unlink needs write on the parent.
    fs::set_permissions(parent, fs::Permissions::from_mode(0o555)).unwrap();

    repo.cmd()
        .arg("prune")
        .arg("--yes")
        .arg("--scan-dir")
        .arg(home.path())
        .env("HOME", home.path().as_os_str())
        .assert()
        .failure()
        .stderr(contains("could not remove"))
        .stderr(contains("see warnings above"));

    // Restore before the tempdir drops so it can clean up cleanly.
    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
    assert!(orphan.is_symlink(), "orphan survived the failed removal");
}

#[test]
fn doctor_allows_plain_repo_without_render_gitignore_entry() {
    let repo = Repo::new();
    fs::remove_file(repo.path().join(".gitignore")).unwrap();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("doctor").assert().success();
}

#[test]
fn doctor_errors_when_template_repo_lacks_render_gitignore_entry() {
    let repo = Repo::new();
    // Wipe the entry that Repo::new seeds.
    fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    repo.make_store("nvim", &["init.lua.tmpl"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(contains(".stitch/render/"));
}

#[test]
fn diff_exit_code_detects_and_preserves_staged_render_mode_drift() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig.tmpl"), "v=1\n").unwrap();
    let target = repo.path().join("home/.config/git");
    repo.write_state(&format!(
        "[stores.git]\ntarget = \"{}\"\nfiles = [\"gitconfig.tmpl\"]\n",
        target.to_string_lossy()
    ));
    repo.cmd().arg("apply").assert().success();

    let staged = repo.path().join(".stitch/render/git/gitconfig");
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o644)).unwrap();

    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(14)
        .stdout(contains("content:"));
    assert_eq!(
        fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
        0o644
    );

    repo.cmd().arg("apply").assert().success();
    assert_eq!(
        fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
        0o600
    );
    repo.cmd().args(["diff", "--exit-code"]).assert().success();
}

#[test]
fn diff_exit_code_detects_and_preserves_staged_render_hard_link_drift() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig.tmpl"), "v=1\n").unwrap();
    let target = repo.path().join("home/.config/git");
    repo.write_state(&format!(
        "[stores.git]\ntarget = \"{}\"\nfiles = [\"gitconfig.tmpl\"]\n",
        target.to_string_lossy()
    ));
    repo.cmd().arg("apply").assert().success();

    let staged = repo.path().join(".stitch/render/git/gitconfig");
    let alias = repo.path().join("staged-alias");
    fs::hard_link(&staged, &alias).unwrap();
    let shared_inode = fs::metadata(&staged).unwrap().ino();
    assert_eq!(fs::metadata(&staged).unwrap().nlink(), 2);

    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(14)
        .stdout(contains("content:"));
    assert_eq!(fs::metadata(&staged).unwrap().ino(), shared_inode);
    assert_eq!(fs::metadata(&alias).unwrap().ino(), shared_inode);

    repo.cmd().arg("apply").assert().success();
    let staged_meta = fs::metadata(&staged).unwrap();
    let alias_meta = fs::metadata(&alias).unwrap();
    assert_ne!(staged_meta.ino(), alias_meta.ino());
    assert_eq!(staged_meta.nlink(), 1);
    assert_eq!(alias_meta.nlink(), 1);
    assert_eq!(fs::read_to_string(&alias).unwrap(), "v=1\n");
    repo.cmd().args(["diff", "--exit-code"]).assert().success();
}

#[test]
fn import_registers_existing_links() {
    let repo = Repo::new();
    // Build a store dir with a file, hand-create a symlink into a scan area.
    let store = repo.make_store("nvim", &["init.lua"]);
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &target).unwrap();

    repo.cmd()
        .arg("import")
        .arg("--scan-dir")
        .arg(home.path().join(".config"))
        .env("HOME", home.path().as_os_str())
        .assert()
        .success()
        .stdout(contains("import 'nvim'"))
        .stdout(contains("Imported 1"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.nvim]"),
        "state must record nvim: {state}"
    );
    assert!(
        state.contains("target"),
        "state must have a target: {state}"
    );
}

#[test]
fn import_registers_nested_file_links() {
    let repo = Repo::new();

    // Build a store with nested files under lua/.
    let store = repo.path().join("nvim");
    fs::create_dir_all(&store).unwrap();
    for f in &["init.lua", "lua/plugin.lua", "lua/foo/bar.lua"] {
        let p = store.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, format!("contents of {f}")).unwrap();
    }

    // Create hand-made symlinks for each file under a fake ~/.config/nvim.
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let target = home_path.join(".config").join("nvim");
    for f in &["init.lua", "lua/plugin.lua", "lua/foo/bar.lua"] {
        let link = target.join(f);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(store.join(f), &link).unwrap();
    }

    let home_str = home_path.to_str().unwrap();

    repo.cmd()
        .arg("import")
        .arg("--scan-dir")
        .arg(home_path.join(".config"))
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("import 'nvim'"))
        .stdout(contains("Imported 1"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.nvim]"),
        "state must record nvim: {state}"
    );
    assert!(
        state.contains(r#"target = "~/.config/nvim""#),
        "state must record the common target dir: {state}"
    );
    for f in &["init.lua", "lua/plugin.lua", "lua/foo/bar.lua"] {
        assert!(
            state.contains(&format!("\"{f}\"")),
            "state must record file {f}: {state}"
        );
    }

    // The imported state must be directly re-applicable.
    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .success()
        .stdout(contains("Summary: 3 ok"));
}

#[test]
fn import_registers_stow_fan_in_as_multi_target() {
    // Regression: a stow-style package can fan out to several target dirs —
    // one store directory's files are symlinked into multiple parents that do
    // not mirror the source tree (here: flat store `mixed` with `a` linked at
    // `~/.config/alpha/a` and `b` linked at `~/.config/beta/b`). `import` used
    // to skip the whole store with a warning; it must instead register a
    // multi-target store with one named target per parent, each carrying its
    // own file set, so the migration is not silently lossy. (Parents that
    // overlap — one nested under the other — are still emitted; apply's
    // existing overlap validation then rejects them with a clear error rather
    // than import dropping them silently. This case uses sibling parents so
    // the full import → apply round-trip converges.)
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dotfiles");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("stitch.toml"), "").unwrap();
    let stitch_dir = repo.join(".stitch");
    fs::create_dir_all(&stitch_dir).unwrap();
    fs::write(stitch_dir.join("state.toml"), "").unwrap();
    fs::write(stitch_dir.join("state.lock"), "").unwrap();
    fs::write(repo.join(".gitignore"), ".stitch/render/\n").unwrap();

    // Flat store `mixed` whose two files are symlinked into sibling parents.
    let store_dir = repo.join("mixed");
    fs::create_dir_all(&store_dir).unwrap();
    fs::write(store_dir.join("a"), "a\n").unwrap();
    fs::write(store_dir.join("b"), "b\n").unwrap();

    let alpha = home.join(".config").join("alpha");
    let beta = home.join(".config").join("beta");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&beta).unwrap();
    std::os::unix::fs::symlink(store_dir.join("a"), alpha.join("a")).unwrap();
    std::os::unix::fs::symlink(store_dir.join("b"), beta.join("b")).unwrap();

    let mut cmd = Command::cargo_bin("stitch").expect("stitch binary");
    cmd.current_dir(&repo)
        .env("HOME", &home)
        .env_remove("STITCH_REPO")
        .arg("import")
        .arg("--scan-dir")
        .arg(&home)
        .assert()
        .success()
        .stdout(contains("import 'mixed'"))
        .stdout(contains("multi-target"));

    let state = fs::read_to_string(stitch_dir.join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.mixed.targets.target-1]"),
        "state must have target-1: {state}"
    );
    assert!(
        state.contains("[stores.mixed.targets.target-2]"),
        "state must have target-2: {state}"
    );
    assert!(state.contains("\"a\""), "state must record file a: {state}");
    assert!(state.contains("\"b\""), "state must record file b: {state}");
    assert!(
        state.contains("target = \"~/.config/alpha\""),
        "state must record the alpha target: {state}"
    );
    assert!(
        state.contains("target = \"~/.config/beta\""),
        "state must record the beta target: {state}"
    );

    // The imported state must be directly re-applicable: both links converge.
    let mut cmd = Command::cargo_bin("stitch").expect("stitch binary");
    cmd.current_dir(&repo)
        .env("HOME", &home)
        .env_remove("STITCH_REPO")
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("Summary: 2 ok"));

    assert!(alpha.join("a").is_symlink());
    assert!(beta.join("b").is_symlink());
}

#[test]
fn exit_code_and_hint_outside_repo() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(tmp.path())
        .env_remove("STITCH_REPO")
        .arg("apply")
        .assert()
        .failure()
        .code(4)
        .stderr(contains("does not point at a stitch repo"))
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_and_hint_legacy_v02_config() {
    let tmp = tempfile::tempdir().unwrap();
    let stitch = tmp.path().join(".stitch");
    fs::create_dir_all(&stitch).unwrap();
    fs::write(stitch.join("config.toml"), "[store]\npath = '.stitch'\n").unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(tmp.path())
        .env_remove("STITCH_REPO")
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("v0.2 config"))
        .stderr(contains("stitch migrate"))
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_and_hint_unknown_store() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#,
    ));
    repo.cmd()
        .args(["apply", "--only", "missing"])
        .assert()
        .failure()
        .code(5)
        .stderr(contains("unknown store"))
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_and_hint_real_file_conflict() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(6)
        .stdout(contains("conflict"))
        .stderr(contains("apply --force"))
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_and_hint_foreign_symlink_conflict() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/etc/foreign", &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(7)
        .stdout(contains("conflict"))
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_and_hint_render_missing_env() {
    let repo = Repo::new();
    let store_dir = repo.make_store("env", &[]);
    fs::write(
        store_dir.join("foo.tmpl"),
        "value={{ env('STITCH_EXIT_CODE_TEST_MISSING_ENV') }}\n",
    )
    .unwrap();
    let target = repo.path().join("home").join(".foo");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.env]
target = "{target_str}"
files = ["foo.tmpl"]
"#
    ));
    repo.cmd()
        .env_remove("STITCH_EXIT_CODE_TEST_MISSING_ENV")
        .arg("apply")
        .assert()
        .failure()
        .code(8)
        .stderr(contains("hint:"))
        .stderr(contains("env"));
}

#[test]
fn exit_code_and_hint_path_validation() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
files = ["../init.lua"]
"#
    ));
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_and_hint_hook_failure() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join(".s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s]
target = "{target_str}"
files = ["f"]
"#
    ));
    repo.write_authored(
        r#"
[stores.s.hooks]
pre = "exit 1"
"#,
    );
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(10)
        .stdout(contains("hook"))
        .stderr(contains("hook"))
        .stderr(contains("hint:"));
}

#[test]
fn exit_code_mixed_when_apply_has_multiple_failure_classes() {
    // §3 aggregation rule: a single failure class → that class's code;
    // multiple classes → 11. Exercise the multi-class path by giving one
    // store a real-file conflict (code 6) and another a foreign symlink
    // (code 7) in the same `apply` run.
    let repo = Repo::new();
    repo.make_store("real", &["init.lua"]);
    repo.make_store("foreign", &["init.lua"]);

    let real_target = repo.path().join("home").join(".config").join("real");
    let foreign_target = repo.path().join("home").join(".config").join("foreign");
    fs::create_dir_all(real_target.parent().unwrap()).unwrap();
    fs::create_dir_all(foreign_target.parent().unwrap()).unwrap();

    // Real file blocks `real`; foreign symlink blocks `foreign`.
    fs::write(&real_target, "real file").unwrap();
    std::os::unix::fs::symlink("/etc/foreign", &foreign_target).unwrap();

    repo.write_state(&format!(
        r#"
[stores.real]
target = "{real}"

[stores.foreign]
target = "{foreign}"
"#,
        real = real_target.to_string_lossy(),
        foreign = foreign_target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(11)
        .stdout(contains("conflict"))
        .stderr(contains("hint:"));
}

#[test]
fn source_less_stale_removal_keeps_its_explicit_shared_target_store() {
    // `alpha` sorts before `zeta`, but the stale link belongs to `zeta`.
    // Restricting execution to zeta proves source-less cleanup is no longer
    // attributed by an ambiguous shared target path.
    let repo = Repo::new();
    repo.make_store("alpha", &["keep"]);
    repo.make_store("zeta", &["old", "new"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.alpha]
target = "{0}"
files = ["keep"]

[stores.zeta]
target = "{0}"
files = ["old", "new"]
"#,
        home.display()
    ));
    repo.cmd().arg("apply").assert().success();
    assert!(home.join("old").is_symlink());

    let marker = repo.path().join("zeta-pre-hook-ran");
    repo.write_split(
        &format!(
            r#"
[stores.alpha]
target = "{0}"
files = ["keep"]

[stores.zeta]
target = "{0}"
files = ["new"]
"#,
            home.display()
        ),
        &format!(
            r#"
[stores.zeta.hooks]
pre = "touch {}"
"#,
            marker.display()
        ),
    );

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let removal = plan["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| {
            op["op"] == "remove_link" && op["target"] == home.join("old").to_string_lossy().as_ref()
        })
        .expect("zeta's stale link is captured");
    assert_eq!(removal["store"], "zeta");
    assert!(
        removal.get("source").is_none(),
        "stale cleanup is source-less"
    );
    plan["stores"] = serde_json::json!(["zeta"]);

    let plan_path = repo.path().join("shared-target-removal.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();

    assert!(marker.exists(), "zeta's hook must own the removal group");
    assert!(!home.join("old").exists());
    assert!(home.join("keep").is_symlink());
    assert!(home.join("new").is_symlink());
}

#[test]
fn status_rejects_hard_linked_state_file() {
    // Same bypass through a hard link must be rejected by `status` too.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();

    let external = tempfile::tempdir().unwrap();
    let external_state = external.path().join("state.toml");
    fs::write(
        &external_state,
        "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n",
    )
    .unwrap();

    let state = repo.path().join(".stitch/state.toml");
    fs::remove_file(&state).unwrap();
    fs::hard_link(&external_state, &state).unwrap();

    repo.cmd()
        .arg("status")
        .env("HOME", home.path())
        .assert()
        .failure()
        .code(3)
        .stderr(contains("hard-linked"));
}

#[test]
fn status_succeeds_with_regular_state_file() {
    // A normal state.toml (nlink == 1) must still load and report status.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state("[stores.app]\ntarget = \"~\"\nfiles = [\"file\"]\n");

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("app"));

    // Sanity check: the state file we wrote has the expected link count.
    let state = repo.path().join(".stitch/state.toml");
    assert_eq!(fs::metadata(&state).unwrap().nlink(), 1);
}

#[test]
fn status_rejects_absolute_target_outside_home() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    repo.make_store("evil", &["a"]);

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("abs_status_target");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.evil]
target = "{target_str}"
files = ["a"]
"#
    ));

    repo.cmd()
        .arg("status")
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"))
        .stderr(contains(format!("'{target_str}'")));

    assert!(!target.exists());
}

#[test]
fn status_rejects_store_with_files_but_no_target() {
    let repo = Repo::new();
    repo.write_state(
        r#"
[stores.a]
files = ["f"]
"#,
    );

    repo.cmd()
        .arg("status")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("store 'a'"))
        .stderr(contains("must have a target"))
        .stderr(contains("internal error").not());
}

#[test]
fn list_rejects_store_with_files_but_no_target() {
    let repo = Repo::new();
    repo.write_state(
        r#"
[stores.a]
files = ["f"]
"#,
    );

    repo.cmd()
        .arg("list")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("store 'a'"))
        .stderr(contains("must have a target"));
}

#[test]
fn status_works_for_valid_store_with_target() {
    let repo = Repo::new();
    repo.make_store("bash", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bash]
target = "~/.bashrc"
files = [".bashrc"]
"#,
    );

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("bash"));
}

#[test]
fn list_allows_targetless_store_with_no_files() {
    // Per SPEC, a store with no inventory is legal authored/dead behavior.
    let repo = Repo::new();
    repo.write_state("[stores.behavior]\n");

    repo.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(contains("behavior"))
        .stdout(contains("(no target)"));
}
