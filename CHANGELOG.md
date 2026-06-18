# Changelog

## 0.2.0 — 2026-06-17

First trust-worthy release. All P0/P1/P2 issues from the 2026-06 trust review
resolved. Oracle #2 re-review confirmed: "would personally use for real Linux
dotfiles now."

### Trust & safety

- **No external uploads.** Old gist/snapshot machinery removed. No network calls
  in any command.
- **Foreign symlinks are always conflicts.** Symlinks not pointing into the
  stitch repo are never silently clobbered — even under `--force`.
- **`apply --force` creates `.bak` backups.** Real file/dir conflicts are renamed
  to `{target}.bak` before linking. Refuses to overwrite an existing `.bak`.
- **`adopt` is atomic.** Pre-checks collisions, moves files into the repo, links
  back, and rolls back on any failure.
- **`add` is atomic.** Applies in-memory before persisting config; rolls back on
  conflict or config-save failure.
- **Path traversal validated.** Absolute and `..`-containing file/pattern entries
  are rejected at config load and CLI input.
- **Honest exit codes.** `apply`, `adopt`, `add`, `doctor`, `diff`, `status` all
  return non-zero on errors, conflicts, or unknown store names.
- **Atomic config writes.** `Config::save()` writes to temp file then `rename(2)`
  — no truncation/corruption window.
- **Symlink ownership hardening.** `points_into_repo()` normalizes `..` components
  before checking whether a symlink belongs to this repo.

### Features

- `init`, `apply`, `status`, `diff`, `list`, `doctor`
- `adopt`, `add`, `remove`, `edit`
- Whole-directory and file mode (with recursive glob via `walkdir`)
- Platform conditionals (`when`: os, arch, distro, hostname, shell)
- Multi-target stores
- Per-store hooks (`pre`/`post`) + global `.stitch/hooks/` executables
- Global ignores (`.git`, `.stitch`, `.gitignore`, `.DS_Store`) + per-store
  ignore patterns with directory exclusion
- Whole-dir → file-mode promotion when ignored content exists in the store

### Testing

106 tests (36 unit + 70 CLI), all passing. `cargo fmt` and `cargo clippy -D
warnings` clean.

## 0.1.0 — 2026-06-15

Initial release. Core symlink engine, config parsing, basic commands.
