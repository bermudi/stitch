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
use serde_json::Value;

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
        // Trust foundation: doctor requires `.stitch/render/` in .gitignore.
        // Real `init` writes this; tests that bypass init need it too.
        fs::write(dir.path().join(".gitignore"), ".stitch/render/\n").expect("write .gitignore");
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

// ---------------------------------------------------------------------------
// apply --force (.bak backups)
// ---------------------------------------------------------------------------

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
fn plan_resolves_multi_target_inventory_per_target() {
    let repo = Repo::new();
    repo.make_store("shells", &["laptop", "server"]);
    let laptop = repo.path().join("home-laptop");
    let server = repo.path().join("home-server");
    repo.write_state(&format!(
        r#"
[stores.shells.targets.laptop]
target = "{laptop}"
files = ["laptop"]

[stores.shells.targets.server]
target = "{server}"
files = ["server"]
"#,
        laptop = laptop.to_string_lossy(),
        server = server.to_string_lossy(),
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(plan["errors"].as_array().unwrap().is_empty());
    let ops = plan["ops"].as_array().unwrap();
    assert!(ops.iter().any(|op| {
        op["op"] == "create_link"
            && op["source"] == repo.path().join("shells/laptop").to_string_lossy().as_ref()
    }));
    assert!(ops.iter().any(|op| {
        op["op"] == "create_link"
            && op["source"] == repo.path().join("shells/server").to_string_lossy().as_ref()
    }));

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, &output.stdout).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();
    assert!(laptop.join("laptop").is_symlink());
    assert!(server.join("server").is_symlink());
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
        .stderr(contains("unknown store"));
}

/// P0 regression: a pre-remove hook repoints a store's link to a truly foreign
/// target between `status_all` and the unlink. `remove` must refuse to clobber
/// it, leave the repointed symlink untouched, and preserve the store's state
/// entry because removal aborted before the generated state was saved.
#[test]
fn remove_refuses_foreign_repoint_after_status_collection() {
    let repo = Repo::new();
    repo.make_store("a", &["f"]);
    let target = repo.path().join("home").join("file");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.a]
target = "{target_str}"
"#
    ));

    // A truly foreign target, outside the repo root.
    let foreign = tempfile::tempdir().unwrap();
    let foreign_path = foreign.path().join("foreign.txt");
    fs::write(&foreign_path, "not ours").unwrap();
    let foreign_str = foreign_path.to_string_lossy().into_owned();

    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), repo.path().join("a"));

    // Global pre-remove hook repoints the link after status collection.
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(
        hooks_dir.join("pre-remove"),
        format!(
            "#!/bin/sh\nrm -f \"$STITCH_TARGET\" && ln -s \"{foreign_str}\" \"$STITCH_TARGET\"\n"
        ),
    )
    .unwrap();
    make_executable(&hooks_dir.join("pre-remove"));

    // Removal must fail with a foreign conflict, not clobber the repointed link.
    repo.cmd()
        .args(["remove", "a"])
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    // The repointed symlink is untouched and still points at the foreign path.
    assert!(target.is_symlink(), "repointed symlink must remain");
    assert_eq!(
        fs::read_link(&target).unwrap(),
        foreign_path,
        "repointed symlink must still point at the foreign target"
    );

    // The generated state entry must be preserved because removal aborted.
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state_text.contains("[stores.a]"),
        "state entry must be preserved"
    );
    assert!(
        state_text.contains(&target_str),
        "state target must be preserved"
    );
}

/// P0 regression: a pre-remove hook repoints a store's link to another store's
/// source entry. The exact-entry guard must still refuse because the link no
/// longer points at the store's own source, and removal must abort without
/// touching the repointed link or deleting the store's state.
#[test]
fn remove_refuses_sibling_store_repoint_after_status_collection() {
    let repo = Repo::new();
    repo.make_store("a", &["f"]);
    repo.make_store("b", &["g"]);
    let target_a = repo.path().join("home").join("a");
    let target_b = repo.path().join("home").join("b");
    let target_a_str = target_a.to_string_lossy().into_owned();
    let target_b_str = target_b.to_string_lossy().into_owned();
    let b_source = repo.path().join("b");
    let b_source_str = b_source.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.a]
target = "{target_a_str}"

[stores.b]
target = "{target_b_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();
    assert!(target_a.is_symlink());
    assert!(target_b.is_symlink());
    assert_eq!(fs::read_link(&target_a).unwrap(), repo.path().join("a"));
    assert_eq!(fs::read_link(&target_b).unwrap(), repo.path().join("b"));

    // Global pre-remove hook repoints a's link to b's repo source.
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(
        hooks_dir.join("pre-remove"),
        format!(
            "#!/bin/sh\nrm -f \"$STITCH_TARGET\" && ln -s \"{b_source_str}\" \"$STITCH_TARGET\"\n"
        ),
    )
    .unwrap();
    make_executable(&hooks_dir.join("pre-remove"));

    // The exact-source guard must reject the mismatch even though the new link
    // still resolves into the repo.
    repo.cmd()
        .args(["remove", "a"])
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    assert!(target_a.is_symlink(), "repointed symlink must remain");
    assert_eq!(
        fs::read_link(&target_a).unwrap(),
        b_source,
        "repointed symlink must still point at store b's source"
    );

    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state_text.contains("[stores.a]"),
        "state entry for a must be preserved"
    );
    assert!(
        state_text.contains(&target_a_str),
        "state target for a must be preserved"
    );
    assert!(
        state_text.contains("[stores.b]"),
        "state entry for b must be untouched"
    );
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

/// A whole-dir store with a nested ignored file (e.g. `lua/secret.bak`) must
/// promote to file mode and not symlink the ignored file into the target.
#[test]
fn whole_dir_promoted_when_nested_ignored_file() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    let lua = store_dir.join("lua");
    fs::create_dir_all(&lua).unwrap();
    fs::write(lua.join("plugin.lua"), "plugin").unwrap();
    fs::write(lua.join("secret.bak"), "secret").unwrap();

    repo.write_authored(
        r#"
[stores.nvim]
ignore = ["*.bak"]
"#,
    );

    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.is_dir());
    assert!(
        !target.is_symlink(),
        "promoted store must not be a whole-dir symlink"
    );
    assert!(target.join("init.lua").is_symlink());
    assert!(target.join("lua").join("plugin.lua").is_symlink());
    assert!(
        !target.join("lua").join("secret.bak").exists(),
        "nested ignored file must not be linked"
    );
}

/// A promoted whole-dir store must not write a nested link through a foreign
/// symlink ancestor. A foreign `<target>/lua` must be reported as a conflict,
/// not silently followed.
#[test]
fn apply_conflicts_on_foreign_symlink_ancestor() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    let lua = store_dir.join("lua");
    fs::create_dir_all(&lua).unwrap();
    fs::write(lua.join("plugin.lua"), "plugin").unwrap();
    fs::write(lua.join("secret.bak"), "secret").unwrap();

    repo.write_authored(
        r#"
[stores.nvim]
ignore = ["*.bak"]
"#,
    );

    let target = repo.path().join("home").join(".config").join("nvim");
    fs::create_dir_all(&target).unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let foreign_dir = foreign.path().join("lua");
    fs::create_dir_all(&foreign_dir).unwrap();
    std::os::unix::fs::symlink(&foreign_dir, target.join("lua")).unwrap();

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
        .stdout(contains("conflict"));

    // The top-level file links, but the nested file was blocked and the
    // foreign directory was not written through.
    assert!(target.join("init.lua").is_symlink());
    assert!(target.join("lua").is_symlink());
    assert!(!foreign_dir.join("plugin.lua").exists());
    assert!(!target.join("lua").join("plugin.lua").exists());
}

/// A whole-dir store with a nested ignored directory (e.g. `pack/plugins/foo/.git`)
/// must promote to file mode and not symlink the ignored directory or its children.
#[test]
fn whole_dir_promoted_when_nested_ignored_dir() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    let git = store_dir
        .join("pack")
        .join("plugins")
        .join("foo")
        .join(".git");
    fs::create_dir_all(&git).unwrap();
    fs::write(git.join("config"), "[core]\n").unwrap();

    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.is_dir());
    assert!(
        !target.is_symlink(),
        "promoted store must not be a whole-dir symlink"
    );
    assert!(target.join("init.lua").is_symlink());
    assert!(
        !target
            .join("pack")
            .join("plugins")
            .join("foo")
            .join(".git")
            .exists(),
        "nested ignored .git directory must not be linked"
    );
}

/// A clean whole-dir store with nested but non-ignored content stays whole-dir.
#[test]
fn whole_dir_stays_with_deep_clean_nesting() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    let lua = store_dir.join("lua");
    fs::create_dir_all(&lua).unwrap();
    fs::write(lua.join("plugin.lua"), "plugin").unwrap();
    let pack = store_dir.join("pack").join("plugins").join("foo");
    fs::create_dir_all(&pack).unwrap();
    fs::write(pack.join("init.lua"), "foo init").unwrap();

    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{target_str}"
"#
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.is_symlink(), "clean nested store stays whole-dir");
    assert!(target.join("init.lua").exists());
    assert!(target.join("lua").join("plugin.lua").exists());
    assert!(
        target
            .join("pack")
            .join("plugins")
            .join("foo")
            .join("init.lua")
            .exists()
    );
}

// --- P1: preserve symlink sources when whole-dir promotion to file mode ---

/// A whole-dir store with a nested ignored file and a non-ignored symlink
/// source must promote to file mode without dropping the symlink.
#[test]
fn whole_dir_promoted_preserves_symlink_source() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    std::os::unix::fs::symlink("init.lua", store_dir.join("init.vim")).unwrap();
    fs::write(store_dir.join("secret.bak"), "secret").unwrap();

    repo.write_authored(
        r#"
[stores.nvim]
ignore = ["*.bak"]
"#,
    );

    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.is_dir());
    assert!(
        !target.is_symlink(),
        "promoted store must not be a whole-dir symlink"
    );
    assert!(target.join("init.lua").is_symlink());
    assert!(target.join("init.vim").is_symlink());
    assert_eq!(
        std::fs::read_link(target.join("init.vim")).unwrap(),
        store_dir.join("init.vim"),
        "target link must point at the source symlink path"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("init.vim")).unwrap(),
        "contents of init.lua",
        "following the target symlink must resolve through the source symlink"
    );
    assert!(
        !target.join("secret.bak").exists(),
        "ignored file must not be linked"
    );
}

/// A dangling symlink source in a promoted whole-dir store must be carried to
/// the target as-is, not dropped or resolved.
#[test]
fn whole_dir_promoted_preserves_dangling_symlink_source() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    std::os::unix::fs::symlink("nonexistent", store_dir.join("dangling")).unwrap();
    fs::write(store_dir.join("secret.bak"), "secret").unwrap();

    repo.write_authored(
        r#"
[stores.nvim]
ignore = ["*.bak"]
"#,
    );

    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.is_dir());
    assert!(
        !target.is_symlink(),
        "promoted store must not be a whole-dir symlink"
    );
    assert!(target.join("init.lua").is_symlink());
    assert!(target.join("dangling").is_symlink());
    assert!(
        !target.join("dangling").exists(),
        "dangling target link must remain dangling"
    );
    assert_eq!(
        std::fs::read_link(target.join("dangling")).unwrap(),
        store_dir.join("dangling"),
        "target link must point at the dangling source symlink path"
    );
    assert!(
        !target.join("secret.bak").exists(),
        "ignored file must not be linked"
    );
}

/// A promoted store with both nested regular files and a nested symlink should
/// link all of them and skip only ignored content.
#[test]
fn whole_dir_promoted_links_nested_regular_files_and_symlinks() {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);
    let lua = store_dir.join("lua");
    fs::create_dir_all(&lua).unwrap();
    fs::write(lua.join("plugin.lua"), "plugin").unwrap();
    fs::write(lua.join("secret.bak"), "secret").unwrap();
    std::os::unix::fs::symlink("plugin.lua", lua.join("plugin.vim")).unwrap();

    repo.write_authored(
        r#"
[stores.nvim]
ignore = ["*.bak"]
"#,
    );

    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();

    assert!(target.is_dir());
    assert!(
        !target.is_symlink(),
        "promoted store must not be a whole-dir symlink"
    );
    assert!(target.join("init.lua").is_symlink());
    assert!(target.join("lua").join("plugin.lua").is_symlink());
    assert!(target.join("lua").join("plugin.vim").is_symlink());
    assert_eq!(
        std::fs::read_link(target.join("lua").join("plugin.vim")).unwrap(),
        lua.join("plugin.vim"),
        "nested symlink target must point at the source symlink path"
    );
    assert!(
        !target.join("lua").join("secret.bak").exists(),
        "ignored file must not be linked"
    );
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
        .stdout(contains("hook failed").and(contains("pre")));

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
        .stderr(contains("pre-apply"));
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
    // Snapshot after adding A then B. `add` persists absolute targets; replace
    // each scratch root before comparing so only ordering remains significant.
    let repo_a = Repo::new();
    repo_a.cmd().args(["add", "home/zebra"]).assert().success();
    repo_a.cmd().args(["add", "home/alpha"]).assert().success();
    let snap_a = fs::read_to_string(repo_a.path().join(".stitch").join("state.toml"))
        .unwrap()
        .replace(repo_a.path().to_string_lossy().as_ref(), "<repo>");

    // Snapshot after adding B then A.
    let repo_b = Repo::new();
    repo_b.cmd().args(["add", "home/alpha"]).assert().success();
    repo_b.cmd().args(["add", "home/zebra"]).assert().success();
    let snap_b = fs::read_to_string(repo_b.path().join(".stitch").join("state.toml"))
        .unwrap()
        .replace(repo_b.path().to_string_lossy().as_ref(), "<repo>");

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
    repo.write_state(&format!(
        "[stores.s]\ntarget = \"{}\"\nfiles = [\"c\", \"a\", \"b\"]\n",
        repo.path().join("home/s").display()
    ));

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
// Red-line regression nets: read/plan commands leave stitch.toml untouched;
// remove dispatches its hooks; post-hook failure warns rather than aborts.
//
// Each of these locks an invariant that a refactor can silently break with no
// other test firing. They are not "assert the error path exists" ceremony —
// they pin behavior the SPEC/AGENTS red lines call out by name.
// ===========================================================================

/// Red line: "Authored config is read-only to the tool." Only `init`/`migrate`
/// (which create it), `add`/`remove` (covered by
/// `stitch_toml_is_byte_stable_across_add_remove`), and `import` (covered by
/// `import_leaves_stitch_toml_byte_stable`) are allowed to touch config files
/// — and all three write only `state.toml`, never `stitch.toml`. The read/plan
/// commands (`apply`, `diff`, `plan`, `status`, `list`, `doctor`, `prune`,
/// `render`) must never write it. (`edit` intentionally opens `stitch.toml` in
/// `$EDITOR` — that's the user, not the tool, so it's excluded from this net.)
/// A stray `loaded.config.save()` or `config::atomic_write(&authored_path, …)`
/// threaded into any of them would silently destroy user comments — the
/// motivating bug of the v0.3 split — and today nothing catches it. This test
/// pins all eight commands in one place.
#[test]
fn read_and_plan_commands_leave_stitch_toml_byte_stable() {
    let repo = Repo::new();
    // Authored half with a comment + a `when` clause + a `vars` table — the
    // exact shape a v0.2 reserializer would mangle. Include a configured store
    // so apply/diff/plan have something to operate on.
    let store = repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    let target_str = target.to_string_lossy().into_owned();
    let authored = "# do not let the tool rewrite this — comment must survive\n\
         [vars]\neditor = \"nvim\"\n\
         \n[stores.nvim]\nwhen = { os = \"linux\" }\n";
    repo.write_split(
        &format!("\n[stores.nvim]\ntarget = \"{target_str}\"\n"),
        authored,
    );

    let before = fs::read(repo.path().join("stitch.toml")).unwrap();

    for cmd in [
        ["apply"],
        ["diff"],
        ["plan"],
        ["status"],
        ["list"],
        ["doctor"],
        ["prune"],
    ] {
        repo.cmd().args(cmd).assert().success();
    }
    // render needs a templated entry; add one without re-running apply.
    fs::write(store.join("init.lua.tmpl"), "{{ vars.editor }}\n").unwrap();
    repo.cmd()
        .args(["render", "nvim/init.lua.tmpl"])
        .assert()
        .success();

    let after = fs::read(repo.path().join("stitch.toml")).unwrap();
    assert_eq!(
        before, after,
        "stitch.toml must be byte-stable across read/plan commands \
         (apply/diff/plan/status/list/doctor/prune/render)"
    );
}

/// Red line: `cmd_remove` dispatches `pre-remove` and `post-remove` global
/// hooks. The dispatch exists in main.rs but no test exercises it — a refactor
/// that drops either call (or renames the hook file lookup) regresses silently.
/// This test pins both hooks firing on a real remove, with `STITCH_ACTION=remove`
/// visible to the hook so the action dispatch is also locked.
#[test]
fn remove_dispatches_pre_and_post_remove_hooks() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!("\n[stores.s]\ntarget = \"{target_str}\"\n"));

    // Apply first so there's a real link to remove.
    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let pre_marker = repo.path().join("pre-remove-ran");
    let post_marker = repo.path().join("post-remove-ran");
    // The hooks record the STITCH_ACTION they saw, so the action dispatch is
    // pinned alongside the hook dispatch itself. The post-remove hook also
    // asserts the link is already gone — pinning pre-before-unlink,
    // post-after-unlink ordering.
    fs::write(
        hooks_dir.join("pre-remove"),
        format!(
            "#!/bin/sh\necho \"pre:$STITCH_ACTION:$STITCH_STORE\" > {}\n\
             test -e \"$STITCH_TARGET\" && echo \"link-still-present\" >> {} || \
             echo \"link-already-gone\" >> {}\n",
            pre_marker.display(),
            pre_marker.display(),
            pre_marker.display()
        ),
    )
    .unwrap();
    fs::write(
        hooks_dir.join("post-remove"),
        format!(
            "#!/bin/sh\necho \"post:$STITCH_ACTION:$STITCH_STORE\" > {}\n\
             test -e \"$STITCH_TARGET\" && echo \"link-still-present\" >> {} || \
             echo \"link-already-gone\" >> {}\n",
            post_marker.display(),
            post_marker.display(),
            post_marker.display()
        ),
    )
    .unwrap();
    make_executable(&hooks_dir.join("pre-remove"));
    make_executable(&hooks_dir.join("post-remove"));

    repo.cmd().args(["remove", "s"]).assert().success();

    assert!(pre_marker.exists(), "pre-remove hook must run on remove");
    assert!(post_marker.exists(), "post-remove hook must run on remove");
    let pre_text = fs::read_to_string(&pre_marker).unwrap();
    let post_text = fs::read_to_string(&post_marker).unwrap();
    assert_eq!(
        pre_text.lines().next().unwrap(),
        "pre:remove:s",
        "pre-remove must see STITCH_ACTION=remove and the store name"
    );
    assert!(
        pre_text.contains("link-still-present"),
        "pre-remove must fire before the link is unlinked, got: {pre_text}"
    );
    assert_eq!(
        post_text.lines().next().unwrap(),
        "post:remove:s",
        "post-remove must see STITCH_ACTION=remove and the store name"
    );
    assert!(
        post_text.contains("link-already-gone"),
        "post-remove must fire after the link is unlinked, got: {post_text}"
    );
    assert!(!target.exists(), "remove must still unlink the target");
}

/// Red line: per SPEC §Hooks, "post failure warns" — it must NOT abort the
/// operation. The code uses `eprintln!("warning: …")` + continue (not `?`); a
/// one-character refactor to `?` would turn a post-hook hiccup into a failed
/// apply that leaves the user unsure whether their links were created. This
/// test pins the warn-don't-abort contract for both per-store and global
/// post-hooks: the post hook fails, the apply still exits 0, the link is in
/// place, and stderr carries the warning.
#[test]
fn post_hook_failure_warns_without_aborting_apply() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();

    // Per-store post hook fails.
    repo.write_split(
        &format!("\n[stores.s]\ntarget = \"{target_str}\"\n"),
        "\n[stores.s]\nhooks = { post = \"exit 1\" }\n",
    );

    let output = repo.cmd().arg("apply").assert().success();
    assert!(
        target.is_symlink(),
        "link must be created even when the post hook fails"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("warning:"),
        "post-hook failure must surface as a warning: line on stderr, got: {stderr}"
    );

    // Global post-apply hook fails too — same contract: warn, don't abort.
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(hooks_dir.join("post-apply"), "#!/bin/sh\nexit 1\n").unwrap();
    make_executable(&hooks_dir.join("post-apply"));

    // Reset the link so apply has work to do (otherwise it's a no-op and the
    // post-apply hook path still runs, but we want to assert success end-to-end
    // with both per-store and global post hooks failing in the same run).
    fs::remove_file(&target).unwrap();
    let output = repo.cmd().arg("apply").assert().success();
    assert!(
        target.is_symlink(),
        "link must be created even with per-store + global post hooks both failing"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("post-apply"),
        "global post-apply failure must be named in the warning, got: {stderr}"
    );
}

/// Red line: `cmd_import` writes `state.toml` but must never rewrite
/// `stitch.toml` — its own doc comment says so ("Never rewrites
/// `stitch.toml`"). Import is the fourth mutation command (after init/migrate
/// and add/remove), and it falls through the existing byte-stability nets: the
/// add/remove test doesn't cover it, and the read/plan test doesn't cover
/// mutation commands. A stray `atomic_write(&authored_path, …)` in import
/// would regress silently — precisely the failure mode this diff exists to
/// catch. This test pins the red line for import specifically.
#[test]
fn import_leaves_stitch_toml_byte_stable() {
    let repo = Repo::new();
    // Authored half with a comment — the shape a reserializer would destroy.
    repo.write_authored("# my dotfiles — do not rewrite\n[vars]\neditor = \"nvim\"\n");
    let store = repo.make_store("nvim", &["init.lua"]);
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join(".config").join("nvim");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &target).unwrap();

    let before = fs::read(repo.path().join("stitch.toml")).unwrap();

    repo.cmd()
        .arg("import")
        .arg("--scan-dir")
        .arg(home.path().join(".config"))
        .assert()
        .success()
        .stdout(contains("Imported 1"));

    let after = fs::read(repo.path().join("stitch.toml")).unwrap();
    assert_eq!(
        before, after,
        "stitch.toml must be byte-stable across import — \
         import writes state.toml only, never the authored half"
    );

    // Sanity: import did write state.toml (so the test isn't vacuously passing
    // because import was a no-op).
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.nvim]"),
        "import must record the store in state.toml: {state}"
    );
}

/// Red line: `pre-remove` hook failure aborts the removal via `?` → exit 10
/// (SPEC §Hooks: "pre failure aborts the store"). `cmd_remove` has its own
/// inline hook dispatch (not a shared runner), so the same one-character `?`
/// refactor risk that test 3 pins for apply exists independently here. This
/// test pins: non-zero exit, the store entry survives in `state.toml`, and
/// the link is still present — i.e. the remove was genuinely aborted, not
/// completed-and-then-errored.
#[test]
fn pre_remove_hook_failure_aborts_remove() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!("\n[stores.s]\ntarget = \"{target_str}\"\n"));

    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(hooks_dir.join("pre-remove"), "#!/bin/sh\nexit 1\n").unwrap();
    make_executable(&hooks_dir.join("pre-remove"));

    repo.cmd()
        .args(["remove", "s"])
        .assert()
        .failure()
        .stderr(contains("pre-remove"));

    // The remove was aborted: link survives, store survives in state.toml.
    assert!(
        target.is_symlink(),
        "link must survive when pre-remove aborts"
    );
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.s]"),
        "store entry must survive in state.toml when pre-remove aborts: {state}"
    );
}

/// Red line: `post-remove` hook failure warns, not aborts — the mirror of test
/// 3 for the remove side. `cmd_remove`'s post-remove dispatch uses
/// `eprintln!("warning: …")` + continue (main.rs:1377), the same pattern as
/// post-apply. A `?` refactor would turn a post-remove hiccup into a failed
/// remove that leaves the user unsure whether the link was unlinked. This
/// test pins: exit 0, link gone, warning on stderr.
#[test]
fn post_remove_hook_failure_warns_without_aborting() {
    let repo = Repo::new();
    repo.make_store("s", &["f"]);
    let target = repo.path().join("home").join("s");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!("\n[stores.s]\ntarget = \"{target_str}\"\n"));

    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(hooks_dir.join("post-remove"), "#!/bin/sh\nexit 1\n").unwrap();
    make_executable(&hooks_dir.join("post-remove"));

    let output = repo.cmd().args(["remove", "s"]).assert().success();
    assert!(
        !target.exists(),
        "link must be unlinked even when post-remove fails"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("warning:"),
        "post-remove failure must surface as a warning: line on stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("post-remove"),
        "post-remove failure must name the hook in the warning, got: {stderr}"
    );
    // Store entry is gone — the remove completed.
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state.contains("[stores.s]"),
        "store entry must be removed from state.toml even when post-remove fails: {state}"
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

// ---------------------------------------------------------------------------
// templates (v0.6)
// ---------------------------------------------------------------------------

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
fn template_apply_requires_render_gitignore_before_staging() {
    let repo = Repo::new();
    fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig.tmpl"), "name = {{ hostname }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .args(["apply", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains(".stitch/render/"));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains(".stitch/render/"));
    assert!(
        !repo
            .path()
            .join(".stitch")
            .join("render")
            .join("git")
            .exists(),
        "preflight must fail before staging output"
    );
}

#[test]
fn template_apply_renders_and_links_into_staging() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(
        store.join("gitconfig.tmpl"),
        "user.name = {{ hostname }}\neditor = {{ vars.editor }}\n",
    )
    .unwrap();
    repo.write_authored(
        r#"
[vars]
editor = "nvim"
"#,
    );
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy().into_owned();
    // File-mode: list the source name (with .tmpl).
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
files = ["gitconfig.tmpl"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    // Staging exists, mode 0600, rendered content.
    let staged = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("gitconfig");
    assert!(staged.is_file(), "staged render must exist");
    let mode = fs::metadata(&staged).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "staged render must be 0600");
    let content = fs::read_to_string(&staged).unwrap();
    assert!(
        content.contains("editor = nvim"),
        "vars interpolated: {content}"
    );
    assert!(
        content.contains("user.name = "),
        "hostname interpolated: {content}"
    );

    // Target is a symlink into the repo (staging), not the .tmpl source.
    let link = target.join("gitconfig");
    assert!(link.is_symlink(), "target must be a symlink");
    let resolved = fs::read_link(&link).unwrap();
    assert!(
        resolved.ends_with(".stitch/render/git/gitconfig")
            || resolved == staged
            || resolved.canonicalize().unwrap() == staged.canonicalize().unwrap(),
        "link must point at staging, got {}",
        resolved.display()
    );
    // points_into_repo invariant: read through the link.
    assert_eq!(fs::read_to_string(&link).unwrap(), content);
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
fn failed_template_promotion_keeps_existing_whole_directory_link() {
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

    fs::write(store.join("broken.tmpl"), "{{").unwrap();
    repo.cmd().arg("apply").assert().failure();

    assert!(
        target.is_symlink(),
        "a failed render must not remove the working whole-dir link"
    );
    assert_eq!(
        fs::read_to_string(target.join("config")).unwrap(),
        "contents of config"
    );
    assert!(
        store.join("broken").symlink_metadata().is_err(),
        "promotion must not write a child link through the root symlink"
    );
}

#[test]
fn missing_source_promotion_keeps_existing_whole_directory_link() {
    let repo = Repo::new();
    repo.make_store("git", &["config"]);
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));
    repo.cmd().arg("apply").assert().success();

    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["missing"]
"#,
        target.to_string_lossy(),
    ));
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("source does not exist"));

    assert!(
        target.is_symlink(),
        "a missing file-mode source must not remove the working whole-dir link"
    );
    assert_eq!(
        fs::read_to_string(target.join("config")).unwrap(),
        "contents of config"
    );
}

#[test]
fn template_whole_dir_promotes_and_strips_suffix() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(store.join("hooks")).unwrap();
    fs::write(store.join("config"), "plain\n").unwrap();
    fs::write(
        store.join("hooks").join("pre-commit.tmpl"),
        "#!/bin/sh\necho {{ os }}\n",
    )
    .unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();

    // Plain file linked directly.
    assert!(target.join("config").is_symlink());
    assert_eq!(
        fs::read_to_string(target.join("config")).unwrap(),
        "plain\n"
    );

    // Nested template rendered; link name has no .tmpl.
    let hook = target.join("hooks").join("pre-commit");
    assert!(hook.is_symlink(), "nested template must be linked");
    assert!(!target.join("hooks").join("pre-commit.tmpl").exists());
    let body = fs::read_to_string(&hook).unwrap();
    assert!(
        body.contains("linux") || body.contains("macos"),
        "os interpolated: {body}"
    );
}

#[test]
fn template_collision_rejected() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig"), "plain\n").unwrap();
    fs::write(store.join("gitconfig.tmpl"), "t={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig", "gitconfig.tmpl"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("name collision"));
}

#[test]
fn template_resolution_error_preserves_existing_staging_and_link() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("good.tmpl"), "good = {{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["good.tmpl"]
"#,
        target.to_string_lossy(),
    ));
    repo.cmd().arg("apply").assert().success();

    let staged = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("good");
    let link = target.join("good");
    assert!(staged.is_file());
    assert!(fs::read_to_string(&link).is_ok());

    // Adding the plain source makes resolution fail, but must not reap the
    // staging file that the existing target link still needs.
    fs::write(store.join("good"), "plain\n").unwrap();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["good", "good.tmpl"]
"#,
        target.to_string_lossy(),
    ));
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("name collision"));

    assert!(staged.is_file(), "resolution error must preserve staging");
    assert!(
        fs::read_to_string(&link).is_ok(),
        "resolution error must not leave the live link dangling"
    );
}

#[test]
fn when_skipped_target_preserves_shared_target_link_and_staging() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("a.tmpl"), "a = {{ os }}\n").unwrap();
    fs::write(store.join("b.tmpl"), "b = {{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy();
    repo.write_state(&format!(
        r#"
[stores.git.targets.active]
target = "{target_str}"
files = ["a.tmpl"]

[stores.git.targets.skipped]
target = "{target_str}"
files = ["b.tmpl"]
"#,
    ));
    repo.cmd().arg("apply").assert().success();

    let link = target.join("b");
    let staged = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("b");
    assert!(link.is_symlink());
    assert!(staged.is_file());

    repo.write_authored(
        r#"
[stores.git.targets.skipped]
when = { os = "definitely-not-linux-or-macos" }
"#,
    );
    repo.cmd().arg("apply").assert().success();

    assert!(link.is_symlink(), "skipped target link must not be reaped");
    assert!(
        fs::read_to_string(&link).is_ok(),
        "skipped target link must remain readable"
    );
    assert!(
        staged.is_file(),
        "skipped target staging must not be reaped"
    );
}

#[test]
fn template_missing_env_fails_entry_not_link() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(
        store.join("gitconfig.tmpl"),
        r#"x = {{ env("STITCH_TEST_UNSET_ENV_ABC_123") }}
"#,
    )
    .unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        target.to_string_lossy(),
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("STITCH_TEST_UNSET_ENV_ABC_123"));

    // No broken link, no half-written staging for a failed render.
    assert!(!target.join("gitconfig").exists());
}

#[test]
fn template_diff_shows_content_drift() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig.tmpl"), "v=1\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
files = ["gitconfig.tmpl"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    // Edit the template without applying.
    fs::write(store.join("gitconfig.tmpl"), "v=2\n").unwrap();

    repo.cmd()
        .arg("diff")
        .assert()
        .success()
        .stdout(contains("content:"));

    // After apply, diff is clean.
    repo.cmd().arg("apply").assert().success();
    let out = repo.cmd().arg("diff").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(
        !stdout.contains("content:"),
        "diff should be empty after apply: {stdout}"
    );
}

#[test]
fn apply_removes_target_link_for_deleted_template() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("keep.tmpl"), "keep={{ os }}\n").unwrap();
    fs::write(store.join("drop.tmpl"), "drop={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();
    let stale_link = target.join("drop");
    let stale_render = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("drop");
    assert!(stale_link.is_symlink());
    assert!(stale_render.exists());

    fs::remove_file(store.join("drop.tmpl")).unwrap();
    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("remove:"));

    assert!(
        stale_link.symlink_metadata().is_err(),
        "deleted templates must not leave a dangling target symlink"
    );
    assert!(
        !stale_render.exists(),
        "deleted templates must not leave a frozen staged render"
    );
    assert!(
        target.join("keep").is_symlink(),
        "remaining entries survive"
    );
}

#[test]
fn apply_removes_target_link_for_deleted_plain_file_in_promoted_store() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    // The template promotes this otherwise whole-dir store to file mode.
    fs::write(store.join("keep.tmpl"), "keep={{ os }}\n").unwrap();
    fs::write(store.join("drop"), "plain\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();
    let stale_link = target.join("drop");
    assert!(stale_link.is_symlink());

    fs::remove_file(store.join("drop")).unwrap();
    repo.cmd().arg("apply").assert().success();

    assert!(
        stale_link.symlink_metadata().is_err(),
        "file-mode cleanup must include plain entries, not only templates"
    );
    assert!(target.join("keep").is_symlink());
}

#[test]
fn removing_last_template_restores_whole_directory_link() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("config"), "plain\n").unwrap();
    fs::write(store.join("config.local.tmpl"), "local={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();
    assert!(target.is_dir());
    assert!(!target.is_symlink(), "template promotion uses file mode");

    fs::remove_file(store.join("config.local.tmpl")).unwrap();
    repo.cmd()
        .arg("diff")
        .assert()
        .success()
        .stdout(contains("remove:"))
        .stdout(contains("replace:"));
    assert!(
        target.is_dir() && !target.is_symlink(),
        "diff must not replace the target directory"
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("replace:"));
    assert!(
        target.is_symlink(),
        "after the last template disappears, restore whole-directory mode"
    );
    assert_eq!(
        target.canonicalize().unwrap(),
        store.canonicalize().unwrap(),
        "the whole-directory target must point back to the store"
    );
    assert!(
        !repo
            .path()
            .join(".stitch")
            .join("render")
            .join("git")
            .exists(),
        "the final stale render must be removed"
    );
}

#[test]
fn apply_plan_restores_whole_directory_after_last_template_is_removed() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("config"), "plain\n").unwrap();
    fs::write(store.join("config.local.tmpl"), "local={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        "[stores.git]\ntarget = \"{}\"\n",
        target.display()
    ));

    repo.cmd().arg("apply").assert().success();
    assert!(
        target.is_dir() && !target.is_symlink(),
        "the initial template must promote this store to file mode"
    );
    fs::remove_file(store.join("config.local.tmpl")).unwrap();

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        plan["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "remove_link"),
        "the plan must remove stale file-mode links"
    );
    assert!(
        plan["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "replace_link"),
        "the plan must replace the now-empty target directory"
    );
    let plan_path = repo.path().join("restore-whole-dir.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(fs::read_link(&target).unwrap(), store);
}

#[test]
fn deleted_source_cleanup_preserves_foreign_replacement() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("keep.tmpl"), "keep={{ os }}\n").unwrap();
    fs::write(store.join("drop.tmpl"), "drop={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();
    fs::remove_file(store.join("drop.tmpl")).unwrap();

    let replaced = target.join("drop");
    fs::remove_file(&replaced).unwrap();
    let foreign = repo.path().join("foreign-file");
    fs::write(&foreign, "hands off\n").unwrap();
    std::os::unix::fs::symlink(&foreign, &replaced).unwrap();

    repo.cmd().arg("apply").assert().success();

    assert_eq!(fs::read_link(&replaced).unwrap(), foreign);
    assert!(
        !repo
            .path()
            .join(".stitch")
            .join("render")
            .join("git")
            .join("drop")
            .exists(),
        "staging remains tool-owned even when a target is replaced externally"
    );
}

#[test]
fn diff_previews_deleted_source_cleanup_without_mutating() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("keep.tmpl"), "keep={{ os }}\n").unwrap();
    fs::write(store.join("drop.tmpl"), "drop={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();
    let stale_link = target.join("drop");
    let stale_render = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("drop");
    fs::remove_file(store.join("drop.tmpl")).unwrap();

    repo.cmd()
        .arg("diff")
        .assert()
        .success()
        .stdout(contains("remove:"));

    assert!(stale_link.is_symlink(), "diff must not unlink targets");
    assert!(stale_render.exists(), "diff must not delete staged renders");
}

#[test]
fn template_remove_cleans_staging() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("gitconfig.tmpl"), "x={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        target.to_string_lossy(),
    ));
    repo.cmd().arg("apply").assert().success();

    let staged_dir = repo.path().join(".stitch").join("render").join("git");
    assert!(staged_dir.exists());

    repo.cmd().args(["remove", "git"]).assert().success();
    assert!(!staged_dir.exists(), "remove must wipe store staging");
    assert!(!target.join("gitconfig").exists());
}

#[test]
fn template_edit_opens_source_not_staging() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    let tmpl = store.join("gitconfig.tmpl");
    fs::write(&tmpl, "x={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
files = ["gitconfig.tmpl"]
"#
    ));
    // Works pre-apply: config-based resolution.
    let marker = repo.path().join("edited");
    // Fake editor: write the path it was given into a marker file.
    let editor = repo.path().join("fake-editor.sh");
    fs::write(
        &editor,
        format!("#!/bin/sh\necho \"$1\" > {}\n", marker.to_string_lossy()),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();

    let entry_path = target.join("gitconfig");
    let entry_str = entry_path.to_string_lossy().into_owned();
    repo.cmd()
        .env("EDITOR", &editor)
        .args(["edit", &entry_str])
        .assert()
        .success();

    let opened = fs::read_to_string(&marker).unwrap();
    let opened = opened.trim();
    assert!(
        opened.ends_with("gitconfig.tmpl"),
        "edit must open the .tmpl source, got {opened}"
    );
    assert!(
        !opened.contains(".stitch/render"),
        "edit must never open staging: {opened}"
    );
}

#[test]
fn edit_linked_target_opens_source() {
    // The standard post-apply state: the target is a symlink into the repo.
    // `stitch edit <target_path>` must resolve back to the source, not the
    // staged render or the symlink's own resolved path.
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    let tmpl = store.join("gitconfig.tmpl");
    fs::write(&tmpl, "x={{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
files = ["gitconfig.tmpl"]
"#
    ));

    repo.cmd().arg("apply").assert().success();

    let marker = repo.path().join("edited");
    let editor = repo.path().join("fake-editor.sh");
    fs::write(
        &editor,
        format!("#!/bin/sh\necho \"$1\" > {}\n", marker.to_string_lossy()),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();

    let entry_path = target.join("gitconfig");
    let entry_str = entry_path.to_string_lossy().into_owned();
    repo.cmd()
        .env("EDITOR", &editor)
        .args(["edit", &entry_str])
        .assert()
        .success();

    let opened = fs::read_to_string(&marker).unwrap();
    let opened = opened.trim();
    assert!(
        opened.ends_with("gitconfig.tmpl"),
        "edit must open the .tmpl source, got {opened}"
    );
    assert!(
        !opened.contains(".stitch/render"),
        "edit must never open staging: {opened}"
    );
}

#[test]
fn edit_rejects_foreign_symlink() {
    // A foreign symlink at the target must not be silently resolved to a repo
    // source when the user runs `stitch edit` on it.
    let repo = Repo::new();
    repo.make_store("git", &["gitconfig"]);
    let target = repo.path().join("home");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{target_str}"
files = ["gitconfig"]
"#
    ));

    fs::create_dir_all(&target).unwrap();

    let foreign_tmp = tempfile::tempdir().unwrap();
    let foreign = foreign_tmp.path().join("foreign");
    fs::write(&foreign, "not ours\n").unwrap();
    let link = target.join("gitconfig");
    std::os::unix::fs::symlink(&foreign, &link).unwrap();

    repo.cmd()
        .args(["edit", &link.to_string_lossy()])
        .assert()
        .failure()
        .stderr(contains("foreign"));
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

// ---------------------------------------------------------------------------
// Exit code + resolution hint tests (v0.7 Milestone 1)
// ---------------------------------------------------------------------------

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
    repo.write_authored(&format!(
        r#"
[stores.s]
target = "{target_str}"
files = ["f"]

[stores.s.hooks]
pre = "exit 1"
"#
    ));
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

// ---------------------------------------------------------------------------
// v0.7 JSON / agent interface (M2)
// ---------------------------------------------------------------------------

fn json_output(output: &std::process::Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).expect("utf8 stdout");
    serde_json::from_str(stdout).expect("valid JSON envelope")
}

fn assert_plan_summary_fields(summary: &Value) {
    for key in [
        "created",
        "replaced",
        "backed_up",
        "removed",
        "content_changed",
        "already_linked",
        "conflicts",
        "errors",
        "skipped",
    ] {
        assert!(
            summary.get(key).and_then(Value::as_u64).is_some(),
            "summary[{key}] must be a non-negative integer"
        );
    }
}

fn assert_envelope_shape(value: &Value, command: &str, ok: bool) {
    assert_eq!(value.get("schema").and_then(Value::as_u64), Some(1));
    assert_eq!(value.get("command").and_then(Value::as_str), Some(command));
    assert_eq!(value.get("ok").and_then(Value::as_bool), Some(ok));
    assert!(value.get("warnings").is_some());
    // Lock the schema-stable omission pattern: both `data` and `error` are
    // always present; the absent one serializes as `null`.
    assert!(
        value.get("data").is_some(),
        "envelope must carry a data field"
    );
    assert!(
        value.get("error").is_some(),
        "envelope must carry an error field"
    );
    if ok {
        assert!(
            value["error"].is_null(),
            "error must be null on a successful envelope"
        );
    } else {
        assert!(
            value["error"].is_object(),
            "error must be an object on a failed envelope"
        );
    }
}

/// Lock the §1 error-object contract: `{class, code, message, hint, details}`.
/// Every `--json` failure envelope must carry a non-empty class, the matching
/// numeric code, a non-empty message, and the `hint` and `details` keys
/// (serialized as `null` when not populated).
fn assert_error_shape(value: &Value, class: &str, code: i64) {
    let error = value
        .get("error")
        .expect("envelope must carry an error object on failure");
    assert_eq!(error["class"].as_str(), Some(class), "error.class mismatch");
    assert_eq!(error["code"].as_i64(), Some(code), "error.code mismatch");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .expect("error.message present");
    assert!(!message.is_empty(), "error.message must be non-empty");
    assert!(
        error.get("hint").is_some() && (error["hint"].is_string() || error["hint"].is_null()),
        "error.hint must be present as a string or null"
    );
    assert!(
        error.get("details").is_some()
            && (error["details"].is_string() || error["details"].is_null()),
        "error.details must be present as a string or null"
    );
}

#[test]
fn json_status_reports_linked_and_missing() {
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

    let output = repo.cmd().args(["--json", "status"]).output().unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "status", true);
    let data = value.get("data").unwrap().as_array().unwrap();
    assert_eq!(data.len(), 2);

    let states: std::collections::BTreeMap<&str, &str> = data
        .iter()
        .map(|row| {
            (
                row["store"].as_str().unwrap(),
                row["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(states["nvim"], "linked");
    assert_eq!(states["shells"], "missing");

    // Whole-dir and file-mode rows have the expected shapes.
    let nvim = data.iter().find(|r| r["store"] == "nvim").unwrap();
    assert_eq!(nvim["templated"], false);
    assert_eq!(nvim.get("staged_path"), None);

    let shells = data.iter().find(|r| r["store"] == "shells").unwrap();
    assert_eq!(
        shells["source"].as_str().unwrap(),
        repo.path().join("shells/.bashrc").to_string_lossy()
    );
    assert_eq!(
        shells["target"].as_str().unwrap(),
        shells_target.join(".bashrc").to_string_lossy()
    );
    assert_eq!(shells["state"], "missing");
}

#[test]
fn json_status_filter_by_name() {
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

    let output = repo
        .cmd()
        .args(["--json", "status", "nvim"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    let data = value.get("data").unwrap().as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["store"], "nvim");
}

#[test]
fn json_status_unknown_store_returns_typed_error() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let nvim_target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        nvim_target.to_string_lossy(),
    ));

    let output = repo
        .cmd()
        .args(["--json", "status", "nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "status", false);
    assert_error_shape(&value, "unknown-store", 5);
    let error = value.get("error").unwrap();
    assert!(error["hint"].as_str().unwrap().contains("nvim"));
}

#[test]
fn json_list_reports_stores_and_targets() {
    let repo = Repo::new();
    let t1 = repo.path().join("home1");
    let t2 = repo.path().join("home2");
    repo.write_authored(
        r#"
[stores.shells.targets.laptop]
when = { hostname = "laptop" }

[stores.shells.targets.server]
when = { hostname = "server" }
"#,
    );
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

    let output = repo.cmd().args(["--json", "list"]).output().unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "list", true);
    let data = value.get("data").unwrap().as_array().unwrap();
    assert_eq!(data.len(), 1);

    let shells = &data[0];
    assert_eq!(shells["name"], "shells");
    assert_eq!(shells["mode"], "multi-target");
    let targets = shells["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    assert!(targets.iter().any(|t| t["name"] == "laptop"));
    assert!(targets.iter().any(|t| t["name"] == "server"));
}

#[test]
fn json_doctor_passes() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home").join(".config").join("nvim");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy()
    ));
    repo.cmd().arg("apply").assert().success();

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "doctor", true);
    let summary = value["data"]["summary"].as_object().unwrap();
    assert_eq!(summary["errors"], 0);
}

#[test]
fn json_doctor_reports_error_findings() {
    let repo = Repo::new();
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        repo.path()
            .join("home")
            .join(".config")
            .join("nvim")
            .to_string_lossy()
    ));

    let output = repo.cmd().args(["--json", "doctor"]).output().unwrap();
    assert!(!output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "doctor", false);
    assert_error_shape(&value, "doctor", 13);
    let summary = value["data"]["summary"].as_object().unwrap();
    assert!(summary["errors"].as_u64().unwrap() > 0);
    let findings = value["data"]["findings"].as_array().unwrap();
    assert!(findings.iter().any(|f| f["id"] == "missing-store-dir"));
}

#[test]
fn json_prune_lists_orphan() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let home = tempfile::tempdir().unwrap();
    let covered = home.path().join(".config").join("nvim");
    let orphan = home.path().join("orphan");
    fs::create_dir_all(covered.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(repo.path().join("nvim"), &covered).unwrap();
    std::os::unix::fs::symlink(repo.path().join("nvim"), &orphan).unwrap();

    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        covered.to_string_lossy()
    ));

    let output = repo
        .cmd()
        .args([
            "--json",
            "prune",
            "--scan-dir",
            home.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "prune", true);
    let data = value.get("data").unwrap();
    let orphans = data["orphans"].as_array().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(
        orphans[0]["link"].as_str().unwrap(),
        orphan.to_string_lossy()
    );
    assert_eq!(data["removed"], 0);
    assert_eq!(data["failed"], 0);
}

#[test]
fn json_render_renders_template() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_state("[stores.git]\n");
    let src = store_dir.join("gitconfig.tmpl");
    fs::write(
        &src,
        "# managed by stitch\nhost={{ env(\"STITCH_TEST_RENDER\", \"x\") }}\n",
    )
    .unwrap();

    let output = repo
        .cmd()
        .args(["--json", "render", "git/gitconfig.tmpl"])
        .env("STITCH_TEST_RENDER", "myhost")
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "render", true);

    let data = value.get("data").unwrap();
    assert_eq!(data["source"].as_str().unwrap(), src.to_string_lossy());
    assert_eq!(data["link_name"], "gitconfig");
    assert_eq!(data["content"], "# managed by stitch\nhost=myhost\n");
    let sha = data["sha256"].as_str().unwrap();
    assert_eq!(sha.len(), 64);
}

#[test]
fn json_apply_reports_plan() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().args(["--json", "apply"]).output().unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "apply", true);

    let data = value.get("data").unwrap();
    let stores = data["stores"].as_array().expect("stores array");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["store_name"], "shells");
    let ops = stores[0]["ops"].as_array().expect("ops array");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["action"], "create_link");
    assert_eq!(
        ops[0]["target"].as_str().unwrap(),
        home.join(".bashrc").to_string_lossy()
    );
    assert_eq!(
        ops[0]["source"].as_str().unwrap(),
        repo.path().join("shells/.bashrc").to_string_lossy()
    );
    assert_eq!(
        ops[0]["requires"]["target"],
        serde_json::json!({"target": "absent"})
    );

    // The link was actually created.
    assert!(home.join(".bashrc").is_symlink());
    assert_plan_summary_fields(&data["summary"]);
}

#[test]
fn json_diff_reports_plan() {
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{}"
"#,
        target.to_string_lossy(),
    ));

    repo.cmd().arg("apply").assert().success();

    let output = repo.cmd().args(["--json", "diff"]).output().unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "diff", true);

    let data = value.get("data").unwrap();
    let stores = data["stores"].as_array().expect("stores array");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["store_name"], "nvim");
    let ops = stores[0]["ops"].as_array().expect("ops array");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["action"], "already_linked");
    assert_eq!(ops[0]["target"].as_str().unwrap(), target.to_string_lossy());
    assert_eq!(
        ops[0]["source"].as_str().unwrap(),
        repo.path().join("nvim").to_string_lossy()
    );
    let requires = &ops[0]["requires"]["target"];
    assert_eq!(requires["target"].as_str().unwrap(), "symlink_to");
    assert_eq!(
        requires["value"].as_str().unwrap(),
        repo.path().join("nvim").to_string_lossy()
    );
    assert_plan_summary_fields(&data["summary"]);
}

#[test]
fn json_apply_reports_conflict_real_file() {
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

    let output = repo.cmd().args(["--json", "apply"]).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(6));
    let value = json_output(&output);
    assert_envelope_shape(&value, "apply", false);
    assert_error_shape(&value, "conflict-real", 6);

    let data = value["data"].as_object().expect("partial plan data");
    let summary = &data["summary"];
    assert_plan_summary_fields(summary);
    assert_eq!(summary["conflicts"].as_u64().unwrap(), 1);
    let stores = data["stores"].as_array().unwrap();
    assert!(stores.iter().any(|s| {
        s["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["action"] == "conflict")
    }));

    assert!(target.is_file());
    assert_eq!(fs::read_to_string(&target).unwrap(), "real file");
}

#[test]
fn json_diff_reports_conflict_foreign_symlink() {
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

    let output = repo.cmd().args(["--json", "diff"]).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(7));
    let value = json_output(&output);
    assert_envelope_shape(&value, "diff", false);
    assert_error_shape(&value, "conflict-foreign", 7);

    let data = value["data"].as_object().expect("partial plan data");
    let summary = &data["summary"];
    assert_plan_summary_fields(summary);
    assert_eq!(summary["conflicts"].as_u64().unwrap(), 1);
    let stores = data["stores"].as_array().unwrap();
    assert!(stores.iter().any(|s| {
        s["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["action"] == "conflict")
    }));

    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), Path::new("/etc/foreign"));
}

#[test]
fn render_text_prints_content() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_state("[stores.git]\n");
    fs::write(
        store_dir.join("gitconfig.tmpl"),
        "host={{ env(\"STITCH_TEST_RENDER2\", \"fallback\") }}\n",
    )
    .unwrap();

    repo.cmd()
        .args(["render", "git/gitconfig.tmpl"])
        .env("STITCH_TEST_RENDER2", "rendered")
        .assert()
        .success()
        .stdout("host=rendered\n");
}

#[test]
fn render_rejects_non_template() {
    let repo = Repo::new();
    repo.make_store("git", &["gitconfig"]);
    repo.cmd()
        .args(["render", "git/gitconfig"])
        .assert()
        .failure()
        .stderr(contains("only .tmpl files"));
}

#[test]
fn json_rejected_on_write_commands_as_usage_envelope() {
    // Write/mutating commands other than `apply`/`diff` are not JSON-enabled.
    // The rejection must go through the same envelope contract an agent already
    // parses — a `usage` error (code 2) on stdout, honest exit 2 — not a prose
    // stderr line. The check fires before repo resolution, so no real repo is
    // needed, but we use one to keep the test realistic.
    let repo = Repo::new();
    let output = repo.cmd().args(["--json", "add", "~/x"]).output().unwrap();
    assert!(!output.status.success(), "add --json must not succeed");
    assert_eq!(
        output.status.code(),
        Some(2),
        "add --json must exit with the usage code 2"
    );

    // The envelope goes to stdout (one-stream rule); stderr is hook passthrough.
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", false);
    assert_error_shape(&value, "usage", 2);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--json is not supported for add"),
        "message should name the unsupported flag"
    );

    // Spot-check a second write command so the boundary isn't single-cmd.
    let output = repo.cmd().args(["--json", "remove", "x"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value = json_output(&output);
    assert_envelope_shape(&value, "remove", false);
    assert_error_shape(&value, "usage", 2);
}

// ---------------------------------------------------------------------------
// v0.7 M4 plan / apply --plan
// ---------------------------------------------------------------------------

#[test]
fn plan_captures_executable_plan_file() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let plan: Value = serde_json::from_str(stdout).expect("plan is valid JSON");
    assert_eq!(plan["schema"], 2);
    assert_eq!(plan["kind"], "stitch/plan");
    assert_eq!(
        plan["repo"].as_str().unwrap(),
        repo.path().to_string_lossy()
    );
    assert!(plan["config_sha256"].as_str().is_some());
    let platform = &plan["platform"];
    assert!(platform["os"].is_string());
    assert!(platform["arch"].is_string());
    assert!(platform["hostname"].is_string());
    assert!(platform["shell"].is_string());
    let ops = plan["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "create_link");
    assert_eq!(
        ops[0]["target"].as_str().unwrap(),
        home.join(".bashrc").to_string_lossy()
    );
    assert_eq!(
        ops[0]["source"].as_str().unwrap(),
        repo.path().join("shells/.bashrc").to_string_lossy()
    );
    assert_eq!(ops[0]["requires"]["target"], "absent");
    assert!(plan["conflicts"].as_array().unwrap().is_empty());
    let errors = plan["errors"].as_array().expect("plan errors array");
    assert!(errors.is_empty());
}

#[test]
fn plan_json_wraps_in_envelope() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().args(["--json", "plan"]).output().unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "plan", true);
    let data = value.get("data").unwrap();
    assert_eq!(data["kind"], "stitch/plan");
    assert!(data["ops"].as_array().unwrap().len() == 1);
}

#[test]
fn plan_reports_conflicts_and_exits_nonzero() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".bashrc"), "existing").unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(6));
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let plan: Value = serde_json::from_str(stdout).expect("plan emitted despite conflict");
    assert_eq!(plan["conflicts"][0]["kind"], "real_entry");
    assert!(
        std::str::from_utf8(&output.stderr)
            .unwrap()
            .contains("conflict")
    );
}

#[test]
fn apply_plan_executes_create_link() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    repo.cmd()
        .arg("plan")
        .assert()
        .success()
        .stdout(predicates::function::function(|s: &str| {
            fs::write(&plan_path, s).unwrap();
            true
        }));

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Executed 1/1 ops"));

    assert!(home.join(".bashrc").is_symlink());
}

#[test]
fn apply_plan_rejects_live_symlinked_parent() {
    // A symlink introduced after capture is rejected during the whole-plan
    // preflight, before any hook or mutation can traverse it. The destination
    // is intentionally external: all target symlink ancestors are unsafe.
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let external = tempfile::tempdir().unwrap();
    let real_config = external.path().join("real_config");
    fs::create_dir_all(&real_config).unwrap();
    let config_link = home.join(".config");
    fs::create_dir_all(&config_link).unwrap();

    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        config_link.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    fs::remove_dir(&config_link).unwrap();
    std::os::unix::fs::symlink(&real_config, &config_link).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));

    assert!(
        !real_config.join(".bashrc").exists(),
        "must not write through an external symlink"
    );
}

#[test]
fn apply_plan_rejects_symlinked_parent_into_repo() {
    // P0: a parent symlink that resolves *inside* the repo must be refused,
    // otherwise the link is created inside another store/directory.
    let repo = Repo::new();
    let store_dir = repo.make_store("shells", &[]);
    let nested = store_dir.join(".config");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join(".bashrc"), "contents").unwrap();

    // Directory inside the repo that the parent symlink will resolve to.
    let real_dir = repo.path().join("real_dir");
    fs::create_dir_all(&real_dir).unwrap();

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let config_link = home.join(".config");

    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".config/.bashrc"]
"#,
        home.to_string_lossy(),
    ));

    // Capture the plan while the parent is a real directory.
    fs::create_dir_all(&config_link).unwrap();
    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&plan_path, &output.stdout).unwrap();

    // Replace the real parent with a repo-pointing symlink before execution.
    fs::remove_dir(&config_link).unwrap();
    std::os::unix::fs::symlink(&real_dir, &config_link).unwrap();

    // The plan executor must refuse to write through the repo-pointing parent.
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));

    // Direct `apply` must also reject the repo-pointing parent as a conflict.
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("conflict"));

    // No write-through: nothing was created or renamed inside real_dir.
    assert!(!real_dir.join(".bashrc").exists());
    assert!(!real_dir.join(".bashrc.bak").exists());
}

#[test]
fn apply_plan_rejects_symlinked_higher_ancestor_into_repo() {
    // P1: a higher ancestor (not just the immediate parent) that resolves
    // *inside* the repo must be refused. The immediate parent is absent, so
    // create_dir_all would follow the ancestor symlink and write into the repo.
    let repo = Repo::new();
    let store_dir = repo.make_store("shells", &[]);
    let nested = store_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("file"), "contents").unwrap();

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();

    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = ["nested/file"]
"#,
        home.to_string_lossy(),
    ));

    // Capture the plan while home is a real directory and home/nested is absent.
    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&plan_path, &output.stdout).unwrap();

    // Create a directory inside the repo that the higher ancestor will resolve to.
    let victim = repo.path().join("victim");
    fs::create_dir_all(&victim).unwrap();

    // Replace the higher ancestor with a repo-pointing symlink before execution.
    fs::remove_dir(&home).unwrap();
    std::os::unix::fs::symlink(&victim, &home).unwrap();

    // The plan executor must refuse to write through the repo-pointing ancestor.
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));

    // No write-through: nothing was created or renamed inside the victim directory.
    assert!(!victim.join("nested").join("file").exists());
    assert!(!victim.join("nested").join("file.bak").exists());
}

#[test]
fn apply_plan_preflight_rechecks_parent_after_hook_replaces_dir_with_repo_symlink() {
    // A pre-apply hook can replace a real directory with a repo-pointing
    // symlink after the initial preflight has passed. The per-op preflight must
    // re-check ancestors and refuse to write through the new symlink.
    let repo = Repo::new();
    let store_dir = repo.make_store("shells", &[]);
    let nested = store_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("file"), "contents").unwrap();

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let victim = repo.path().join("victim");
    fs::create_dir_all(&victim).unwrap();

    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = ["nested/file"]
"#,
        home.to_string_lossy(),
    ));

    // The store pre-hook replaces the real `home` directory with a symlink to
    // `victim` (inside the repo) after the initial preflight has approved it.
    let hook = format!(
        "rm -rf {} && ln -s {} {}",
        home.display(),
        victim.display(),
        home.display()
    );
    repo.write_authored(&format!(
        r#"
[stores.shells]
hooks = {{ pre = "{}" }}
"#,
        hook
    ));

    // Capture the plan while home is still a real directory.
    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&plan_path, &output.stdout).unwrap();

    // Apply --plan: the store pre-hook runs between the initial preflight and
    // the op, and the per-op preflight must reject the new repo-pointing symlink.
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("symlink into the repository"));

    // No write-through: the hook must not have tricked the executor into the repo.
    assert!(!victim.join("nested").join("file").exists());
    assert!(!victim.join("nested").join("file.bak").exists());
}

#[test]
fn apply_plan_supports_dangling_source_symlink() {
    // P2: ordinary apply supports dangling source symlink entries (via
    // create_link_to_entry + symlink_metadata), so an unchanged generated plan
    // for such an entry must also execute. Previously plan validation used
    // exists()/is_file() which follow the link and rejected dangling sources.
    let repo = Repo::new();
    let store_dir = repo.make_store("app", &["regular"]);
    // A dangling source symlink: points at a non-existent path.
    std::os::unix::fs::symlink("nonexistent", store_dir.join("alias")).unwrap();

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{}"
files = ["regular", "alias"]
"#,
        home.to_string_lossy(),
    ));

    // Capture the plan. Plan-build must succeed (ordinary apply supports this).
    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&plan_path, &output.stdout).unwrap();

    // Apply the unchanged plan. Must succeed, not fail with
    // "source file does not exist: alias".
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();

    // The target link for the dangling source must be created and point at the
    // repo source entry (itself dangling).
    let target_alias = home.join("alias");
    assert!(target_alias.is_symlink());
    assert!(
        !target_alias.exists(),
        "dangling target link must remain dangling"
    );
    assert_eq!(
        std::fs::read_link(&target_alias).unwrap(),
        store_dir.join("alias"),
        "target link must point at the dangling source symlink path"
    );
    assert!(home.join("regular").is_symlink());
}

#[test]
fn apply_plan_dry_run_does_not_mutate() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap(), "--dry-run"])
        .assert()
        .success()
        .stdout(contains("Dry run"))
        .stdout(contains("Executed 0/1 ops"));

    assert!(!home.join(".bashrc").exists());
}

#[test]
fn apply_plan_accepts_force_but_rejects_only_as_usage_error() {
    let repo = Repo::new();
    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, "{}").unwrap();

    // --force is execution-time authority for a captured backup_and_link plan,
    // so it reaches plan parsing rather than being rejected as usage.
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("invalid plan file"));

    repo.cmd()
        .args([
            "apply",
            "--plan",
            plan_path.to_str().unwrap(),
            "--only",
            "shells",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("--plan is not compatible with --only"));
}

#[test]
fn apply_plan_operation_list_is_authority_not_only_capture_scope() {
    let repo = Repo::new();
    repo.make_store("alpha", &["file"]);
    repo.make_store("beta", &["file"]);
    let alpha_target = repo.path().join("home/alpha");
    let beta_target = repo.path().join("home/beta");
    repo.write_state(&format!(
        r#"
[stores.alpha]
target = "{}"
files = ["file"]

[stores.beta]
target = "{}"
files = ["file"]
"#,
        alpha_target.display(),
        beta_target.display()
    ));

    let scoped_output = repo
        .cmd()
        .args(["plan", "--only", "alpha"])
        .output()
        .unwrap();
    assert!(scoped_output.status.success());
    let full_output = repo.cmd().arg("plan").output().unwrap();
    assert!(full_output.status.success());
    let mut scoped: Value = serde_json::from_slice(&scoped_output.stdout).unwrap();
    let full: Value = serde_json::from_slice(&full_output.stdout).unwrap();

    // Plan files have no hidden signature. Execution authorizes the reviewed
    // operations, provided every one exactly matches a fresh normal apply plan.
    scoped["ops"] = full["ops"].clone();
    scoped["stores"] = full["stores"].clone();
    let plan_path = repo.path().join("broadened-plan.json");
    fs::write(&plan_path, serde_json::to_vec(&scoped).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();
    assert!(alpha_target.join("file").is_symlink());
    assert!(beta_target.join("file").is_symlink());
}

#[test]
fn apply_plan_detects_stale_env_render_hash() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    fs::write(
        store_dir.join("gitconfig.tmpl"),
        "host={{ env(\"STITCH_PLAN_TEST\", \"fallback\") }}\n",
    )
    .unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo
        .cmd()
        .arg("plan")
        .env("STITCH_PLAN_TEST", "alpha")
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .env("STITCH_PLAN_TEST", "beta")
        .assert()
        .failure()
        .code(12)
        .stderr(contains("render operation is not present"));
}

#[test]
fn apply_plan_rejects_a_different_repository() {
    let first = Repo::new();
    let second = Repo::new();
    let output = first.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan_path = second.path().join("plan.json");
    fs::write(&plan_path, output.stdout).unwrap();

    second
        .cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("repository mismatch"));
}

#[test]
fn apply_plan_detects_stale_config_hash() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    // Mutate the pinned config: editing state.toml changes the hash.
    let state_path = repo.path().join(".stitch/state.toml");
    let mut state = fs::read_to_string(&state_path).unwrap();
    state.push_str("\n# mutation\n");
    fs::write(&state_path, state).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("config hash mismatch"));
}

#[test]
fn apply_plan_detects_target_state_drift() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    // Someone put a foreign symlink at the target after the plan was captured.
    std::os::unix::fs::symlink("/tmp/foreign", home.join(".bashrc")).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));

    // The foreign link must not have been touched.
    assert_eq!(
        fs::read_link(home.join(".bashrc"))
            .unwrap()
            .to_string_lossy(),
        "/tmp/foreign"
    );
}

#[test]
fn apply_plan_cannot_edit_create_into_unforced_real_file_replacement() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    plan["ops"][0]["op"] = "replace_link".into();
    plan["ops"][0]["requires"]["target"] = "real_entry".into();
    plan["ops"][0]["requires"]
        .as_object_mut()
        .unwrap()
        .remove("value");

    let target = home.join(".bashrc");
    fs::write(&target, "USER DATA").unwrap();
    let plan_path = repo.path().join("edited-plan.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));
    assert_eq!(fs::read_to_string(target).unwrap(), "USER DATA");
}

#[test]
fn plan_force_captures_backup_and_link() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".bashrc"), "existing").unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().args(["plan", "--force"]).output().unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    let ops = plan["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "backup_and_link");
    assert_eq!(
        ops[0]["backup"].as_str().unwrap(),
        home.join(".bashrc.bak").to_string_lossy()
    );
    assert_eq!(ops[0]["requires"]["target"], "real_entry");
    assert_eq!(ops[0]["requires"]["backup"], "absent");
}

#[test]
fn apply_plan_executes_backup_and_link() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".bashrc"), "existing").unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().args(["plan", "--force"]).output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap(), "--force"])
        .assert()
        .success();

    assert!(home.join(".bashrc").is_symlink());
    assert!(home.join(".bashrc.bak").is_file());
    assert_eq!(
        fs::read_to_string(home.join(".bashrc.bak")).unwrap(),
        "existing"
    );
}

#[test]
fn apply_plan_json_envelope_reports_partial_execution_on_hook_failure() {
    let repo = Repo::new();
    repo.make_store("alpha", &[".bashrc"]);
    repo.make_store("omega", &["gitconfig"]);
    let home = repo.path().join("home");
    repo.write_split(
        &format!(
            r#"
[stores.alpha]
target = "{}"
files = [".bashrc"]

[stores.omega]
target = "{}"
files = ["gitconfig"]
"#,
            home.to_string_lossy(),
            home.to_string_lossy(),
        ),
        r#"
[stores.alpha.hooks]
pre = "exit 1"
"#,
    );

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    // Execute the store groups in the opposite order from the plan ops. The
    // remainder must still be reconstructed from operation identity/index.
    plan["stores"] = serde_json::json!(["omega", "alpha"]);
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    let output = repo
        .cmd()
        .args(["--json", "apply", "--plan", plan_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(10));
    let value = json_output(&output);
    assert_envelope_shape(&value, "apply", false);
    assert_error_shape(&value, "hook", 10);
    let data = value.get("data").expect("abort report must be in data");
    let executed = data["ops_executed"].as_array().unwrap();
    let remaining = data["ops_remaining"].as_array().unwrap();
    assert_eq!(executed.len(), 1, "first store's op should have run");
    assert_eq!(remaining.len(), 1, "second store's op should remain");
    assert_eq!(
        executed[0],
        format!("create_link {}", home.join("gitconfig").display())
    );
    assert_eq!(
        remaining[0],
        format!("create_link {}", home.join(".bashrc").display())
    );
    assert!(
        home.join("gitconfig").is_symlink(),
        "first link should exist"
    );
    assert!(
        !home.join(".bashrc").exists(),
        "second target must not be linked"
    );
}

#[test]
fn apply_plan_rejects_hand_edited_source_outside_repo() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    plan["ops"][0]["source"] = serde_json::json!("/tmp/evil.bashrc");
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("is not under the repo"));
}

#[test]
fn apply_plan_rejects_hand_edited_target_with_parent_dir() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    let bad_target = format!("{}/.bashrc/../evil", home.to_string_lossy());
    plan["ops"][0]["target"] = serde_json::json!(bad_target);
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("contains '..'"));
}

#[test]
fn apply_plan_rejects_injected_backup_operation() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".bashrc"), "existing").unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().args(["plan", "--force"]).output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    let bad_backup = repo
        .path()
        .join(".bashrc.bak")
        .to_string_lossy()
        .into_owned();
    plan["ops"][0]["backup"] = serde_json::json!(bad_backup);
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));
}

#[test]
fn apply_plan_rejects_hand_edited_unknown_store() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    let bad_source = repo
        .path()
        .join("badstore/.bashrc")
        .to_string_lossy()
        .into_owned();
    plan["ops"][0]["source"] = serde_json::json!(bad_source);
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("not in config"));
}

#[test]
fn apply_plan_rejects_hand_edited_removal_of_desired_link() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.to_string_lossy(),
    ));

    // Apply once so the target is a repo-owned symlink.
    repo.cmd().arg("apply").assert().success();
    assert!(home.join(".bashrc").is_symlink());

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    plan["ops"] = serde_json::json!([{
        "op": "remove_link",
        "store": "shells",
        "target": home.join(".bashrc").to_string_lossy(),
        "source": null,
        "requires": { "target": "symlink_into_repo" }
    }]);
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("still desired"));

    assert!(home.join(".bashrc").is_symlink());
}

#[test]
fn apply_plan_requires_render_gitignore_before_staging() {
    // B3 regression guard: `apply --plan` with a StageRender op must
    // refuse to stage when .gitignore does not cover .stitch/render/,
    // matching cmd_apply's v0.6 staging discipline. .gitignore is not
    // pinned by the config hash, so this is a runtime safety check.
    let repo = Repo::new();
    fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    let store_dir = repo.make_store("git", &[]);
    fs::write(store_dir.join("gitconfig.tmpl"), "host={{ hostname }}\n").unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        home.to_string_lossy(),
    ));

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains(".stitch/render/"));
    assert!(
        !repo
            .path()
            .join(".stitch")
            .join("render")
            .join("git")
            .exists(),
        "plan exec must fail before staging output"
    );
}

// ---------------------------------------------------------------------------
// P1 hardening: plan execution, promotion, staged cleanup, JSON scope
// ---------------------------------------------------------------------------

/// Whole-directory promotion must produce an executable plan instead of failing
/// preflight. The store with ignored content is promoted to file mode, links
/// the desired file, and never links the ignored directory.
#[test]
fn plan_promotes_whole_dir_when_ignored_content_present() {
    let repo = Repo::new();
    let store_dir = repo.make_store("vim", &["vimrc"]);
    fs::create_dir(store_dir.join(".git")).unwrap();
    fs::write(store_dir.join(".git").join("config"), "[core]\n").unwrap();

    let target = repo.path().join("home").join(".vim");
    repo.write_state(&format!(
        r#"
[stores.vim]
target = "{}"
"#,
        target.to_string_lossy()
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan should succeed for promoted store"
    );
    let plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    let ops = plan["ops"].as_array().unwrap();
    assert!(
        ops.iter().any(|o| {
            o["op"] == "create_link"
                && o["target"].as_str().unwrap_or("") == target.join("vimrc").to_string_lossy()
        }),
        "plan should link the individual file"
    );
    assert!(
        !ops.iter().any(|o| {
            o["target"]
                .as_str()
                .is_some_and(|t| t == target.to_string_lossy())
        }),
        "plan must not link the whole directory"
    );

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, &output.stdout).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();

    assert!(target.join("vimrc").is_symlink());
    assert!(
        !target.join(".git").exists(),
        ".git must not be linked into the target"
    );
}

/// A hand-edited remove_link whose source belongs to a different store than its
/// target must be rejected at validation time, even if the paths are both
/// repo-owned.
#[test]
fn apply_plan_rejects_remove_with_source_in_wrong_store() {
    let repo = Repo::new();
    repo.make_store("alpha", &["x"]);
    repo.make_store("beta", &["x"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.alpha]
target = "{0}/.alpha"
files = ["x"]

[stores.beta]
target = "{0}/.beta"
files = ["x"]
"#,
        home.to_string_lossy()
    ));

    repo.cmd().arg("apply").assert().success();
    assert!(home.join(".alpha").join("x").is_symlink());
    assert!(home.join(".beta").join("x").is_symlink());

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");

    // Hand-edit the plan: target is beta/x, but source claims repo/alpha/x.
    plan["ops"] = serde_json::json!([{
        "op": "remove_link",
        "store": "alpha",
        "target": home.join(".beta").join("x").to_string_lossy(),
        "source": repo.path().join("alpha").join("x").to_string_lossy(),
        "requires": { "target": "symlink_to", "value": repo.path().join("alpha").join("x").to_string_lossy() }
    }]);

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains(
            "is not under a configured target for store 'alpha'",
        ));
}

/// A staged link must be justified by a preceding StageRender op in the same
/// plan; a hand-edited create_link that points into the render tree without one
/// is rejected.
#[test]
fn apply_plan_rejects_staged_link_without_pinned_stage_render() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    fs::write(store_dir.join("gitconfig.tmpl"), "x\n").unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        home.to_string_lossy()
    ));

    let staged = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("gitconfig")
        .to_string_lossy()
        .into_owned();

    // Capture a valid plan, then drop the StageRender op so only the staged
    // create_link remains. The create_link must be rejected without its pinned
    // StageRender.
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    plan["ops"] = serde_json::json!([{
        "op": "create_link",
        "target": home.join("gitconfig").to_string_lossy(),
        "source": staged,
        "requires": { "target": "absent" }
    }]);

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("no pinned stage_render"));
}

#[test]
fn apply_plan_rejects_unselected_injected_stage_render() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    fs::write(store_dir.join("active.tmpl"), "same\n").unwrap();
    fs::write(store_dir.join("orphan.tmpl"), "same\n").unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.git]\ntarget = \"{}\"\nfiles = [\"active.tmpl\"]\n",
        home.display()
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut injected = plan["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["op"] == "stage_render")
        .unwrap()
        .clone();
    injected["source_rel"] = "orphan.tmpl".into();
    injected["staged"] = repo
        .path()
        .join(".stitch/render/git/orphan")
        .to_string_lossy()
        .into_owned()
        .into();
    plan["ops"].as_array_mut().unwrap().insert(0, injected);

    let plan_path = repo.path().join("injected-plan.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains(
            "render operation is not present in the freshly computed apply plan",
        ));
    assert!(!repo.path().join(".stitch/render/git/orphan").exists());
}

/// When a template is removed from state, `stitch plan` captures a `remove_staged`
/// op to clean up the stale render before its link is removed.
#[test]
fn plan_captures_stale_render_cleanup() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    fs::write(store_dir.join("gitconfig.tmpl"), "x\n").unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        home.to_string_lossy()
    ));

    // Apply once so a staged render and link exist.
    repo.cmd().arg("apply").assert().success();
    let staged = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("gitconfig");
    assert!(staged.exists());
    assert!(home.join("gitconfig").is_symlink());

    // Drop the gitconfig from the desired set while leaving the source file in
    // the store; it is now a stale link/render to be cleaned up. `ignore` lives
    // in the authored half.
    repo.write_split(
        &format!(
            r#"
[stores.git]
target = "{}"
files = []
"#,
            home.to_string_lossy()
        ),
        r#"
[stores.git]
ignore = ["gitconfig.tmpl"]
"#,
    );

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan should succeed: {}",
        std::str::from_utf8(&output.stderr).unwrap_or("???")
    );
    let plan: Value = serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap())
        .expect("plan is valid JSON");
    let ops = plan["ops"].as_array().unwrap();
    assert!(
        ops.iter()
            .any(|o| o["op"] == "remove_staged" && o["rel"] == "gitconfig"),
        "plan must include a remove_staged op for the stale render"
    );

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, &output.stdout).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();

    assert!(!staged.exists(), "staged render must be removed");
    assert!(!home.join("gitconfig").exists());
}

/// An injected backup operation must fail fresh-plan authorization before any
/// earlier operation can mutate the filesystem.
#[test]
fn apply_plan_rejects_injected_backup_before_mutation() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc", ".zshrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".bashrc"), "existing").unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc", ".zshrc"]
"#,
        home.to_string_lossy(),
    ));

    // Capture a normal --force plan, then hand-edit its backup operation and
    // place another valid create before it. Fresh-plan authorization must
    // reject the injected operation before either one executes.
    let output = repo.cmd().args(["plan", "--force"]).output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();

    // Reorder so create_link (for .zshrc) precedes the injected backup operation.
    let ops = plan["ops"].as_array_mut().unwrap();
    let mut create = None;
    let mut backup = None;
    for op in ops.drain(..) {
        match op["op"].as_str() {
            Some("create_link") => create = Some(op),
            Some("backup_and_link") => backup = Some(op),
            _ => {}
        }
    }
    let reordered = vec![
        create.expect("create_link for .zshrc"),
        backup.expect("backup_and_link for .bashrc"),
    ];
    *plan["ops"].as_array_mut().unwrap() = reordered;

    // Change the backup precondition and create the backup as a real file.
    plan["ops"][1]["requires"]["backup"] = "real_entry".into();
    let backup_path = Path::new(plan["ops"][1]["backup"].as_str().unwrap()).to_path_buf();
    fs::write(&backup_path, "backup").unwrap();

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));

    // The create_link must not have run; the real target and backup are untouched.
    assert!(
        !home.join(".zshrc").exists(),
        "no earlier op should have run"
    );
    assert!(home.join(".bashrc").is_file());
    assert!(backup_path.is_file());
}

/// A hand-edited replacement must be rejected by fresh-plan authorization
/// before any filesystem mutation.
#[test]
fn apply_plan_rejects_injected_store_replacement_before_mutation() {
    let repo = Repo::new();
    repo.make_store("alpha", &["x"]);
    repo.make_store("beta", &["x", "y"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.alpha]
target = "{0}"
files = ["x"]

[stores.beta]
target = "{0}"
files = ["x", "y"]
"#,
        home.to_string_lossy(),
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();

    // The captured plan wants to create home/x for both alpha and beta. Inject
    // a replacement that tries to claim alpha's link for beta. The current
    // plan never authorizes that replacement, so validation must reject it
    // before any filesystem mutation.
    let alpha_create = plan["ops"][0].clone();
    let beta_create_y = plan["ops"][2].clone();
    let beta_replace = serde_json::json!({
        "op": "replace_link",
        "target": home.join("x").to_string_lossy(),
        "source": repo.path().join("beta").join("x").to_string_lossy(),
        "requires": {
            "target": "symlink_to",
            "value": repo.path().join("alpha").join("x").to_string_lossy(),
        },
    });
    plan["ops"] = serde_json::json!([alpha_create, beta_create_y, beta_replace]);
    plan["stores"] = serde_json::json!(["beta", "alpha"]);

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("link operation is not present"));

    assert!(
        !home.join("y").exists(),
        "fresh-plan validation must not mutate"
    );
    assert!(!home.join("x").exists());
}

/// An inactive store's leftover staged render must not produce a RemoveStaged op
/// and must not block `apply --plan`.
#[test]
fn apply_plan_skips_stale_render_sweep_for_inactive_store() {
    let repo = Repo::new();
    repo.make_store("inactive", &["gitconfig.tmpl"]);
    let home = repo.path().join("home");
    repo.write_split(
        &format!(
            r#"
[stores.inactive]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
            home.to_string_lossy(),
        ),
        r#"
[stores.inactive]
when = { os = "definitely-not-this-os" }
"#,
    );

    // Pre-create a stale staged render as if the store was once active.
    let staged_dir = repo.path().join(".stitch").join("render").join("inactive");
    fs::create_dir_all(&staged_dir).unwrap();
    let staged = staged_dir.join("gitconfig");
    fs::write(&staged, "stale").unwrap();

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !plan["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["op"] == "remove_staged"),
        "inactive store must not produce RemoveStaged ops"
    );

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Executed 0/0 ops"));

    assert!(
        staged.exists(),
        "stale render of inactive store must not be swept"
    );
}

/// A target-level `when` skip must preserve its configured staged render, just
/// as normal `apply` does, even while active siblings in the store are planned.
#[test]
fn apply_plan_preserves_staging_for_inactive_target() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("a.tmpl"), "a = {{ os }}\n").unwrap();
    fs::write(store.join("b.tmpl"), "b = {{ os }}\n").unwrap();
    let target = repo.path().join("home").join(".config").join("git");
    let target_str = target.to_string_lossy();
    repo.write_state(&format!(
        r#"
[stores.git.targets.active]
target = "{target_str}"
files = ["a.tmpl"]

[stores.git.targets.skipped]
target = "{target_str}"
files = ["b.tmpl"]
"#,
    ));

    // Establish live renders and links while both targets are active.
    repo.cmd().arg("apply").assert().success();
    let link = target.join("b");
    let staged = repo
        .path()
        .join(".stitch")
        .join("render")
        .join("git")
        .join("b");
    assert!(link.is_symlink());
    assert!(staged.is_file());

    repo.write_authored(
        r#"
[stores.git.targets.skipped]
when = { os = "definitely-not-this-os" }
"#,
    );

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !plan["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| { op["op"] == "remove_staged" && op["store"] == "git" && op["rel"] == "b" }),
        "inactive target's configured render must not be swept"
    );

    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, &output.stdout).unwrap();
    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();

    assert!(link.is_symlink(), "inactive target link must remain");
    assert!(staged.is_file(), "inactive target render must remain");
    assert!(
        fs::read_to_string(&link).is_ok(),
        "inactive target link must remain readable"
    );
}

// ---------------------------------------------------------------------------
// Plan schema 2 safety regressions
// ---------------------------------------------------------------------------

#[test]
fn apply_plan_rejects_schema_one_as_stale() {
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        home.display()
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    plan["schema"] = serde_json::json!(1);
    let plan_path = repo.path().join("schema-one.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("unsupported plan schema: 1 (expected 2)"));
    assert!(!home.join(".bashrc").exists());
}

#[test]
fn plan_omits_link_below_existing_external_symlink_ancestor() {
    // The store target itself is a symlink, so the older nested-only guard
    // would miss it. Plan generation must make the plan non-executable.
    let repo = Repo::new();
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    let config = home.join(".config");
    let external = tempfile::tempdir().unwrap();
    // `config` itself is absent, so file-mode promotion and the existing
    // nested-parent guard cannot classify it. Only the plan builder sees the
    // higher external `home` symlink ancestor.
    std::os::unix::fs::symlink(external.path(), &home).unwrap();
    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
files = [".bashrc"]
"#,
        config.display()
    ));

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(!output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !plan["ops"].as_array().unwrap().iter().any(|op| {
            op["op"] == "create_link"
                && op["target"] == config.join(".bashrc").to_string_lossy().as_ref()
        }),
        "a link below a symlinked target root must be omitted"
    );
    assert!(
        plan["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|conflict| {
                conflict["kind"] == "symlink_ancestor"
                    && conflict["target"] == home.to_string_lossy().as_ref()
            }),
        "unexpected plan: {plan}"
    );
    assert!(!external.path().join(".bashrc").exists());
}

#[test]
fn apply_plan_rejects_whole_directory_then_child_link() {
    // Neither target has a live symlink when captured. The preflight simulator
    // must still reject beta's child because alpha creates its parent as a
    // symlink earlier in the store-grouped execution order.
    let repo = Repo::new();
    repo.make_store("alpha", &["profile"]);
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

    let plan_path = repo.path().join("overlap.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("symlinked ancestor"));
    assert!(
        !config.is_symlink(),
        "whole-directory link must not run before child preflight fails"
    );
    assert!(!config.join("nvim").join("init.lua").exists());
}

#[test]
fn apply_plan_rejects_hand_edited_removal_of_desired_staged_render() {
    let repo = Repo::new();
    let store = repo.make_store("git", &[]);
    fs::write(store.join("gitconfig.tmpl"), "name = stitch\n").unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["gitconfig.tmpl"]
"#,
        home.display()
    ));
    repo.cmd().arg("apply").assert().success();
    let staged = repo.path().join(".stitch/render/git/gitconfig");
    assert!(staged.is_file());

    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    plan["ops"] = serde_json::json!([{
        "op": "remove_staged",
        "store": "git",
        "rel": "gitconfig"
    }]);
    let plan_path = repo.path().join("remove-desired-render.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("still desired"));
    assert!(staged.is_file(), "desired staged render must remain");
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
fn remove_cleans_broken_template_link_when_staging_is_missing() {
    let repo = Repo::new();
    repo.make_store("app", &["config.tmpl"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join("config");
    let staged = repo.path().join(".stitch/render/app/config");
    std::os::unix::fs::symlink(&staged, &target).unwrap();
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"config.tmpl\"]\n",
        home.display()
    ));

    repo.cmd().args(["remove", "app"]).assert().success();
    assert!(fs::symlink_metadata(&target).is_err());
    let state = fs::read_to_string(repo.path().join(".stitch/state.toml")).unwrap();
    assert!(!state.contains("stores.app"));
}

#[test]
fn config_rejects_relative_target_paths() {
    let repo = Repo::new();
    repo.write_state("[stores.app]\ntarget = \"relative/home\"\n");
    repo.cmd()
        .arg("list")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("targets must expand to absolute paths"));
}

#[test]
fn template_apply_rejects_broad_gitignore_negation() {
    let repo = Repo::new();
    repo.make_store("app", &["secret.tmpl"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"secret.tmpl\"]\n",
        home.display()
    ));
    // `!**` re-includes the staging tree even though it does not spell out
    // `.stitch/render`; Git would track a rendered secret in this case.
    fs::write(repo.path().join(".gitignore"), ".stitch/render/\n!**\n").unwrap();

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains(".gitignore"));
    assert!(!home.join("secret").exists());
}

#[test]
fn whole_dir_mode_does_not_expose_symlink_named_as_template() {
    let repo = Repo::new();
    let store = repo.make_store("app", &[]);
    let external = tempfile::tempdir().unwrap();
    let secret = external.path().join("secret");
    fs::write(&secret, "external secret").unwrap();
    std::os::unix::fs::symlink(&secret, store.join("secret.tmpl")).unwrap();
    let target = repo.path().join("home/app");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\n",
        target.display()
    ));

    repo.cmd().arg("apply").assert().failure();
    assert!(fs::symlink_metadata(&target).is_err());
}

#[test]
fn whole_dir_mode_rejects_fifo_named_as_template_without_unlinking_target() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let repo = Repo::new();
    let store = repo.make_store("app", &[]);
    let template = store.join("secret.tmpl");
    let path = CString::new(template.as_os_str().as_bytes()).unwrap();
    // SAFETY: `path` is a NUL-terminated pathname owned by this test.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

    let target = repo.path().join("home/app");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &target).unwrap();
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\n",
        target.display()
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("template source"))
        .stdout(contains("direct regular file"));
    assert_eq!(fs::read_link(&target).unwrap(), store);
}

#[test]
fn template_gateway_source_is_rejected_by_apply_render_and_edit() {
    let repo = Repo::new();
    let store = repo.make_store("app", &[]);
    let external = tempfile::tempdir().unwrap();
    fs::write(
        external.path().join("secret.tmpl"),
        "external={{ hostname }}\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(external.path(), store.join("gateway")).unwrap();
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"gateway/secret.tmpl\"]\n",
        home.display()
    ));

    repo.cmd().arg("apply").assert().failure();
    assert!(!home.join("gateway/secret").exists());
    assert!(
        !repo
            .path()
            .join(".stitch/render/app/gateway/secret")
            .exists()
    );
    repo.cmd()
        .args(["render", "app/gateway/secret.tmpl"])
        .assert()
        .failure();
    repo.cmd()
        .args(["edit", home.join("gateway/secret").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("unsafe edit source"));
}

#[test]
fn template_hook_cannot_remove_gitignore_before_staging() {
    let repo = Repo::new();
    repo.make_store("app", &["secret.tmpl"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"secret.tmpl\"]\n",
        home.display()
    ));
    let hooks = repo.path().join(".stitch/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(
        hooks.join("pre-apply"),
        "#!/bin/sh\nrm -f \"$STITCH_ROOT/.gitignore\"\n",
    )
    .unwrap();
    make_executable(&hooks.join("pre-apply"));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("gitignore"));
    assert!(!repo.path().join(".stitch/render/app/secret").exists());
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
        .stdout(contains("repository or config changed during pre-hook"));
    assert!(
        !home.join("new").exists(),
        "a store hook that changes config must not reach target mutation"
    );
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
fn plan_promotion_rechecks_all_sources_after_store_hook() {
    let repo = Repo::new();
    let store = repo.make_store("app", &["a"]);
    let target = repo.path().join("home/app");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &target).unwrap();
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"a\"]\n",
        target.display()
    ));
    repo.write_authored(&format!(
        "[stores.app]\nhooks = {{ pre = \"rm -f {}\" }}\n",
        store.join("a").display()
    ));
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan = repo.path().join("plan.json");
    fs::write(&plan, output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("post-hook preflight"));
    assert_eq!(fs::read_link(&target).unwrap(), store);
}

#[test]
fn plan_multi_file_promotion_executes_without_false_preflight_conflict() {
    let repo = Repo::new();
    let store = repo.make_store("app", &["a", "b"]);
    let target = repo.path().join("home/app");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &target).unwrap();
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"a\", \"b\"]\n",
        target.display()
    ));
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan = repo.path().join("plan.json");
    fs::write(&plan, output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan.to_str().unwrap()])
        .assert()
        .success();
    assert!(target.join("a").is_symlink());
    assert!(target.join("b").is_symlink());
}

#[test]
fn remove_rejects_symlinked_state_before_unlinking_targets() {
    let repo = Repo::new();
    let store = repo.make_store("app", &["file"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"file\"]\n",
        home.display()
    ));
    repo.cmd().arg("apply").assert().success();
    let target = home.join("file");
    assert!(target.is_symlink());

    let state = repo.path().join(".stitch/state.toml");
    let external = repo.path().join("external-state.toml");
    fs::rename(&state, &external).unwrap();
    std::os::unix::fs::symlink(&external, &state).unwrap();
    repo.cmd().args(["remove", "app"]).assert().failure();
    assert!(target.is_symlink());
    assert!(fs::read_to_string(external).unwrap().contains("stores.app"));
    assert!(store.join("file").exists());
}

#[test]
fn config_rejects_target_overlap_through_filesystem_alias() {
    let repo = Repo::new();
    repo.make_store("app", &["a", "b"]);
    let external = tempfile::tempdir().unwrap();
    let root = external.path().join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    let gateway = repo.path().join("gateway");
    std::os::unix::fs::symlink(external.path(), &gateway).unwrap();
    repo.write_state(&format!(
        "[stores.app.targets.one]\ntarget = \"{}\"\nfiles = [\"a\"]\n\n[stores.app.targets.two]\ntarget = \"{}\"\nfiles = [\"b\"]\n",
        root.display(),
        gateway.join("root/sub").display()
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("overlapping target paths"));
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
fn remove_dry_run_reports_foreign_broken_link_as_conflict() {
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join("file");
    std::os::unix::fs::symlink("/tmp/foreign", &target).unwrap();
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"file\"]\n",
        home.display()
    ));

    repo.cmd()
        .args(["remove", "app", "--dry-run"])
        .assert()
        .failure()
        .code(7);
    assert_eq!(
        fs::read_link(&target).unwrap(),
        PathBuf::from("/tmp/foreign")
    );
}

#[test]
fn plan_rejects_config_changed_by_global_hook_before_mutation() {
    let repo = Repo::new();
    repo.make_store("app", &["new"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        "[stores.app]\ntarget = \"{}\"\nfiles = [\"new\"]\n",
        home.display()
    ));
    let hooks = repo.path().join(".stitch/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(
        hooks.join("pre-apply"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' '[stores.app]' 'target = \"{}\"' 'files = [\"new\", \"old\"]' > \"$STITCH_ROOT/.stitch/state.toml\"\n",
            home.display()
        ),
    )
    .unwrap();
    make_executable(&hooks.join("pre-apply"));
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(output.status.success());
    let plan = repo.path().join("plan.json");
    fs::write(&plan, output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("config changed during pre-apply hook"));
    assert!(!home.join("new").exists());
}
