# Changelog

## 0.3.0 — 2026-06-18

### BREAKING — config/state split

Human-authored config and tool-generated desired state are now separate files.
**v0.2 repos must run `stitch migrate` once.**

- **`stitch.toml`** (repo root, authored) — `vars`, `when`, `hooks`, `ignore`.
  Written once by `init`; thereafter the tool **never rewrites it**. Your
  comments and formatting survive every mutation.
- **`.stitch/state.toml`** (hidden, generated) — `target`, `files`, `patterns`.
  The only file `add`/`adopt`/`remove` touch. May be hand-edited, but the tool
  reserializes it on the next mutation (sorted, with a tool-owned header).

**Motivation:** v0.2 kept everything in one `.stitch/config.toml` and
reserialized it via `to_string_pretty` on every mutation, silently destroying
comments and hand-formatting. The split ends that — the motivating bug is not
fixable inside the single-file model.

**Migration:** `stitch migrate` splits a v0.2 `.stitch/config.toml` in place,
writing `stitch.toml` + `.stitch/state.toml` and preserving the original as
`.stitch/config.toml.bak`. Migration is **comment-lossy by design** (a
structural one-shot conversion): v0.2 comments decorate a single-file layout
that no longer exists, so there is no faithful place to carry them. The `.bak`
is the recovery path; the conversion prints a note so the loss is not a
surprise.

### Features

- `stitch migrate` — one-shot, deterministic v0.2 → v0.3 split. `--dry-run`
  previews the planned files without writing.
- Multi-target entries are now **name-keyed** (the cross-file join key), so two
  targets can share a path (e.g. the same `~/.config/helix` on different
  hostnames). `list` prints `name → target` for multi-target stores.
- `doctor` flags **orphaned behavior**: a store declared in `stitch.toml` with
  no `state.toml` entry (e.g. deliberately left behind by `remove`, which never
  rewrites the authored file).
- `state.toml` ordering is **deterministic** (sorted maps + sorted
  `files`/`patterns`) for stable git diffs across invocations.

### Trust & safety

- **Authored config is read-only to the tool.** After `init`, stitch never
  rewrites `stitch.toml` — mutations touch `.stitch/state.toml` only.
- **v0.2 repos are rejected with an actionable error** pointing at `migrate`,
  not silently read in a dual-format mode.

### Testing

132 tests (49 unit + 83 CLI), all passing. New regression coverage:
comment-preservation across mutations, v0.2 rejection, stale-config warning,
authored-only-target load-and-skip, `state.toml` ordering stability, and a
migrate golden-file roundtrip. `cargo fmt` and `cargo clippy -D warnings` clean.

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
