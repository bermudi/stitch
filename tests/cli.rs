//! End-to-end tests for the `stitch` CLI binary.
//!
//! These tests build and exercise the binary via `assert_cmd`. Each test gets
//! a fresh tempdir that acts as the repo root, and writes a `.stitch/config.toml`
//! directly (bypassing the `init` command) to keep the test bodies focused.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::str::contains;

/// A scratch repo: a tempdir with `.stitch/` initialized and a configured
/// `Config` written to `.stitch/config.toml`. Tests can further mutate the
/// filesystem (e.g. create store directories, source files) as needed.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let stitch = dir.path().join(".stitch");
        fs::create_dir_all(&stitch).expect("mkdir .stitch");
        let empty = "vars = {}\n\n[stores]\n";
        fs::write(stitch.join("config.toml"), empty).expect("write config");
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a complete config.toml from a TOML string.
    fn write_config(&self, toml: &str) {
        fs::write(self.dir.path().join(".stitch").join("config.toml"), toml)
            .expect("write config");
    }

    /// Convenience: create a directory with some files inside the repo.
    fn make_store(&self, name: &str, files: &[&str]) -> PathBuf {
        let store_dir = self.dir.path().join(name);
        fs::create_dir_all(&store_dir).expect("mkdir store");
        for f in files {
            fs::write(store_dir.join(f), format!("contents of {f}")).expect("write file");
        }
        store_dir
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("stitch").expect("stitch binary");
        c.current_dir(self.dir.path());
        c.env_remove("EDITOR"); // avoid any inherited editor
        c
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

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

    let config_path = dir.path().join(".stitch").join("config.toml");
    assert!(config_path.exists());
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

// ---------------------------------------------------------------------------
// not in a repo
// ---------------------------------------------------------------------------

#[test]
fn apply_outside_repo_errors() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("not inside a stitch repo"));
}

#[test]
fn list_outside_repo_errors() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("stitch")
        .unwrap()
        .current_dir(dir.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(contains("not inside a stitch repo"));
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

#[test]
fn apply_whole_dir_creates_symlink() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_config(&format!(
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
    repo.write_config(&format!(
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
    repo.write_config(&format!(
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
    repo.write_config(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();
    // Second run should report each link as already-present (the `✓` glyph)
    // and still succeed.
    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("✓"));
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
    repo.write_config(&format!(
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
fn apply_replaces_broken_symlink() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    // Plant a symlink pointing somewhere that doesn't exist (broken).
    std::os::unix::fs::symlink("/nonexistent/path/that/does/not/exist", &target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    repo.write_config(&format!(
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

    // After apply, the link should now resolve to our store.
    assert!(target.is_symlink());
    let resolved = fs::read_link(&target).unwrap();
    assert!(resolved.starts_with(repo.path()));
}

#[test]
fn apply_only_filter_restricts_to_named_stores() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.make_store("shells", &[".bashrc"]);
    let nvim_target = repo.path().join("home").join(".config").join("nvim");
    let shells_target = repo.path().join("home");
    repo.write_config(&format!(
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
    repo.write_config(&format!(
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
    repo.write_config(
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

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_reports_linked_and_missing() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.make_store("shells", &[".bashrc"]);

    let nvim_target = repo.path().join("home").join(".config").join("nvim");
    let shells_target = repo.path().join("home");
    repo.write_config(&format!(
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
    repo.write_config(&format!(
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
    repo.write_config(&format!(
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
fn status_name_filter_shows_only_matching_store() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.make_store("shells", &[".bashrc"]);

    let nvim_target = repo.path().join("home").join(".config").join("nvim");
    let shells_target = repo.path().join("home");
    repo.write_config(&format!(
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

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

#[test]
fn diff_is_dry_run_apply() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_config(&format!(
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

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[test]
fn list_shows_single_target_stores() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_config(&format!(
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
    let repo = Repo::new();
    let t1 = repo.path().join("home1");
    let t2 = repo.path().join("home2");
    repo.write_config(&format!(
        r#"
[stores.shells]
targets = [
    {{ target = "{}" }},
    {{ target = "{}" }},
]
"#,
        t1.to_string_lossy(),
        t2.to_string_lossy(),
    ));

    repo.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(contains("shells"))
        .stdout(contains("2 targets"));
}

#[test]
fn list_marks_stores_without_target() {
    let repo = Repo::new();
    repo.write_config(
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

// ---------------------------------------------------------------------------
// adopt
// ---------------------------------------------------------------------------

#[test]
fn adopt_dry_run_makes_no_changes() {
    let repo = Repo::new();
    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["adopt", src.to_str().unwrap(), "--dry-run"])
        .assert()
        .success()
        .stdout(contains("Would adopt"));

    // Nothing was moved.
    assert!(src.exists());
    assert!(!repo.path().join("myrc").exists());
}

#[test]
fn adopt_file_moves_and_links_back() {
    let repo = Repo::new();
    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["adopt", src.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Adopted"));

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
fn adopt_dir_moves_and_links_back() {
    let repo = Repo::new();
    let src = repo.path().join("external").join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    repo.cmd()
        .args(["adopt", src.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Adopted"));

    // The directory should now be inside the repo.
    assert!(repo.path().join("myconfig").is_dir());
    // And the original location should be a symlink back.
    assert!(src.is_symlink());
}

#[test]
fn adopt_rejects_existing_symlink() {
    let repo = Repo::new();
    let src = repo.path().join("external").join("myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/elsewhere", &src).unwrap();

    repo.cmd()
        .args(["adopt", src.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("already a symlink"));
}

#[test]
fn adopt_missing_path_errors() {
    let repo = Repo::new();
    repo.cmd()
        .args(["adopt", "/nonexistent/path/abc"])
        .assert()
        .failure()
        .stderr(contains("path does not exist"));
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

#[test]
fn add_creates_store_without_immediate_link() {
    let repo = Repo::new();
    repo.cmd()
        .args(["add", "shells"])
        .assert()
        .success()
        .stdout(contains("Added store 'shells'"));

    // Store directory should be created.
    assert!(repo.path().join("shells").is_dir());
    // Config should have the entry.
    let config_text =
        fs::read_to_string(repo.path().join(".stitch").join("config.toml")).unwrap();
    assert!(config_text.contains("shells"));
}

#[test]
fn add_with_target_links_immediately() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();

    repo.cmd()
        .args(["add", "nvim", &target_str])
        .assert()
        .success()
        .stdout(contains("linked"));

    // Store was created, target symlinked.
    assert!(repo.path().join("nvim").is_dir());
    assert!(target.is_symlink());
}

#[test]
fn add_duplicate_store_errors() {
    let repo = Repo::new();
    repo.cmd().args(["add", "shells"]).assert().success();

    repo.cmd()
        .args(["add", "shells"])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

#[test]
fn remove_drops_store_and_unlinks() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();

    // Add with a target so the link is created.
    repo.cmd()
        .args(["add", "nvim", &target_str])
        .assert()
        .success();
    assert!(target.is_symlink());

    repo.cmd()
        .args(["remove", "nvim"])
        .assert()
        .success()
        .stdout(contains("Removed store 'nvim'"));

    // Config entry gone, symlink gone, repo directory left untouched.
    assert!(!target.exists());
    assert!(repo.path().join("nvim").is_dir());
    let config_text =
        fs::read_to_string(repo.path().join(".stitch").join("config.toml")).unwrap();
    assert!(!config_text.contains("nvim"));
}

#[test]
fn remove_missing_store_errors() {
    let repo = Repo::new();
    repo.cmd()
        .args(["remove", "nope"])
        .assert()
        .failure()
        .stderr(contains("not found in config"));
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_passes_on_healthy_repo() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_config(&format!(
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
    repo.write_config(&format!(
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
        .stdout(contains("[error]"))
        .stdout(contains("nvim"));
}

#[test]
fn doctor_warns_on_empty_store() {
    let repo = Repo::new();
    // Create an empty store dir, no target means no apply is needed.
    fs::create_dir_all(repo.path().join("nvim")).unwrap();
    repo.write_config(
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
    repo.write_config(&format!(
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
