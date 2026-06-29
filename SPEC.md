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

## Commands

### `stitch init`

Create `stitch.toml` (empty, with a header documenting it is authored/read-only to the tool) and `.stitch/state.toml` (empty, generated) in the current directory. Refuses if either exists, or if a v0.2 `.stitch/config.toml` is present (pointing at `migrate` instead).

### `stitch apply`

Reconcile all stores. Creates missing symlinks, replaces broken ones, reports conflicts.

| Flag | Short | Description |
|---|---|---|
| `--only` | `-o` | Apply only named stores (repeatable) |
| `--dry-run` | | Preview without changes |
| `--force` | | Back up real-file/dir conflicts to `{target}.bak`, then link |

### `stitch status [name]`

Show symlink state for one or all stores. States: `linked`, `missing`, `conflict`, `broken`.

### `stitch diff`

Preview what `stitch apply` would do. Reports `ok`, `create`, `conflict`, `replace`, `backed up` per target.

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

Remove store symlinks and the store's `state.toml` entry. `stitch.toml` behavior is left in place (the tool never rewrites authored config) — `doctor` flags the orphaned behavior; remove it via `stitch edit`. Store directory left untouched.

### `stitch edit`

Open `stitch.toml` in `$EDITOR`.

### `stitch doctor`

Health check: missing store dirs, broken symlinks, conflicting targets, empty stores, orphaned behavior (store in `stitch.toml` with no `state.toml` entry), orphaned links (symlinks into the repo referenced by no store).

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

The scanner lives in `src/scan.rs` and is shared with the planned `import`
command, which will register existing repo-pointing symlinks in config instead
of removing them.

### `stitch import` *(planned — not yet implemented)*

Scan for existing symlinks pointing into the repo and import them into config.
Shares the `src/scan.rs` scanner with `prune`.

| Flag | Description |
|---|---|
| `--scan-dir` | Directories to scan, full depth (repeatable). Default: `~` (top-level dotfiles only), `~/.config`, `~/.local/share` |
| `--dry-run` | Preview |

## Platform support

**Linux only.** stitch is built on POSIX symlinks (`std::os::unix::fs::symlink`) and does not compile on Windows. macOS is not officially supported or tested. The `when.os` / `when.distro` conditionals are Linux-targeted; `os` mirrors `std::env::consts::OS`.

## Platform conditionals (`when`)

All fields optional. All specified must match.

| Field | Values |
|---|---|
| `os` | `linux`, `macos`, `windows` — mirrors `std::env::consts::OS` (note: `macos`, **not** `darwin`) |
| `arch` | `x86_64`, `aarch64`, ... |
| `distro` | `ubuntu`, `arch`, `debian`, ... |
| `hostname` | Machine hostname |
| `shell` | `zsh`, `bash`, `fish`, `nu` |

## Templates & secrets (v0.4 — planned, not yet implemented)

*This feature is on the v0.4 roadmap and does not exist yet. The design below is
the intended contract.*

Files containing `{{ ... }}` references will be rendered through a template engine
and symlinked from a staging dir. Files without template expressions will be
symlinked directly.

| Expression | Description |
|---|---|
| `{{ env "VAR" }}` | Environment variable |
| `{{ secret "name" }}` | Encrypted secret |
| `{{ .Hostname }}` | Hostname |
| `{{ .OS }}` | Operating system |
| `{{ .Vars.key }}` | User-defined variable |

Secrets will be stored encrypted in `.stitch/secrets.enc`. Rendered files will go
to `~/.local/state/stitch/<repo-hash>/`.

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

### v0.4 — Templates & secrets (planned)
- [ ] `modify` command
- [ ] `import` (scan existing symlinks)
- [ ] Go-style text/template engine
- [ ] `{{ env }}`, `{{ .Vars }}`, `{{ .Hostname }}` etc.
- [ ] Encrypted secrets (`age` or XChaCha20-Poly1305)
- [ ] Template staging dir

### v0.5 — TUI (planned)
- [ ] Interactive dashboard (ratatui)
- [ ] Command palette
- [ ] Activity log

## Design decisions

- **TOML over YAML** — no quoting gotchas for `~` paths, Rust-idiomatic, unambiguous.
- **Symlinks, not copies** — edits hit the repo directly. No drift possible.
- **Explicit config** — no inferring targets from directory layout. You declare it.
- **Absolute symlinks** — resolved to absolute source paths so cwd doesn't matter.
- **Authored vs generated, never mixed** — `stitch.toml` is yours and hand-editable; `.stitch/state.toml` is the tool's. The tool never rewrites the authored file after `init`, so comments and formatting survive every mutation. v0.2's single-file model violated this and ate comments on every `add`/`remove`.
- **Desired state is truth** — `apply` reconciles the filesystem to match the merged config. Change config, re-apply. That's the loop.
- **Non-destructive by default** — conflicts stop, not clobber. `--force` for scripted use.
