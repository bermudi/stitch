//! End-to-end tests for the `stitch` CLI binary.
//!
//! These tests build and exercise the binary via `assert_cmd`. Each test gets
//! a fresh tempdir that acts as the repo root, and writes the two-file v0.3
//! layout (`stitch.toml` authored + `.stitch/state.toml` generated) directly
//! (bypassing `init`) to keep the test bodies focused.

#![allow(unused_imports)]
#![allow(clippy::all)]
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::Value;

/// A scratch repo: a tempdir with `.stitch/` initialized and the two-file
/// config layout written (`stitch.toml` + `.stitch/state.toml`). Tests can
/// further mutate the filesystem (e.g. create store directories, source files)
/// as needed.
pub struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let stitch = dir.path().join(".stitch");
        fs::create_dir_all(&stitch).expect("mkdir .stitch");
        // Authored half: empty.
        fs::write(dir.path().join("stitch.toml"), "").expect("write stitch.toml");
        // Generated half: empty (the header is optional on read; keep it minimal).
        fs::write(stitch.join("state.toml"), "").expect("write state.toml");
        // The state lock file is normally created by the first mutating command;
        // seed it here so tests that make .stitch/ read-only still lock first.
        fs::write(stitch.join("state.lock"), "").expect("write lock");
        // Trust foundation: doctor requires `.stitch/render/` in .gitignore.
        // Real `init` writes this; tests that bypass init need it too.
        fs::write(dir.path().join(".gitignore"), ".stitch/render/\n").expect("write .gitignore");
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write the generated half (`.stitch/state.toml`) from a TOML string.
    /// Used by tests that only set inventory — the authored half stays empty.
    pub fn write_state(&self, toml: &str) {
        fs::write(self.dir.path().join(".stitch").join("state.toml"), toml)
            .expect("write state.toml");
    }

    /// Write the authored half (`stitch.toml`) from a TOML string.
    pub fn write_authored(&self, toml: &str) {
        fs::write(self.dir.path().join("stitch.toml"), toml).expect("write stitch.toml");
    }

    /// Write a complete store split across both files: `state` is the inventory
    /// half, `authored` is the behavior half. Both default to empty.
    pub fn write_split(&self, state: &str, authored: &str) {
        self.write_state(state);
        self.write_authored(authored);
    }

    /// Convenience: create a directory with some files inside the repo.
    pub fn make_store(&self, name: &str, files: &[&str]) -> PathBuf {
        let store_dir = self.dir.path().join(name);
        fs::create_dir_all(&store_dir).expect("mkdir store");
        for f in files {
            fs::write(store_dir.join(f), format!("contents of {f}")).expect("write file");
        }
        store_dir
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("stitch").expect("stitch binary");
        c.current_dir(self.dir.path());
        c.env("HOME", self.dir.path().as_os_str());
        c.env_remove("EDITOR"); // avoid any inherited editor
        c.env_remove("STITCH_REPO"); // tests drive --repo explicitly when needed
        c
    }
}

/// If running as root, file mode bits don't constrain writes, so tests that
/// rely on making state.toml read-only can't trigger the failure path they're
/// meant to exercise. Returns true to indicate the caller should skip (loudly)
/// rather than pass spuriously.
pub fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// A scratch environment that simulates a symlinked `$HOME`:
/// `home_link` is a symlink to `real_home`, and the repo lives under the same
/// temp root. This is the setup for issue #3.
pub struct SymlinkedHomeRepo {
    tmp: tempfile::TempDir,
}

impl SymlinkedHomeRepo {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_home = tmp.path().join("real_home");
        let home_link = tmp.path().join("home_link");
        let repo = tmp.path().join("repo");

        fs::create_dir_all(&real_home).expect("mkdir real_home");
        fs::create_dir_all(&repo).expect("mkdir repo");
        std::os::unix::fs::symlink(&real_home, &home_link).expect("symlink home");

        // Initialize a stitch repo in the normal way so the trust foundations
        // (.gitignore, .stitch/render/) are present.
        Command::cargo_bin("stitch")
            .expect("stitch binary")
            .current_dir(&repo)
            .env("HOME", &home_link)
            .env_remove("STITCH_REPO")
            .arg("init")
            .assert()
            .success();

        Self { tmp }
    }

    pub fn repo(&self) -> PathBuf {
        self.tmp.path().join("repo")
    }

    pub fn home_link(&self) -> PathBuf {
        self.tmp.path().join("home_link")
    }

    pub fn real_home(&self) -> PathBuf {
        self.tmp.path().join("real_home")
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("stitch").expect("stitch binary");
        c.current_dir(self.repo());
        c.env("HOME", self.home_link());
        c.env_remove("STITCH_REPO");
        c
    }
}

pub fn repo_with_bashrc_store() -> (Repo, std::path::PathBuf) {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.bashrc]
target = "{target}"
files = [".bashrc"]
"#,
        target = target.to_string_lossy(),
    ));
    (repo, target)
}

pub fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

pub fn prune_fixture() -> (Repo, PathBuf, PathBuf, tempfile::TempDir) {
    let repo = Repo::new();
    let store_dir = repo.make_store("nvim", &["init.lua"]);

    let home = tempfile::tempdir().unwrap();
    let covered = home.path().join(".config").join("nvim");
    let orphan = home.path().join(".config").join("old-nvim");
    fs::create_dir_all(covered.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store_dir, &covered).unwrap();
    std::os::unix::fs::symlink(&store_dir, &orphan).unwrap();

    // Use `~` so the covered target stays inside $HOME when tests set HOME to
    // the fake home dir; the orphan output assertions still use absolute paths.
    repo.write_state("[stores.nvim]\ntarget = \"~/.config/nvim\"\n");

    (repo, covered, orphan, home)
}

pub fn json_output(output: &std::process::Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).expect("utf8 stdout");
    serde_json::from_str(stdout).expect("valid JSON envelope")
}

pub fn assert_plan_summary_fields(summary: &Value) {
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

pub fn assert_envelope_shape(value: &Value, command: &str, ok: bool) {
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

pub fn assert_error_shape(value: &Value, class: &str, code: i64) {
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

/// Guard that restores a path's permissions on drop so a tempdir can be
/// cleaned up even when a test assertion panics.
pub struct RestoreMode<'a> {
    pub path: &'a Path,
    pub mode: u32,
}

impl<'a> Drop for RestoreMode<'a> {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.path, fs::Permissions::from_mode(self.mode));
    }
}

/// Helper: create a symlinked-$HOME environment with a repo that has a store
/// applied. Returns the temp dir, repo path, home symlink, and real home.
pub struct MatrixHomeEnv {
    pub _tmp: tempfile::TempDir,
    pub repo: PathBuf,
    pub home_link: PathBuf,
    pub real_home: PathBuf,
}

impl MatrixHomeEnv {
    /// Set up: symlinked home → real_home, repo with store "app" (whole-dir),
    /// store already applied so `~/.app -> repo/app` exists inside real_home.
    pub fn new_applied() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_home = tmp.path().join("real_home");
        let home_link = tmp.path().join("home_link");
        let repo = tmp.path().join("repo");

        fs::create_dir_all(&real_home).expect("mkdir real_home");
        fs::create_dir_all(&repo).expect("mkdir repo");
        std::os::unix::fs::symlink(&real_home, &home_link).expect("symlink home");

        // Init repo.
        Command::cargo_bin("stitch")
            .expect("bin")
            .current_dir(&repo)
            .env("HOME", &home_link)
            .env_remove("STITCH_REPO")
            .arg("init")
            .assert()
            .success();

        // Create store and state.
        let store_dir = repo.join("app");
        fs::create_dir_all(&store_dir).expect("mkdir store");
        fs::write(store_dir.join("file"), "contents").expect("write file");

        fs::write(
            repo.join(".stitch").join("state.toml"),
            "[stores.app]\ntarget = \"~/.app\"\n",
        )
        .expect("write state");

        // Apply so the link exists inside real_home.
        Command::cargo_bin("stitch")
            .expect("bin")
            .current_dir(&repo)
            .env("HOME", &home_link)
            .env_remove("STITCH_REPO")
            .arg("apply")
            .assert()
            .success();

        assert!(real_home.join(".app").is_symlink(), "link must exist");

        Self {
            _tmp: tmp,
            repo,
            home_link,
            real_home,
        }
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("stitch").expect("bin");
        c.current_dir(&self.repo);
        c.env("HOME", &self.home_link);
        c.env_remove("STITCH_REPO");
        c.env_remove("EDITOR");
        c.env_remove("VISUAL");
        c
    }
}
