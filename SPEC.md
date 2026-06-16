# stitch

A dotfile manager. You keep your config files in one repo; `stitch` reads a TOML config and symlinks them into place.

Symlinks point from the target (`~/.bashrc`, `~/.config/nvim`) back to the repo. Edits hit the repo file directly — no source/target split, no drift, no re-add step. Agents, scripts, whatever — if it writes to a symlink, it writes to the repo.

## Config

`.stitch/config.toml` at the repo root. Declared explicitly — directory layout is freeform.

```toml
[vars]
editor = "nvim"
email = "you@example.com"

[stores.nvim]
target = "~/.config/nvim"

[stores.shells]
target = "~"
files = [".bashrc", ".zshrc"]

[stores.shells.when]
os = "linux"

[stores.git]
target = "~/.config/git"
hooks = { post = "git config --global core.editor nvim" }
```

Multi-target (one store, multiple destinations):

```toml
[[stores.helix.targets]]
target = "~/.config/helix"
when = { hostname = "laptop" }

[[stores.helix.targets]]
target = "~/.config/helix"
when = { hostname = "server" }
```

## Core concepts

- **Store** — a top-level directory in the repo. One unit of config.
- **Target** — where the symlink(s) land on disk. Declared explicitly.
- **Whole-directory mode** — no `files` or `patterns` → the entire store dir is one symlink.
- **File mode** — `files` and/or `patterns` → individual files are symlinked into the target dir.
- **when** — platform filter. All specified fields must match. Omit = always applies.
- **Hooks** — `pre` and `post` shell commands per store.
- **Config is truth** — `stitch apply` reconciles the filesystem to match. The entire update loop.

## Commands

### `stitch init`

Create `.stitch/config.toml` in the current directory.

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

### `stitch adopt <path>`

Move an existing file or directory into the repo, create a config entry, symlink back.

| Flag | Short | Description |
|---|---|---|
| `--name` | `-n` | Override derived store name |
| `--dry-run` | | Preview |

### `stitch add <name> [target]`

Create a store directory and config entry. Links immediately if target provided.

| Flag | Short | Description |
|---|---|---|
| `--target` | `-t` | Target path (or pass positionally) |
| `--files` | `-f` | Files to link individually (repeatable) |
| `--patterns` | `-p` | Glob patterns (repeatable) |

### `stitch remove <name>`

Remove store symlinks and config entry. Store directory left untouched.

### `stitch edit`

Open `.stitch/config.toml` in `$EDITOR`.

### `stitch doctor`

Health check: missing store dirs, broken symlinks, conflicting targets, empty stores.

### `stitch import` *(planned — not yet implemented)*

Scan for existing symlinks pointing into the repo and import them into config.

| Flag | Description |
|---|---|
| `--scan-dir` | Directories to scan (repeatable). Default: `~`, `~/.config`, `~/.local/share` |
| `--dry-run` | Preview |

## Platform support

**Linux only.** stitch is built on POSIX symlinks (`std::os::unix::fs::symlink`) and does not compile on Windows. macOS is not officially supported or tested. The `when.os` / `when.distro` conditionals are Linux-targeted; `os` mirrors `std::env::consts::OS`.

## Platform conditionals (`when`)

All fields optional. All specified must match.

| Field | Values |
|---|---|
| `os` | `linux` |
| `arch` | `x86_64`, `aarch64`, ... |
| `distro` | `ubuntu`, `arch`, `debian`, ... |
| `hostname` | Machine hostname |
| `shell` | `zsh`, `bash`, `fish`, `nu` |

## Templates & secrets (v0.3)

Files containing `{{ ... }}` references are rendered through a template engine and symlinked from a staging dir. Files without template expressions are symlinked directly.

| Expression | Description |
|---|---|
| `{{ env "VAR" }}` | Environment variable |
| `{{ secret "name" }}` | Encrypted secret |
| `{{ .Hostname }}` | Hostname |
| `{{ .OS }}` | Operating system |
| `{{ .Vars.key }}` | User-defined variable |

Secrets stored encrypted in `.stitch/secrets.enc`. Rendered files go to `~/.local/state/stitch/<repo-hash>/`.

## Hooks (v0.2)

Per-store `pre` and `post` shell commands. `pre` failure aborts the store. `post` failure warns.
Hooks are run via `sh -c` with the user's privileges (like git hooks). They are skipped under `--dry-run`/`diff`.

Global hooks in `.stitch/hooks/` (must be executable — `chmod +x`):
- `pre-apply` / `post-apply` — run before/after all stores
- `pre-remove` / `post-remove` — run before/after removals

Hooks receive env vars: `STITCH_ROOT`, `STITCH_STORE`, `STITCH_TARGET`, `STITCH_ACTION`, plus platform vars (`STITCH_OS`, `STITCH_ARCH`, `STITCH_DISTRO`, `STITCH_HOSTNAME`, `STITCH_SHELL`).

## Ignore patterns (v0.2)

```toml
[stores.nvim]
target = "~/.config/nvim"
ignore = ["*.bak", "scratch/"]
```

Global ignores always active: `.stitch`, `.stitch/**`, `.git`, `.git/**`, `.gitignore`, `.DS_Store`.

If ignored content exists, whole-directory mode is promoted to file mode.

## Conflict handling

Before linking, if a real file/dir exists at the target (not a stitch-managed
symlink):
1. Stop.
2. Offer to move it into the repo (store `adopt` behavior) — the interactive
   path.
3. Then symlink back.

`apply --force` is the scripted path: it renames the conflicting target to
`{target}.bak` and links in place, leaving a recoverable copy alongside.
(`adopt` moves the file *into* the repo to manage it; `--force` leaves the
backup in the target dir.)

Nothing overwritten silently. Foreign symlinks (stow/chezmoi/Nix/Home-Manager)
are always conflicts — even under `--force`. If `{target}.bak` already exists,
`--force` fails rather than destroy a prior backup.

## Architecture

Current source modules:

```
src/
  main.rs       CLI entry point (clap) + command handlers (apply, add, adopt, remove, status, diff, list, doctor, edit)
  cli.rs        Command definitions
  config.rs     Serde types, TOML parsing
  store.rs      Store model, apply/remove logic
  linker.rs     Symlink create/remove/verify
  platform.rs   OS, arch, distro, hostname detection
  hooks.rs      Per-store + global hook execution
```

`conflict.rs`, `adopt.rs`, `doctor.rs`, `template.rs`, `secrets.rs` appear in the roadmap
(v0.2–v0.3) and are not yet implemented as separate modules — that logic currently lives
inline in `main.rs` / `store.rs`.

## Roadmap

### v0.1 — Core
- [x] Config parsing (TOML + serde)
- [x] `init`, `apply`, `status`, `list`, `doctor`
- [x] Whole-directory and file mode
- [x] Platform conditionals
- [x] Conflict detection
- [x] Absolute symlinks
- [x] Root discovery (walk up to `.stitch/`)

### v0.2 — Management
- [x] `adopt`, `add`, `remove`, `edit`
- [ ] `modify`
- [x] `diff` (dry run)
- [ ] `import` (scan existing symlinks)
- [x] Hooks (per-store pre/post + global hooks, env vars)
- [x] Ignore patterns (per-store + global ignores active; whole-dir promotion)
- [x] Multi-target stores

### v0.3 — Templates & secrets
- [ ] Go-style text/template engine
- [ ] `{{ env }}`, `{{ .Vars }}`, `{{ .Hostname }}` etc.
- [ ] Encrypted secrets (`age` or XChaCha20-Poly1305)
- [ ] Template staging dir

### v0.4 — TUI
- [ ] Interactive dashboard (ratatui)
- [ ] Command palette
- [ ] Activity log

## Design decisions

- **TOML over YAML** — no quoting gotchas for `~` paths, Rust-idiomatic, unambiguous.
- **Symlinks, not copies** — edits hit the repo directly. No drift possible.
- **Explicit config** — no inferring targets from directory layout. You declare it.
- **Absolute symlinks** — resolved to absolute source paths so cwd doesn't matter.
- **Config is truth** — `apply` reconciles to match. Change config, re-apply. That's the loop.
- **Non-destructive by default** — conflicts stop, not clobber. `--force` for scripted use.
