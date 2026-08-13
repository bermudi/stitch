//! Templates and whole-dir promotion — `file_mode`, `patterns`, `*.tmpl` rendering (split from `tests/cli.rs`).
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
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(14)
        .stdout(contains("remove:"))
        .stdout(contains("remove staged:"));

    assert!(stale_link.is_symlink(), "diff must not unlink targets");
    assert!(stale_render.exists(), "diff must not delete staged renders");
}

#[test]
fn diff_exit_code_reports_stale_render_without_a_stale_target_link() {
    let repo = Repo::new();
    let store = repo.path().join("git");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("keep.tmpl"), "keep={{ os }}\n").unwrap();
    let target = repo.path().join("home/.config/git");
    repo.write_state(&format!(
        "[stores.git]\ntarget = \"{}\"\n",
        target.to_string_lossy()
    ));
    repo.cmd().arg("apply").assert().success();

    let orphan = repo.path().join(".stitch/render/git/orphan");
    fs::write(&orphan, "old rendered secret\n").unwrap();

    repo.cmd()
        .args(["diff", "--exit-code"])
        .assert()
        .failure()
        .code(14)
        .stdout(contains("remove staged:"));
    assert!(orphan.exists(), "diff must not delete staged renders");

    repo.cmd().arg("apply").assert().success();
    assert!(!orphan.exists(), "apply must remove stale staged renders");
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

/// `stitch edit` with neither $EDITOR nor $VISUAL set must fail with a clear
/// message and a non-zero exit code (no silent success, no raw I/O error).
/// PATH is constrained so the fallback to `vi` fails quickly instead of
/// launching an interactive editor.
#[test]
fn edit_fails_nonzero_when_editor_unset() {
    let repo = Repo::new();
    repo.cmd()
        .env_remove("EDITOR")
        .env_remove("VISUAL")
        .env("PATH", "/no-such-dir")
        .arg("edit")
        .assert()
        .failure()
        .stderr(contains("could not run editor 'vi'"));
}

/// `stitch edit` must use the `vi` fallback when neither $EDITOR nor $VISUAL
/// is set, preserving the pre-round-1 behavior.
#[test]
fn edit_uses_vi_fallback_when_editor_unset() {
    let repo = Repo::new();
    let vi_dir = tempfile::tempdir().unwrap();
    let vi = vi_dir.path().join("vi");
    fs::write(&vi, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&vi, fs::Permissions::from_mode(0o755)).unwrap();

    repo.cmd()
        .env_remove("EDITOR")
        .env_remove("VISUAL")
        .env("PATH", vi_dir.path().as_os_str())
        .arg("edit")
        .assert()
        .success();
}

/// `stitch edit` must still work when $EDITOR is set to a valid no-op editor.
#[test]
fn edit_works_when_editor_set() {
    let repo = Repo::new();
    repo.cmd()
        .env_remove("VISUAL")
        .env("EDITOR", "/bin/true")
        .arg("edit")
        .assert()
        .success();
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
fn render_undefined_var_errors() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_state("[stores.git]\n");
    fs::write(
        store_dir.join("config.tmpl"),
        "name = {{ vars.does_not_exist }}\n",
    )
    .unwrap();

    repo.cmd()
        .args(["render", "git/config.tmpl"])
        .assert()
        .failure()
        .code(8)
        .stderr(contains("does_not_exist"))
        .stderr(contains("undefined"));
}

#[test]
fn render_undefined_var_in_apply_errors() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    let home = repo.path().join("home");
    let target = home.join(".config").join("git");
    fs::write(
        store_dir.join("config.tmpl"),
        "name = {{ vars.does_not_exist }}\n",
    )
    .unwrap();
    repo.write_state(&format!(
        r#"
[stores.git]
target = "{}"
files = ["config.tmpl"]
"#,
        target.to_string_lossy()
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(8)
        .stdout(contains("does_not_exist"))
        .stdout(contains("undefined"));

    assert!(
        !repo.path().join(".stitch/render/git/config").exists(),
        "undefined var must not produce a staged render"
    );
    assert!(
        !home.join(".config/git/config").exists(),
        "undefined var must not create the target link"
    );
}

#[test]
fn render_defined_var_works() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_authored("[vars]\nemail = \"you@example.com\"\n");
    repo.write_state("[stores.git]\n");
    fs::write(
        store_dir.join("gitconfig.tmpl"),
        "email = {{ vars.email }}\n",
    )
    .unwrap();

    repo.cmd()
        .args(["render", "git/gitconfig.tmpl"])
        .assert()
        .success()
        .stdout("email = you@example.com\n");
}

#[test]
fn render_builtin_hostname_works() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_state("[stores.git]\n");
    fs::write(store_dir.join("gitconfig.tmpl"), "host={{ hostname }}\n").unwrap();

    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    repo.cmd()
        .args(["render", "git/gitconfig.tmpl"])
        .assert()
        .success()
        .stdout(format!("host={hostname}\n"));
}

#[test]
fn render_env_with_default_works() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_state("[stores.git]\n");
    fs::write(
        store_dir.join("gitconfig.tmpl"),
        "editor={{ env(\"EDITOR\", \"nvim\") }}\n",
    )
    .unwrap();

    repo.cmd()
        .args(["render", "git/gitconfig.tmpl"])
        .env_remove("EDITOR")
        .assert()
        .success()
        .stdout("editor=nvim\n");
}

#[test]
fn render_expression_works() {
    let repo = Repo::new();
    let store_dir = repo.make_store("git", &[]);
    repo.write_state("[stores.git]\n");
    fs::write(store_dir.join("gitconfig.tmpl"), "answer={{ 7*6 }}\n").unwrap();

    repo.cmd()
        .args(["render", "git/gitconfig.tmpl"])
        .assert()
        .success()
        .stdout("answer=42\n");
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
fn file_mode_root_linked_to_other_repo_dir_reports_consistently() {
    // The root symlink points INTO the repo but at a directory that is not
    // this store's source. status/doctor must not report the per-file entries
    // as missing; apply must conflict; remove must refuse to drop state while
    // apply conflicts (the symlink belongs to something else).
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    let other = repo.path().join("other_dir");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("g"), "g").unwrap();
    let root_dir = repo.path().join(".config").join("app");
    fs::create_dir_all(root_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&other, &root_dir).unwrap();
    repo.write_state(
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    );

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("foreign"));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stderr(contains("conflict"));

    repo.cmd()
        .args(["remove", "app"])
        .assert()
        .failure()
        .stderr(contains("foreign"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("stores.app"),
        "remove must keep state while the root is a conflict:\n{state}"
    );
    assert!(root_dir.is_symlink(), "root symlink must be untouched");
}
