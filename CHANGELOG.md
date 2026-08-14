# Changelog

## Unreleased

## 0.11.4 — 2026-08-14

### Fixed

- `apply`: suppress pre/post hooks for stores skipped by `when`
  clauses. Previously a skipped store could still trigger its hooks.

### Internal

- Module refactor: split `main.rs` (5,222 lines), `store.rs`
  (3,827), `config.rs` (2,795), and `plan_exec.rs` (3,632) into
  smaller modules. No behavior or public API change. Full plan in
  `docs/plans/2026-08-13-module-refactor.md`.
- Tests: added characterization tests for `add` rollback paths
  before the refactor.

## 0.11.3 — 2026-08-13

### Fixed

- Tests: fix `prop_source_ancestors_within_direct_child` collision when
  `repo_name` equals `ext` by using a non-colliding external parent.

## 0.11.2 — 2026-08-13

### Fixed

- Tests: make `matrix_home_*` hook tests robust against inode reuse (use
  `mv`+`mkdir` instead of `rm -rf`+`mkdir`). Fixes flaky CI failure on
  filesystems that quickly recycle inode numbers.

## 0.11.1 — 2026-08-13

### Fixed

- CI: pin actions to valid SHAs (`actions/checkout`, `dtolnay/rust-toolchain`,
  `Swatinem/rust-cache`) and make inode-identity tests robust against
  filesystem inode reuse (rename-based replacement).

## 0.11.0 — 2026-08-13

### Added

- `stitch add <missing-path> --file` creates an empty regular file-backed store.
- `stitch add <existing-file> --to <store>` adopts one regular file into an
  existing single-target file-mode store, preserving its contents and adding
  the new path to generated state. Both forms support `--dry-run`.
- Add validates repository boundaries and destination/target ancestors before
  mutation, including in dry-run mode.

### CI and testing

- New `ci` workflow runs `cargo fmt`, `cargo clippy -D warnings`, and
  `cargo test --locked --all-targets` on every push/PR with concurrency
  control, timeout, pinned action SHAs, and `persist-credentials: false`.
  Pinned toolchain via `rust-toolchain.toml` (1.97) and Dependabot for actions.
- Property tests (proptest) for path normalization (`is_safe_fragment` /
  `normalize_fragment`), config merging/hash, and ownership checks
  (`points_into_repo`, `source_ancestors_within`, `link_name`). Fixes
  tautological `prop_unsafe_rejected` and false-idempotent `prop_link_name_idempotent`.
- Verify step now reuses single test log, uses `set -eo pipefail` +
  `PIPESTATUS`, checks both `skipped under root` and `skipping: running as root`,
  and ensures `cargo test --locked --all-targets` is exercised as non-root.

## 0.10.0 — 2026-08-12

### Added

- `stitch diff --exit-code` provides a branchable convergence check: exit
  0 means fully converged, exit 14 (`drift`) means safe changes are pending,
  and conflicts/errors retain their existing codes. JSON output keeps the full
  plan alongside the drift error. Platform-skipped stores do not count as drift.

### Fixed

- Authored config, generated state, and legacy config migration now reject
  unknown keys at the root, store, target, and hook levels, so misspelled
  safety-sensitive fields such as `ignore` or `pre` cannot be silently ignored.
- Edited executable plans cannot remove staged output before every live stale
  link that depends on it; omitted or reordered cleanup is rejected before
  hooks or filesystem changes.
- `diff` is now an exact mirror of `apply`: it reports removal of stale
  rendered files and detects private-render drift (mode not `0600`, or more
  than one hard link), matching what `apply` would replace. `doctor` inherits
  the same mode/hard-link checks.

## 0.9.0 — 2026-08-12

### Trust and safety

- **New `safety` module** defines the invariants every mutating command must
  uphold, replacing one-path-at-a-time fixes that left adjacent paths uncovered:
  - **`HomeIdentity`** pins `$HOME` as a location, not a live pathname. Both
    the entry itself (lstat) and the directory it resolves to (stat) are
    captured before any hook and revalidated after. A hook that replaces the
    directory *behind* a symlinked `$HOME` — without changing the symlink — is
    detected and rejected before any target mutation. Enforced in `apply`
    (text and JSON), `remove`, and plan capture.
  - **`InventoryCheck`** enforces inventory validity (symlinked source roots,
    source-name collisions, unreadable store dirs, unsupported template
    sources) for *all* stores regardless of platform match. "Skipped" changes
    whether a command acts on a store, not whether it validates it: a
    platform-skipped store with a symlinked source root or colliding sources
    is still invalid and is no longer silently removed or state-dropped.
- **Config revalidation now uses the fd-validated reader.** The apply path
  replaced the path-based `compute_config_hash` with
  `config::revalidate_config_hash`, which re-reads via the same no-follow,
  fd-validated reader as `ConfigSnapshot::load`. A path replacement (symlink,
  hard link, rename) targeting the config file between open and read can no
  longer substitute bytes for the revalidation hash. A read failure is
  surfaced as an explicit config error (exit 3) with path and checkpoint
  context, not silently collapsed into "hash mismatch". Parent-directory
  replacement remains within the documented same-user race boundary.
- **`status`/`doctor`/`remove` surface store and config errors** via new
  `LinkStatus::StoreError` and `LinkStatus::ConfigError` variants instead of
  treating a bad source root or unresolvable config as a missing link.
- **Link ownership check tightened in `check_link`:** the immediate link
  target must be an in-repo path (the same `source_ancestors_within` guard
  used by `points_at_source`); a link pointing directly at an external
  endpoint is now foreign under the two-tier ownership rule.

### Tests

- ~2100 lines of new CLI integration tests covering `$HOME` identity
  replacement, inventory validation for platform-skipped stores, the
  fd-validated revalidation parse-then-restore TOCTOU (deterministic test
  seam in `cmd_apply`, covering both text and JSON paths), and the new
  `status` error variants.

## 0.8.0 — 2026-08-11

### Trust and safety

- Plan schema 2 rejects schema-1 plans, attributes stale removals to their
  originating store, revalidates target ancestors after hooks, and requires
  execution-time `--force` for backup operations. `plan --only` limits capture
  but is not authenticated in editable JSON; execution validates every listed
  operation against a fresh normal apply plan.
- Symlink ownership now resolves gateway chains with POSIX-correct `..`
  semantics. Broad repo/store ownership remains canonical, while the narrow
  external source-symlink exception matches only the exact configured entry.
- Link creation rejects source paths that escape through a store ancestor;
  target mutations reject ancestors resolving into the repository.
- Render staging uses private, unpredictable exclusive temporary files,
  refuses symlink/non-directory ancestors and non-regular leaves, and never
  chmods a hard-linked staged inode. Direct dry-runs enforce the same
  `.gitignore` staging prerequisite as real apply.
- Store names, file fragments, glob patterns, and ignore patterns are validated
  and normalized in memory without rewriting authored configuration.
- Hidden quarantine artifacts were deliberately rejected: successful removal
  does not retain live links or old rendered secret material.

### Breaking changes

- Executable plan schema is now 2; existing plans must be recaptured.
- Store names `.git`, `.stitch`, nested paths, and `.`/`..` are invalid.
- Configured targets must expand to absolute paths; `stitch add` resolves a
  relative CLI path before persisting it.
- Overlapping ancestor/descendant targets within one store are rejected because
  independent reconciliation cannot safely determine ownership.
- Template sources must be direct regular files, not symlinks, FIFOs, devices,
  or directories.
- The global `post-apply` hook no longer runs when the plan reports conflicts
  or errors. It previously ran first even after a partial apply; per-store
  post hooks still run for every attempted store, so cleanup for partial
  applies belongs there rather than in the global hook.

## 0.7.1 — 2026-08-07

### Trust & safety

Closes all seven remaining items from the 2026-06 trust review. Each
upholds the core contract: never surprise the user with data movement,
exposure, or silent replacement.

- **P0 — nested ignore promotion no longer writes through foreign parent
  symlinks.** A `foreign_ancestor` guard in `apply_file_entry` rejects
  nested link creation when a parent symlink resolves outside the repo,
  for both dry-run and real operations.
- **P1 — `edit` rejects path traversal.** `match_target_to_source`
  validates relative target fragments with `is_safe_fragment`, rejecting
  `.` and `..` components before joining onto the store directory.
- **P1 — `apply --plan` rejects parent symlinks that resolve into the
  repo.** `resolved_target_points_into_repo` in `plan_exec` blocks
  executable plans from clobbering repository content through symlinked
  parents, closing the TOCTOU gap from the P0 fix.
- **P1 — ignore promotion preserves source symlinks.** `resolve_files`
  now accepts `is_symlink()` entries, and `create_link_to_entry` links
  to the source symlink path without canonicalizing — so relative,
  absolute, and dangling symlinks match whole-dir behavior after
  promotion to file mode.
- **P1 — `doctor` allows mutually-exclusive `when` targets sharing a
  path.** `WhenClause::are_compatible` makes duplicate-target detection
  `when`-aware, matching SPEC's documented laptop/server multi-target
  layout. Genuine duplicates (compatible or absent `when`) are still
  reported.
- **P2 — init/migrate overwrite guards detect dangling symlinks.** The
  refuse-existing-path guards now use `symlink_metadata` instead of
  `Path::exists`, so a dangling `state.toml`/`stitch.toml`/`.bak`
  symlink is refused rather than silently replaced by a regular file.
- **P2 — `migrate` validates the split state before writing.** A v0.2
  entry the new validator rejects (e.g. `files = ["./bashrc"]`) now
  fails fast before any file is written or the legacy config is backed
  up, instead of stranding the user with unloadable state.

All changes are covered by unit + integration tests. `cargo fmt`,
`cargo clippy -D warnings`, and the full suite (91 unit + 213 CLI) are
clean.

## 0.7.0 — 2026-08-06

### Features

- **Typed error taxonomy** (`StitchError` / `FailureClass`): 14 stable exit
  codes (0–13) with class names, messages, and resolution hints. Text mode
  prints `hint:` lines; `--json` envelopes include `class`, `code`, `message`,
  `hint`, and `details`.
- **JSON envelope** with a stable `schema: 1`. Global `--json` flag on
  `status`, `list`, `doctor`, `prune`, `render`, `apply`, `diff`, and `plan`.
  The envelope (including `error`) goes to stdout; the exit code stays honest;
  stderr is reserved for hook/subprocess output.
- **`stitch render <store>/<file>`**: read-only in-memory template render to
  stdout; `--json` emits `{source, link_name, sha256, content}`.
- **`stitch plan`**: capture `apply`'s exact op list as a `stitch/plan` JSON
  file. Includes staged-render hashes, config hash, and platform fingerprint.
  Text mode emits the raw plan; `--json` wraps it in the envelope.
- **`stitch apply --plan <file>`**: verbatim plan execution with preflight +
  per-op precondition re-checks + stale-plan detection. Aborts at the first
  failed op. `--dry-run` runs validation with no mutations. `--plan` is
  incompatible with `--only` and `--force` (usage error, exit 2).

### Trust & safety

- **Plan files are untrusted input.** `apply --plan` validates every op
  against the pinned config: path traversal rejection, backup path validation
  (same directory as target), source under repo, target under configured
  target, and `.stitch/render/` gitignore guard.
- **No plaintext in plans.** `stage_render` ops pin only the SHA-256 of the
  approved render; rendered content is not stored in the plan.
- **Stale-plan refusal.** Config edits, `state.toml` edits, platform drift, or
  render-hash drift cause `apply --plan` to exit 12 before any mutation.
- **Honest exit codes for agents.** `diff`/`plan` exit with the conflict/error
  class; `apply` aggregates per-entry classes (single → that code, multiple →
  11); `doctor` exits 13 on error-level findings.

### Internals

- **Refactored `compute_plan`**. `apply`, `diff`, `plan`, and the JSON/text
  renderers all consume the same `Plan` struct, so the two views cannot drift.
- **New dependencies:** `serde_json`, `sha2`.

## 0.6.0 — 2026-08-03

### Features

- **Templates.** Files ending in `.tmpl` are rendered with minijinja and
  symlinked from `.stitch/render/<store>/...` (inside the repo, so
  `points_into_repo` / `remove` / `prune` keep working). Context:
  `{{ hostname }}`, `{{ os }}`, `{{ arch }}`, `{{ distro }}`, `{{ shell }}`,
  `{{ vars.key }}`, `{{ env("VAR") }}` (hard-fail) / `{{ env("VAR", "default") }}`.
  Whole-dir stores containing any `.tmpl` promote to per-file links. State
  records source names; `resolve_entry` strips the suffix for the link target.
- **Trust foundation for staging.** `init` appends `.stitch/render/` to
  `.gitignore` and creates the staging root at `0700`; renders are `0600`.
  `doctor` errors if the gitignore entry is missing. Rendering is in-memory
  only (no tempfile under a default umask).
- **`diff` content dimension** for templated entries: fresh render vs staged.
  Staging is hash-gated (preserves mtime) and reconciled on `apply`/`remove`.
  `apply` also removes stale stitch-owned file-mode links when a source is
  deleted or renamed; foreign symlinks remain untouched.
- **`stitch edit <entry>`** opens a store or target's repo source (the `.tmpl`
  when present) — never the staged render. Config-based; works pre-apply.
- **`stitch import`** scans for existing repo-pointing symlinks and registers
  them in `state.toml` (shares `scan.rs` with `prune`).

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
