# Changelog

## 0.5.0 — 2026-06-28

### Features

- **Run `stitch` from outside the repo.** A global `--repo <path>` flag and
  `STITCH_REPO` env var let you operate on a repo without `cd`-ing into it.
  Precedence: `--repo` > `STITCH_REPO` > upward cwd walk (unchanged default).
  An override must point at a directory containing `.stitch/` — a typo is
  rejected, not silently applied to the wrong directory. `init` is cwd-anchored
  and ignores both (it creates a new repo in the current directory).

## 0.4.1 — 2026-06-28

### Fixes

- **`stitch add` with a trailing slash now works.** `stitch add ~/.config/alacritty/`
  used to fail at the link step with a confusing rollback error. Root cause:
  `symlink(2)` rejects a linkpath with a trailing slash (the kernel treats it as
  "must resolve to an existing directory", but the path doesn't exist yet when
  we're creating the symlink). `expand_home` now strips trailing slashes from
  its result, fixing the issue for every caller (`add`, `apply`, `scan`,
  config load). Root (`/`) is preserved.

## 0.4.0 — 2026-06-28

### BREAKING — `adopt` merged into `add`

`stitch adopt` is removed. `stitch add` now does both jobs based on whether the
path exists:

| Old | New |
|---|---|
| `stitch adopt <path>` | `stitch add <path>` (path exists → move into repo + link back) |
| `stitch add <name> [target]` | `stitch add <path>` (path missing → create empty store + link) |

**Why:** the old split had two commands with different signatures, different
name-derivation rules, and a footgun — `stitch add ~/.shrc` silently created a
store named `~/.shrc` with no target, a literal `~` directory in the repo, and
linked nothing. One command with one rule (path is the positional, name derived
from basename) eliminates the ambiguity.

**Migration:** replace `stitch adopt <path>` with `stitch add <path>`. Replace
`stitch add <name> <target>` with `stitch add <target> [--name <name>]`. The
no-target form (`stitch add <name>`) is gone — it created a dead-end store with
no link; use `stitch add <target>` instead.

**New error:** `--files`/`--patterns` on an existing path now errors (the moved
content determines the store layout; they were silently ignored before).

### Features

- `stitch add --dry-run` — preview both adopt-existing and create-empty paths.

## 0.3.1 — 2026-06-18

### Features

- **`stitch prune` (alias `gc`)** — find symlinks pointing into the repo that
  no store references, and remove them. Closes the last v0.3 checkbox
  (orphaned-link detection). Non-destructive by default: lists only; `--yes`
  removes, `--dry-run` is an explicit alias for the default. Removal routes
  through the `points_into_repo`-guarded `remove_link`, so foreign symlinks are
  never touched and a link repointed between scan and unlink is skipped.
- **`src/scan.rs`** — shared filesystem scanner. By default walks `~`
  *shallowly* (top-level dotfiles only — `~/.bashrc`, `~/.gitconfig`, …) so a
  bare `prune` never descends into `~/.cache`, `node_modules`, or other slow
  `$HOME` trees, plus `~/.config` and `~/.local/share` at full depth; an
  explicit `--scan-dir` is always full depth. Scan dirs are a parameter, not
  hardwired to `$HOME`, so the scanner is testable. Platform-gated: a store
  skipped on this host doesn't own its target, so a stray link there counts as
  an orphan. Foundation for the planned `import` command.

### Trust & safety

- **Honest exit codes.** `prune --yes` returns non-zero if any link could not
  be removed (a permissions error, or a link repointed between scan and unlink).
  A scripted `stitch prune --yes && …` won't sail past failures.

### Design note

`doctor` deliberately stays a fast, repo-local health check — the home-dir scan
lives in `prune` only. Scanning `$HOME` on every `doctor` run would be slow and
surprising, and would force every `doctor` test to override `$HOME`. This is an
intentional deviation from the original v0.3 plan (which sketched orphan-link
detection as a `doctor` check); see `docs/plans/v0.3-config-state-split.md`.

### Testing

150 tests (58 unit + 92 CLI), all passing. New coverage: 9 scanner unit tests
(repo-pointing, foreign, missing-scan-dir, repo-pruning, covered-vs-orphan,
platform-skipped target, max-depth cap, dangling repo-pointing link,
file-mode target coverage) + 8 `prune` CLI tests (default lists, `--yes`
removes only the orphan, `--dry-run`, `--yes --dry-run` still lists, foreign
ignored, no-orphans, `gc` alias, non-zero exit on removal failure). `cargo fmt`
and `cargo clippy -D warnings` clean.

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
