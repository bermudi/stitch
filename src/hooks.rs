//! Hook execution — per-store `pre`/`post` shell commands and global hook
//! executables in `.stitch/hooks/`.
//!
//! Hooks are user-configured shell commands run with the user's own privileges
//! (like git hooks). Per-store hooks wrap a store's apply lifecycle; global
//! hooks wrap the whole `apply`/`remove` operation. Every hook receives
//! `STITCH_*` environment variables for context.

use crate::platform::Platform;
use std::path::Path;
use std::process::{Command, Stdio};

/// Context injected as `STITCH_*` env vars into every hook process.
pub struct HookEnv<'a> {
    pub root: &'a Path,
    pub store: Option<&'a str>,
    pub target: Option<&'a str>,
    pub action: &'a str,
}

/// Set `STITCH_*` env vars on `cmd`, plus platform vars. Inherits the rest of
/// the environment (PATH, HOME, ...) so hooks behave like normal shell commands.
fn inject_env(cmd: &mut Command, env: &HookEnv, platform: &Platform) {
    cmd.env("STITCH_ROOT", env.root);
    cmd.env("STITCH_ACTION", env.action);
    if let Some(store) = env.store {
        cmd.env("STITCH_STORE", store);
    }
    if let Some(target) = env.target {
        cmd.env("STITCH_TARGET", target);
    }
    cmd.env("STITCH_OS", &platform.os);
    cmd.env("STITCH_ARCH", &platform.arch);
    cmd.env("STITCH_HOSTNAME", &platform.hostname);
    cmd.env("STITCH_SHELL", &platform.shell);
    if let Some(ref distro) = platform.distro {
        cmd.env("STITCH_DISTRO", distro);
    }
}

/// Format a non-zero exit status (code or signal).
fn fmt_status(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into())
}

/// When `json` is true, redirect the hook's stdout to stderr so hook output
/// cannot corrupt the machine-readable JSON envelope on stdout. Stderr is
/// still inherited so the user/agent sees hook messages.
fn isolate_stdout_if_json(cmd: &mut Command, json: bool) {
    if json {
        // Open /dev/stderr as a File and use it as the hook's stdout. On
        // Linux /dev/stderr is a symlink to /proc/self/fd/2.
        if let Ok(stderr_file) = std::fs::OpenOptions::new().write(true).open("/dev/stderr") {
            cmd.stdout(Stdio::from(stderr_file));
        }
    }
}

/// Run a per-store hook: a shell command string from config, via `sh -c`.
///
/// Returns `Ok(())` on exit 0, `Err(message)` on non-zero exit or failure to
/// launch. The caller decides policy: pre-hook failure aborts the store,
/// post-hook failure is a warning.
///
/// When `json` is true, the hook's stdout is redirected to stderr so it
/// cannot corrupt the JSON envelope.
pub fn run_store_hook(
    command: &str,
    env: &HookEnv,
    platform: &Platform,
    json: bool,
) -> Result<(), String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    inject_env(&mut cmd, env, platform);
    isolate_stdout_if_json(&mut cmd, json);
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "hook '{command}' exited with {}",
            fmt_status(status)
        )),
        Err(e) => Err(format!("hook '{command}' failed to execute: {e}")),
    }
}

/// Run a global hook executable at `.stitch/hooks/<name>`, if it exists.
///
/// Returns `Ok(true)` if it existed and succeeded, `Ok(false)` if no such hook
/// file is present (nothing to run), `Err(message)` if it existed and failed.
/// The hook must be executable (`chmod +x`) — like git hooks, stitch does not
/// auto-chmod.
///
/// When `json` is true, the hook's stdout is redirected to stderr so it
/// cannot corrupt the JSON envelope.
pub fn run_global_hook(
    root: &Path,
    name: &str,
    env: &HookEnv,
    platform: &Platform,
    json: bool,
) -> Result<bool, String> {
    let hook_path = root.join(".stitch").join("hooks").join(name);
    if !hook_path.exists() {
        return Ok(false);
    }
    let mut cmd = Command::new(&hook_path);
    inject_env(&mut cmd, env, platform);
    isolate_stdout_if_json(&mut cmd, json);
    match cmd.status() {
        Ok(status) if status.success() => Ok(true),
        Ok(status) => Err(format!(
            "global hook '{}' exited with {}",
            hook_path.display(),
            fmt_status(status)
        )),
        Err(e) => Err(format!(
            "global hook '{}' failed to execute: {e}",
            hook_path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform() -> Platform {
        Platform::detect()
    }

    #[test]
    fn store_hook_success() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let env = HookEnv {
            root: tmp.path(),
            store: Some("s"),
            target: None,
            action: "apply",
        };
        let cmd = format!("touch {}", marker.display());
        run_store_hook(&cmd, &env, &platform(), false).unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn store_hook_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HookEnv {
            root: tmp.path(),
            store: Some("s"),
            target: None,
            action: "apply",
        };
        let err = run_store_hook("exit 1", &env, &platform(), false).unwrap_err();
        assert!(err.contains("exited with 1"), "got: {err}");
    }

    #[test]
    fn store_hook_receives_env_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let outfile = tmp.path().join("env.txt");
        let env = HookEnv {
            root: tmp.path(),
            store: Some("mystore"),
            target: Some("/target/path"),
            action: "apply",
        };
        let cmd = format!("env | grep ^STITCH > {}", outfile.display());
        run_store_hook(&cmd, &env, &platform(), false).unwrap();
        let captured = std::fs::read_to_string(&outfile).unwrap();
        assert!(captured.contains("STITCH_STORE=mystore"), "got: {captured}");
        assert!(
            captured.contains("STITCH_TARGET=/target/path"),
            "got: {captured}"
        );
        assert!(captured.contains("STITCH_ACTION=apply"), "got: {captured}");
        assert!(
            captured.contains(&format!("STITCH_ROOT={}", tmp.path().display())),
            "got: {captured}"
        );
        assert!(captured.contains("STITCH_OS="), "got: {captured}");
    }

    #[test]
    fn global_hook_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HookEnv {
            root: tmp.path(),
            store: None,
            target: None,
            action: "apply",
        };
        assert!(!run_global_hook(tmp.path(), "pre-apply", &env, &platform(), false).unwrap());
    }

    #[test]
    fn global_hook_runs_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join(".stitch").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let marker = tmp.path().join("ran");
        let hook = hooks_dir.join("pre-apply");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        set_executable(&hook);

        let env = HookEnv {
            root: tmp.path(),
            store: None,
            target: None,
            action: "apply",
        };
        assert!(run_global_hook(tmp.path(), "pre-apply", &env, &platform(), false).unwrap());
        assert!(marker.exists());
    }

    #[test]
    fn global_hook_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join(".stitch").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook = hooks_dir.join("post-apply");
        std::fs::write(&hook, "#!/bin/sh\nexit 3\n").unwrap();
        set_executable(&hook);

        let env = HookEnv {
            root: tmp.path(),
            store: None,
            target: None,
            action: "apply",
        };
        let err = run_global_hook(tmp.path(), "post-apply", &env, &platform(), false).unwrap_err();
        assert!(err.contains("exited with 3"), "got: {err}");
    }

    fn set_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}
