//! End-to-end tests for the `stitch` CLI binary.
//!
//! These tests build and exercise the binary via `assert_cmd`. Each test gets
//! a fresh tempdir that acts as the repo root, and writes the two-file v0.3
//! layout (`stitch.toml` authored + `.stitch/state.toml` generated) directly
//! (bypassing `init`) to keep the test bodies focused.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

/// A scratch repo: a tempdir with `.stitch/` initialized and the two-file
/// config layout written (`stitch.toml` + `.stitch/state.toml`). Tests can
/// further mutate the filesystem (e.g. create store directories, source files)
/// as needed.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let stitch = dir.path().join(".stitch");
        fs::create_dir_all(&stitch).expect("mkdir .stitch");
        // Authored half: empty.
        fs::write(dir.path().join("stitch.toml"), "").expect("write stitch.toml");
        // Generated half: empty (the header is optional on read; keep it minimal).
        fs::write(stitch.join("state.toml"), "").expect("write state.toml");
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write the generated half (`.stitch/state.toml`) from a TOML string.
    /// Used by tests that only set inventory — the authored half stays empty.
    fn write_state(&self, toml: &str) {
        fs::write(self.dir.path().join(".stitch").join("state.toml"), toml)
            .expect("write state.toml");
    }

    /// Write the authored half (`stitch.toml`) from a TOML string.
    fn write_authored(&self, toml: &str) {
        fs::write(self.dir.path().join("stitch.toml"), toml).expect("write stitch.toml");
    }

    /// Write a complete store split across both files: `state` is the inventory
    /// half, `authored` is the behavior half. Both default to empty.
    fn write_split(&self, state: &str, authored: &str) {
        self.write_state(state);
        self.write_authored(authored);
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
        c.env_remove("STITCH_REPO"); // tests drive --repo explicitly when needed
        c
    }
}

/// If running as root, file mode bits don't constrain writes, so tests that
/// rely on making state.toml read-only can't trigger the failure path they're
/// meant to exercise. Returns true to indicate the caller should skip (loudly)
/// rather than pass spuriously.
fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
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

// ---------------------------------------------------------------------------
// not in a repo
// ---------------------------------------------------------------------------

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
        .stderr(contains("not inside a stitch repo"));
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
        .stderr(contains("not inside a stitch repo"));
}

// ---------------------------------------------------------------------------
// --repo flag / STITCH_REPO env — run from outside the repo
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// apply --force (.bak backups)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// add (adopt existing path)
// ---------------------------------------------------------------------------

#[test]
fn add_dry_run_adopt_existing_makes_no_changes() {
    let repo = Repo::new();
    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap(), "--dry-run"])
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
    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
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
    let src = repo.path().join("external").join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap()])
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
    let src = repo.path().join("external").join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    // Pass the path with a trailing slash.
    let src_str = format!("{}/", src.to_str().unwrap());
    repo.cmd()
        .args(["add", &src_str])
        .assert()
        .success()
        .stdout(contains("Added store"));

    assert!(repo.path().join("myconfig").is_dir());
    assert!(src.is_symlink());
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

// ---------------------------------------------------------------------------
// add (create empty store)
// ---------------------------------------------------------------------------

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
    let src = repo.path().join("external").join(".myrc");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "data").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap(), "--files", "x"])
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
    let src = repo.path().join("external").join("myconfig");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.conf"), "a").unwrap();

    repo.cmd()
        .args(["add", src.to_str().unwrap(), "--patterns", "*"])
        .assert()
        .failure()
        .stderr(contains("only apply when creating a new empty store"));

    assert!(src.exists());
    assert!(!repo.path().join("myconfig").exists());
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

#[test]
fn remove_drops_store_and_unlinks() {
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();

    // Add with a target so the link is created (add creates the store dir).
    repo.cmd().args(["add", &target_str]).assert().success();
    assert!(target.is_symlink());

    repo.cmd()
        .args(["remove", "nvim"])
        .assert()
        .success()
        .stdout(contains("Removed store 'nvim'"));

    // State entry gone, symlink gone, repo directory left untouched.
    assert!(!target.exists());
    assert!(repo.path().join("nvim").is_dir());
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(!state_text.contains("nvim"));
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

// ---------------------------------------------------------------------------
// doctor: orphaned-behavior detection (v0.3)
// ---------------------------------------------------------------------------

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

// --- Global ignores + whole-dir promotion (P1#8 D/E) ---

/// Whole-directory mode with ignored content present is promoted to file mode,
/// so repo metadata like `.git` is never symlinked wholesale into the target.
/// This is the core footgun global ignores exist to prevent.
#[test]
fn whole_dir_promoted_when_git_present() {
    let repo = Repo::new();
    let store_dir = repo.make_store("vim", &["vimrc"]);
    // Simulate a .git checked into the store (the footgun).
    fs::create_dir(store_dir.join(".git")).unwrap();
    fs::write(
        store_dir.join(".git").join("config"),
        "[core]
",
    )
    .unwrap();

    let target = repo.path().join("home").join(".vim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.vim]
target = "{target_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();

    // Promoted to file mode: vimrc is linked individually...
    assert!(target.join("vimrc").is_symlink());
    // ...and .git is NOT present at the target (the whole point).
    assert!(
        !target.join(".git").exists(),
        ".git must not be symlinked into the target"
    );
    // The target itself is now a real directory (file mode), not a symlink.
    assert!(target.is_dir());
    assert!(!target.is_symlink());
}

/// Global ignores also apply in file mode: a `files`/`patterns` store cannot
/// opt `.gitignore` or `.git` into a target even by naming them explicitly
/// is unnecessary — they're filtered by default. But an explicitly-listed
/// file that happens to match a global ignore is still linked (explicit wins).
/// Here we verify the common case: patterns don't pull in global-ignored names.
#[test]
fn file_mode_patterns_skip_global_ignored() {
    let repo = Repo::new();
    let store_dir = repo.make_store("shells", &[]);
    fs::write(store_dir.join(".bashrc"), "...").unwrap();
    fs::write(store_dir.join(".gitignore"), "*").unwrap();
    fs::write(store_dir.join(".DS_Store"), "x").unwrap();

    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{target_str}"
patterns = ["*"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.join(".bashrc").is_symlink());
    assert!(
        !target.join(".gitignore").exists(),
        ".gitignore must be globally ignored"
    );
    assert!(
        !target.join(".DS_Store").exists(),
        ".DS_Store must be globally ignored"
    );
}

#[test]
fn file_mode_patterns_match_recursively() {
    let repo = Repo::new();
    let store_dir = repo.make_store("configs", &[]);
    fs::create_dir_all(store_dir.join("sub")).unwrap();
    fs::write(store_dir.join("top.conf"), "top").unwrap();
    fs::write(store_dir.join("sub").join("nested.conf"), "nested").unwrap();
    fs::write(store_dir.join("sub").join("skip.txt"), "skip").unwrap();

    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.configs]
target = "{target_str}"
files = []
patterns = ["*.conf"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    // `*.conf` matches at all depths (leaf-name match).
    assert!(target.join("top.conf").is_symlink());
    assert!(target.join("sub").join("nested.conf").is_symlink());
    // Pattern does not match .txt files.
    assert!(!target.join("sub").join("skip.txt").exists());
    // Subdirectory structure is replicated.
    assert!(target.join("sub").is_dir());
}

/// A clean whole-dir store (no ignored content) stays in whole-dir mode —
/// promotion only triggers when there's something to exclude.
#[test]
fn whole_dir_stays_when_no_ignored_content() {
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

    // Unchanged: single symlink to the whole dir.
    assert!(target.is_symlink());
    assert!(target.join("init.lua").exists());
}

// --- Hooks (P1#8 C) ---

/// Helper: chmod +x a path (for global hook scripts).
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// Per-store pre-hook runs before the store is applied. Hooks are authored
/// behavior, so this test configures them in stitch.toml, not state.toml.
#[test]
fn per_store_pre_hook_runs() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    let marker = repo.path().join("pre-ran");
    repo.write_split(
        &format!(
            r#"
[stores.s]
target = "{target_str}"
"#,
        ),
        &format!(
            r#"
[stores.s]
hooks = {{ pre = "touch {}" }}
"#,
            marker.display()
        ),
    );

    repo.cmd().arg("apply").assert().success();

    assert!(marker.exists(), "pre-hook should have run");
    assert!(target.is_symlink(), "store should still be applied");
}

/// Per-store pre-hook failure aborts the store: no link created, non-zero exit.
#[test]
fn per_store_pre_hook_failure_aborts_store() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_split(
        &format!(
            r#"
[stores.s]
target = "{target_str}"
"#,
        ),
        r#"
[stores.s]
hooks = { pre = "exit 1" }
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("pre-hook"));

    assert!(
        !target.exists(),
        "store must not be linked when pre-hook fails"
    );
}

/// Per-store post-hook runs after the store is applied.
#[test]
fn per_store_post_hook_runs() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    let marker = repo.path().join("post-ran");
    repo.write_split(
        &format!(
            r#"
[stores.s]
target = "{target_str}"
"#,
        ),
        &format!(
            r#"
[stores.s]
hooks = {{ post = "touch {}" }}
"#,
            marker.display()
        ),
    );

    repo.cmd().arg("apply").assert().success();

    assert!(marker.exists(), "post-hook should have run");
    assert!(target.is_symlink());
}

/// Dry-run skips all hooks — no side effects.
#[test]
fn dry_run_skips_hooks() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    let marker = repo.path().join("ran");
    repo.write_split(
        &format!(
            r#"
[stores.s]
target = "{target_str}"
"#,
        ),
        &format!(
            r#"
[stores.s]
hooks = {{ pre = "touch {}", post = "touch {}" }}
"#,
            marker.display(),
            marker.display()
        ),
    );

    repo.cmd().arg("diff").assert().success();

    assert!(!marker.exists(), "hooks must not run under dry-run (diff)");
    assert!(!target.is_symlink(), "dry-run must not link");
}

/// Global pre-apply hook runs before any store is applied.
#[test]
fn global_pre_apply_hook_runs() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let marker = repo.path().join("global-pre-ran");
    fs::write(
        hooks_dir.join("pre-apply"),
        format!("#!/bin/sh\ntouch {}\n", marker.display()),
    )
    .unwrap();
    make_executable(&hooks_dir.join("pre-apply"));

    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s]
target = "{target_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();
    assert!(marker.exists(), "global pre-apply hook should have run");
}

/// Global pre-apply hook failure aborts the entire apply.
#[test]
fn global_pre_apply_failure_aborts() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(hooks_dir.join("pre-apply"), "#!/bin/sh\nexit 1\n").unwrap();
    make_executable(&hooks_dir.join("pre-apply"));

    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.s]
target = "{target_str}"
"#
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("pre-apply hook"));
    assert!(!target.exists(), "apply must abort when pre-apply fails");
}

/// Hooks receive STITCH_* env vars, including STITCH_STORE.
#[test]
fn hook_receives_env_vars() {
    let repo = Repo::new();
    repo.make_store("mystore", &["f"]);
    let target = repo.path().join("home").join("mystore");
    let target_str = target.to_string_lossy().into_owned();
    let outfile = repo.path().join("env.txt");
    repo.write_split(
        &format!(
            r#"
[stores.mystore]
target = "{target_str}"
"#,
        ),
        &format!(
            r#"
[stores.mystore]
hooks = {{ pre = "env | grep ^STITCH > {}" }}
"#,
            outfile.display()
        ),
    );

    repo.cmd().arg("apply").assert().success();

    let captured = fs::read_to_string(&outfile).unwrap();
    assert!(captured.contains("STITCH_STORE=mystore"), "got: {captured}");
    assert!(captured.contains("STITCH_ACTION=apply"), "got: {captured}");
    assert!(captured.contains("STITCH_TARGET="), "got: {captured}");
    assert!(captured.contains("STITCH_ROOT="), "got: {captured}");
}

// ===========================================================================
// v0.3 split: new regression tests (items 1, 2, 4, 5, 6)
// ===========================================================================

/// Comment-preservation regression (item 2): add/remove mutate only
/// state.toml; stitch.toml is byte-stable across mutations, so the user's
/// comments and formatting survive. This is the motivating bug of the split.
#[test]
fn stitch_toml_is_byte_stable_across_add_remove() {
    let repo = Repo::new();
    // Author stitch.toml with a comment the v0.2 reserializer would destroy.
    repo.write_authored(
        "# my dotfiles — do not let the tool rewrite this\n[vars]\neditor = \"nvim\"\n",
    );
    let before = fs::read_to_string(repo.path().join("stitch.toml")).unwrap();

    // add (with a target so a link is created), then remove.
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.cmd().args(["add", &target_str]).assert().success();
    repo.cmd().args(["remove", "nvim"]).assert().success();

    // stitch.toml is byte-identical: comment and formatting preserved.
    let after = fs::read_to_string(repo.path().join("stitch.toml")).unwrap();
    assert_eq!(
        before, after,
        "stitch.toml must be byte-stable across mutations"
    );
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

/// Authored-only target (item 1 merge): load-OK + skip. The target contributes
/// no link, a warning is emitted, and unrelated stores still apply.
#[test]
fn authored_only_target_loads_ok_and_skips() {
    let repo = Repo::new();
    repo.make_store("helix", &["settings.toml"]);
    let target = repo.path().join("home").join(".config").join("helix");
    let target_str = target.to_string_lossy().into_owned();
    // Generated half: store-level target so the store still applies.
    repo.write_state(&format!(
        r#"
[stores.helix]
target = "{target_str}"
"#,
    ));
    // Authored half: declares a per-target entry "laptop" with behavior, but
    // state.toml has no matching generated entry → orphaned-authored target.
    repo.write_authored(
        r#"
[stores.helix.targets.laptop]
when = { hostname = "laptop" }
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stderr(contains("orphaned"));
    // The store-level link is created despite the orphaned authored-only target.
    assert!(target.is_symlink());
}

/// state.toml ordering stability (item 6): adding stores in different orders
/// produces a byte-identical state.toml (BTreeMap keys + sorted files).
#[test]
fn state_toml_ordering_is_stable_across_operation_order() {
    // Use relative target paths so state.toml content is independent of the
    // tempdir path — only the store names and targets matter for ordering.
    // Snapshot after adding A then B.
    let repo_a = Repo::new();
    repo_a.cmd().args(["add", "home/zebra"]).assert().success();
    repo_a.cmd().args(["add", "home/alpha"]).assert().success();
    let snap_a = fs::read_to_string(repo_a.path().join(".stitch").join("state.toml")).unwrap();

    // Snapshot after adding B then A.
    let repo_b = Repo::new();
    repo_b.cmd().args(["add", "home/alpha"]).assert().success();
    repo_b.cmd().args(["add", "home/zebra"]).assert().success();
    let snap_b = fs::read_to_string(repo_b.path().join(".stitch").join("state.toml")).unwrap();

    assert_eq!(snap_a, snap_b, "state.toml must be order-stable");
    // And keys appear sorted.
    let za = snap_a.find("[stores.zebra]");
    let aa = snap_a.find("[stores.alpha]");
    assert!(aa < za, "alpha should sort before zebra");
}

/// files are emitted sorted in state.toml (item 6). Pre-seed state with
/// unsorted files, then trigger a re-save by adding another store — the save
/// re-serializes all stores with sorted files.
#[test]
fn state_toml_emits_files_sorted() {
    let repo = Repo::new();
    repo.make_store("s", &["c", "a", "b"]);
    repo.write_state("[stores.s]\ntarget = \"home/s\"\nfiles = [\"c\", \"a\", \"b\"]\n");

    // Adding a different store triggers a state.toml rewrite, which
    // re-serializes all stores with sorted files.
    repo.cmd().args(["add", "home/other"]).assert().success();

    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    // files emitted sorted as a, b, c — not insertion order (c, a, b). The toml
    // crate pretty-prints multi-element arrays across lines, so assert on the
    // relative positions of the entries rather than an exact inline string.
    let a = state_text.find("\"a\"").unwrap();
    let b = state_text.find("\"b\"").unwrap();
    let c = state_text.find("\"c\"").unwrap();
    assert!(
        a < b && b < c,
        "files must be sorted a<b<c, got: {state_text}"
    );
}

// ===========================================================================
// migrate (item 4)
// ===========================================================================

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

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

/// Set up a repo with one store (`nvim`) linked at a covered target, plus a
/// second repo-pointing symlink at an uncovered path. Returns (repo, covered,
/// orphan) so tests can assert on each. A dedicated tempdir stands in for the
/// home dir and is passed via `--scan-dir` (no $HOME override needed).
fn prune_fixture() -> (Repo, PathBuf, PathBuf, tempfile::TempDir) {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);

    let home = tempfile::tempdir().unwrap();
    let covered = home.path().join(".config").join("nvim");
    let orphan = home.path().join(".config").join("old-nvim");
    fs::create_dir_all(covered.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store_dir, &covered).unwrap();
    std::os::unix::fs::symlink(&store_dir, &orphan).unwrap();

    let covered_str = covered.to_string_lossy().into_owned();
    repo.write_state(&format!("[stores.nvim]\ntarget = \"{covered_str}\"\n"));

    (repo, covered, orphan, home)
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
        .assert()
        .failure()
        .stderr(contains("could not remove"))
        .stderr(contains("see warnings above"));

    // Restore before the tempdir drops so it can clean up cleanly.
    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
    assert!(orphan.is_symlink(), "orphan survived the failed removal");
}
