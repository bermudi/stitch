//! Path helpers: home expansion, root discovery, fragment/name validation,
//! and target path normalization.

use std::path::{Component, Path, PathBuf};

use super::error::ConfigError;

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static TEST_HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Override `$HOME` for the current thread during unit tests. This avoids
/// unsynchronized environment-variable mutation and lets tests that place
/// targets outside the real home directory run safely in parallel.
#[cfg(test)]
pub(crate) fn set_test_home(home: Option<PathBuf>) {
    TEST_HOME.with(|h| *h.borrow_mut() = home);
}

#[cfg(test)]
pub(crate) struct TestHomeGuard;

#[cfg(test)]
impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        set_test_home(None);
    }
}

/// Set the test `$HOME` for the current thread and clear it when the guard
/// is dropped.
#[cfg(test)]
pub(crate) fn test_home_guard(home: PathBuf) -> TestHomeGuard {
    set_test_home(Some(home));
    TestHomeGuard
}

fn home_dir() -> Result<PathBuf, ConfigError> {
    #[cfg(test)]
    {
        if let Some(home) = TEST_HOME.with(|h| h.borrow().clone()) {
            return Ok(home);
        }
    }
    match std::env::var("HOME") {
        Ok(value) if value.is_empty() => Err(ConfigError::Home(
            "$HOME is set to an empty string; stitch needs $HOME to resolve targets.".into(),
        )),
        Ok(value) => {
            let path = PathBuf::from(value);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => Ok(path),
                Ok(_) => Err(ConfigError::Home(format!(
                    "$HOME '{}' is not a directory; stitch needs $HOME to resolve targets.",
                    path.display()
                ))),
                Err(_) => Err(ConfigError::Home(format!(
                    "$HOME '{}' does not exist; stitch needs $HOME to resolve targets.",
                    path.display()
                ))),
            }
        }
        Err(_) => Err(ConfigError::Home(
            "$HOME is not set; stitch needs $HOME to resolve targets.".into(),
        )),
    }
}

/// Expand `~` at the start of a path.
pub fn expand_home(path: &str) -> Result<PathBuf, ConfigError> {
    let raw = if let Some(rest) = path.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else if path == "~" {
        home_dir()?
    } else {
        PathBuf::from(path)
    };
    // Strip trailing slashes: symlink(2) fails with ENOENT when the linkpath
    // has a trailing slash (the kernel treats it as "must resolve to a
    // directory", but the path doesn't exist yet when we're creating a link).
    // User input like `stitch add ~/.config/alacritty/` would otherwise fail
    // at the link step with a confusing rollback error.
    let mut s = raw.to_string_lossy().into_owned();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    Ok(PathBuf::from(s))
}

/// Walk upward from `start` to find a directory containing `.stitch/`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };

    loop {
        if current.join(".stitch").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Whether `fragment` is safe to join onto a store or target directory.
///
/// Safe means: non-empty, relative (no leading `/`), and containing only
/// normal path components and harmless current-directory (`./`) components.
/// `..`, a leading `/`, and a bare `.` are rejected. Nested paths like
/// `config/app.conf` and `./bashrc` are allowed; `.` is rejected because it
/// normalizes to an empty path. The check is lexical — it inspects
/// [`Path::components`] without touching the filesystem, so it is TOCTOU-free
/// and accepts entries for files that do not exist yet.
pub fn is_safe_fragment(fragment: &str) -> bool {
    if fragment.is_empty() {
        return false;
    }
    let path = Path::new(fragment);
    if path.is_absolute() {
        return false;
    }
    let mut has_normal = false;
    for c in path.components() {
        match c {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            _ => return false,
        }
    }
    has_normal
}

/// Whether `name` is exactly one normal path component. Store names become
/// repo directory names, so unlike file fragments they may not be nested.
pub fn is_store_name(name: &str) -> bool {
    if name.is_empty() || name.contains('/') || matches!(name, ".stitch" | ".git") {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Canonical (symlink-following) form of `$HOME`. Config-time target
/// validation and apply-time confinement both use it: a target's ancestors
/// must resolve inside this path even after a hook replaced them.
pub(crate) fn canonical_home() -> Result<PathBuf, ConfigError> {
    normalized_target_path("~")
}

pub(crate) fn normalized_target_path(target: &str) -> Result<PathBuf, ConfigError> {
    let expanded = expand_home(target)?;
    if let Some(resolved) = crate::linker::resolve_path_with_missing(&expanded) {
        return Ok(resolved);
    }
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

pub(super) fn validate_store_names<'a>(
    names: impl Iterator<Item = &'a String>,
    source: &str,
) -> Result<(), ConfigError> {
    for name in names {
        if !is_store_name(name) {
            return Err(ConfigError::InvalidPath(format!(
                "invalid store name '{name}' in {source}: store names must be exactly one normal path component"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_expand_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let _guard = test_home_guard(home.clone());
        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("~/foo/bar").unwrap(), home.join("foo/bar"));
        assert_eq!(
            expand_home("/absolute/path").unwrap(),
            PathBuf::from("/absolute/path")
        );
        // Trailing slashes are stripped — symlink(2) fails on a linkpath
        // with a trailing slash, so `stitch add ~/.config/foo/` must not
        // carry the slash through to the linker.
        assert_eq!(expand_home("~/foo/").unwrap(), home.join("foo"));
        assert_eq!(expand_home("~/foo///").unwrap(), home.join("foo"));
        assert_eq!(
            expand_home("/absolute/path/").unwrap(),
            PathBuf::from("/absolute/path")
        );
        // Root stays root — the `len() > 1` guard prevents stripping "/" to "".
        assert_eq!(expand_home("/").unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn test_find_root() {
        let tmp = tempfile::tempdir().unwrap();
        let stitch_dir = tmp.path().join(".stitch");
        std::fs::create_dir_all(&stitch_dir).unwrap();

        assert_eq!(find_root(tmp.path()), Some(tmp.path().to_path_buf()));

        let sub = tmp.path().join("some").join("nested").join("dir");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_root(&sub), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn test_is_store_name() {
        assert!(is_store_name("shells"));
        assert!(is_store_name(".hidden"));
        for invalid in [
            "",
            ".",
            "..",
            ".git",
            ".stitch",
            "nested/name",
            "nested/",
            "/absolute",
        ] {
            assert!(!is_store_name(invalid), "{invalid:?} must be rejected");
        }
    }

    #[test]
    fn test_is_safe_fragment() {
        assert!(is_safe_fragment(".bashrc"));
        assert!(is_safe_fragment("config/app.conf"));
        assert!(is_safe_fragment("./bashrc"));
        assert!(is_safe_fragment("bashrc"));
        assert!(is_safe_fragment("foo/./bar"));
        assert!(is_safe_fragment("././bashrc"));
        assert!(!is_safe_fragment(""));
        assert!(!is_safe_fragment("/"));
        assert!(!is_safe_fragment("/etc/passwd"));
        assert!(!is_safe_fragment("."));
        assert!(!is_safe_fragment(".."));
        assert!(!is_safe_fragment("../escape"));
        assert!(!is_safe_fragment("foo/../bar"));
        assert!(!is_safe_fragment("ok/../../escape"));
    }

    #[test]
    fn test_is_safe_fragment_rejects_dot() {
        assert!(!is_safe_fragment("."));
        assert!(!is_safe_fragment("./."));
        assert!(!is_safe_fragment("././"));
        assert!(is_safe_fragment("foo/./bar"));
        assert!(is_safe_fragment("gitconfig"));
        assert!(is_safe_fragment("lua/plugin.lua"));
        assert!(is_safe_fragment("./bashrc"));
        assert!(is_safe_fragment("././bashrc"));
    }

    proptest! {
        #[test]
        fn prop_store_name_implies_safe(s in "[a-zA-Z0-9._-]{1,20}") {
            // Single-component safe names without slash — subset of safe fragments
            // Exclude reserved names that are rejected as store names but are safe fragments
            if s == ".stitch" || s == ".git" || s == "." || s == ".." {
                prop_assert!(!is_store_name(&s));
            } else {
                prop_assert!(is_store_name(&s));
                prop_assert!(is_safe_fragment(&s), "store name must be a safe fragment");
            }
        }

        #[test]
        fn prop_store_name_rejects_slash(s in "[a-z]+/[a-z]+") {
            prop_assert!(!is_store_name(&s), "store name with slash must be rejected: {s}");
        }
    }
}
