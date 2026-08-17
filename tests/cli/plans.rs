//! Plan interface — `stitch plan`, `stitch apply --plan`, and JSON output (split from `tests/cli.rs`).
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
        .env("HOME", home.path().as_os_str())
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
    // Composite apply --json: data = {desired, plan, result, post_status}.
    assert!(
        data["desired"]["stores"].is_array(),
        "desired.stores present"
    );
    assert!(data["result"].is_object(), "result present on real apply");
    assert!(data["post_status"].is_array(), "post_status present");
    let plan = &data["plan"];
    let stores = plan["stores"].as_array().expect("plan.stores array");
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
    assert_plan_summary_fields(&plan["summary"]);
    // post_status confirms convergence.
    let post = data["post_status"].as_array().unwrap();
    assert_eq!(post.len(), 1);
    assert_eq!(post[0]["state"], "linked");
}

#[test]
fn json_apply_dry_run_composite_has_null_result() {
    // On --dry-run, the composite `result` field is null (no execution) and
    // `post_status` reflects pre-apply state.
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

    let output = repo
        .cmd()
        .args(["--json", "apply", "--dry-run"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "apply", true);
    let data = &value["data"];
    assert!(data["desired"]["stores"].is_array(), "desired present");
    assert!(data["plan"]["stores"].is_array(), "plan present");
    assert!(data["result"].is_null(), "result is null on dry-run");
    assert!(data["post_status"].is_array(), "post_status present");
    // No link was created.
    assert!(!home.join(".bashrc").exists());
}

#[test]
fn json_apply_composite_desired_matches_explain() {
    // The `desired` field of `apply --json` should match `explain --json`
    // output for the same repo (same platform, same stores).
    let repo = Repo::new();
    repo.make_store("nvim", &["init.lua"]);
    repo.make_store("shells", &[".bashrc"]);
    let home = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.nvim]
target = "{home}/.config/nvim"

[stores.shells]
target = "{home}"
files = [".bashrc"]
"#,
        home = home.to_string_lossy(),
    ));
    repo.write_authored(
        r#"
[stores.shells]
when = { os = "linux" }
"#,
    );

    let apply_output = repo
        .cmd()
        .args(["--json", "apply", "--dry-run"])
        .output()
        .unwrap();
    assert!(apply_output.status.success());
    let apply_value = json_output(&apply_output);
    let explain_output = repo.cmd().args(["--json", "explain"]).output().unwrap();
    assert!(explain_output.status.success());
    let explain_value = json_output(&explain_output);
    // `desired` from apply should equal `data` from explain (same platform,
    // same stores, same resolution).
    assert_eq!(apply_value["data"]["desired"], explain_value["data"]);
}

#[test]
fn json_apply_only_filters_composite_data() {
    // `apply --only foo --json` should only include store `foo` in the
    // composite desired/plan/result/post_status fields.
    let repo = Repo::new();
    repo.make_store("foo", &[".foorc"]);
    repo.make_store("bar", &[".barrc"]);
    let home = repo.path().join("home");
    fs::create_dir_all(&home).unwrap();
    repo.write_state(&format!(
        r#"
[stores.foo]
target = "{home}"
files = [".foorc"]

[stores.bar]
target = "{home}"
files = [".barrc"]
"#,
        home = home.to_string_lossy(),
    ));

    let output = repo
        .cmd()
        .args(["--json", "apply", "--only", "foo"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "apply --only foo --json should succeed"
    );

    let value = json_output(&output);
    assert_envelope_shape(&value, "apply", true);

    let data = &value["data"];
    let desired_stores = data["desired"]["stores"]
        .as_array()
        .expect("desired stores");
    assert_eq!(desired_stores.len(), 1, "desired should only contain foo");
    assert_eq!(desired_stores[0]["name"], "foo");

    let plan_stores = data["plan"]["stores"].as_array().expect("plan stores");
    assert_eq!(plan_stores.len(), 1, "plan should only contain foo");
    assert_eq!(plan_stores[0]["store_name"], "foo");

    let result = data["result"].as_object().expect("result object");
    let result_stores = result["stores"].as_array().expect("result stores");
    assert_eq!(result_stores.len(), 1, "result should only contain foo");
    assert_eq!(result_stores[0]["store"], "foo");

    let post_status = data["post_status"].as_array().expect("post_status");
    assert!(
        post_status.iter().all(|row| row["store"] == "foo"),
        "post_status must only contain store foo"
    );

    // The unselected store should not have been linked.
    assert!(home.join(".foorc").is_symlink(), "foo should be linked");
    assert!(!home.join(".barrc").exists(), "bar should not be linked");
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
fn json_diff_exit_code_reports_plan_and_drift_class() {
    let repo = Repo::new();
    repo.make_store("bashrc", &[".bashrc"]);
    let target = repo.path().join("home");
    repo.write_state(&format!(
        r#"
[stores.bashrc]
target = "{}"
files = [".bashrc"]
"#,
        target.to_string_lossy(),
    ));

    let output = repo
        .cmd()
        .args(["--json", "diff", "--exit-code"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(14));
    let value = json_output(&output);
    assert_envelope_shape(&value, "diff", false);
    assert_error_shape(&value, "drift", 14);
    assert_eq!(value["data"]["summary"]["created"], 1);
    assert!(!target.join(".bashrc").exists());
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
    // Composite apply --json: data = {desired, plan, result, post_status}.
    assert!(
        data["desired"]["stores"].is_array(),
        "desired.stores present"
    );
    assert!(data["result"].is_object(), "result present on real apply");
    assert!(data["post_status"].is_array(), "post_status present");
    let plan = &data["plan"];
    let summary = &plan["summary"];
    assert_plan_summary_fields(summary);
    assert_eq!(summary["conflicts"].as_u64().unwrap(), 1);
    let stores = plan["stores"].as_array().unwrap();
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
fn json_supported_on_mutation_commands() {
    // `add` and `remove` both support `--json` for real mutations (not just
    // dry-run). The post-op envelope goes to stdout (one-stream rule) with
    // an honest exit code.
    let repo = Repo::new();

    // Dry-run add is supported and returns its normal envelope.
    let target = repo.path().join("home").join(".config").join("x");
    let output = repo
        .cmd()
        .args(["--json", "add", &target.to_string_lossy(), "--dry-run"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);
    assert_eq!(value["data"]["mode"], "create");

    // Real add is also supported and reports the created link.
    let output = repo
        .cmd()
        .args(["--json", "add", &target.to_string_lossy()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);
    assert!(value["data"]["link_created"].is_string());

    // Real remove is supported and reports the removed link.
    let output = repo.cmd().args(["--json", "remove", "x"]).output().unwrap();
    assert!(output.status.success(), "remove --json must succeed");
    let value = json_output(&output);
    assert_envelope_shape(&value, "remove", true);
    assert_eq!(value["data"]["store"], "x");
    assert!(value["data"]["behavior_orphaned"].is_boolean());
}

#[test]
fn add_json_post_op_reports_created_link() {
    // `add --json` without --dry-run emits a post-op report with the created
    // symlink path, so an agent can verify the mutation without a second call.
    let repo = Repo::new();
    let target = repo.path().join("home").join(".config").join("x");
    let output = repo
        .cmd()
        .args(["--json", "add", &target.to_string_lossy()])
        .output()
        .unwrap();
    assert!(output.status.success(), "add --json must succeed");
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);
    assert_eq!(value["data"]["mode"], "create");
    assert!(
        value["data"]["link_created"].is_string(),
        "link_created must be present"
    );
    // The link should actually exist at the reported path.
    let link = value["data"]["link_created"].as_str().unwrap();
    assert!(
        std::path::Path::new(link).is_symlink(),
        "reported link must exist"
    );
}

#[test]
fn add_json_post_op_adopt_reports_moved_source() {
    // `add --json` on an existing file (adopt) reports the mode as "adopt"
    // and the original source path, so an agent can verify the move.
    let repo = Repo::new();
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join(".gitconfig");
    fs::write(&target, "[user]\nname = test\n").unwrap();

    let output = repo
        .cmd()
        .args(["--json", "add", &target.to_string_lossy()])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "add --json adopt must succeed");
    let value = json_output(&output);
    assert_envelope_shape(&value, "add", true);
    assert_eq!(value["data"]["mode"], "adopt");
    assert!(
        value["data"]["source"].is_string(),
        "source must be present for adopt"
    );
    assert!(
        value["data"]["link_created"].is_string(),
        "link_created must be present"
    );
    // The target is now a symlink.
    assert!(target.is_symlink(), "target must be a symlink after adopt");
}

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

/// When a template is removed from state, `stitch plan` captures a `remove_link`
/// before `remove_staged`, so the render stays readable until its live link is gone.
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
    let remove_link_idx = ops
        .iter()
        .position(|o| {
            o["op"] == "remove_link"
                && o["target"] == home.join("gitconfig").to_string_lossy().as_ref()
        })
        .expect("plan must remove the stale target link");
    let remove_staged_idx = ops
        .iter()
        .position(|o| o["op"] == "remove_staged" && o["rel"] == "gitconfig")
        .expect("plan must remove the stale render");
    assert!(
        remove_link_idx < remove_staged_idx,
        "stale link removal must precede staging cleanup"
    );

    // Omitting the dependent link removal must fail before hooks or deletion.
    let hook_marker = repo.path().join("cleanup-hook-ran");
    let hook = repo.path().join(".stitch/hooks/pre-apply");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        format!("#!/bin/sh\ntouch {}\n", hook_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let mut omitted = plan.clone();
    omitted["ops"]
        .as_array_mut()
        .unwrap()
        .retain(|op| op["op"] != "remove_link");
    let omitted_path = repo.path().join("omitted-link-plan.json");
    fs::write(&omitted_path, serde_json::to_vec(&omitted).unwrap()).unwrap();
    repo.cmd()
        .args(["apply", "--plan", omitted_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("requires preceding remove_link"));
    assert!(staged.exists());
    assert_eq!(fs::read_to_string(home.join("gitconfig")).unwrap(), "x\n");
    assert!(!hook_marker.exists(), "validation must run before hooks");

    // Reordering staging cleanup before its link removal is equally unsafe.
    let mut reordered = plan.clone();
    let reordered_ops = reordered["ops"].as_array_mut().unwrap();
    let staged_op = reordered_ops.remove(remove_staged_idx);
    let link_idx = reordered_ops
        .iter()
        .position(|op| op["op"] == "remove_link")
        .unwrap();
    reordered_ops.insert(link_idx, staged_op);
    let reordered_path = repo.path().join("reordered-cleanup-plan.json");
    fs::write(&reordered_path, serde_json::to_vec(&reordered).unwrap()).unwrap();
    repo.cmd()
        .args(["apply", "--plan", reordered_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("omitted or reordered"));
    assert!(staged.exists());
    assert_eq!(fs::read_to_string(home.join("gitconfig")).unwrap(), "x\n");
    assert!(!hook_marker.exists(), "validation must run before hooks");

    // Remove the marker hook before executing the original valid plan.
    fs::remove_file(hook).unwrap();
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
    fs::create_dir_all(&home).unwrap();
    // The store target itself is a symlink to an external path. The plan
    // builder must see the symlinked ancestor and refuse to create a link
    // beneath it. The target is still inside $HOME, so config validation passes
    // and the conflict is the plan's responsibility.
    std::os::unix::fs::symlink(external.path(), &config).unwrap();
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
                (conflict["kind"] == "foreign_symlink" || conflict["kind"] == "symlink_ancestor")
                    && conflict["target"] == config.to_string_lossy().as_ref()
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
fn apply_plan_rejects_platform_skipped_store_before_hooks_or_mutations() {
    // A hand-edited plan that injects a platform-skipped store must abort
    // before the global pre-apply hook and before any earlier store creates a
    // link. This guards the broader "abort before side effects" property; the
    // plan is rejected by `validate_op` before the platform-skip scan is reached.
    let repo = Repo::new();
    let _matched_store = repo.make_store("matched", &["profile"]);
    let skipped_store = repo.make_store("skipped", &["profile"]);
    let matched_target = repo.path().join("home").join("matched");
    let skipped_target = repo.path().join("home").join("skipped");
    fs::create_dir_all(&matched_target).unwrap();
    fs::create_dir_all(&skipped_target).unwrap();
    repo.write_state(&format!(
        r#"
[stores.matched]
target = "{}"
files = ["profile"]

[stores.skipped]
target = "{}"
files = ["profile"]
"#,
        matched_target.display(),
        skipped_target.display(),
    ));
    repo.write_authored("\n[stores.skipped]\nwhen = { os = \"macos\" }\n");

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut plan: Value = serde_json::from_slice(&output.stdout).unwrap();

    // Hand-edit the plan to select the platform-skipped store and add its op.
    let skipped_source = skipped_store.join("profile").display().to_string();
    let skipped_target_file = skipped_target.join("profile").display().to_string();
    let mut ops = plan["ops"].as_array().unwrap().clone();
    ops.push(serde_json::json!({
        "op": "create_link",
        "target": skipped_target_file,
        "source": skipped_source,
        "requires": { "target": "absent" }
    }));
    plan["ops"] = serde_json::Value::Array(ops);
    plan["stores"] = serde_json::json!(["matched", "skipped"]);

    // Install a global pre-apply hook that would mark the filesystem if it ran.
    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let marker = repo.path().join("hook-ran");
    let hook = hooks_dir.join("pre-apply");
    fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    let mut perms = fs::metadata(&hook).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).unwrap();

    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains(
            "link operation is not present in the freshly computed apply plan",
        ));

    assert!(!marker.exists(), "global pre-apply hook must not run");
    assert!(
        !matched_target.join("profile").is_symlink(),
        "earlier matched store must not create a link"
    );
    assert!(
        !skipped_target.join("profile").exists(),
        "skipped store must not create a link"
    );
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

/// `apply --plan` also pins target ancestors across the global pre-apply hook.
/// A real-directory replacement (not only a symlink) must be rejected.
#[test]
fn apply_plan_rejects_global_hook_real_dir_ancestor_redirect() {
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

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&plan_path, &output.stdout).unwrap();

    let hooks_dir = repo.path().join(".stitch").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-apply");
    fs::write(
        &hook,
        "#!/bin/sh\nset -e\nrm -rf \"$HOME/.config\"\nmv \"$HOME/.ssh\" \"$HOME/.config\"\n",
    )
    .unwrap();
    make_executable(&hook);

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("changed identity"));

    assert!(
        !repo.path().join(".config").join("app").join("f").exists(),
        "apply --plan must not write through the redirected real directory"
    );
}

/// `apply --plan` also pins target ancestors across a per-store pre-hook.
#[test]
fn apply_plan_rejects_store_hook_real_dir_ancestor_redirect() {
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
hooks = { pre = "rm -rf $HOME/.config && mv $HOME/.ssh $HOME/.config" }
"#,
    );

    let plan_path = repo.path().join("plan.json");
    let output = repo.cmd().arg("plan").output().unwrap();
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&plan_path, &output.stdout).unwrap();

    repo.cmd()
        .args(["apply", "--plan", plan_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(12)
        .stderr(contains("changed identity"));

    assert!(
        !repo.path().join(".config").join("app").join("f").exists(),
        "apply --plan must not write through the per-store hook redirect"
    );
}
