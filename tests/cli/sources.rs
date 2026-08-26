//! Sources (v0.14) — fan-in, repo-relative why, remove inbound, multi-target ownership.

#![allow(clippy::all)]
use std::fs;

use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

use crate::support::{Repo, assert_envelope_shape, assert_error_shape, json_output};

#[test]
fn add_source_registers_without_moving() {
    // One real hub file, one consumer store that owns a target directory.
    // `add --source` must register the mapping without moving or copying,
    // and `apply` must then create the link.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    // Hub file lives at repo/shared/hub.txt
    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub contents").unwrap();

    // Consumer store "consumer" with target ~/.consumer and one existing file
    // to make it file-mode (otherwise whole-dir stores cannot declare sources).
    let _consumer_store = repo.make_store("consumer", &["existing.txt"]);
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    // Register alias.txt -> shared/hub.txt on the consumer store
    let alias_target = consumer_target.join("alias.txt");
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
        ])
        .assert()
        .success()
        .stdout(contains("Registered"));

    // Nothing was moved: hub still exists, alias target not yet linked (add --source is register-only)
    assert!(hub.exists(), "hub file must remain");
    assert!(
        !alias_target.exists(),
        "add --source must not create the link; apply does"
    );

    // State now contains the sources mapping
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("shared/hub.txt"),
        "state.toml must contain the source mapping:\n{state}"
    );
    assert!(
        state.contains("alias.txt"),
        "state.toml must contain the link name:\n{state}"
    );

    // Apply creates the link
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();

    assert!(
        alias_target.is_symlink(),
        "apply must create the alias link"
    );
    assert_eq!(
        fs::read_to_string(&alias_target).unwrap(),
        "hub contents",
        "link must read through to hub"
    );
    assert_eq!(
        fs::read_link(&alias_target).unwrap(),
        fs::canonicalize(&hub).unwrap(),
        "link must point at the hub file"
    );

    // Idempotent second add --source for same mapping is ok
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
        ])
        .assert()
        .success();

    // Changing the source for same link name must be rejected (create the other hub first)
    let other = repo.path().join("shared").join("other.txt");
    fs::write(&other, "other").unwrap();
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/other.txt",
        ])
        .assert()
        .failure()
        .stderr(contains("already mapped"));
}

#[test]
fn add_source_dry_run_and_json() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let alias_target = consumer_target.join("alias.txt");

    // Dry run does not write state
    let before = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(contains("Would register"));
    let after = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert_eq!(before, after, "dry run must not change state");

    // JSON mode reports the mapping
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args([
            "--json",
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);
    assert_eq!(value["data"]["store"], "consumer");
    assert_eq!(value["data"]["mode"], "add-source");
    assert_eq!(value["data"]["source"], "shared/hub.txt");
}

#[test]
fn add_adopts_existing_repo_link_without_move() {
    // Manually create a symlink at the target pointing into the repo,
    // then `add <path>` (without --source) should adopt it as a sources entry.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let alias_target = consumer_target.join("alias.txt");
    // Create the symlink manually, as a user would with `ln -s`
    std::os::unix::fs::symlink(&hub, &alias_target).unwrap();
    assert!(alias_target.is_symlink());

    // Now `add` on that existing symlink should register it, not move anything
    repo.cmd()
        .env("HOME", home_path)
        .args(["add", alias_target.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Registered"));

    // Hub still exists, symlink still points at hub
    assert!(hub.exists());
    assert!(alias_target.is_symlink());
    assert_eq!(fs::read_link(&alias_target).unwrap(), hub);

    // State contains the mapping
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.contains("alias.txt") && state.contains("shared/hub.txt"));

    // Apply is a no-op (already linked)
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success()
        .stdout(contains("ok"));
}

#[test]
fn status_shows_source_mapping() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let alias_target = consumer_target.join("alias.txt");
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
        ])
        .assert()
        .success();
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();

    // Text status must show the `alias.txt ← shared/hub.txt` marker
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("alias.txt ← shared/hub.txt"),
        "status text must show source mapping: {stdout}"
    );

    // JSON status must carry source_rel for sources entries
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "status"])
        .output()
        .unwrap();
    let value = json_output(&output);
    assert_envelope_shape(&value, "status", true);
    let rows = value["data"].as_array().unwrap();
    let alias_row = rows
        .iter()
        .find(|r| r["target"].as_str().unwrap().ends_with("alias.txt"))
        .expect("status must contain alias.txt entry");
    assert_eq!(alias_row["source_rel"], "shared/hub.txt");
    assert_eq!(alias_row["state"], "linked");
    // The absolute source should be the hub file
    assert!(
        alias_row["source"]
            .as_str()
            .unwrap()
            .ends_with("shared/hub.txt"),
        "source should be absolute hub path: {}",
        alias_row["source"]
    );
}

#[test]
fn why_reverse_lookup_repo_relative_and_absolute() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    // Consumer store with two aliases to the same hub (fan-in)
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    for name in ["a.txt", "b.txt"] {
        let target = consumer_target.join(name);
        repo.cmd()
            .env("HOME", home_path)
            .args([
                "add",
                target.to_str().unwrap(),
                "--source",
                "shared/hub.txt",
            ])
            .assert()
            .success();
    }
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();

    // Repo-relative query
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "why", "shared/hub.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "why", true);
    assert!(
        value["data"]["entry"].is_null(),
        "reverse lookup has no entry"
    );
    let consumers = value["data"]["consumers"].as_array().unwrap();
    assert_eq!(consumers.len(), 2, "hub should have two consumers");
    let targets: Vec<_> = consumers
        .iter()
        .map(|c| c["target"].as_str().unwrap().to_string())
        .collect();
    assert!(targets.iter().any(|t| t.ends_with("a.txt")));
    assert!(targets.iter().any(|t| t.ends_with("b.txt")));

    // Absolute query should give same result
    let abs = hub.to_string_lossy().into_owned();
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "why", &abs])
        .output()
        .unwrap();
    let value = json_output(&output);
    let consumers2 = value["data"]["consumers"].as_array().unwrap();
    assert_eq!(consumers2.len(), 2);

    // Traversal query must not be considered a repo source
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "why", "shared/../shared/hub.txt"])
        .output()
        .unwrap();
    // This is an invalid fragment for sources, but why should not treat it as reverse;
    // it will be looked up as a target (which does not exist) and return no entry/consumers.
    let value = json_output(&output);
    // Not a valid repo-relative source, so it should be a target lookup with no owner
    assert!(value["data"]["entry"].is_null());
    // Consumers should be empty or absent because the traversal is not a valid source mapping
    let consumers_empty = value["data"]
        .get("consumers")
        .and_then(|v| v.as_array())
        .map(|arr| arr.is_empty())
        .unwrap_or(true);
    assert!(consumers_empty, "traversal should not yield consumers");
}

#[test]
fn remove_refuses_when_inbound_sources_and_force_retains() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    // Provider store "hub" with a real file
    let hub_store = repo.make_store("hub", &["hub.txt"]);
    let hub_target = home_path.join(".hub");
    fs::create_dir_all(&hub_target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.hub]
target = "{}"
files = ["hub.txt"]
"#,
        hub_target.to_string_lossy(),
    ));

    // Consumer store references hub's file via sources
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.hub]
target = "{}"
files = ["hub.txt"]

[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        hub_target.to_string_lossy(),
        consumer_target.to_string_lossy(),
    ));

    // Add the fan-in
    let alias = consumer_target.join("alias.txt");
    repo.cmd()
        .env("HOME", home_path)
        .args(["add", alias.to_str().unwrap(), "--source", "hub/hub.txt"])
        .assert()
        .success();
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();

    // Remove without --force must refuse
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["remove", "hub"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("other stores reference") || stderr.contains("refusing to remove"),
        "remove must refuse with inbound message: {stderr}"
    );
    // Hub file must still exist
    assert!(hub_store.join("hub.txt").exists());
    // State still contains hub
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.contains("[stores.hub]"));

    // Remove with --force must succeed and retain the source file
    repo.cmd()
        .env("HOME", home_path)
        .args(["remove", "hub", "--force"])
        .assert()
        .success()
        .stdout(contains("Removed store"));

    // Source file retained in place (remove never deletes the store directory's content when referenced)
    assert!(
        hub_store.join("hub.txt").exists(),
        "force remove must retain referenced source file"
    );
    // State entry for hub removed
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(!state.contains("[stores.hub]"), "hub state must be removed");

    // JSON variant also carries retained_sources
    // Re-create hub for JSON test
    fs::create_dir_all(&hub_store).unwrap();
    fs::write(hub_store.join("hub.txt"), "hub").unwrap();
    repo.write_state(&format!(
        r#"
[stores.hub]
target = "{}"
files = ["hub.txt"]

[stores.consumer]
target = "{}"
files = ["existing.txt"]

[stores.consumer.sources]
"alias.txt" = "hub/hub.txt"
"#,
        hub_target.to_string_lossy(),
        consumer_target.to_string_lossy(),
    ));

    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "remove", "hub", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "remove", true);
    assert!(
        value["data"]["retained_sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap() == "hub/hub.txt"),
        "retained_sources should list hub/hub.txt"
    );
}

#[test]
fn remove_dry_run_shows_retained_sources() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub_store = repo.make_store("hub", &["hub.txt"]);
    let hub_target = home_path.join(".hub");
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&hub_target).unwrap();
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.hub]
target = "{}"
files = ["hub.txt"]

[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        hub_target.to_string_lossy(),
        consumer_target.to_string_lossy(),
    ));
    let alias = consumer_target.join("alias.txt");
    repo.cmd()
        .env("HOME", home_path)
        .args(["add", alias.to_str().unwrap(), "--source", "hub/hub.txt"])
        .assert()
        .success();

    // Dry run of remove without force should still refuse (dry run does not bypass the red line)
    repo.cmd()
        .env("HOME", home_path)
        .args(["remove", "hub", "--dry-run"])
        .assert()
        .failure();

    // Dry run with --json and --force shows retained_sources without mutating
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "remove", "hub", "--dry-run", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert!(value["data"]["retained_sources"].as_array().unwrap().len() > 0);
    // State still present
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(state.contains("[stores.hub]"));
    assert!(hub_store.join("hub.txt").exists());
}

#[test]
fn add_source_multi_target_ownership_picks_deepest() {
    // Two targets for one store, where one target is nested deeper. Add --source
    // must pick the deepest (longest ancestor) target directory.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    // Store "app" with two targets: outer at ~/.config/app and inner at ~/.other
    // (non-nested, as nested targets are forbidden by validation).
    let outer = home_path.join(".config").join("app");
    let inner = home_path.join(".other").join("app");
    fs::create_dir_all(&outer).unwrap();
    fs::create_dir_all(&inner).unwrap();
    repo.make_store("app", &["base.txt"]);
    // Use multi-target: need to write both authored and generated halves
    repo.write_state(&format!(
        r#"
[stores.app.targets.outer]
target = "{}"
files = ["base.txt"]

[stores.app.targets.inner]
target = "{}"
files = ["other.txt"]
"#,
        outer.to_string_lossy(),
        inner.to_string_lossy(),
    ));

    // Add a file under the inner target; it must land on the inner target's sources
    let target_file = inner.join("alias.txt");
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            target_file.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
        ])
        .assert()
        .success();

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    // The mapping must be under inner, not outer
    assert!(
        state.contains("[stores.app.targets.inner.sources]"),
        "state should have inner sources: {state}"
    );
    assert!(
        state.contains("alias.txt"),
        "alias.txt should be in state: {state}"
    );
    // Ensure outer did not get the mapping
    let outer_section = state
        .split("[stores.app.targets.inner.sources]")
        .next()
        .unwrap();
    assert!(
        !outer_section.contains("alias.txt"),
        "alias should not be in outer"
    );
}

#[test]
fn add_source_audit_and_json_error_consistency() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    // Need a store to own the target
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let alias = consumer_target.join("alias.txt");

    // Invalid source (contains ..) must fail with path-validation and produce JSON envelope + audit
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args([
            "--json",
            "add",
            alias.to_str().unwrap(),
            "--source",
            "../escape.txt",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(9));
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", false);
    assert_error_shape(&value, "path-validation", 9);

    // Audit log should have an error entry for add
    let log = repo.cmd().args(["--json", "log"]).output().unwrap();
    let log_value = json_output(&log);
    let entries = log_value["data"].as_array().unwrap();
    let last = entries.last().unwrap();
    assert_eq!(last["command"], "add");
    assert_eq!(last["outcome"], "error");
    assert_eq!(last["exit_class"], "path-validation");

    // Successful add --source must also be audited (when not dry-run)
    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args([
            "--json",
            "add",
            alias.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);

    let log = repo.cmd().args(["--json", "log"]).output().unwrap();
    let log_value = json_output(&log);
    let entries = log_value["data"].as_array().unwrap();
    let last = entries.last().unwrap();
    assert_eq!(last["command"], "add");
    assert_eq!(last["outcome"], "ok");
}

#[test]
fn add_adopt_link_audit_and_json() {
    // Adopting an existing symlink via `add` without --source
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let alias = consumer_target.join("alias.txt");
    std::os::unix::fs::symlink(&hub, &alias).unwrap();

    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args(["--json", "add", alias.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);
    assert_eq!(value["data"]["mode"], "add-source");

    // Audit
    let log = repo.cmd().args(["--json", "log"]).output().unwrap();
    let log_value = json_output(&log);
    let last = log_value["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last["command"], "add");
    assert_eq!(last["outcome"], "ok");
}

#[test]
fn protected_source_dot_slash_is_rejected() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let target = home_path.join(".app");
    fs::create_dir_all(&target).unwrap();
    repo.make_store("app", &["a"]);
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{}"
files = ["a"]
"#,
        target.to_string_lossy(),
    ));
    // Hand-edit a protected source with ./ prefix should be rejected at load/apply
    repo.write_state(&format!(
        r#"
[stores.app]
target = "{}"
files = ["a"]

[stores.app.sources]
leak = "./.stitch/state.toml"
"#,
        target.to_string_lossy(),
    ));
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .failure()
        .code(9)
        .stderr(contains("must not live under"));
}

#[test]
fn shared_template_plan_is_executable() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    // .gitignore for render
    fs::write(repo.path().join(".gitignore"), ".stitch/render/\n").unwrap();
    let shared = repo.path().join("shared");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("h.tmpl"), "hi {{ os }}\n").unwrap();
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.make_store("consumer", &["existing.txt"]);
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]

[stores.consumer.sources]
out = "shared/h.tmpl"
"#,
        consumer_target.to_string_lossy(),
    ));
    // Direct apply works
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();
    // Plan capture and apply --plan must also succeed
    let plan_output = repo
        .cmd()
        .env("HOME", home_path)
        .arg("plan")
        .output()
        .unwrap();
    assert!(plan_output.status.success());
    let plan_path = repo.path().join("plan.json");
    fs::write(&plan_path, &plan_output.stdout).unwrap();
    repo.cmd()
        .env("HOME", home_path)
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn ancestor_source_sweep_spares_nested_target_dirs() {
    // The dots hub layout: a store owning ~/.config declares the hub file via
    // `sources`, while a multi-target store owns nested targets (~/.config/kilo,
    // ~/.config/mimocode) linking the SAME real file by its own name. The
    // ancestor's stale-link sweep must not descend into the nested stores'
    // target directories: before v0.14.1 its declared-source ownership check
    // classified the nested stores' healthy links as its own stale, repointed
    // entries and deleted them — every apply fought the previous one.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();
    let config_dir = home_path.join(".config");
    fs::create_dir_all(config_dir.join("kilo")).unwrap();
    fs::create_dir_all(config_dir.join("mimocode")).unwrap();

    // The one real hub file, plus the store dir that links it by its own name.
    repo.make_store("agents", &["AGENTS.md"]);
    // A sources-only store still owns a (possibly empty) store directory.
    fs::create_dir_all(repo.path().join("agentsconfig")).unwrap();

    repo.write_state(&format!(
        r#"
[stores.agents.targets.kilo]
target = "{kilo}"
files = ["AGENTS.md"]

[stores.agents.targets.mimocode]
target = "{mimocode}"
files = ["AGENTS.md"]

[stores.agentsconfig]
target = "{config}"

[stores.agentsconfig.sources]
"AGENTS.md" = "agents/AGENTS.md"
"#,
        kilo = config_dir.join("kilo").to_string_lossy(),
        mimocode = config_dir.join("mimocode").to_string_lossy(),
        config = config_dir.to_string_lossy(),
    ));

    // First apply creates all three links.
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();

    for link in [
        config_dir.join("AGENTS.md"),
        config_dir.join("kilo").join("AGENTS.md"),
        config_dir.join("mimocode").join("AGENTS.md"),
    ] {
        assert!(link.is_symlink(), "link must exist: {}", link.display());
        assert_eq!(
            fs::read_to_string(&link).unwrap(),
            "contents of AGENTS.md",
            "link must read through to the hub: {}",
            link.display()
        );
    }

    // Second apply must be a no-op: the ancestor store's sweep must not
    // remove the nested stores' links (the pre-0.14.1 tug-of-war).
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success()
        .stdout(predicates::str::contains("remove:").not());

    for link in [
        config_dir.join("kilo").join("AGENTS.md"),
        config_dir.join("mimocode").join("AGENTS.md"),
    ] {
        assert!(
            link.is_symlink(),
            "link must survive re-apply: {}",
            link.display()
        );
    }

    // And diff reports full convergence.
    repo.cmd()
        .env("HOME", home_path)
        .args(["diff", "--exit-code"])
        .assert()
        .success();
}

#[test]
fn add_source_migrates_existing_files_entry() {
    // The fan-in conversion the v0.14 plan §9 envisioned `stitch add`
    // performing: a target name previously served by a `files` entry
    // (implicit source <store_dir>/<name>) is repointed at a repo-relative
    // `sources` entry. `add --source` must drop the `files` entry and insert
    // the `sources` entry in one atomic state.toml write — no hand-edit, no
    // remove+add round-trip that briefly unlinks the live target link.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    // Hub file lives outside the consumer store dir.
    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub contents").unwrap();

    // Consumer store currently links AGENTS.md from its own dir (files entry).
    let consumer_dir = repo.make_store("consumer", &["AGENTS.txt"]);
    fs::write(consumer_dir.join("AGENTS.txt"), "old in-store contents").unwrap();
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["AGENTS.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    // Apply so the link exists and points at the in-store file.
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();
    let link = consumer_target.join("AGENTS.txt");
    assert!(link.is_symlink(), "link must exist before migration");
    assert_eq!(
        fs::read_to_string(&link).unwrap(),
        "old in-store contents",
        "link must read the in-store file before migration"
    );

    // Migrate: same target name, new repo-relative source.
    repo.cmd()
        .env("HOME", home_path)
        .args(["add", link.to_str().unwrap(), "--source", "shared/hub.txt"])
        .assert()
        .success()
        .stdout(contains("Registered"))
        .stdout(contains("migrated from `files` entry"));

    // State.toml: sources entry present, files entry gone — one atomic write.
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("shared/hub.txt"),
        "state must contain the new source:\n{state}"
    );
    assert!(
        !state.contains("files"),
        "state must no longer contain a files entry:\n{state}"
    );

    // The link was NOT unlinked by the migration (no round-trip gap).
    assert!(
        link.is_symlink(),
        "link must remain a symlink through the migration"
    );

    // Apply repoints the link to the hub.
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&link).unwrap(),
        "hub contents",
        "link must read through to the hub after apply"
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        fs::canonicalize(&hub).unwrap(),
        "link must point at the hub after apply"
    );

    // Doctor is clean: no empty-store (store dir still has AGENTS.txt, but
    // more importantly the store declares sources), no missing-link.
    repo.cmd()
        .env("HOME", home_path)
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("0 errors"))
        .stdout(contains("0 warnings"));
}

#[test]
fn add_source_migrates_existing_files_entry_dry_run_and_json() {
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    repo.make_store("consumer", &["AGENTS.txt"]);
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["AGENTS.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let link = consumer_target.join("AGENTS.txt");

    // Dry-run reports the migration would happen but does not write state.
    let before = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            link.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(contains("Would register"))
        .stdout(contains("files→sources migration"));
    let after = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert_eq!(before, after, "dry run must not change state");

    // JSON dry-run reports migrated_from_files: true.
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args([
            "--json",
            "add",
            link.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_eq!(value["data"]["migrated_from_files"], true);
}

#[test]
fn add_source_without_files_entry_reports_no_migration() {
    // When there is no files entry to migrate, migrated_from_files must be
    // absent (false) in JSON and the text output must not mention migration.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    repo.make_store("consumer", &["existing.txt"]);
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let alias_target = consumer_target.join("alias.txt");
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args([
            "--json",
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert!(
        value["data"].get("migrated_from_files").is_none(),
        "migrated_from_files must be absent when no files entry was migrated: {:?}",
        value["data"]
    );
}

#[test]
fn add_source_migrates_files_entry_on_named_target() {
    // The migration's `Some(t)` arm (src/commands/add.rs) handles a `files`
    // entry declared on a named target of a multi-target store, not just a
    // top-level store. The existing migration tests only exercise top-level
    // stores, so a regression in the named-target path — wrong target
    // selected, panic on missing target, files entry left behind on the
    // target — would not be caught. This test covers that path end to end.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub contents").unwrap();

    // Multi-target store "app". The "inner" target currently links config.txt
    // from the store dir via a files entry on the named target.
    repo.make_store("app", &["config.txt"]);
    fs::write(repo.path().join("app").join("config.txt"), "old in-store").unwrap();
    let outer = home_path.join(".config").join("app");
    let inner = home_path.join(".other").join("app");
    fs::create_dir_all(&outer).unwrap();
    fs::create_dir_all(&inner).unwrap();
    repo.write_state(&format!(
        r#"
[stores.app.targets.outer]
target = "{}"
files = ["base.txt"]

[stores.app.targets.inner]
target = "{}"
files = ["config.txt"]
"#,
        outer.to_string_lossy(),
        inner.to_string_lossy(),
    ));
    // base.txt must exist for the outer target to apply cleanly.
    fs::write(repo.path().join("app").join("base.txt"), "base").unwrap();

    // Apply so the inner link exists and points at the in-store file.
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();
    let link = inner.join("config.txt");
    assert!(link.is_symlink(), "link must exist before migration");
    assert_eq!(
        fs::read_to_string(&link).unwrap(),
        "old in-store",
        "link must read the in-store file before migration"
    );

    // Migrate the named-target files entry to a repo-relative source.
    repo.cmd()
        .env("HOME", home_path)
        .args(["add", link.to_str().unwrap(), "--source", "shared/hub.txt"])
        .assert()
        .success()
        .stdout(contains("Registered"))
        .stdout(contains("migrated from `files` entry"));

    // State: sources under the inner named target, files entry gone on inner.
    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("[stores.app.targets.inner.sources]"),
        "state must have inner sources section:\n{state}"
    );
    assert!(
        state.contains("shared/hub.txt"),
        "state must contain the new source:\n{state}"
    );
    // The inner target's files entry for config.txt must be gone. The outer
    // target's files = ["base.txt"] must remain undisturbed. The name
    // "config.txt" still appears as the sources map key, so check for the
    // files-list syntax specifically.
    assert!(
        !state.contains(r#"files = ["config.txt"]"#),
        "state must no longer list config.txt as a files entry:\n{state}"
    );
    assert!(
        state.contains(r#"files = ["base.txt"]"#),
        "outer target's files entry must remain:\n{state}"
    );

    // The link was NOT unlinked by the migration.
    assert!(
        link.is_symlink(),
        "link must remain a symlink through the migration"
    );

    // Apply repoints the inner link to the hub; outer is undisturbed.
    repo.cmd()
        .env("HOME", home_path)
        .arg("apply")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&link).unwrap(),
        "hub contents",
        "inner link must read through to the hub after apply"
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        fs::canonicalize(&hub).unwrap(),
        "inner link must point at the hub after apply"
    );

    // JSON reports the migration.
    // (Re-check via dry-run on a fresh setup to inspect the field directly.)
    let repo2 = Repo::new();
    let home2 = tempfile::tempdir().unwrap();
    let hub2 = repo2.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub2.parent().unwrap()).unwrap();
    fs::write(&hub2, "hub").unwrap();
    repo2.make_store("app", &["config.txt"]);
    let inner2 = home2.path().join(".other").join("app");
    fs::create_dir_all(&inner2).unwrap();
    repo2.write_state(&format!(
        r#"
[stores.app.targets.inner]
target = "{}"
files = ["config.txt"]
"#,
        inner2.to_string_lossy(),
    ));
    let link2 = inner2.join("config.txt");
    let output = repo2
        .cmd()
        .env("HOME", home2.path())
        .args([
            "--json",
            "add",
            link2.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_eq!(
        value["data"]["migrated_from_files"], true,
        "named-target migration must report migrated_from_files: true"
    );
}

#[test]
fn add_source_migrates_files_entry_with_noncanonical_fragment() {
    // The migration compares normalized fragments so a `files` entry written
    // as "./AGENTS.txt" (legal, pre-normalization) and a target name of
    // "AGENTS.txt" are treated as the same entry. A regression that compared
    // raw strings would silently fail to drop the files entry, leaving both a
    // files and a sources entry for the same name — which load-time
    // validation would reject on the next load, surfacing as a confusing
    // error rather than the intended migration.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();

    let consumer_dir = repo.make_store("consumer", &["AGENTS.txt"]);
    fs::write(consumer_dir.join("AGENTS.txt"), "old in-store").unwrap();
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    // Note the non-canonical "./AGENTS.txt" files entry.
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["./AGENTS.txt"]
"#,
        consumer_target.to_string_lossy(),
    ));

    let link = consumer_target.join("AGENTS.txt");
    let output = repo
        .cmd()
        .env("HOME", home_path)
        .args([
            "--json",
            "add",
            link.to_str().unwrap(),
            "--source",
            "shared/hub.txt",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_eq!(
        value["data"]["migrated_from_files"], true,
        "non-canonical './AGENTS.txt' files entry must match target name 'AGENTS.txt' after normalization"
    );

    // Real run: the files entry must be dropped and the sources entry inserted.
    repo.cmd()
        .env("HOME", home_path)
        .args(["add", link.to_str().unwrap(), "--source", "shared/hub.txt"])
        .assert()
        .success()
        .stdout(contains("migrated from `files` entry"));

    let state = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert!(
        state.contains("shared/hub.txt"),
        "state must contain the new source:\n{state}"
    );
    // No files entry should remain for AGENTS.txt — neither "./AGENTS.txt"
    // nor "AGENTS.txt". The name still appears as the sources map key, so
    // check for the files-list syntax specifically. The consumer store has
    // no other files entries, so the whole `files = ` line should be absent.
    assert!(
        !state.contains("files = ["),
        "state must no longer have a files list on the consumer store:\n{state}"
    );

    // The state must reload cleanly (no files/sources collision).
    repo.cmd()
        .env("HOME", home_path)
        .arg("doctor")
        .assert()
        .success();
}

#[test]
fn add_source_already_mapped_guard_preserves_files_entries() {
    // The "already mapped" rejection (src/commands/add.rs) fires when a
    // sources entry for the target name already exists with a different
    // source. The guard runs before the files-entry removal step, so a
    // rejection must not partially mutate state — no files entry may be
    // dropped. This test confirms the guard is all-or-nothing in the
    // presence of other files entries on the same store.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path();

    let hub = repo.path().join("shared").join("hub.txt");
    fs::create_dir_all(hub.parent().unwrap()).unwrap();
    fs::write(&hub, "hub").unwrap();
    let other = repo.path().join("shared").join("other.txt");
    fs::write(&other, "other").unwrap();

    repo.make_store("consumer", &["existing.txt"]);
    let consumer_target = home_path.join(".consumer");
    fs::create_dir_all(&consumer_target).unwrap();
    // alias.txt already has a sources entry; existing.txt is a files entry.
    repo.write_state(&format!(
        r#"
[stores.consumer]
target = "{}"
files = ["existing.txt"]

[stores.consumer.sources]
"alias.txt" = "shared/hub.txt"
"#,
        consumer_target.to_string_lossy(),
    ));

    let before = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();

    // Attempt to repoint alias.txt at a different source — must be rejected.
    let alias_target = consumer_target.join("alias.txt");
    repo.cmd()
        .env("HOME", home_path)
        .args([
            "add",
            alias_target.to_str().unwrap(),
            "--source",
            "shared/other.txt",
        ])
        .assert()
        .failure()
        .stderr(contains("already mapped"));

    // State is byte-for-byte unchanged — no partial mutation.
    let after = fs::read_to_string(repo.path().join(".stitch").join("state.toml")).unwrap();
    assert_eq!(
        before, after,
        "rejected migration must not change state.toml"
    );
}
