//! Hooks — per-store and global `pre`/`post` hooks (split from `tests/cli.rs`).
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
fn pre_apply_hook_ancestor_symlink_does_not_escape_home() {
    // A pre-apply hook replaces a valid target ancestor with a symlink to an
    // external directory. Apply must revalidate target confinement after the
    // hook and conflict instead of writing through the escape.
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    let cfg = home.path().join(".config").join("app_dir");
    fs::create_dir_all(&cfg).unwrap();
    // The external directory the hook will redirect the ancestor to.
    let external = home.path().parent().unwrap().join("external_escape");
    fs::create_dir_all(&external).unwrap();
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app_dir/nested"
files = ["f"]
"#,
    );
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nset -e\nrm -rf \"$HOME/.config/app_dir\"\nln -s \"{}\" \"$HOME/.config/app_dir\"\n",
            external.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    repo.cmd()
        .arg("apply")
        .env("HOME", home_str)
        .assert()
        .failure()
        .stderr(contains("conflict"));

    // Nothing was created through the escape.
    assert!(
        !external.join("nested").exists(),
        "apply must not write through a hook-introduced external ancestor"
    );
    assert!(
        home.path().join(".config").join("app_dir").is_symlink(),
        "the hook's symlink must remain untouched"
    );
}

/// A global pre-apply hook replaces `~/.config` with a symlink to `~/.ssh`.
/// Apply must conflict before writing the new link through the redirected
/// ancestor.
#[test]
fn global_pre_apply_hook_in_home_redirect_blocks_apply() {
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    fs::create_dir_all(repo.path().join(".config")).unwrap();
    fs::create_dir_all(repo.path().join(".ssh")).unwrap();
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    );

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    fs::write(
        &hook,
        "#!/bin/sh\nset -e\nrm -rf \"$HOME/.config\"\nln -s \"$HOME/.ssh\" \"$HOME/.config\"\n",
    )
    .unwrap();
    make_executable(&hook);

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("conflict"));

    assert!(
        !repo.path().join(".ssh").join("app").join("f").exists(),
        "apply must not write through the redirected ancestor"
    );
    assert!(
        repo.path().join(".config").is_symlink(),
        "the hook-created symlink must remain untouched"
    );
}

/// A per-store pre-hook can also redirect a target ancestor. The per-store
/// ancestor snapshot must catch it independently of the global hook.
#[test]
fn per_store_pre_hook_redirects_target_ancestor() {
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    fs::create_dir_all(repo.path().join(".config")).unwrap();
    fs::create_dir_all(repo.path().join(".ssh")).unwrap();
    repo.write_split(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
        r#"
[stores.app]
hooks = { pre = "rm -rf $HOME/.config && ln -s $HOME/.ssh $HOME/.config" }
"#,
    );

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("conflict"));

    assert!(
        !repo.path().join(".ssh").join("app").join("f").exists(),
        "apply must not write through the per-store hook redirect"
    );
    assert!(
        repo.path().join(".config").is_symlink(),
        "the per-store hook-created symlink must remain untouched"
    );
}

/// A pre-apply hook may create a missing real target ancestor. Apply must
/// continue and create the expected link.
#[test]
fn global_pre_apply_hook_creates_missing_real_ancestor() {
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    // Intentionally do NOT create ~/.config.
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    );

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    fs::write(&hook, "#!/bin/sh\nset -e\nmkdir -p \"$HOME/.config/app\"\n").unwrap();
    make_executable(&hook);

    repo.cmd().arg("apply").assert().success();

    assert!(
        repo.path()
            .join(".config")
            .join("app")
            .join("f")
            .is_symlink(),
        "apply should create the link through the hook-created real ancestor"
    );
}

/// P0: a pre-apply hook replaces the real `$HOME` directory with a symlink to
/// an external directory. The target (`~/.app`) is directly under `$HOME`, the
/// case where the previous "intermediate ancestors only" fix captured nothing.
/// Apply must conflict before it can create `external/.app`.
#[test]
fn pre_apply_hook_replace_home_direct_target_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let external = tmp.path().join("external");
    fs::create_dir_all(&external).unwrap();

    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    repo.write_state(
        r#"
[stores.app]
target = "~/.app"
"#,
    );

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    fs::write(
        &hook,
        "#!/bin/sh\nset -e\n[ -n \"$HOME\" ] || exit 1\n[ -n \"$EXTERNAL_HOME\" ] || exit 1\nrm -rf \"$HOME\"\nln -s \"$EXTERNAL_HOME\" \"$HOME\"\n",
    )
    .unwrap();
    make_executable(&hook);

    repo.cmd()
        .env("HOME", &home)
        .env("EXTERNAL_HOME", &external)
        .arg("apply")
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    assert!(home.is_symlink(), "the hook must have replaced $HOME");
    assert!(
        !external.join(".app").exists(),
        "apply must not create a link in the external home"
    );
    assert!(
        !home.join(".app").exists(),
        "apply must not create a link through the redirected $HOME"
    );
}

/// P0: a pre-apply hook replaces `$HOME` with a symlink to an external
/// directory that already has the missing intermediate ancestor (`~/.config`).
/// The previous fix treated `~/.config` going from absent to real as benign;
/// with `$HOME` pinned the change at `$HOME` itself is caught first.
#[test]
fn pre_apply_hook_replace_home_with_existing_intermediate_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let external = tmp.path().join("external");
    fs::create_dir_all(external.join(".config")).unwrap();

    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    );

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    fs::write(
        &hook,
        "#!/bin/sh\nset -e\n[ -n \"$HOME\" ] || exit 1\n[ -n \"$EXTERNAL_HOME\" ] || exit 1\nrm -rf \"$HOME\"\nln -s \"$EXTERNAL_HOME\" \"$HOME\"\n",
    )
    .unwrap();
    make_executable(&hook);

    repo.cmd()
        .env("HOME", &home)
        .env("EXTERNAL_HOME", &external)
        .arg("apply")
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    assert!(home.is_symlink(), "the hook must have replaced $HOME");
    assert!(
        !external.join(".config").join("app").join("f").exists(),
        "apply must not create a link in the external home"
    );
    assert!(
        !home.join(".config").join("app").join("f").exists(),
        "apply must not create a link through the redirected $HOME"
    );
}

/// Regression guard: a real, unmodified `$HOME` still applies successfully.
#[test]
fn apply_with_real_home_direct_target_still_works() {
    let home = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    repo.write_state(
        r#"
[stores.app]
target = "~/.app"
"#,
    );

    repo.cmd()
        .env("HOME", home.path())
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("created"));

    assert!(home.path().join(".app").is_symlink());
    assert_eq!(
        fs::read_link(home.path().join(".app")).unwrap(),
        repo.path().join("app").canonicalize().unwrap()
    );
}
