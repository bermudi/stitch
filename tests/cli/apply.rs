//! Core `stitch apply` — whole-dir and file-mode linking, conflicts, and `--force`/`--only` (split from `tests/cli.rs`).
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
fn apply_whole_dir_creates_symlink() {
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
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("created"));

    assert!(target.is_symlink());
    assert_eq!(
        fs::read_link(&target).unwrap(),
        repo.path().join("nvim").canonicalize().unwrap()
    );
}

#[test]
fn apply_file_mode_links_individual_files() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc", ".zshrc", ".profile"]);
    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{target_str}"
files = [".bashrc", ".zshrc"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.join(".bashrc").is_symlink());
    assert!(target.join(".zshrc").is_symlink());
    // .profile was not in the explicit files list, must not be linked.
    assert!(!target.join(".profile").exists());
}

#[test]
fn apply_dry_run_makes_no_changes() {
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
        .args(["apply", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("Dry run"));

    assert!(!target.exists());
}

#[test]
fn apply_already_linked_reports_no_change() {
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
    // Second run should report each link as already-present (the `ok` label)
    // and still succeed.
    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("ok"));
}

#[test]
fn apply_reports_conflict_for_real_file_at_target() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    // Plant a real file at the target location.
    fs::write(&target, "I am a real file").unwrap();
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
        .stdout(contains("conflict"));

    // The real file must still be there.
    assert!(target.is_file());
    assert_eq!(fs::read_to_string(&target).unwrap(), "I am a real file");
}

#[test]
fn apply_replaces_repo_owned_broken_symlink() {
    // A symlink pointing into THIS repo (but at a now-missing path) is stale
    // stitch state — the store was moved or a file renamed. apply self-heals by
    // relinking rather than treating it as a conflict.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    // Broken, but repo-owned: points into the store at a path that no longer exists.
    let stale = repo.path().join("nvim").join("does-not-exist");
    std::os::unix::fs::symlink(&stale, &target).unwrap();
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
        .success()
        .stdout(contains("replaced"));

    // After apply, the link resolves into our store.
    assert!(target.is_symlink());
    let resolved = fs::read_link(&target).unwrap();
    assert!(resolved.starts_with(repo.path()));
}

#[test]
fn apply_atomic_replace_leaves_no_temp_files() {
    // The atomic-replace path creates temp siblings (.name.stitch-link-*, .name.stitch-orig-*)
    // and must clean them up on success. A stale repo-owned symlink is the trigger.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let parent = target.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    let stale = repo.path().join("nvim").join("does-not-exist");
    std::os::unix::fs::symlink(&stale, &target).unwrap();
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
        .success()
        .stdout(contains("replaced"));

    assert!(target.is_symlink());
    // No leftover temp artifacts in the target's parent directory.
    let leftovers: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with(".nvim.stitch-link-") || s.starts_with(".nvim.stitch-orig-")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic replace left temp files: {leftovers:?}"
    );
}

#[test]
fn apply_atomic_replace_preserves_original_when_create_fails() {
    // If the new link cannot be created (target parent is read-only), the
    // original stale symlink must remain in place — not be deleted first and
    // then left absent. This is the v0.11.4 release assessment's "failed apply
    // deletes a dangling link" finding.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let parent = target.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    let stale = repo.path().join("nvim").join("does-not-exist");
    std::os::unix::fs::symlink(&stale, &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    // Make the parent read-only so creating the temp symlink fails. Skip under
    // root, which bypasses permission checks.
    if is_root() {
        eprintln!("skipping read-only-parent assertion under root");
        return;
    }
    let mut perms = fs::metadata(parent).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(parent, perms).unwrap();

    let result = repo.cmd().arg("apply").output().unwrap();
    // Restore permissions so cleanup works.
    let mut perms = fs::metadata(parent).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(parent, perms).unwrap();

    assert!(
        !result.status.success(),
        "apply must fail when the target parent is read-only"
    );
    // The original stale symlink must still be present (atomic replace did not
    // remove it before failing to create the replacement).
    assert!(
        target.symlink_metadata().is_ok(),
        "original stale symlink must survive a failed atomic replace"
    );
    assert_eq!(
        fs::read_link(&target).unwrap(),
        stale,
        "original symlink target must be unchanged after a failed replace"
    );
}

#[test]
fn apply_conflicts_on_foreign_symlink() {
    // A symlink managed by another tool (stow/chezmoi/Nix/Home-Manager) points
    // outside this repo — even when its target is valid. apply must report a
    // conflict and leave it untouched, never silently clobber it.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    // A valid foreign link: another manager's store lives outside this repo.
    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign = foreign_dir.path().join("nvim");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("init.lua"), "not ours").unwrap();
    std::os::unix::fs::symlink(&foreign, &target).unwrap();
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
        .stdout(contains("conflict"));

    // The foreign symlink is untouched and still resolves into the foreign store.
    assert!(target.is_symlink(), "foreign symlink must not be clobbered");
    assert_eq!(fs::read_link(&target).unwrap(), foreign);
    assert_eq!(
        fs::read_to_string(target.join("init.lua")).unwrap(),
        "not ours"
    );
}

#[test]
fn apply_conflicts_on_dangling_foreign_symlink() {
    // A dangling symlink to a path outside this repo (stale user link, leftover
    // from another tool) is foreign, so it's a conflict — not silently replaced.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/nonexistent/path/that/does/not/exist", &target).unwrap();
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
        .stdout(contains("conflict"));

    // Untouched: still the same dangling foreign symlink.
    assert!(target.is_symlink());
    assert_eq!(
        fs::read_link(&target).unwrap(),
        Path::new("/nonexistent/path/that/does/not/exist")
    );
}

#[test]
fn apply_does_not_clobber_gateway_foreign_symlink() {
    // P0 regression: a hand-managed link that points *through* a repo gateway
    // symlink to an external path must be a conflict, not silently replaced.
    //
    //   repo/gateway -> /external
    //   home/file    -> repo/gateway/victim
    //
    // The immediate-hop readlink is beneath the repo, so the old lexical
    // ownership check classified this link as repo-owned and apply silently
    // replaced it. Broad canonical ownership follows the chain out of the repo
    // and reports a conflict instead.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    let target_dir = repo.path().join("home");
    fs::create_dir_all(&target_dir).unwrap();

    // Repo gateway symlink -> external dir with a real victim inside.
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("victim"), "foreign").unwrap();
    let gateway = repo.path().join("gateway");
    std::os::unix::fs::symlink(external.path(), &gateway).unwrap();

    // Hand-managed link pointing through the gateway.
    let target = target_dir.join("file");
    std::os::unix::fs::symlink(gateway.join("victim"), &target).unwrap();

    let target_str = target_dir.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
files = ["file"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("conflict"));

    // The hand-managed link is untouched and still resolves through the gateway.
    assert!(
        target.is_symlink(),
        "gateway foreign link must not be clobbered"
    );
    assert_eq!(fs::read_link(&target).unwrap(), gateway.join("victim"));
    assert_eq!(fs::read_to_string(&target).unwrap(), "foreign");
}

#[test]
fn apply_does_not_clobber_dangling_gateway_foreign_symlink() {
    // Same gateway shape, but the victim does not exist. The gateway itself
    // resolves, so partial resolution still follows it out of the repo — the
    // dangling-through-gateway link is foreign, not a stale stitch link, and
    // apply must not replace it.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    let target_dir = repo.path().join("home");
    fs::create_dir_all(&target_dir).unwrap();

    let external = tempfile::tempdir().unwrap();
    let gateway = repo.path().join("gateway");
    std::os::unix::fs::symlink(external.path(), &gateway).unwrap();

    let target = target_dir.join("file");
    std::os::unix::fs::symlink(gateway.join("gone"), &target).unwrap();

    let target_str = target_dir.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
files = ["file"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("conflict"));

    assert!(
        target.is_symlink(),
        "dangling gateway link must not be clobbered"
    );
    assert_eq!(fs::read_link(&target).unwrap(), gateway.join("gone"));
}

#[test]
fn apply_validates_escaped_source_before_removing_old_link() {
    let repo = Repo::new();
    let store = repo.make_store("s", &["old"]);
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("new"), "external").unwrap();
    std::os::unix::fs::symlink(external.path(), store.join("gateway")).unwrap();

    let home = repo.path().join("home");
    let target = home.join("gateway/new");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(store.join("old"), &target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.s]
target = "{}"
files = ["gateway/new"]
"#,
        home.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().failure();
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(target).unwrap(), store.join("old"));
}

#[test]
fn apply_force_backs_up_real_file_and_links() {
    // A real file at the target + --force: the file is renamed to
    // {target}.bak, the symlink takes its place, and the original content is
    // preserved.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "I am a real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .args(["apply", "--force"])
        .assert()
        .success()
        .stdout(contains("backed up"));

    // Original content is now at {target}.bak.
    let backup = format!("{}.bak", target.display());
    assert!(Path::new(&backup).is_file());
    assert_eq!(fs::read_to_string(&backup).unwrap(), "I am a real file");
    // The target is now a symlink into the store.
    assert!(target.is_symlink());
}

#[test]
fn apply_force_backs_up_real_directory() {
    // A real directory at the target (the common case — e.g. a pre-existing
    // ~/.config/nvim) is backed up the same way under --force.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("old.txt"), "legacy").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .args(["apply", "--force"])
        .assert()
        .success()
        .stdout(contains("backed up"));

    let backup = format!("{}.bak", target.display());
    assert!(Path::new(&backup).is_dir());
    assert_eq!(
        fs::read_to_string(Path::new(&backup).join("old.txt")).unwrap(),
        "legacy"
    );
    assert!(target.is_symlink());
}

#[test]
fn apply_force_fails_when_bak_already_exists() {
    // If {target}.bak already exists, --force must fail rather than destroy
    // the prior backup. The original target and the existing .bak are both
    // left untouched.
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
        .args(["apply", "--force"])
        .assert()
        .failure()
        .stdout(contains("conflict"));

    // Nothing moved: target is still the real file, .bak unchanged.
    assert_eq!(fs::read_to_string(&target).unwrap(), "current");
    assert!(!target.is_symlink());
    assert_eq!(fs::read_to_string(&backup).unwrap(), "previous backup");
}

#[test]
fn apply_force_does_not_clobber_foreign_symlink() {
    // --force resolves real-file/dir conflicts only. A foreign symlink
    // (another tool's managed link) stays a hard conflict even under --force.
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign = foreign_dir.path().join("nvim");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("init.lua"), "not ours").unwrap();
    std::os::unix::fs::symlink(&foreign, &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .args(["apply", "--force"])
        .assert()
        .failure()
        .stdout(contains("conflict"));

    // Untouched.
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), foreign);
    assert!(!Path::new(&format!("{}.bak", target.display())).exists());
}

#[test]
fn apply_conflicts_on_foreign_symlink_at_target_root_file_mode() {
    // File-mode store with the target root itself a foreign symlink. The
    // symlink must be treated as a conflict, not silently followed, and no
    // child link may be written into the foreign directory.
    let repo = Repo::new();
    repo.make_store("config", &["foo"]);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();

    // Foreign directory containing a real file at the same name.
    let foreign = tempfile::tempdir().unwrap();
    let foreign_dir = foreign.path().join("config");
    fs::create_dir_all(&foreign_dir).unwrap();
    let foreign_file = foreign_dir.join("foo");
    fs::write(&foreign_file, "foreign foo").unwrap();

    // ~/.config is a symlink into the foreign directory.
    let config_link = home.path().join(".config");
    std::os::unix::fs::symlink(&foreign_dir, &config_link).unwrap();

    repo.write_state(
        r#"
[stores.config]
target = "~/.config"
files = ["foo"]
"#,
    );

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(7)
        .stdout(contains("conflict"));

    // The target-root symlink is untouched.
    assert!(config_link.is_symlink());
    assert_eq!(fs::read_link(&config_link).unwrap(), foreign_dir);

    // No stitch link was created inside the foreign directory and the
    // existing foreign file is unchanged.
    assert!(!foreign_dir.join("foo").is_symlink());
    assert_eq!(fs::read_to_string(&foreign_file).unwrap(), "foreign foo");
    assert!(!foreign_dir.join("foo.bak").exists());
}

#[test]
fn apply_force_does_not_clobber_foreign_symlink_at_target_root_file_mode() {
    // Even with --force, a file-mode target root that is a foreign symlink
    // must remain a hard conflict and must not displace foreign content.
    let repo = Repo::new();
    repo.make_store("config", &["foo"]);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();

    let foreign = tempfile::tempdir().unwrap();
    let foreign_dir = foreign.path().join("config");
    fs::create_dir_all(&foreign_dir).unwrap();
    let foreign_file = foreign_dir.join("foo");
    fs::write(&foreign_file, "foreign foo").unwrap();

    let config_link = home.path().join(".config");
    std::os::unix::fs::symlink(&foreign_dir, &config_link).unwrap();

    repo.write_state(
        r#"
[stores.config]
target = "~/.config"
files = ["foo"]
"#,
    );

    repo.cmd()
        .args(["apply", "--force"])
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(7)
        .stdout(contains("conflict"));

    assert!(config_link.is_symlink());
    assert_eq!(fs::read_link(&config_link).unwrap(), foreign_dir);
    assert!(!foreign_dir.join("foo").is_symlink());
    assert_eq!(fs::read_to_string(&foreign_file).unwrap(), "foreign foo");
    assert!(!foreign_dir.join("foo.bak").exists());
}

#[test]
fn apply_rejects_traversal_in_files() {
    // A `../` file entry would symlink outside the target dir. Config load
    // must reject it before any link is created.
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{target_str}"
files = ["../escape"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("invalid file entry"))
        .stderr(contains("'../escape'"));

    // Validation happened at load, before apply ran — nothing was linked.
    assert!(!target.exists());
}

#[test]
fn apply_rejects_absolute_in_files() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{target_str}"
files = ["/etc/passwd"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("invalid file entry"))
        .stderr(contains("'/etc/passwd'"));
}

#[test]
fn apply_allows_nested_file_entries() {
    // Regression guard: nested relative paths are legitimate and must still
    // link correctly now that validation runs at load time.
    let repo = Repo::new();
    let store_dir = repo.path().join("nvim");
    fs::create_dir_all(store_dir.join("lua")).unwrap();
    fs::write(store_dir.join("lua").join("init.lua"), "...").unwrap();
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
files = ["lua/init.lua"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.join("lua").join("init.lua").is_symlink());
}

/// v0.7.0 accepted `files = ["./bashrc"]`; v0.7.1 regressed and rejected it
/// at load time. Config validation must pass and the link must be created.
#[test]
fn apply_accepts_dot_slash_file_entries() {
    let repo = Repo::new();
    repo.make_store("shells", &["bashrc"]);
    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{target_str}"
files = ["./bashrc"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    let link = target.join("bashrc");
    assert!(link.is_symlink());
    assert_eq!(
        fs::read_link(&link).unwrap(),
        repo.path()
            .join("shells")
            .join("bashrc")
            .canonicalize()
            .unwrap()
    );
}

#[test]
fn apply_only_filter_restricts_to_named_stores() {
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

    repo.cmd()
        .args(["apply", "--only", "nvim"])
        .assert()
        .success();

    assert!(nvim_target.is_symlink());
    // shells was filtered out — nothing should have been linked for it.
    assert!(!shells_target.join(".bashrc").exists());
}

#[test]
fn apply_missing_store_dir_reports_error() {
    let repo = Repo::new();
    // Config references a store but the directory does not exist.
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("error"))
        .stdout(contains("nvim"));
}

#[test]
fn apply_store_with_no_target_reports_error() {
    let repo = Repo::new();
    // Empty store, no target configured.
    repo.make_store("nvim", &["init.lua"]);
    repo.write_state(
        r#"
[stores.nvim]
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("no target configured"));
}

#[test]
fn apply_only_unknown_store_errors() {
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
        .args(["apply", "--only", "nonexistent"])
        .assert()
        .failure()
        .stderr(contains("unknown store"));
}

#[test]
fn apply_only_partial_unknown_errors_and_aborts() {
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

    // Even though "nvim" is a real store, the unknown "typo" should
    // abort the whole apply — partial application on typos is confusing.
    repo.cmd()
        .args(["apply", "--only", "nvim", "--only", "typo"])
        .assert()
        .failure()
        .stderr(contains("unknown store"))
        .stderr(contains("typo"));

    // nvim was NOT applied — we abort on any unknown.
    assert!(!target.is_symlink());
}

#[test]
fn apply_promotion_validates_all_sources_before_removing_whole_dir_link() {
    let repo = Repo::new();
    let store = repo.make_store("app", &[]);
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("new"), "external").unwrap();
    std::os::unix::fs::symlink(external.path(), store.join("gateway")).unwrap();
    let target = repo.path().join("home/app");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &target).unwrap();
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"gateway/new\"]\n",
        target.display()
    ));

    repo.cmd().arg("apply").assert().failure();
    assert_eq!(fs::read_link(&target).unwrap(), store);
}

#[test]
fn apply_rejects_config_changed_by_store_hook_before_mutation() {
    let repo = Repo::new();
    repo.make_store("app", &["new"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"new\"]\n",
        home.display()
    ));
    repo.write_authored(&format!(
        "[stores.app]\nhooks = {{ pre = \"echo '# hook mutation' >> {}\" }}\n",
        repo.path().join(".stitch/state.toml").display()
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("config hash mismatch after pre-hook"));
    assert!(
        !home.join("new").exists(),
        "a store hook that changes config must not reach target mutation"
    );
}

#[test]
fn apply_rejects_hard_linked_state_file_before_linking() {
    // A hard link to an external state file must not be used to author the
    // link inventory. The tool should reject state.toml with nlink > 1 and
    // must not create any target symlink.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
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
        .arg("apply")
        .env("HOME", home.path())
        .assert()
        .failure()
        .code(3)
        .stderr(contains("hard-linked"));

    // The externally authored store must not have been applied.
    assert!(!home.path().join(".app").exists());
    assert!(!home.path().join(".app/file").exists());
}

#[test]
fn apply_rejects_symlinked_stitch_toml_before_hook() {
    // A symlinked stitch.toml would let an external file author store behavior
    // and hooks. Apply must refuse it before running the pre-hook or linking.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state(
        r#"[stores.app]
target = "~/.config/app"
"#,
    );

    let marker = repo.path().join("pwned");
    let external = tempfile::tempdir().unwrap();
    let external_authored = external.path().join("stitch.toml");
    fs::write(
        &external_authored,
        format!(
            "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
            marker.display()
        ),
    )
    .unwrap();

    let authored = repo.path().join("stitch.toml");
    fs::remove_file(&authored).unwrap();
    std::os::unix::fs::symlink(&external_authored, &authored).unwrap();

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("authored config"));

    assert!(
        !marker.exists(),
        "pre-hook must not run on symlinked stitch.toml"
    );
    assert!(
        !repo.path().join(".config").join("app").exists(),
        "apply must not link through a symlinked stitch.toml"
    );
}

#[test]
fn apply_rejects_hard_linked_stitch_toml_before_hook() {
    // A hard link to an external stitch.toml must not be used to author hooks.
    // Apply must reject nlink > 1 before the pre-hook or linking.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state(
        r#"[stores.app]
target = "~/.config/app"
"#,
    );

    let marker = repo.path().join("pwned");
    let external = tempfile::tempdir().unwrap();
    let external_authored = external.path().join("stitch.toml");
    fs::write(
        &external_authored,
        format!(
            "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
            marker.display()
        ),
    )
    .unwrap();

    let authored = repo.path().join("stitch.toml");
    fs::remove_file(&authored).unwrap();
    fs::hard_link(&external_authored, &authored).unwrap();

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("hard-linked"));

    assert!(
        !marker.exists(),
        "pre-hook must not run on hard-linked stitch.toml"
    );
    assert!(
        !repo.path().join(".config").join("app").exists(),
        "apply must not link through a hard-linked stitch.toml"
    );
}

#[test]
fn apply_succeeds_with_regular_stitch_toml() {
    // A normal stitch.toml (nlink == 1, regular file) must still load, run
    // hooks, and apply links.
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state(
        r#"[stores.app]
target = "~/.config/app"
"#,
    );

    let marker = repo.path().join("pre-ran");
    repo.write_authored(&format!(
        "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
        marker.display()
    ));

    repo.cmd().arg("apply").assert().success();

    let target = repo.path().join(".config").join("app");
    assert!(marker.exists(), "pre-hook should have run");
    assert!(target.is_symlink(), "store should be applied");

    let authored = repo.path().join("stitch.toml");
    assert_eq!(fs::metadata(&authored).unwrap().nlink(), 1);
}

#[test]
fn apply_rejects_absolute_target_outside_home() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    repo.make_store("evil", &["authorized_keys"]);

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("abs_target");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.evil]
target = "{target_str}"
files = ["authorized_keys"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"))
        .stderr(contains(format!("'{target_str}'")));

    assert!(!target.exists());
    assert!(!target.join("authorized_keys").exists());
}

#[test]
fn apply_rejects_dotdot_target_escaping_home() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    repo.make_store("evil", &["authorized_keys"]);

    let outside = tempfile::tempdir().unwrap();
    let evil = outside.path().join("evil_target");
    let target = format!("~/../../../{}", evil.display());
    repo.write_state(&format!(
        r#"
[stores.evil]
target = "{target}"
files = ["authorized_keys"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"))
        .stderr(contains("'~/../../../"));

    assert!(!evil.exists());
}

#[test]
fn apply_force_rejects_target_outside_home_and_preserves_victim_file() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    repo.make_store("evil", &["authorized_keys"]);

    // A real file outside $HOME that must not be touched.
    let victim = tempfile::tempdir().unwrap();
    let victim_dir = victim.path().join("victim");
    fs::create_dir_all(&victim_dir).unwrap();
    let victim_file = victim_dir.join("authorized_keys");
    let original = "victim's real authorized_keys\n";
    fs::write(&victim_file, original).unwrap();

    let target_str = victim_dir.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.evil]
target = "{target_str}"
files = ["authorized_keys"]
"#
    ));

    repo.cmd()
        .args(["apply", "--force"])
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"));

    assert!(!victim_dir.join("authorized_keys.bak").exists());
    assert!(!victim_file.is_symlink());
    assert_eq!(fs::read_to_string(&victim_file).unwrap(), original);
}

#[test]
fn apply_accepts_tilde_bashrc_target_inside_home() {
    // Regression guard: a `~` target that stays inside $HOME must still work.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    repo.make_store("bash", &[".bashrc"]);
    repo.write_state(
        r#"
[stores.bash]
target = "~/.bashrc"
"#,
    );

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .success();

    let link = home.path().join(".bashrc");
    assert!(link.is_symlink());
    assert_eq!(
        fs::read_link(&link).unwrap(),
        repo.path().join("bash").canonicalize().unwrap()
    );
}

#[test]
fn apply_rejects_multi_target_outside_home() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    repo.make_store("evil", &["a"]);

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("x");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.evil.targets.foo]
target = "{target_str}"
files = ["a"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .failure()
        .code(9)
        .stderr(contains("invalid target"))
        .stderr(contains(format!("'{target_str}'")));

    assert!(!target.exists());
    assert!(!target.join("a").exists());
}

#[test]
fn apply_io_error_includes_path_context() {
    // Force an I/O error while apply is computing the config hash by making
    // .stitch/ unreadable. Config::load sees the missing state (it cannot read
    // the directory), but compute_config_hash tries to read it and surfaces a
    // raw I/O error. The message must name the file/operation.
    if is_root() {
        eprintln!("note: apply_io_error_includes_path_context skipped under root");
        return;
    }
    let repo = Repo::new();
    let stitch_dir = repo.path().join(".stitch");
    fs::set_permissions(&stitch_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = RestoreMode {
        path: &stitch_dir,
        mode: 0o755,
    };

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("state.toml"))
        .stderr(contains("Permission denied"))
        .stderr(contains("I/O error").not());
}

#[test]
fn apply_file_mode_skips_unreadable_target_subtree() {
    // A file-mode store whose target directory contains an unreadable subtree
    // (e.g. a root-owned podman overlay under `$HOME` when `target = "~"`)
    // must not abort apply. The stale-link scan skips the unreadable subtree
    // and still reconciles the readable parts: the configured link is created
    // and a pre-existing stale link next to the unreadable dir is removed.
    if is_root() {
        eprintln!("note: apply_file_mode_skips_unreadable_target_subtree skipped under root");
        return;
    }
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc", ".zshrc"]);
    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{target_str}"
files = [".bashrc"]
"#
    ));

    // Pre-create the target root with: a stale repo-pointing link (`old` -> a
    // store file not in the kept set) and an unreadable subtree mimicking a
    // foreign root-owned tree under a `target = "~"` store.
    fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(
        repo.path().join("shells").join(".zshrc"),
        target.join("old"),
    )
    .unwrap();
    let unreadable = target.join("unreadable");
    fs::create_dir_all(&unreadable).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = RestoreMode {
        path: &unreadable,
        mode: 0o755,
    };

    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("created"));

    // The configured link is created...
    let link = target.join(".bashrc");
    assert!(link.is_symlink());
    assert_eq!(
        fs::read_link(&link).unwrap(),
        repo.path()
            .join("shells")
            .join(".bashrc")
            .canonicalize()
            .unwrap()
    );
    // ...and the stale link next to the unreadable subtree is still reconciled,
    // proving the scan continues past the skipped error rather than aborting.
    assert!(!target.join("old").exists(), "stale link must be removed");
}

#[test]
fn apply_real_file_root_conflicts_even_with_force() {
    // A regular file at the file-mode target root blocks the whole store.
    // status/doctor report a conflict; diff must not preview child creation;
    // apply (with or without --force) must report the conflict instead of
    // failing internally.
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    let root = repo.path().join(".config").join("app");
    fs::create_dir_all(root.parent().unwrap()).unwrap();
    fs::write(&root, "i am a file, not a directory").unwrap();
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    );

    repo.cmd()
        .arg("diff")
        .assert()
        .failure()
        .stderr(contains("conflict"))
        // `diff` is a read-only preview; its error must not claim "apply failed".
        .stderr(contains("apply failed").not())
        .stderr(contains("diff reported"));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(6)
        .stderr(contains("conflict"));

    repo.cmd()
        .args(["apply", "--force"])
        .assert()
        .failure()
        .code(6)
        .stderr(contains("conflict"));

    // The blocking file must still be there, un-renamed, un-deleted.
    assert_eq!(
        fs::read_to_string(&root).unwrap(),
        "i am a file, not a directory"
    );
}

// ===========================================================================
// Target/link-path collision rejection (v0.11.4 release assessment blocker).
//
// Two active stores claiming the same link path produce a self-contradictory
// desired state: `apply` would report success while the filesystem never
// converges, and `diff --exit-code` alarms forever. Both `apply` and `diff`
// must reject the collision up front (config error, exit 3) instead of
// silently fighting. File-mode stores sharing a target *directory* with
// disjoint files remain legitimate.
// ===========================================================================

#[test]
fn apply_rejects_two_whole_dir_stores_on_same_target() {
    let repo = Repo::new();
    repo.make_store("s1", &[]);
    repo.make_store("s2", &[]);
    let target = repo.path().join("home").join("t");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s1]
target = "{target_str}"

[stores.s2]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("both claim link path"))
        .stderr(contains("store 's1'"))
        .stderr(contains("store 's2'"))
        // The hint must guide the user toward fixing the collision, not
        // toward path-validation issues.
        .stderr(contains("distinct target"));

    // No symlink created: the rejection happens before any mutation.
    assert!(!target.exists());
}

#[test]
fn diff_rejects_two_whole_dir_stores_on_same_target() {
    let repo = Repo::new();
    repo.make_store("s1", &[]);
    repo.make_store("s2", &[]);
    let target = repo.path().join("home").join("t");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s1]
target = "{target_str}"

[stores.s2]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("diff")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("both claim link path"));

    // `diff --exit-code` must not report eternal drift on a self-contradictory
    // repo; it rejects the config instead.
    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(3)
        .stderr(contains("both claim link path"));
}

#[test]
fn apply_rejects_two_file_mode_stores_linking_same_file() {
    let repo = Repo::new();
    repo.make_store("s1", &["x"]);
    repo.make_store("s2", &["x"]);
    let target = repo.path().join("home").join("t");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s1]
target = "{target_str}"
files = ["x"]

[stores.s2]
target = "{target_str}"
files = ["x"]
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("both claim link path"))
        .stderr(contains(&target.join("x").to_string_lossy().to_string()));
}

#[test]
fn apply_allows_file_mode_stores_sharing_target_dir_with_disjoint_files() {
    // The legitimate shared-directory pattern: two file-mode stores link
    // different files into the same target directory. This must NOT be flagged
    // as a collision.
    let repo = Repo::new();
    repo.make_store("alpha", &["keep"]);
    repo.make_store("zeta", &["old", "new"]);
    let home = repo.path().join("home");
    let home_str = home.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.alpha]
target = "{home_str}"
files = ["keep"]

[stores.zeta]
target = "{home_str}"
files = ["old", "new"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(home.join("keep").is_symlink());
    assert!(home.join("old").is_symlink());
    assert!(home.join("new").is_symlink());
}

#[test]
fn apply_rejects_whole_dir_store_overlapping_file_mode_child() {
    // A whole-dir store targeting ~/.config and a file-mode store linking into
    // ~/.config/nvim: the whole-dir symlink would clobber the directory the
    // file-mode store links into. Structurally incompatible -> rejected.
    let repo = Repo::new();
    repo.make_store("alpha", &[]);
    repo.make_store("beta", &["init.lua"]);
    let config = repo.path().join("home").join(".config");
    repo.write_state(&format!(
        r#"
[stores.alpha]
target = "{}"

[stores.beta]
target = "{}/nvim"
files = ["init.lua"]
"#,
        config.display(),
        config.display(),
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("claims nested link path"));
}

#[test]
fn apply_only_skips_collision_check_for_unselected_store() {
    // `apply --only s1` must not error on s2's competing claim, because s2 is
    // not being applied and cannot fight s1 on this run.
    let repo = Repo::new();
    repo.make_store("s1", &[]);
    repo.make_store("s2", &[]);
    let target = repo.path().join("home").join("t");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s1]
target = "{target_str}"

[stores.s2]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .args(["apply", "--only", "s1"])
        .assert()
        .success();

    assert!(target.is_symlink());
    assert_eq!(
        fs::read_link(&target).unwrap(),
        repo.path().join("s1").canonicalize().unwrap()
    );
}

#[test]
fn apply_orphaned_authored_store_reports_config_error_not_internal() {
    // A store declared in stitch.toml but with no link inventory in state.toml
    // (e.g. left behind by `remove`) is a user-facing config problem. It must
    // surface as a config error (exit 3) with a fix hint, not an internal
    // error (exit 1).
    let repo = Repo::new();
    repo.make_store("ghost", &["init.lua"]);
    repo.write_authored(
        r#"
[stores.ghost]
when = { os = "linux" }
"#,
    );
    // No state.toml entry for `ghost` — orphaned authored behavior.
    repo.write_state("");

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stdout(contains("ghost"))
        .stdout(contains("no target configured"));
}

#[test]
fn apply_target_home_does_not_eat_repo_internal_symlinks() {
    // Regression: the stale-link sweep (`reconcile_store_links`) walks the
    // file-mode target to remove repo-pointing symlinks no longer in the
    // store's keep set. A file-mode store with `target = "~"` (the stow-mirror
    // layout) walks all of `$HOME`; when the repo lives under `~`, the sweep
    // used to descend *into the repo* and `remove_link` repo-internal
    // organizational symlinks that resolved into their own store dir. The sweep
    // must prune the repo subtree, so repo-internal layout survives apply.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dotfiles");
    fs::create_dir_all(&repo).unwrap();
    // Minimal v0.3 layout: empty authored half + generated half + gitignore.
    fs::write(repo.join("stitch.toml"), "").unwrap();
    let stitch_dir = repo.join(".stitch");
    fs::create_dir_all(&stitch_dir).unwrap();
    fs::write(stitch_dir.join("state.toml"), "").unwrap();
    fs::write(stitch_dir.join("state.lock"), "").unwrap();
    fs::write(repo.join(".gitignore"), ".stitch/render/\n").unwrap();

    // Store `shells` targets `~` and manages only `.bashrc`.
    let store_dir = repo.join("shells");
    fs::create_dir_all(&store_dir).unwrap();
    fs::write(store_dir.join(".bashrc"), "# bashrc\n").unwrap();
    // A repo-internal organizational symlink inside the store dir: `alias ->
    // realfile`. It points into the store, so without the repo prune the sweep
    // classified it as a stale link for this store and removed it.
    fs::write(store_dir.join("realfile"), "real\n").unwrap();
    std::os::unix::fs::symlink("realfile", store_dir.join("alias")).unwrap();

    fs::write(
        stitch_dir.join("state.toml"),
        "[stores.shells]\ntarget = \"~\"\nfiles = [\".bashrc\"]\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("stitch").expect("stitch binary");
    cmd.current_dir(&repo)
        .env("HOME", &home)
        .env_remove("STITCH_REPO")
        .arg("apply")
        .assert()
        .success();

    // The configured link is created at the top of $HOME...
    let link = home.join(".bashrc");
    assert!(link.is_symlink(), "managed .bashrc link created");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        store_dir.join(".bashrc").canonicalize().unwrap()
    );
    // ...and the repo-internal symlink survives — the sweep never entered the
    // repo, so it was not classified as a stale link for this store.
    assert!(
        store_dir.join("alias").is_symlink(),
        "repo-internal symlink must survive the stale-link sweep"
    );
    assert_eq!(
        fs::read_to_string(store_dir.join("alias")).unwrap(),
        "real\n",
        "repo-internal symlink still resolves"
    );
}

#[test]
fn apply_json_conflict_real_has_recoverable_via() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "I am a real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(v["ok"], false);
    let err = &v["error"];
    assert_eq!(err["class"], "conflict-real");
    assert_eq!(err["code"], 6);
    let actions = err["recoverable_via"]
        .as_array()
        .expect("recoverable_via array");
    // apply aggregates into an Apply error with a single ConflictReal class.
    // It delegates to force-apply (no target-specific args at the aggregate
    // level; the agent should consult the plan ops in `data` for per-entry targets).
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["kind"], "force-apply");
    assert_eq!(actions[0]["command"], "apply");
    assert_eq!(actions[0]["flags"][0], "--force");
}

#[test]
fn apply_json_conflict_foreign_has_manual_recoverable_via() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    // Foreign symlink: points outside this repo (another manager's store).
    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign = foreign_dir.path().join("nvim");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("init.lua"), "not ours").unwrap();
    std::os::unix::fs::symlink(&foreign, &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(v["ok"], false);
    let err = &v["error"];
    assert_eq!(err["class"], "conflict-foreign");
    let actions = err["recoverable_via"]
        .as_array()
        .expect("recoverable_via array");
    // Red line: foreign symlinks are never auto-clobbered — only manual.
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["kind"], "manual");
    assert!(actions[0].get("command").is_none() || actions[0]["command"].is_null());
    assert!(
        actions[0]["reason"]
            .as_str()
            .unwrap()
            .contains("never auto-clobbered")
    );
}

#[test]
fn apply_json_result_happy_path() {
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

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .success()
        .get_output()
        .clone();
    let v = json_output(&output);
    assert_envelope_shape(&v, "apply", true);

    let data = &v["data"];
    assert!(data.is_object());
    let result = &data["result"];
    assert!(result.is_object(), "data.result must be an object");
    assert_eq!(
        result["ops_executed"].as_u64(),
        result["ops_total"].as_u64()
    );
    assert!(result["ops_executed"].as_u64().is_some());
    assert_eq!(result["conflicts"].as_u64(), Some(0));
    assert_eq!(result["errors"].as_u64(), Some(0));

    let stores = result["stores"]
        .as_array()
        .expect("result.stores must be an array");
    assert_eq!(stores.len(), 1);
    let nvim = &stores[0];
    assert_eq!(nvim["store"].as_str(), Some("nvim"));
    assert!(nvim["ok"].as_u64().unwrap_or(0) > 0);
    assert_eq!(nvim["conflicts"].as_u64(), Some(0));
    assert_eq!(nvim["errors"].as_u64(), Some(0));
    assert_eq!(nvim["skipped"].as_u64(), Some(0));
}

#[test]
fn apply_json_result_conflict_real_file() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "I am a real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .failure()
        .get_output()
        .clone();
    let v = json_output(&output);
    assert_envelope_shape(&v, "apply", false);

    let data = &v["data"];
    assert!(data.is_object());
    let result = &data["result"];
    assert!(result.is_object(), "data.result must be an object");
    assert!(result["conflicts"].as_u64().unwrap_or(0) > 0);
    assert_eq!(result["errors"].as_u64(), Some(0));
    assert!(
        result["ops_executed"].as_u64().unwrap_or(0) < result["ops_total"].as_u64().unwrap_or(0)
    );

    let stores = result["stores"]
        .as_array()
        .expect("result.stores must be an array");
    let nvim = stores
        .iter()
        .find(|s| s["store"].as_str() == Some("nvim"))
        .expect("nvim in result.stores");
    assert!(nvim["conflicts"].as_u64().unwrap_or(0) > 0);
    assert_eq!(nvim["errors"].as_u64(), Some(0));
}

#[test]
fn apply_json_result_error_no_target() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.write_state(
        r#"
[stores.nvim]
"#,
    );

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .failure()
        .get_output()
        .clone();
    let v = json_output(&output);
    assert_envelope_shape(&v, "apply", false);

    let data = &v["data"];
    assert!(data.is_object());
    let result = &data["result"];
    assert!(result.is_object(), "data.result must be an object");
    assert!(result["errors"].as_u64().unwrap_or(0) > 0);
    assert_eq!(result["conflicts"].as_u64(), Some(0));

    let stores = result["stores"]
        .as_array()
        .expect("result.stores must be an array");
    let nvim = stores
        .iter()
        .find(|s| s["store"].as_str() == Some("nvim"))
        .expect("nvim in result.stores");
    assert!(nvim["errors"].as_u64().unwrap_or(0) > 0);
    assert_eq!(nvim["conflicts"].as_u64(), Some(0));
}

#[test]
fn apply_json_post_status_conflict_real_file() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "I am a real file").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .failure()
        .get_output()
        .clone();
    let v = json_output(&output);
    let post_status = v["data"]["post_status"]
        .as_array()
        .expect("data.post_status must be an array");
    assert!(
        !post_status.is_empty(),
        "post_status must not be empty on conflict"
    );
    let entry = &post_status[0];
    assert_eq!(entry["store"].as_str(), Some("nvim"));
    assert_eq!(entry["state"].as_str(), Some("conflict"));
}

#[test]
fn apply_json_post_status_foreign_symlink() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign = foreign_dir.path().join("nvim");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("init.lua"), "not ours").unwrap();
    std::os::unix::fs::symlink(&foreign, &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    let output = repo
        .cmd()
        .arg("--json")
        .arg("apply")
        .assert()
        .failure()
        .get_output()
        .clone();
    let v = json_output(&output);
    let post_status = v["data"]["post_status"]
        .as_array()
        .expect("data.post_status must be an array");
    assert!(
        !post_status.is_empty(),
        "post_status must not be empty on foreign symlink"
    );
    let entry = &post_status[0];
    assert_eq!(entry["store"].as_str(), Some("nvim"));
    assert_eq!(entry["state"].as_str(), Some("foreign"));
}
