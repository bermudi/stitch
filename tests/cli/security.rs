//! Security and filesystem invariants — symlinks, hard links, gateway, and matrix (split from `tests/cli.rs`).
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
fn config_rejects_target_overlap_through_filesystem_alias() {
    let repo = Repo::new();
    repo.make_store("app", &["a", "b"]);
    // Create two in-$HOME paths, one a strict ancestor of the other, where the
    // child path resolves through a gateway symlink. The overlap check must
    // still catch this filesystem alias even though strict validation now
    // requires targets to be under $HOME.
    let real = repo.path().join("home").join("real");
    fs::create_dir_all(real.join("sub")).unwrap();
    let gateway = repo.path().join("home").join("gateway");
    std::os::unix::fs::symlink(&real, &gateway).unwrap();
    repo.write_state(&format!(
        "[stores.app.targets.one]\ntarget = \"{}\"\nfiles = [\"a\"]\n\n[stores.app.targets.two]\ntarget = \"{}\"\nfiles = [\"b\"]\n",
        real.display(),
        gateway.join("sub").display()
    ));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("overlapping target paths"));
}

#[test]
fn state_lock_never_chmods_hard_linked_file() {
    // A pre-existing .stitch/state.lock may share its inode with an unrelated
    // file via a hard link. Opening it for locking must not re-permission the
    // shared inode — only a freshly created lock file may get 0600.
    let repo = Repo::new();
    let lock = repo.path().join(".stitch").join("state.lock");
    fs::write(&lock, "").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    let victim = repo.path().join("victim");
    // Hard-link first (the destination must not exist yet), then write
    // through either name — both share the inode.
    fs::hard_link(&lock, &victim).unwrap();
    fs::write(&victim, "precious").unwrap();

    // `add` acquires the lock first, so this exercises the open path. The
    // source must sit inside a real subdir of $HOME (a target equal to the
    // future store dir would be a conflict).
    let cfg_dir = repo.path().join(".config");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::write(cfg_dir.join("nested-file"), "ignored").unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_cfg = home.path().join(".config");
    fs::create_dir_all(&home_cfg).unwrap();
    fs::write(home_cfg.join("nested-file"), "ignored").unwrap();
    repo.cmd()
        .args(["add", "~/.config/nested-file"])
        .env("HOME", home.path())
        .assert()
        .success();

    use std::os::unix::fs::MetadataExt;
    let mode = fs::metadata(&victim).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o644, "hard-linked victim must keep its permissions");
    // Same inode: if the lock had been chmodded, the victim would show it.
    assert_eq!(
        fs::metadata(&lock).unwrap().mode() & 0o777,
        mode,
        "lock and victim share an inode; modes must match"
    );
}

#[test]
fn state_lock_existing_path_refuses_symlink() {
    // The existing-lock open must not follow a symlinked .stitch/state.lock.
    // Point the lock at an external, read-only file. A plain O_RDWR open would
    // follow the symlink and fail with "Permission denied" on the read-only
    // target; with O_NOFOLLOW, the open fails with ELOOP before it touches the
    // target, so the error must mention symbolic links and the external file
    // must remain unopened and unmodified.
    let repo = Repo::new();
    let external = tempfile::tempdir().unwrap();
    let external_file = external.path().join("external");
    fs::write(&external_file, "sensitive lock target").unwrap();

    let mut perms = fs::metadata(&external_file).unwrap().permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&external_file, perms).unwrap();

    let lock = repo.path().join(".stitch").join("state.lock");
    // Repo::new seeds the production lock filename so permission-failure tests
    // can acquire it before making .stitch read-only. Replace that fixture
    // lock with the symlink this test is meant to reject.
    fs::remove_file(&lock).unwrap();
    std::os::unix::fs::symlink(&external_file, &lock).unwrap();

    use std::os::unix::fs::MetadataExt;
    let before = fs::metadata(&external_file).unwrap();
    let before_mtime = (before.mtime(), before.mtime_nsec());

    let output = repo
        .cmd()
        .args(["add", "~/.config/locked"])
        .output()
        .expect("stitch add should run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected add to fail, got:\n{stderr}"
    );
    assert!(
        stderr.contains("symbolic"),
        "expected ELOOP symlink refusal, got:\n{stderr}"
    );

    let after = fs::metadata(&external_file).unwrap();
    let after_mtime = (after.mtime(), after.mtime_nsec());
    assert_eq!(
        after_mtime, before_mtime,
        "external target must not be opened or modified"
    );
    assert_eq!(
        fs::read_to_string(&external_file).unwrap(),
        "sensitive lock target",
        "external target content must be unchanged"
    );
}

#[test]
fn load_rejects_symlinked_stitch_dir() {
    // A symlinked .stitch could point state reads (and template staging)
    // anywhere. Load must refuse it before any command acts on its contents.
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    let external = repo.path().join("external_stitch");
    fs::create_dir_all(&external).unwrap();
    // The external "state" tries to create a $HOME link if it were followed.
    fs::write(
        external.join("state.toml"),
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    )
    .unwrap();
    let stitch = repo.path().join(".stitch");
    fs::remove_dir_all(&stitch).unwrap();
    std::os::unix::fs::symlink(&external, &stitch).unwrap();

    repo.cmd()
        .arg("status")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("state directory"));
    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .code(3)
        .stderr(contains("state directory"));

    // Nothing was created from the external state.
    assert!(
        !repo.path().join(".config").join("app").exists(),
        "apply must not act on state read through a symlinked .stitch"
    );
}

#[test]
fn load_rejects_symlinked_state_file() {
    // A symlinked state.toml would let an external file author the link
    // inventory. Every command that reads state must refuse it before acting.
    let repo = Repo::new();
    repo.make_store("app", &["f"]);
    let external = repo.path().join("external_state");
    fs::create_dir_all(&external).unwrap();
    // The external "state" tries to create a $HOME link if it were followed.
    fs::write(
        external.join("state.toml"),
        r#"
[stores.app]
target = "~/.config/app"
files = ["f"]
"#,
    )
    .unwrap();
    let state = repo.path().join(".stitch").join("state.toml");
    fs::remove_file(&state).unwrap();
    std::os::unix::fs::symlink(external.join("state.toml"), &state).unwrap();

    for (cmd, args) in [
        ("status", vec![]),
        ("apply", vec![]),
        ("plan", vec![]),
        ("diff", vec![]),
        ("doctor", vec![]),
        ("remove", vec!["app"]),
        ("prune", vec!["--yes"]),
    ] {
        repo.cmd()
            .arg(cmd)
            .args(args)
            .assert()
            .failure()
            .code(3)
            .stderr(contains("refusing symlinked or non-regular state file"));
    }

    // Nothing was created from the external state.
    assert!(
        !repo.path().join(".config").join("app").exists(),
        "apply must not act on state read through a symlinked state.toml"
    );
}

/// A repo store directory that is itself a symlink must not be treated as
/// healthy by any command. `status`, `doctor`, `apply`, and `remove` must all
/// agree that the store is invalid and `remove` must not drop the generated
/// state or touch the target link.
#[test]
fn symlinked_store_root_is_unhealthy() {
    let repo = Repo::new();
    let external = tempfile::tempdir().unwrap();
    let external_dir = external.path().join("shells");
    fs::create_dir_all(&external_dir).unwrap();
    fs::write(external_dir.join("rc"), "data").unwrap();

    // Replace the real store directory with a symlink to an external directory.
    let store_dir = repo.path().join("shells");
    repo.make_store("shells", &[]);
    fs::remove_dir(&store_dir).unwrap();
    std::os::unix::fs::symlink(&external_dir, &store_dir).unwrap();

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join(".shells");
    // Target link points at the repo symlink entry itself.
    std::os::unix::fs::symlink(&store_dir, &target).unwrap();

    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
"#,
        target.to_string_lossy()
    ));

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("error"))
        .stdout(contains("store directory"));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("store directory"));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .code(13)
        .stdout(contains("store directory"));

    repo.cmd()
        .args(["remove", "shells"])
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    // The target link is untouched and the state entry is preserved.
    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), store_dir);
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state_text.contains("[stores.shells]"));
}

/// Same as `symlinked_store_root_is_unhealthy`, but the target link points
/// directly at the external endpoint rather than the repo symlink entry.
#[test]
fn symlinked_store_root_with_foreign_target_is_unhealthy() {
    let repo = Repo::new();
    let external = tempfile::tempdir().unwrap();
    let external_dir = external.path().join("shells");
    fs::create_dir_all(&external_dir).unwrap();
    fs::write(external_dir.join("rc"), "data").unwrap();

    let store_dir = repo.path().join("shells");
    repo.make_store("shells", &[]);
    fs::remove_dir(&store_dir).unwrap();
    std::os::unix::fs::symlink(&external_dir, &store_dir).unwrap();

    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let target = home.join(".shells");
    std::os::unix::fs::symlink(&external_dir, &target).unwrap();

    repo.write_state(&format!(
        r#"
[stores.shells]
target = "{}"
"#,
        target.to_string_lossy()
    ));

    repo.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("error"))
        .stdout(contains("store directory"));

    repo.cmd()
        .arg("apply")
        .assert()
        .failure()
        .stdout(contains("store directory"));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .code(13)
        .stdout(contains("store directory"));

    repo.cmd()
        .args(["remove", "shells"])
        .assert()
        .failure()
        .code(7)
        .stderr(contains("conflict: foreign symlink"));

    assert!(target.is_symlink());
    assert_eq!(fs::read_link(&target).unwrap(), external_dir);
    let state_text = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state_text.contains("[stores.shells]"));
}

/// Regression guard for the `check_link` source-symlink branch: a source entry
/// inside a real store directory that is itself a symlink must still report
/// `linked` and remain removable.
#[test]
fn source_symlink_is_still_linked_and_removable() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let store_dir = repo.make_store("store", &["real"]);
    let real = store_dir.join("real");
    let alias = store_dir.join("alias");
    std::os::unix::fs::symlink("real", &alias).unwrap();

    repo.write_state(&format!(
        r#"
[stores.store]
target = "{}"
files = ["alias"]
"#,
        home.path().to_string_lossy()
    ));

    repo.cmd()
        .env("HOME", home.path())
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("ok"));

    let link = home.path().join("alias");
    assert!(link.is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), alias);

    repo.cmd()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(contains("linked"));

    repo.cmd()
        .env("HOME", home.path())
        .args(["remove", "store"])
        .assert()
        .success()
        .stdout(contains("Removed store 'store'"));

    assert!(!link.exists());
    assert!(real.exists());
}

/// **P0 matrix cell.** A pre-apply hook replaces the *directory behind* the
/// symlinked `$HOME` with a different directory. The symlink `$HOME` itself
/// is unchanged. Apply must detect the resolved-directory identity change and
/// refuse to write into the replacement.
#[test]
fn matrix_home_apply_hook_replaces_dir_behind_symlinked_home() {
    let env = MatrixHomeEnv::new_applied();

    // Create a hook that replaces real_home with a different directory.
    let hooks_dir = env.repo.join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).expect("mkdir hooks");
    let hook = hooks_dir.join("pre-apply");
    let real_home = env.real_home.clone();
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nset -e\nrm -rf \"{real}\"\nmkdir \"{real}\"\n",
            real = real_home.display()
        ),
    )
    .expect("write hook");
    make_executable(&hook);

    // Add a new file to the store so apply would try to create a new link.
    fs::write(env.repo.join("app").join("newfile"), "new").expect("write newfile");
    fs::write(
        env.repo.join(".stitch").join("state.toml"),
        "[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\", \"newfile\"]\n",
    )
    .expect("write state");

    env.cmd().arg("apply").assert().failure();

    // The new link must NOT have been created in the replacement directory.
    // (The old link was destroyed when real_home was rm -rf'd and recreated.)
    assert!(
        !env.real_home.join(".app").join("newfile").exists(),
        "apply must not write through the replaced home directory"
    );
}

/// **Positive counterpart.** A pre-apply hook that does NOT touch `$HOME`
/// must succeed normally with a symlinked home.
#[test]
fn matrix_home_apply_succeeds_with_symlinked_home_no_attack() {
    let env = MatrixHomeEnv::new_applied();

    // Hook that just touches a marker, doesn't touch home.
    let hooks_dir = env.repo.join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).expect("mkdir hooks");
    let hook = hooks_dir.join("pre-apply");
    let marker = env.repo.join("hook_ran");
    fs::write(
        &hook,
        format!("#!/bin/sh\ntouch \"{}\"\n", marker.display()),
    )
    .expect("write hook");
    make_executable(&hook);

    env.cmd().arg("apply").assert().success();
    assert!(marker.exists(), "hook should have run");
}

/// **P0 matrix cell.** A pre-remove hook replaces the directory behind the
/// symlinked `$HOME`. Remove must detect the change and refuse to delete the
/// external link or drop state.
#[test]
fn matrix_home_remove_hook_replaces_dir_behind_symlinked_home() {
    let env = MatrixHomeEnv::new_applied();

    // Capture the original link target for later verification.
    let original_link = env.real_home.join(".app");
    assert!(original_link.is_symlink(), "precondition: link exists");

    // Create a pre-remove hook that replaces real_home.
    let hooks_dir = env.repo.join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).expect("mkdir hooks");
    let hook = hooks_dir.join("pre-remove");
    let real_home = env.real_home.clone();
    let external = env._tmp.path().join("external");
    fs::create_dir_all(&external).expect("mkdir external");
    // The hook creates a new directory and puts a symlink in it, simulating
    // an external replacement that remove might delete.
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nset -e\nrm -rf \"{real}\"\nmkdir \"{real}\"\nln -s \"{repo}/app\" \"{real}/.app\"\n",
            real = real_home.display(),
            repo = env.repo.display()
        ),
    )
    .expect("write hook");
    make_executable(&hook);

    let result = env.cmd().arg("remove").arg("app").assert();

    // Remove must fail (exit non-zero).
    result.failure();

    // State must be preserved (the store must still be in state.toml).
    let state =
        fs::read_to_string(env.repo.join(".stitch").join("state.toml")).expect("read state");
    assert!(
        state.contains("stores.app"),
        "state must be preserved, got: {state}"
    );
}

/// **Positive counterpart.** Remove with a symlinked home and no attack must
/// succeed and clean up properly.
#[test]
fn matrix_home_remove_succeeds_with_symlinked_home_no_attack() {
    let env = MatrixHomeEnv::new_applied();

    env.cmd().arg("remove").arg("app").assert().success();

    assert!(!env.real_home.join(".app").exists(), "link must be removed");
    let state =
        fs::read_to_string(env.repo.join(".stitch").join("state.toml")).expect("read state");
    assert!(
        !state.contains("stores.app"),
        "state must be dropped, got: {state}"
    );
}

/// **P1 matrix cell.** `stitch edit` (no entry) opens `stitch.toml`. If that
/// file is a symlink to an external file, edit must refuse rather than editing
/// the external file.
#[test]
fn matrix_config_edit_rejects_symlinked_stitch_toml() {
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state("[stores.app]\ntarget = \"~/.app\"\n");

    let external = tempfile::tempdir().unwrap();
    let external_authored = external.path().join("stitch.toml");
    fs::write(&external_authored, "# external\n[stores.app]\n").unwrap();

    let authored = repo.path().join("stitch.toml");
    fs::remove_file(&authored).unwrap();
    std::os::unix::fs::symlink(&external_authored, &authored).unwrap();

    // Use /bin/true as editor so we can detect if it was invoked.
    repo.cmd()
        .env("EDITOR", "/bin/true")
        .arg("edit")
        .assert()
        .failure();

    // The external file must NOT have been modified by the editor.
    let content = fs::read_to_string(&external_authored).unwrap();
    assert_eq!(
        content, "# external\n[stores.app]\n",
        "external file must not be modified"
    );
}

/// **Positive counterpart.** `stitch edit` with a regular stitch.toml must
/// succeed.
#[test]
fn matrix_config_edit_succeeds_with_regular_stitch_toml() {
    let repo = Repo::new();
    // stitch.toml already exists from Repo::new (empty).
    repo.cmd()
        .env("EDITOR", "/bin/true")
        .arg("edit")
        .assert()
        .success();
}

/// **P1 matrix cell.** A platform-skipped store whose source root is a
/// symlink must NOT be removable. Active stores correctly refuse this; the
/// skipped path must agree.
#[test]
fn matrix_inventory_remove_skipped_store_with_symlinked_source_root() {
    let repo = Repo::new();

    // Create a symlinked store root pointing to an external directory.
    let external = tempfile::tempdir().unwrap();
    let external_store = external.path().join("evil");
    fs::create_dir_all(&external_store).unwrap();
    std::os::unix::fs::symlink(&external_store, repo.path().join("app")).unwrap();

    // Authored config with a when clause that never matches.
    repo.write_authored("[stores.app]\nwhen = { os = \"nonexistent\" }\n");
    repo.write_state("[stores.app]\ntarget = \"~/.app\"\n");

    // Create the link so there's something to remove.
    let home = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&external_store, home.path().join(".app")).unwrap();

    repo.cmd()
        .env("HOME", home.path())
        .arg("remove")
        .arg("app")
        .assert()
        .failure();

    // State must be preserved.
    let state =
        fs::read_to_string(repo.path().join(".stitch").join("state.toml")).expect("read state");
    assert!(
        state.contains("stores.app"),
        "state must be preserved for invalid inventory, got: {state}"
    );
}

/// **P1 matrix cell.** A platform-skipped store with colliding sources
/// (foo and foo.tmpl) must NOT be removable. Remove must reject the invalid
/// inventory rather than deleting a live link and dropping state.
#[test]
fn matrix_inventory_remove_skipped_store_with_source_name_collision() {
    let repo = Repo::new();
    repo.make_store("app", &["foo", "foo.tmpl"]);

    // Authored config with a when clause that never matches.
    repo.write_authored("[stores.app]\nwhen = { os = \"nonexistent\" }\n");
    repo.write_state("[stores.app]\ntarget = \"~/.app\"\nfiles = [\"foo\", \"foo.tmpl\"]\n");

    // Create the link so there's something to remove.
    let home = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(repo.path().join("app"), home.path().join(".app")).unwrap();

    repo.cmd()
        .env("HOME", home.path())
        .arg("remove")
        .arg("app")
        .assert()
        .failure();

    // State must be preserved.
    let state =
        fs::read_to_string(repo.path().join(".stitch").join("state.toml")).expect("read state");
    assert!(
        state.contains("stores.app"),
        "state must be preserved for colliding sources, got: {state}"
    );

    // The link must NOT have been removed.
    assert!(
        home.path().join(".app").is_symlink(),
        "link must not be removed when inventory is invalid"
    );
}

/// **P1 matrix cell.** A global pre-apply hook that swaps `stitch.toml` to
/// install a malicious per-store hook must be caught by the post-hook config
/// hash re-check. The malicious hook must not run.
///
/// `ConfigSnapshot` (in `src/config.rs`) binds the parsed config to the hash
/// of the exact bytes captured at load time, so the post-hook re-check
/// compares against the snapshot hash — not a re-read. This test exercises
/// the swap-without-restore variant; the stronger swap-and-restore variant
/// is covered by `apply_rejects_malicious_config_captured_then_restored`.
#[test]
fn matrix_config_apply_hash_rejects_config_swap_during_hook() {
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state("[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n");

    let home = tempfile::tempdir().unwrap();
    let marker = repo.path().join("pwned");

    // Global pre-apply hook: swap stitch.toml to install a malicious per-store
    // hook. The hook does NOT restore the original config, so the post-hook
    // hash check catches the change.
    let original_authored = fs::read_to_string(repo.path().join("stitch.toml")).unwrap();
    let malicious_authored = format!(
        "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
        marker.display()
    );

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    let authored_path = repo.path().join("stitch.toml");
    let malicious = malicious_authored.clone();
    let original = original_authored.clone();
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nset -e\ncat > \"{authored}\" << 'STITCH_EOF'\n{malicious}STITCH_EOF\n",
            authored = authored_path.display(),
            malicious = malicious,
        ),
    )
    .unwrap();
    make_executable(&hook);

    repo.cmd()
        .env("HOME", home.path())
        .arg("apply")
        .assert()
        .failure();

    // The malicious hook marker must NOT exist.
    assert!(
        !marker.exists(),
        "malicious per-store hook must not run when config was swapped by the global pre-apply hook"
    );

    // Restore authored config so the test doesn't leak.
    let _ = fs::write(&authored_path, &original);
}

/// **Positive counterpart.** A per-store pre-hook that is present in the
/// original config (no swap) must run normally.
#[test]
fn matrix_config_apply_per_store_hook_runs_when_config_stable() {
    let repo = Repo::new();
    repo.make_store("app", &["file"]);
    repo.write_state("[stores.app]\ntarget = \"~/.app\"\nfiles = [\"file\"]\n");

    let home = tempfile::tempdir().unwrap();
    let marker = repo.path().join("hook_ran");

    repo.write_authored(&format!(
        "[stores.app]\nhooks = {{ pre = \"touch {}\" }}\n",
        marker.display()
    ));

    repo.cmd()
        .env("HOME", home.path())
        .arg("apply")
        .assert()
        .success();

    assert!(marker.exists(), "per-store hook should have run");
}
