//! `stitch remove` — unlinking and cleanup (split from `tests/cli.rs`).
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

/// P1 hardening: a store configured for file mode still has a whole-directory
/// symlink at its target root (pending promotion). `remove` must unlink the
/// root before dropping state, otherwise the state becomes empty while the
/// target root still points into the repo.
#[test]
fn remove_cleans_pending_whole_dir_promotion_root() {
    let repo = Repo::new();
    let store_dir = repo.make_store("app", &["f"]);
    let target = repo.path().join("home").join(".config").join("app");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
files = ["f"]
"#
    ));

    // Simulate a whole-dir symlink left over before the file-mode config.
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store_dir, &target).unwrap();
    assert!(target.is_symlink());
    assert_eq!(std::fs::read_link(&target).unwrap(), store_dir);

    // Dry run must report the root link in the planned removal list.
    repo.cmd()
        .args(["remove", "--dry-run", "app"])
        .assert()
        .success()
        .stdout(contains(&target_str));

    repo.cmd()
        .args(["remove", "app"])
        .assert()
        .success()
        .stdout(contains("Removed store 'app'"));

    // Root gone, store directory untouched, state entry removed.
    assert!(
        !target.exists(),
        "pending-promotion root symlink must be removed"
    );
    assert!(store_dir.is_dir(), "store directory must be left untouched");
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state_text.contains("[stores.app]"),
        "state entry must be removed"
    );
}

/// A file-mode store whose target root is a foreign whole-directory symlink
/// must still be rejected by `remove`, leaving both the link and the state
/// untouched.
#[test]
fn remove_rejects_foreign_root_in_file_mode() {
    let repo = Repo::new();
    let store_dir = repo.make_store("app", &["f"]);
    let target = repo.path().join("home").join(".config").join("app");
    let target_str = target.to_string_lossy().into_owned();
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
files = ["f"]
"#
    ));

    let foreign = tempfile::tempdir().unwrap();
    let foreign_dir = foreign.path().join("foreign");
    fs::create_dir_all(&foreign_dir).unwrap();
    fs::write(foreign_dir.join("f"), "not ours").unwrap();

    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&foreign_dir, &target).unwrap();
    assert!(target.is_symlink());
    assert_eq!(std::fs::read_link(&target).unwrap(), foreign_dir);

    repo.cmd()
        .args(["remove", "app"])
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    assert!(target.is_symlink(), "foreign root symlink must remain");
    assert_eq!(
        std::fs::read_link(&target).unwrap(),
        foreign_dir,
        "foreign root symlink must still point at the foreign target"
    );

    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state_text.contains("[stores.app]"),
        "state entry must be preserved"
    );
    assert!(
        state_text.contains(&target_str),
        "state target must be preserved"
    );
    assert!(store_dir.is_dir(), "store directory must be left untouched");
}

/// P1 regression: a whole-directory store that is skipped on the current
/// platform still has an owned symlink. `remove` must unlink it before
/// dropping state, otherwise the link becomes an orphan with no inventory
/// entry covering it.
#[test]
fn remove_whole_dir_when_skipped() {
    let repo = Repo::new();
    let store_dir = repo.make_store("app", &["f"]);
    let target = repo.path().join("home").join(".app");
    let target_str = target.to_string_lossy().into_owned();

    // Apply as a whole-directory store.
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
"#
    ));
    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), store_dir);

    // Skip the store on this platform.
    repo.write_authored(
        r#"
[stores.app]
when = { os = "nonexistent_os" }
"#,
    );

    repo.cmd()
        .args(["remove", "app"])
        .assert()
        .success()
        .stdout(contains("Removed store 'app'"));

    assert!(
        !target.exists(),
        "whole-dir symlink for skipped store must be removed"
    );
    assert!(store_dir.is_dir(), "store directory must be left untouched");
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state_text.contains("[stores.app]"),
        "state entry must be removed"
    );
}

/// P1 regression: a file-mode store that is skipped on the current platform
/// still has a leftover whole-directory symlink (pending promotion). `remove`
/// must unlink that root before dropping state.
#[test]
fn remove_file_mode_promotion_when_skipped() {
    let repo = Repo::new();
    let store_dir = repo.make_store("app", &["f"]);
    let target = repo.path().join("home").join(".config").join("app");
    let target_str = target.to_string_lossy().into_owned();

    // Start as whole-directory and apply.
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
"#
    ));
    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), store_dir);

    // Promote to file mode and skip on this platform.
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
files = ["f"]
"#
    ));
    repo.write_authored(
        r#"
[stores.app]
when = { os = "nonexistent_os" }
"#,
    );

    repo.cmd()
        .args(["remove", "app"])
        .assert()
        .success()
        .stdout(contains("Removed store 'app'"));

    assert!(
        !target.exists(),
        "pending-promotion root for skipped store must be removed"
    );
    assert!(store_dir.is_dir(), "store directory must be left untouched");
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state_text.contains("[stores.app]"),
        "state entry must be removed"
    );
}

/// A whole-directory store that is skipped on the current platform and has a
/// foreign symlink at its target must be rejected by `remove`. The foreign
/// link and the generated state must be preserved.
#[test]
fn remove_rejects_foreign_link_when_skipped() {
    let repo = Repo::new();
    let store_dir = repo.make_store("app", &["f"]);
    let target = repo.path().join("home").join(".app");
    let target_str = target.to_string_lossy().into_owned();

    // Apply as a whole-directory store.
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{target_str}"
"#
    ));
    repo.cmd().arg("apply").assert().success();
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), store_dir);

    // Skip the store and replace the owned link with a foreign one.
    repo.write_authored(
        r#"
[stores.app]
when = { os = "nonexistent_os" }
"#,
    );

    let foreign = tempfile::tempdir().unwrap();
    let foreign_dir = foreign.path().join("foreign");
    fs::create_dir_all(&foreign_dir).unwrap();
    fs::write(foreign_dir.join("f"), "not ours").unwrap();

    fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(&foreign_dir, &target).unwrap();
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), foreign_dir);

    repo.cmd()
        .args(["remove", "app"])
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    assert!(target.is_symlink(), "foreign symlink must remain");
    assert_eq!(
        fs::read_link(&target).unwrap(),
        foreign_dir,
        "foreign symlink must still point at the foreign target"
    );

    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state_text.contains("[stores.app]"),
        "state entry must be preserved"
    );
    assert!(
        state_text.contains(&target_str),
        "state target must be preserved"
    );
    assert!(store_dir.is_dir(), "store directory must be left untouched");
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
fn remove_hooks_can_invoke_mutating_stitch_commands() {
    // Regression: remove used to hold the state flock while running its
    // pre/post hooks; a hook that invoked a mutating stitch command blocked
    // forever on the lock held by its own parent. Hooks now run outside the
    // lock (pre-hook before it, post-hook after the state save).
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    );
    let bin = assert_cmd::cargo::cargo_bin("stitch");
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let pre = hooks_dir.join("pre-remove");
    fs::write(
        &pre,
        format!(
            "#!/bin/sh\n\"{}\" --repo \"$STITCH_ROOT\" add \"$HOME/.config/pre\" --name pre\n",
            bin.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&pre, fs::Permissions::from_mode(0o755)).unwrap();
    let post = hooks_dir.join("post-remove");
    fs::write(
        &post,
        format!(
            "#!/bin/sh\n\"{}\" --repo \"$STITCH_ROOT\" add \"$HOME/.config/post\" --name post\n",
            bin.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&post, fs::Permissions::from_mode(0o755)).unwrap();

    repo.cmd()
        .args(["remove", "app"])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success();

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        !state.contains("stores.app"),
        "removed store must be gone from state:\n{state}"
    );
    assert!(
        state.contains("[stores.pre]") && state.contains("[stores.post]"),
        "hook-invoked adds must have persisted:\n{state}"
    );
}

/// `remove --dry-run --json` reports the links that would be removed without
/// touching the filesystem and omits `behavior_orphaned`.
#[test]
fn remove_dry_run_json_previews_without_removing() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    repo.make_store("app", &["f"]);
    repo.write_state("[stores.app]\ntarget = \"~\"\nfiles = [\"f\"]\n");
    repo.cmd()
        .arg("apply")
        .env("HOME", home_path)
        .assert()
        .success();

    let link = home_path.join("f");
    assert!(link.is_symlink());

    let output = repo
        .cmd()
        .args(["--json", "remove", "app", "--dry-run"])
        .env("HOME", home_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "remove --dry-run --json must succeed"
    );
    let value = json_output(&output);
    assert_envelope_shape(&value, "remove", true);

    let data = &value["data"];
    assert_eq!(data["store"], "app");
    assert_eq!(data["target"], "~");
    assert_eq!(data["dry_run"], true);
    assert!(
        data["behavior_orphaned"].is_null(),
        "behavior_orphaned must be omitted on dry-run"
    );
    let staging = data["staging"].as_str().expect("staging string");
    assert!(
        staging.ends_with("/app"),
        "staging must include store name: {staging}"
    );

    let links = data["links"].as_array().expect("links array");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].as_str(), Some(link.to_str().unwrap()));

    // Nothing was actually removed.
    assert!(link.is_symlink());
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.contains("[stores.app]"));
}

/// A pre-remove hook that removes the generated state before `remove` runs
/// must still clean up the stitch-owned links rather than leaving them as
/// unmanaged orphans. The JSON response reports the cleaned-up links.
#[test]
fn remove_json_already_removed_by_pre_hook() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    repo.make_store("app", &["f"]);
    repo.write_state("[stores.app]\ntarget = \"~\"\nfiles = [\"f\"]\n");
    repo.cmd()
        .arg("apply")
        .env("HOME", home_path)
        .assert()
        .success();

    let link = home_path.join("f");
    assert!(link.is_symlink());

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let pre = hooks_dir.join("pre-remove");
    fs::write(
        &pre,
        format!(
            "#!/bin/sh\nrm -f \"{}\"\n",
            repo.path().join(".stitch").join("state.toml").display()
        ),
    )
    .unwrap();
    make_executable(&pre);

    let output = repo
        .cmd()
        .args(["--json", "remove", "app"])
        .env("HOME", home_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "remove --json on already-removed store must succeed"
    );
    let value = json_output(&output);
    assert_envelope_shape(&value, "remove", true);

    let data = &value["data"];
    assert_eq!(data["store"], "app");
    assert_eq!(data["dry_run"], false);
    assert!(
        data["behavior_orphaned"].is_null(),
        "behavior_orphaned must be omitted for an already-removed store"
    );
    // The link must be reported as cleaned up, not omitted.
    let links = data["links"].as_array().expect("links array");
    assert_eq!(links.len(), 1, "the stitch-owned link must be cleaned up");

    // The pre-hook removed the state; the command cleaned up the link.
    assert!(!repo.path().join(".stitch").join("state.toml").exists());
    assert!(
        !link.exists(),
        "stitch-owned link must be removed, not left as an orphan"
    );
}
