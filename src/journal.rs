//! Render journal — attribution for the tool-owned render tree.
//!
//! stitch owns `.stitch/render/`, but users reach it through target symlinks:
//! `$EDITOR ~/.zcode/AGENTS.md` edits the staged render directly. Without a
//! record of what stitch itself last wrote, apply cannot distinguish "sources
//! changed since the last render" (safe to re-render) from "someone edited the
//! staged file" — and overwriting a hand-edit destroys its only copy, because
//! the render tree is required to be gitignored.
//!
//! The journal maps `<store> → <link> → sha256` of the content stitch last
//! wrote there. [`crate::render::stage_template`] refuses to replace a staged
//! file that no longer matches its journal entry; entries written by stitch
//! (or missing entirely, for pre-journal repos) re-render as before.
//!
//! Crash semantics: the journal is written *after* the staged file it
//! describes. A crash between the two leaves a stale or missing entry, which
//! the next apply treats as unattributable and resolves by trusting sources —
//! the exact pre-journal behavior — then journals the result. The journal
//! therefore never blocks convergence; it only refuses *unattributed*
//! overwrites, which is the safe direction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::render;

/// store name → (staging link name → sha256 hex of last stitch-written content)
type Journal = BTreeMap<String, BTreeMap<String, String>>;

/// Journal location: the render **root**, never inside a store subtree.
/// Staging sweeps (`reconcile_store_staging`, `remove_store_staging`) prune
/// store subtrees, so anything below `.stitch/render/<store>/` would be
/// deleted as stale; the root itself is never swept and its contents are
/// covered by the mandatory `.stitch/render/` gitignore entry.
pub fn journal_path(repo_root: &Path) -> PathBuf {
    render::render_root(repo_root).join(".journal.toml")
}

/// sha256 of rendered content as lowercase hex. Renders are UTF-8 strings by
/// the time they reach the journal (invalid UTF-8 staged content is refused
/// earlier, by the staging read).
pub fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Load the journal. `Ok(None)` when absent. A corrupt or non-regular journal
/// is an error, not a silent reset: stitch must not guess between "trust the
/// staged file" and "trust sources" when its own record is unreliable.
fn load(repo_root: &Path) -> Result<Option<Journal>, String> {
    let path = journal_path(repo_root);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "could not inspect render journal {}: {e}",
            path.display()
        )),
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "refusing symlinked render journal {}",
            path.display()
        )),
        Ok(meta) if !meta.file_type().is_file() => Err(format!(
            "render journal {} is not a regular file",
            path.display()
        )),
        Ok(_) => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read render journal {}: {e}", path.display()))?;
            toml::from_str(&raw).map(Some).map_err(|e| {
                format!(
                    "render journal {} is unreadable ({e}) — delete the file to rebuild it; \
                     the next apply re-renders and re-journals every entry",
                    path.display()
                )
            })
        }
    }
}

/// Atomically replace the journal (temp file + rename, mode `0600`). The
/// render root must already exist as a real directory: `record` runs right
/// after a staged write created/validated it, and a vanished root at this
/// point is an error worth surfacing, not a silent skip.
fn save(repo_root: &Path, journal: &Journal) -> Result<(), String> {
    let path = journal_path(repo_root);
    let dir = path
        .parent()
        .ok_or_else(|| format!("journal path has no parent: {}", path.display()))?;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => return Err(format!("render root {} is not a directory", dir.display())),
        Err(e) => {
            return Err(format!(
                "render root {} disappeared before writing the journal: {e}",
                dir.display()
            ));
        }
    }
    // A symlinked or special journal leaf is refused rather than replaced:
    // the read side refuses it too, and silently swapping it would mask a
    // hostile or accidental substitution.
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing symlinked render journal {}",
                path.display()
            ));
        }
        if !meta.file_type().is_file() {
            return Err(format!(
                "render journal {} is not a regular file",
                path.display()
            ));
        }
    }

    let (mut file, tmp_path) = render::create_secure_temp(dir)?;
    let result = (|| -> Result<(), String> {
        use std::io::Write;
        let contents = toml::to_string_pretty(journal)
            .map_err(|e| format!("could not serialize render journal: {e}"))?;
        file.write_all(contents.as_bytes())
            .map_err(|e| format!("could not write {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("could not fsync {}: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .map_err(|e| format!("could not rename journal into {}: {e}", path.display()))
    })();
    if result.is_err() {
        // Exclusively-created random temp name; cleanup is best-effort so the
        // original error survives.
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Write the journal, or delete it when the last entry is gone (an empty
/// journal file would otherwise keep `has_staged_output`-style checks
/// confused about whether the render tree holds anything).
fn save_or_delete(repo_root: &Path, journal: Journal) -> Result<(), String> {
    if journal.is_empty() {
        match std::fs::remove_file(journal_path(repo_root)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!(
                "could not remove empty render journal {}: {e}",
                journal_path(repo_root).display()
            )),
        }
    } else {
        save(repo_root, &journal)
    }
}

/// The sha256 stitch last wrote for `<store>/<link>`, if any. `Ok(None)`
/// covers both "no journal" and "no entry" — pre-journal repos and stores
/// staged before this record existed, which callers treat as unattributable
/// but trusted (legacy behavior).
pub fn expected(
    repo_root: &Path,
    store_name: &str,
    link_rel: &str,
) -> Result<Option<String>, String> {
    Ok(load(repo_root)?
        .and_then(|journal| journal.get(store_name).cloned())
        .and_then(|entries| entries.get(link_rel).cloned()))
}

/// Record the hash of content stitch just wrote (or confirmed in place).
/// No-op when the entry already matches, so steady-state applies never
/// rewrite the journal.
pub fn record(
    repo_root: &Path,
    store_name: &str,
    link_rel: &str,
    sha_hex: &str,
) -> Result<(), String> {
    let mut journal = load(repo_root)?.unwrap_or_default();
    let entry = journal.entry(store_name.to_string()).or_default();
    if entry.get(link_rel) == Some(&sha_hex.to_string()) {
        return Ok(());
    }
    entry.insert(link_rel.to_string(), sha_hex.to_string());
    save(repo_root, &journal)
}

/// Drop one entry (called when its staged file is removed). Missing file or
/// entry is a no-op — forgetting something absent is already true.
pub fn forget(repo_root: &Path, store_name: &str, link_rel: &str) -> Result<(), String> {
    let Some(mut journal) = load(repo_root)? else {
        return Ok(());
    };
    let Some(entries) = journal.get_mut(store_name) else {
        return Ok(());
    };
    if entries.remove(link_rel).is_none() {
        return Ok(());
    }
    if entries.is_empty() {
        journal.remove(store_name);
    }
    save_or_delete(repo_root, journal)
}

/// Drop every entry for a store (called when its whole staging tree is
/// removed, e.g. by `stitch remove`).
pub fn forget_store(repo_root: &Path, store_name: &str) -> Result<(), String> {
    let Some(mut journal) = load(repo_root)? else {
        return Ok(());
    };
    if journal.remove(store_name).is_none() {
        return Ok(());
    }
    save_or_delete(repo_root, journal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(render::render_root(tmp.path())).unwrap();
        tmp
    }

    #[test]
    fn record_expected_roundtrip() {
        let tmp = repo();
        let root = tmp.path();
        assert_eq!(expected(root, "a", "f").unwrap(), None);
        record(root, "a", "f", "h1").unwrap();
        assert_eq!(expected(root, "a", "f").unwrap().as_deref(), Some("h1"));
        assert_eq!(expected(root, "a", "g").unwrap(), None);
        assert_eq!(expected(root, "b", "f").unwrap(), None);
    }

    #[test]
    fn nested_link_names_survive_the_toml_roundtrip() {
        let tmp = repo();
        let root = tmp.path();
        record(root, "store", "nested/dir/file.cfg", "h1").unwrap();
        assert_eq!(
            expected(root, "store", "nested/dir/file.cfg")
                .unwrap()
                .as_deref(),
            Some("h1")
        );
    }

    #[test]
    fn record_is_stable_when_unchanged() {
        let tmp = repo();
        let root = tmp.path();
        record(root, "a", "f", "h1").unwrap();
        let before = std::fs::metadata(journal_path(root))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        record(root, "a", "f", "h1").unwrap();
        let after = std::fs::metadata(journal_path(root))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            before, after,
            "unchanged record must not rewrite the journal"
        );
    }

    #[test]
    fn forget_and_forget_store() {
        let tmp = repo();
        let root = tmp.path();
        record(root, "a", "f1", "h1").unwrap();
        record(root, "a", "f2", "h2").unwrap();
        record(root, "b", "f3", "h3").unwrap();

        forget(root, "a", "f1").unwrap();
        assert_eq!(expected(root, "a", "f1").unwrap(), None);
        assert_eq!(expected(root, "a", "f2").unwrap().as_deref(), Some("h2"));

        forget_store(root, "a").unwrap();
        assert_eq!(expected(root, "a", "f2").unwrap(), None);
        assert_eq!(expected(root, "b", "f3").unwrap().as_deref(), Some("h3"));

        // Last entry gone → the journal file itself is deleted.
        forget_store(root, "b").unwrap();
        assert!(!journal_path(root).exists());
    }

    #[test]
    fn forget_on_missing_state_is_a_noop() {
        let tmp = repo();
        let root = tmp.path();
        forget(root, "a", "f").unwrap();
        forget_store(root, "a").unwrap();
        assert!(!journal_path(root).exists());
    }

    #[test]
    fn corrupt_journal_is_loud() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(journal_path(root), "{{not toml").unwrap();
        let err = expected(root, "a", "f").unwrap_err();
        assert!(err.contains("unreadable"), "got: {err}");
        assert!(err.contains("delete"), "recovery hint missing: {err}");
    }

    #[test]
    fn symlinked_journal_is_refused() {
        let tmp = repo();
        let root = tmp.path();
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "").unwrap();
        std::os::unix::fs::symlink(&outside, journal_path(root)).unwrap();
        assert!(expected(root, "a", "f").unwrap_err().contains("symlinked"));
        assert!(
            record(root, "a", "f", "h")
                .unwrap_err()
                .contains("symlinked")
        );
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex("stitch"),
            "864a42c4bf81be054d34139817600779e814487f600650fa4636d8bbd127ecb4"
        );
    }
}
