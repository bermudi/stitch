# stitch

A dotfile manager. You keep your config files in one repo; `stitch` reads a TOML config and symlinks them into place.

Symlinks point from the target (`~/.bashrc`, `~/.config/nvim`) back to the repo. Edits hit the repo file directly — no source/target split, no drift, no re-add step. Agents, scripts, whatever — if it writes to a symlink, it writes to the repo.

## Config

Desired state is split across two files by authorship. Load-bearing rule: **human-authored content is never rewritten by the tool.** v0.2 mixed authored and generated fields in one file, so every `add`/`remove` reserialized the whole thing via `to_string_pretty` and silently destroyed comments and hand-formatting. The split ends that.

### `stitch.toml` — repo root, visible, **authored**

You write and edit this. Like `Cargo.toml` — signal, not noise. stitch writes it exactly once, at `init`; thereafter it is read-only from the tool's side.

```toml
[vars]
editor = "nvim"
email = "you@example.com"

[stores.shells.when]
os = "linux"

[stores.git]
hooks = { post = "git config --global core.editor nvim" }

[stores.nvim]
ignore = ["*.bak", "scratch/"]
```

### `.stitch/state.toml` — hidden, **generated**

stitch writes this. It records the concrete link inventory — what to link, where. `add`/`remove` are the only writers. You may hand-edit it (plain TOML), but the tool is authorized to reserialize it on the next mutation, so do not rely on comments or formatting here.

```toml
[stores.nvim]
target = "~/.config/nvim"

[stores.shells]
target = "~"
files = [".bashrc", ".zshrc"]

[stores.git]
target = "~/.config/git"
```

### Field ownership

| Field | Lives in | Kind |
|---|---|---|
| `vars` | `stitch.toml` | user variables |
| `when` | `stitch.toml` | behavior — platform filter |
| `hooks` | `stitch.toml` | behavior — side effects |
| `ignore` | `stitch.toml` | behavior — resolution rule |
| `target` | `state.toml` | inventory — where to link |
| `files` | `state.toml` | inventory — what to link |
| `patterns` | `state.toml` | inventory — what to link |

Every `target` must be absolute after `~` expansion. `stitch add` accepts a
relative command-line path but resolves it against the invocation directory
before writing `state.toml`, so later commands never depend on their current
working directory.

### Load-time merge

A store is the union of its `stitch.toml` behavior and its `state.toml` inventory, merged by store name. A store present in only one file is legal:
- **`state.toml` only** — store with default behavior (`when` always, no hooks).
- **`stitch.toml` only** — behavior for a store with no links yet. `doctor` warns (dead behavior).

### Multi-target stores

A store can fan out to multiple destinations. Each target entry has a **name** — the join key pairing inventory (`state.toml`) with behavior (`stitch.toml`). The name is required because two targets can share a path (the same `~/.config/helix` on different hostnames), so the path cannot be the key.

`stitch.toml` (authored):
```toml
[stores.helix.targets.laptop]
when = { hostname = "laptop" }

[stores.helix.targets.server]
when = { hostname = "server" }
```

`.stitch/state.toml` (generated):
```toml
[stores.helix.targets.laptop]
target = "~/.config/helix"

[stores.helix.targets.server]
target = "~/.config/helix"
```

`add` derives a store name from the target path basename (leading `.` stripped) when `--name` is not given. `migrate` names v0.2 multi-target array entries hostname-first, else `target-<n>` with a `-N` suffix on collision. Renaming in one file without the other orphans the entry; `doctor` warns.

### Migration

`stitch migrate` splits a v0.2 `.stitch/config.toml` in place: authored fields (`vars`, `when`, `hooks`, `ignore`) → `stitch.toml`, inventory fields (`target`, `files`, `patterns`) → `.stitch/state.toml`, then preserves the original as `.stitch/config.toml.bak` (the recovery path). One-shot, deterministic; `--dry-run` previews the planned files.

Migration is **comment-lossy by design**: v0.2 comments decorate a single-file layout that no longer exists, so there is no faithful place to carry them into the split files. The conversion prints a note to that effect; the `.bak` preserves the original so the user can re-add any comments they want to keep.

Multi-target array entries get deterministic names during migration (hostname-first if present, else `target-<n>`, with a `-N` suffix on collision) — the cross-file join key.

## Core concepts

- **Store** — a top-level directory in the repo. One unit of config.
- **Target** — where the symlink(s) land on disk. Declared explicitly.
- **Whole-directory mode** — no `files` or `patterns` → the entire store dir is one symlink.
- **File mode** — `files` and/or `patterns` → individual files are symlinked into the target dir.
- **when** — platform filter. All specified fields must match. Omit = always applies.
- **Hooks** — `pre` and `post` shell commands per store.
- **Authored vs generated** — `stitch.toml` is human-authored and never rewritten by the tool (after `init`); `.stitch/state.toml` is generated and tool-owned. The split exists so mutations to inventory never clobber your comments and formatting.
- **Desired state is truth** — `stitch apply` reconciles the filesystem to match the merged config. The entire update loop.

## Repo discovery

Every command except `init` resolves the repo root by, in order:

1. **`--repo <path>`** global flag (highest precedence),
2. **`STITCH_REPO`** env var,
3. an upward walk from cwd looking for a `.stitch/` directory (the default).

An override (flag or env) must point at a directory that actually contains
`.stitch/`; a typo is rejected rather than silently operating on the wrong
directory. `init` is cwd-anchored and ignores both — it creates a new repo in
the current directory, so honoring an override would be surprising.

Set `STITCH_REPO` once in your shell rc to run `stitch` from anywhere:

```sh
export STITCH_REPO=~/dots
```

## Commands

Most read/plan commands support a global `--json` flag. See [Agent interface (v0.7)](#agent-interface-v07) for the envelope, per-command shapes, and exit-code table.

### `stitch init`

Create `stitch.toml` (empty, with a header documenting it is authored/read-only to the tool) and `.stitch/state.toml` (empty, generated) in the current directory. Also appends `.stitch/render/` to the repo's `.gitignore` (creating the file if needed) and pre-creates `.stitch/render/` at mode `0700`. Refuses if either config file exists, or if a v0.2 `.stitch/config.toml` is present (pointing at `migrate` instead).

### `stitch apply`

Reconcile all stores. Creates missing symlinks, replaces broken ones, reports conflicts.

| Flag | Short | Description |
|---|---|---|
| `--only` | `-o` | Apply only named stores (repeatable) |
| `--dry-run` | | Preview without changes |
| `--force` | | Back up real-file/dir conflicts to `{target}.bak`, then link |
| `--plan <file>` | | Validate and execute a previously captured plan file |

### `stitch plan`

Capture an executable operation list for `apply --plan`. Plan capture is intentionally stricter than direct apply: it rejects symlinked target ancestors (including external mount/gateway links) because an editable pathname-based plan cannot safely pin those traversals. Text mode prints the raw `stitch/plan` JSON to stdout; `--json` wraps it in the [standard envelope](#json-envelope). Exits non-zero when the captured plan contains conflicts or errors, so `stitch plan && stitch apply --plan p.json` forces a branch.

| Flag | Short | Description |
|---|---|---|
| `--only` | `-o` | Plan only named stores (repeatable) |
| `--force` | | Plan `.bak` backup behavior (what `apply --force` would do) |
| `--json` | | Emit the plan in the JSON envelope |

### `stitch render <store>/<file>`

Render a `.tmpl` to stdout. Read-only — no staging write, no link touch. `--json` emits `{source, link_name, sha256, content}`.

### `stitch status [name]`

Show symlink state for one or all stores. States: `linked`, `missing`, `conflict`, `broken`.

### `stitch diff`

Preview what `stitch apply` would do. Reports `ok`, `create`, `conflict`, `replace`, `backed up` per target. For templated entries, also reports `content` when a fresh in-memory render differs from the staged file (link state may already be correct).

| Flag | Short | Description |
|---|---|---|
| `--only` | `-o` | Diff only named stores (repeatable) |
| `--force` | | Preview `.bak` backup behavior (what `apply --force` would do) |

### `stitch list`

Print all configured stores and their targets.

### `stitch add <path>`

Add a path to stitch. If the path exists as a real file or directory, it is moved into the repo and symlinked back (adopt). If it doesn't exist, an empty store directory is created and linked to the path. Either way, the link inventory is recorded in `.stitch/state.toml` only — `stitch.toml` is untouched.

A symlink at the target path is always an error (never silently clobbered). The store name is derived from the path basename with a leading `.` stripped; override with `--name`.

| Flag | Short | Description |
|---|---|---|
| `--name` | `-n` | Override derived store name |
| `--files` | `-f` | Files to link individually (repeatable; create-empty only) |
| `--patterns` | `-p` | Glob patterns (repeatable; create-empty only) |
| `--dry-run` | | Preview |

### `stitch remove <name>`

Remove store symlinks, the store's staged renders under `.stitch/render/<name>/`, and the store's `state.toml` entry. `stitch.toml` behavior is left in place (the tool never rewrites authored config) — `doctor` flags the orphaned behavior; remove it via `stitch edit`. Store directory left untouched.

### `stitch edit [entry]`

Open `stitch.toml` in `$EDITOR`. With an argument, opens an entry's **repo source** instead — the `.tmpl` for a templated entry, the plain file otherwise — resolved via the merged config (works pre-apply, since it reads config rather than filesystem links). Never the staged render. The arg is a store name or a home-expanded target path.

### `stitch doctor`

Health check: missing store dirs, broken symlinks, conflicting targets, empty stores, orphaned behavior (store in `stitch.toml` with no `state.toml` entry), staging permission drift, template staging content drift (staged ≠ fresh render), template render errors. It errors on a missing `.stitch/render/` gitignore entry only when configured templates are active on this platform or staged output exists; plain pre-template repos need no migration.

### `stitch migrate`

One-shot conversion of a v0.2 `.stitch/config.toml` repo to the two-file layout. Splits authored fields (`vars`, `when`, `hooks`, `ignore`) into `stitch.toml` and inventory fields (`target`, `files`, `patterns`) into `.stitch/state.toml`, then preserves the original as `.stitch/config.toml.bak`. Deterministic; see [Migration](#migration). Comment-lossy by design.

| Flag | Description |
|---|---|
| `--dry-run` | Preview the planned `stitch.toml` + `state.toml` without writing |

### `stitch prune` (alias: `gc`)

Find symlinks pointing into this repo that no store references (orphans left
behind by a renamed/removed store), and optionally remove them. Non-destructive
by default: it lists and removes nothing unless `--yes` is given.

A shared scanner walks the scan dirs, classifies each symlink via the same
`points_into_repo` guard the rest of the tool uses, and reports links whose
location is not covered by any merged store's target set (platform-gated — a
store skipped on this host does not own its target here, so a stray link there
is an orphan). Foreign symlinks (stow/chezmoi/Nix/Home-Manager) are never
listed or removed. Only the symlink is ever removed; repo content is untouched.

Removal routes through the `points_into_repo`-guarded `remove_link`, so a link
that was repointed between the scan and the unlink is skipped rather than
clobbered.

| Flag | Description |
|---|---|
| `--scan-dir` | Directories to scan, full depth (repeatable). Default: `~` (top-level dotfiles only), `~/.config`, `~/.local/share` |
| `--dry-run` | Preview only (also the default behavior) |
| `--yes` | Remove the orphaned links (default is list-only) |

By default `~` is walked shallowly — direct children only — so a bare `prune`
catches top-level dotfile links (`~/.bashrc`, `~/.gitconfig`, …) without
descending into slow `$HOME` trees (`~/.cache`, `node_modules`, …).
`~/.config` and `~/.local/share` are walked at full depth. An explicit
`--scan-dir` is always full depth, so `--scan-dir ~` forces a complete sweep.

The scanner lives in `src/scan.rs` and is shared with `import`, which
registers existing repo-pointing symlinks in state instead of removing them.

### `stitch import`

Scan for existing symlinks pointing into the repo and register them in
`.stitch/state.toml`. Shares the `src/scan.rs` scanner with `prune`. Links
already covered by config are skipped. Never rewrites `stitch.toml`. A link
pointing at a store directory becomes a whole-dir store; links into files under
a store become file-mode entries (all must share one target parent).

| Flag | Description |
|---|---|
| `--scan-dir` | Directories to scan, full depth (repeatable). Default: `~` (top-level dotfiles only), `~/.config`, `~/.local/share` |
| `--dry-run` | Preview without writing state |

## Platform support

**Linux only.** stitch is built on POSIX symlinks (`std::os::unix::fs::symlink`) and does not compile on Windows. macOS is not officially supported or tested. The `when.os` / `when.distro` conditionals are Linux-targeted; `os` mirrors `std::env::consts::OS`.

## Platform conditionals (`when`)

All fields optional. All specified must match.

| Field | Values |
|---|---|
| `os` | `linux`, `macos`, `windows` — mirrors `std::env::consts::OS` (note: `macos`, **not** `darwin`) |
| `arch` | `x86_64`, `aarch64`, ... |
| `distro` | Detected distro ID (for example `ubuntu`, `arch`, `debian`) |
| `hostname` | Machine hostname |
| `shell` | `zsh`, `bash`, `fish`, `nu` |

`distro` detection is exact: on Linux, stitch reads the first `ID=` from
`/etc/os-release`; if unavailable, it falls back to `DISTRIB_ID=` from
`/etc/lsb-release` and lowercases that value. On macOS it is `macos`; on other
platforms, or when neither Linux file supplies an ID, it is absent. A
`when.distro` clause matches only an exactly equal detected value, so an absent
distro never matches. In templates, an absent `distro` renders as `none` (the
minijinja representation of the optional value); use `{{ distro or "unknown" }}`
when a concrete fallback is needed.

## Templates (v0.6)

Files ending in `.tmpl` are rendered through minijinja and symlinked from a
staging dir. Non-`.tmpl` files are symlinked directly. Detection is by suffix
only — no content sniffing. Secrets (`{{ secret }}`) are planned for v0.9.

| Expression | Description |
|---|---|
| `{{ env("VAR") }}` | Environment variable (hard-fails if unset; `env("VAR", "default")` for an opt-in default) |
| `{{ hostname }}` | Hostname |
| `{{ os }}` | Operating system |
| `{{ arch }}` | CPU architecture |
| `{{ distro }}` | Distro ID; renders `none` when unavailable |
| `{{ shell }}` | Login shell basename |
| `{{ vars.key }}` | User-defined variable (`[vars]` in `stitch.toml`) |
| `{{ secret("name") }}` | Encrypted secret (planned for v0.9 — same render context) |

Rendered files go to `.stitch/render/<store>/...` — **inside the repo**, so the
symlink still satisfies `points_into_repo` and the existing `prune`/`remove`
machinery works unchanged. (An earlier draft put staging at
`~/.local/state/stitch/<repo-hash>/`; that location is *outside* the repo and
would make every templated symlink look foreign to `remove_link`/`prune`, which
both key off "symlink resolves into `repo_root`".) Secrets, when added in v0.9,
live encrypted in `.stitch/secrets.enc`.

Contract (rationale in `docs/plans/v0.6-templates.md`):

- **Detection is by `.tmpl` suffix, at any depth** — deterministic from the directory entry alone, no content sniffing. A template source must be a direct regular file; symlinks and other special files are rejected before they can promote or replace a target. A whole-dir store containing a `.tmpl` anywhere is promoted to file-mode resolution: one directory symlink becomes N per-file symlinks. Invisible for `~/.config/git`, observable for watched dirs (`conf.d`, systemd units, file watchers) — a documented behavioral consequence, not a surprise.
- **State records source names.** `state.toml` lists `gitconfig.tmpl`; apply/status/remove/diff strip the suffix through one shared resolution path (`resolve_entry`). A store file name is never used directly as a link target. A store containing both `foo` and `foo.tmpl` is rejected at resolution time.
- **Staging is locked down from day one.** `.stitch/render/` is `0700`, rendered files `0600`. All rendering (apply and diff) happens in memory — no tempfile ever holds rendered plaintext under a default umask. Threat model: multi-user machines, shared CI runners, `env()` pulling tokens in v0.6, and encrypted secrets planned for v0.9 all read through these files. `init` appends `.stitch/render/` to the repo's `.gitignore`; an upgraded repo must add the entry manually before its first template apply. `apply` refuses to render without it, and `doctor` errors when templates or staged output make the entry relevant.
- **Failure model is per-entry.** A template error (parse failure, missing `env` key) fails that entry and skips its link — never created, never broken. Render is atomic and happens before linking, so staging is never half-written. `apply` continues with other entries and stores, exiting non-zero at the end if anything failed (same aggregation as conflicts).
- **`diff` gains a content dimension for templated entries only**: a fresh in-memory render compared against the staged file — "would `apply` change anything?" Non-templated entries remain link-state-only.
- **Staging and target links are reconciled and tool-owned.** `apply` removes staged renders and their stitch-owned target links when a source no longer resolves; `remove` deletes the store's staging tree alongside its links. Links to foreign destinations are never removed. Hand-edits inside `.stitch/render/` are unsupported and overwritten on the next `apply`; `doctor` flags drift (staged ≠ fresh render) so this is never silent. Writes are hash-gated: unchanged content preserves mtime.
- **Authoring is by hand.** Write `gitconfig.tmpl` in the store and `apply` — whole-dir stores pick it up via promotion; file-mode stores list the source name in `files`. There is no `add --template` in v0.6.

## Hooks (v0.2)

Per-store `pre` and `post` shell commands. `pre` failure aborts the store. `post` failure warns.
Hooks are run via `sh -c` with the user's privileges (like git hooks). They are skipped under `--dry-run`/`diff`.

Global hooks in `.stitch/hooks/` (must be executable — `chmod +x`):
- `pre-apply` / `post-apply` — run before/after all stores
- `pre-remove` / `post-remove` — run before/after removals

Hooks receive env vars: `STITCH_ROOT`, `STITCH_STORE`, `STITCH_TARGET`, `STITCH_ACTION`, plus platform vars (`STITCH_OS`, `STITCH_ARCH`, `STITCH_DISTRO`, `STITCH_HOSTNAME`, `STITCH_SHELL`).

## Ignore patterns

Authored — lives in `stitch.toml` (a resolution rule, not inventory):

```toml
[stores.nvim]
ignore = ["*.bak", "scratch/"]
```

Global ignores always active: `.stitch`, `.stitch/**`, `.git`, `.git/**`, `.gitignore`, `.DS_Store`.

If ignored content exists, whole-directory mode is promoted to file mode.

## Conflict handling

Before linking, if a real file/dir exists at the target (not a stitch-managed
symlink):
1. Stop.
2. `stitch add <path>` moves it into the repo and symlinks back — the
   interactive path.
3. Or `apply --force` renames the conflicting target to `{target}.bak` and
   links in place, leaving a recoverable copy alongside.

(`add` moves the file *into* the repo to manage it; `--force` leaves the
backup in the target dir.)

Nothing overwritten silently. Foreign symlinks (stow/chezmoi/Nix/Home-Manager)
are always conflicts — even under `--force`. If `{target}.bak` already exists,
`--force` fails rather than destroy a prior backup.

## Architecture

```
src/
  main.rs       CLI entry point (clap) + command handlers
  cli.rs        Command definitions
  config.rs     Serde types, TOML parsing, authored/generated split, load-time merge
  store.rs      Store model, apply/remove logic, file resolution
  linker.rs     Symlink create/remove/verify, ownership checks
  platform.rs   OS, arch, distro, hostname, shell detection
  hooks.rs      Per-store + global hook execution
  scan.rs       Filesystem scan for repo-pointing symlinks (prune + future import)
```

`config.rs` loads `stitch.toml` + `.stitch/state.toml`, merges per store by
name, and validates the merged result (fragment validation, target
uniqueness). Mutating commands write only `state.toml`; `stitch.toml` is
read-only to the tool after `init`.

## Agent interface (v0.7)

v0.7 adds machine-readable output and branchable failures for scripted/agent use.
All read/plan commands share one stable JSON envelope; `stitch plan` and
`stitch apply --plan` provide a capture-then-execute primitive that replays an
exact op list with preflight and fingerprint checks.

### Global `--json` flag

`--json` is global. It is supported for `status`, `list`, `diff`, `apply`,
`plan`, `doctor`, `prune`, and `render`. It is **not** supported for `init`,
`add`, `remove`, `edit`, `import`, or `migrate`; passing it to those commands
exits with code 2.

### JSON envelope

Every `--json` command prints one JSON object to **stdout** and exits with the
appropriate class code. stderr is reserved for hook/subprocess output; command
warnings and errors travel inside the envelope.

```json
{
  "schema": 1,
  "command": "status",
  "ok": true,
  "warnings": [],
  "data": { "...": "command-specific" },
  "error": null
}
```

- `schema` is always `1` for v0.7.x.
- `data` and `error` are always present; the unused one is `null`.
- `error`, when non-null, is `{class, code, message, hint, details}`. `hint`
  and `details` are always present and `null` when unset.
- On partial failure (e.g. `apply` with conflicts, `doctor` with error findings)
  both `data` and `error` are populated and the process exits with the error's
  class code.
- Warnings collected at load time are in `warnings[]`; nothing is written to
  stderr by the reporter.

### Per-command JSON shapes

`status --json`: array of status entries.

```json
[
  {
    "store": "git",
    "target_name": "laptop",
    "target": "/home/daniel/.gitconfig",
    "source": "/home/daniel/dots/git/gitconfig.tmpl",
    "templated": true,
    "staged_path": "/home/daniel/dots/.stitch/render/git/gitconfig",
    "state": "linked",
    "skipped_platform": false,
    "resolves_to": null
  }
]
```

- `store`: store name.
- `target_name`: target entry name for multi-target stores; omitted for
  single-target stores.
- `target`: absolute target path.
- `source`: absolute source path (template source for templated files; store
  file otherwise).
- `templated`: `true` when the source ends in `.tmpl`.
- `staged_path`: absolute staged render path for active templates; omitted
  otherwise.
- `state`: `linked`, `missing`, `conflict`, or `broken`.
- `skipped_platform`: `true` when the store's `when` clause does not match.
- `resolves_to`: for `broken`, the absolute path the broken symlink resolves to;
  `null` otherwise.

`list --json`: array of configured stores.

```json
[
  {
    "name": "shells",
    "mode": "file-mode",
    "target": "~",
    "files": [".bashrc", ".zshrc"],
    "when": { "os": "linux" }
  }
]
```

Modes: `whole-dir`, `file-mode`, `multi-target`, or `none`. Multi-target stores
have `targets` instead of `target`, `files`, and `patterns`. `when` fields are
omitted when empty.

`doctor --json`: typed findings.

```json
{
  "findings": [
    {
      "id": "broken-link",
      "severity": "error",
      "message": "...",
      "path": "/home/daniel/.bashrc",
      "hint": "..."
    }
  ],
  "summary": { "errors": 1, "warnings": 0, "info": 0 }
}
```

`severity` is `error`, `warning`, or `info`. `path` and `hint` are always
present and `null` when unset.

`diff --json` / `apply --json`: the `Plan` shape consumed by both the text and
JSON renderers.

```json
{
  "stores": [
    {
      "store_name": "git",
      "ops": [ ... ]
    }
  ],
  "summary": {
    "created": 0,
    "replaced": 0,
    "backed_up": 0,
    "removed": 0,
    "content_changed": 0,
    "already_linked": 0,
    "conflicts": 0,
    "errors": 0,
    "skipped": 0
  }
}
```

Plan ops are tagged by `action` (snake_case):

- `stage_render`: `{store, source_rel, source, staged, sha256}`
- `create_link`: `{target, source, requires}`
- `replace_link`: `{target, source, old_resolves_to, requires}`
- `backup_and_link`: `{target, source, backup, requires}`
- `remove_link`: `{store, target, source, requires}`
- `already_linked`: `{target, source, requires}`
- `content_changed`: `{target, source, requires}`
- `conflict`: `{target, resolves_to}`
- `error`: `{message, class}`
- `skipped_platform`

`requires` is `{target: TargetState, backup?: TargetState}`. `TargetState` is
tagged by `target` with optional `value`:

```json
{"target": "symlink_to", "value": "/home/daniel/dots/git/gitconfig"}
{"target": "absent"}
{"target": "real_entry"}
{"target": "symlink_into_repo"}
```

`stitch plan --json` returns a `PlanFile`, not the above `Plan`. See
[Plan file format](#plan-file-format-stitchplan) below. Text mode (`stitch plan`)
emits the raw `PlanFile` object.

`prune --json`:

```json
{
  "orphans": [
    {
      "link": "/home/daniel/.oldrc",
      "resolves_to": "/home/daniel/dots/old/.oldrc",
      "status": "listed"
    }
  ],
  "removed": 0,
  "failed": 0
}
```

`status` is `listed` (preview), `removed`, or `failed`.

`render --json`:

```json
{
  "source": "/home/daniel/dots/git/gitconfig.tmpl",
  "link_name": "gitconfig",
  "sha256": "...",
  "content": "..."
}
```

### Exit codes

v0.7 maps every non-zero exit to a typed `FailureClass`. Under `--json` the
error object carries `class` (the stable id) and `code`; text mode prints a
`hint:` line after the error.

| Code | ID | Class | Hint |
|---|---|---|---|
| 0 | `ok` | Success | — |
| 1 | `internal` | Generic / I/O / unexpected | (none) |
| 2 | `usage` | CLI usage error | check the command arguments |
| 3 | `config` | Config load, parse, or v0.2 migration | check the config files or run `stitch migrate` |
| 4 | `repo-resolution` | Repo root resolution failed | run `stitch init` or pass a valid `--repo` path |
| 5 | `unknown-store` | Unknown store name | list valid stores with `stitch list` |
| 6 | `conflict-real` | Real file/dir blocks the target | remove the conflicting target or run `stitch apply --force` |
| 7 | `conflict-foreign` | Foreign symlink blocks the target | remove or repoint the conflicting symlink yourself |
| 8 | `render` | Template render failed | set missing environment variables or fix the template |
| 9 | `path-validation` | Path fragment validation failed | use relative paths without `..` and no leading `/` |
| 10 | `hook` | Hook execution failed | fix or disable the failing hook |
| 11 | `mixed` | Multiple failure classes in one run | see the per-entry messages in JSON |
| 12 | `plan-stale` | Plan is stale or invalid | re-run `stitch plan` |
| 13 | `doctor` | `doctor` reported error-severity findings | address the findings (per-finding hints in JSON) |

Aggregation rule: for `apply`, `diff`, and `plan`, a single failure class
present → that class's code; multiple classes → 11. `apply --plan` exits 12 on
stale or invalid plans, and on any op whose precondition changed between capture
and execution. `doctor` exits 13 when error-level findings are present.

### Plan file format (`stitch/plan`)

`stitch plan` captures the full executable plan, including staged-render hashes
and a platform/config fingerprint. Plan files are versioned and self-describing:

```json
{
  "schema": 2,
  "kind": "stitch/plan",
  "repo": "/home/daniel/dots",
  "config_sha256": "...",
  "platform": {
    "os": "linux",
    "arch": "x86_64",
    "distro": "arch",
    "hostname": "x",
    "shell": "zsh"
  },
  "ops": [ ... ],
  "conflicts": [ ... ],
  "errors": [ ... ]
}
```

- `config_sha256` is a domain-separated SHA-256 fingerprint of
  `stitch.toml` and `.stitch/state.toml`, including each file's identity,
  presence, length, and bytes. Any edit to either file invalidates the plan.
- `platform` is the fingerprint of the machine where the plan was captured.
  Plans are single-machine artifacts.
- `ops` are tagged by `op` (snake_case):
  - `stage_render`: `{store, source_rel, staged, sha256}` — pins a template
    render by hash; no plaintext content is stored.
  - `create_link`: `{target, source, requires}` — target must be absent.
  - `replace_link`: `{target, source, requires}` — target must be a symlink
    pointing at the expected source or a real entry.
  - `backup_and_link`: `{target, backup, source, requires}` — target must be a
    real entry and the backup path must be absent.
  - `remove_link`: `{store, target, source, requires}` — target must be a
    repo-owned symlink. `store` identifies the originating store (including
    source-less stale cleanup); `source` is optional.
  - `remove_staged`: `{store, rel}` — remove a stale rendered template from
    `.stitch/render/<store>/<rel>`.
- `requires` is the plan-file flat form:
  `{target: "<state>", value?: "...", backup?: "<state>", backup_value?: "..."}`.
  `target`/`backup` are one of `absent`, `real_entry`, `symlink_to`, or
  `symlink_into_repo`; `symlink_to` requires a `value`.
- `conflicts`: `{target, kind, resolves_to}`. `kind` is `foreign_symlink` when
  `resolves_to` is present, `real_entry` otherwise.
- `errors`: `{target, message, class}` — op-level errors present at capture
  time. `target` may be `null`.

Semantics:

- **No plaintext.** `stage_render` carries only the hash, so
  `stitch plan > /tmp/p.json` does not leak rendered content.
- **Untrusted input.** `stitch apply --plan` validates every op against the
  pinned config before acting: sources must live under the repo; targets must
  fall under a configured store target; path traversal (`..` or absolute) is
  rejected; backup paths must be in the same directory as the target; and
  `.stitch/render/` gitignore is checked before staging writes.
- **Stale-plan detection.** `apply --plan` accepts only plan schema `2`
  (schema `1` is stale and must be replanned), and refuses if `kind`,
  `config_sha256`, or `platform` do not match, or if any `stage_render` hash
  does not match a fresh in-memory render. All of these exit 12.
- **Preflight and per-op re-check.** Before any mutation, every op's
  preconditions are preflighted; each op's precondition is re-checked
  immediately before execution. Abort at the first failed op.
- **Abort semantics.** If an op fails, the report lists ops executed and
  remaining. The error is `plan-stale` (exit 12) when the failure is a changed
  precondition, or the original class for capture-time conflicts/errors.
- **Hooks.** Global hooks run from the pinned config. Per-store hooks run only
  for stores owning executable plan operations; a converged store omitted from
  `ops` does not run hooks. This prevents an edited `stores` list from becoming
  authority to invoke an otherwise unrelated hook.
- **Execution authority.** `apply --plan` rejects `--only`; the executable
  stores are derived from the plan operations. A plan containing
  `backup_and_link` additionally requires execution-time `--force`, so editable
  JSON alone cannot authorize moving user data. `--dry-run` runs the same
  validation without mutating anything.

### Schema stability

The executable plan schema is `2`; schema-1 plans must be recaptured. Plan
schema changes require a new minor or major release, and the shapes are locked
by golden tests in `tests/cli.rs`.

## Roadmap

### ✅ v0.2 — shipped (2026-06-17)
- [x] Config parsing, `init`, `apply`, `status`, `list`, `doctor`
- [x] `add`, `remove`, `edit`, `diff`
- [x] Whole-directory and file mode with recursive glob
- [x] Platform conditionals, multi-target stores
- [x] Hooks (per-store pre/post + global `.stitch/hooks/` executables)
- [x] Ignore patterns (per-store + global, with directory exclusion)
- [x] Conflict detection + `--force` backup semantics
- [x] Atomic config writes, path traversal validation, honest exit codes

### v0.3 — Config/state split (shipped 2026-06-18, **breaking**)
Separate human-authored config from tool-generated desired state. See
[Config](#config). Motivated by the v0.2 single-file model silently destroying
comments on every `add`/`remove` reserialize.

- [x] `stitch.toml` (authored, root) vs `.stitch/state.toml` (generated)
- [x] Load-time merge by store name; field-ownership table enforced
- [x] Named multi-target entries (name is the cross-file join key)
- [x] `add`/`remove` write `state.toml` only; `stitch.toml` read-only after `init`
- [x] `stitch migrate` — one-shot split of v0.2 `.stitch/config.toml`
- [x] `doctor`: orphaned-behavior detection (authored store with no state entry)
- [x] Deterministic `state.toml` ordering (sorted maps + files/patterns)
- [x] orphaned-link detection + `prune`/`gc` — shared `src/scan.rs` scanner;
  `doctor` stays repo-local (the home-dir scan lives in `prune` only)

### v0.4 / v0.5 — incremental releases (shipped 2026-06-28)
These version numbers shipped for incremental features, not the milestone work
below; planned milestones are renumbered to avoid collision.
- [x] v0.4.0 — `adopt` merged into `add` (breaking)
- [x] v0.4.1 — trailing-slash fix in `expand_home`
- [x] v0.5.0 — `--repo` flag + `STITCH_REPO` env var

### ✅ v0.6 — Templates (shipped)
- [x] Trust foundation: `.gitignore` enforcement in `init`/`doctor`,
      `.stitch/render` permissions (`0700`/`0600`), threat-model docs
- [x] Template engine (minijinja) + render→staging→symlink flow
- [x] `{{ env("VAR") }}`, `{{ vars.key }}`, `{{ hostname }}`, `{{ os }}`, ...
- [x] Staging dir under `.stitch/render/` (inside the repo — invariant-preserving)
- [x] `diff` content dimension for templated entries + staging reconciliation
- [x] `stitch edit <entry>` (open `.tmpl` source in `$EDITOR`)
- [x] `import` — scan existing repo-pointing symlinks into state

### ✅ v0.7 — Agent interface (shipped)
Machine-readable verification and branchable failures for agent/scripted use.
See `docs/plans/v0.7-agent-interface.md`.
- [x] `--json` on read/plan commands; typed `doctor` findings; `stitch render`
- [x] `stitch plan` → `stitch apply --plan` — captured op list with
      preconditions and fresh safety validation before execution
- [x] Distinct exit codes per failure class + resolution hints

### ✅ v0.8 — Filesystem trust hardening (shipped)
- [x] Plan schema 2 with explicit stale-removal ownership and execution-time
      force authority
- [x] POSIX-correct gateway symlink ownership and source/target confinement
- [x] Exclusive private render staging with strict file-type checks
- [x] Config/store/path normalization and reserved-name validation
- [x] Explicit race boundary; no hidden quarantine artifacts

### v0.9 — Encrypted secrets (planned, split out — separate trust surface)
- [ ] `age` or XChaCha20-Poly1305; `.stitch/secrets.enc`
- [ ] Key management + threat model (red lines: no external upload, no data exposure)

### v0.10 — TUI (planned)
- [ ] Interactive dashboard (ratatui)
- [ ] Command palette
- [ ] Activity log

## Design decisions

- **TOML over YAML** — no quoting gotchas for `~` paths, Rust-idiomatic, unambiguous.
- **Symlinks, not copies** — edits hit the repo directly. No drift possible — *except* templated files (v0.6), which are generated from `.tmpl` sources into `.stitch/render/`; you edit the template, not the rendered output. The rendered symlink still points into the repo, so the ownership/prune invariant holds.
- **Explicit config** — no inferring targets from directory layout. You declare it.
- **Absolute symlinks** — resolved to absolute source paths so cwd doesn't matter.
- **Authored vs generated, never mixed** — `stitch.toml` is yours and hand-editable; `.stitch/state.toml` is the tool's. The tool never rewrites the authored file after `init`, so comments and formatting survive every mutation. v0.2's single-file model violated this and ate comments on every `add`/`remove`.
- **Desired state is truth** — `apply` reconciles the filesystem to match the merged config. Change config, re-apply. That's the loop.
- **Non-destructive by default** — conflicts stop, not clobber. `--force` for scripted use.
