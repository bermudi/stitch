//! Config validation — `when`, `ignore`, `hooks`, target parsing, and file stability (split from `tests/cli.rs`).
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
fn apply_rejects_unknown_when_key() {
    let (repo, _target) = repo_with_bashrc_store();
    repo.write_authored(
        r#"
[stores.bashrc.when]
bogus_key = "x"
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("unknown field `bogus_key`"))
        .stderr(contains(
            "expected one of `os`, `arch`, `distro`, `hostname`, `shell`",
        ));
}

#[test]
fn authored_config_rejects_misspelled_ignore_and_hook_keys() {
    for authored in [
        "[stores.bashrc]\nignroe = [\"private\"]\n",
        "[stores.bashrc.hooks]\nprer = \"echo unsafe\"\n",
        "[stores.bashrc.targets.laptop]\nignroe = [\"private\"]\n",
        "unexpected = true\n",
    ] {
        let (repo, _target) = repo_with_bashrc_store();
        repo.write_authored(authored);

        repo.cmd()
            .arg("apply")
            .assert()
            .failure()
            .code(3)
            .stderr(contains("unknown field"));
    }
}

#[test]
fn generated_state_rejects_unknown_keys_without_mutating() {
    for state in [
        "unexpected = true\n",
        "[stores.bashrc]\ntarget = \"~/home\"\nfliles = [\".bashrc\"]\n",
        "[stores.bashrc.targets.laptop]\ntarget = \"~/home\"\nfliles = [\".bashrc\"]\n",
    ] {
        let repo = Repo::new();
        repo.make_store("bashrc", &[".bashrc"]);
        repo.write_state(state);

        repo.cmd()
            .arg("apply")
            .assert()
            .failure()
            .code(3)
            .stderr(contains("unknown field"));
        assert!(!repo.path().join("home").exists());
    }
}

#[test]
fn apply_skips_on_nonmatching_hostname() {
    let (repo, target) = repo_with_bashrc_store();
    repo.write_authored(
        r#"
[stores.bashrc.when]
hostname = "nonexistent-host"
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("skipped: platform"));

    assert!(
        !target.join(".bashrc").exists(),
        "non-matching hostname must skip linking"
    );
}

#[test]
fn apply_works_with_valid_when() {
    let (repo, target) = repo_with_bashrc_store();
    let current_os = std::env::consts::OS;
    repo.write_authored(&format!(
        r#"
[stores.bashrc.when]
os = "{current_os}"
"#,
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("created"));

    assert!(
        target.join(".bashrc").is_symlink(),
        "matching `when.os` must still allow linking"
    );
}

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
        .env("HOME", home.path().as_os_str())
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
fn apply_rejects_store_with_files_but_no_target() {
    let repo = Repo::new();
    repo.write_state(
        r#"
[stores.a]
files = ["f"]
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("store 'a'"))
        .stderr(contains("must have a target"))
        .stderr(contains("internal error: store directory").not());
}

#[test]
fn config_load_io_error_includes_path() {
    // A directly unreadable stitch.toml must be reported with its file name,
    // not a generic I/O error string.
    if is_root() {
        eprintln!("note: config_load_io_error_includes_path skipped under root");
        return;
    }
    let repo = Repo::new();
    let authored = repo.path().join("stitch.toml");
    fs::set_permissions(&authored, fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = RestoreMode {
        path: &authored,
        mode: 0o644,
    };

    repo.cmd()
        .arg("list")
        .assert()
        .failure()
        .stderr(contains("stitch.toml"))
        .stderr(contains("reading"))
        .stderr(contains("I/O error").not());
}
