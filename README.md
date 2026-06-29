# stitch

A dotfile manager for Linux. Keep your config files in one repo; `stitch` reads a
TOML config and symlinks them into place.

Symlinks point from the target (`~/.bashrc`, `~/.config/nvim`) back to the repo.
Edits hit the repo file directly — no source/target split, no drift, no re-add
step. Agents, scripts, whatever — if it writes to a symlink, it writes to the
repo.

**Linux only.** Built on POSIX symlinks. Not tested on macOS, does not compile on
Windows.

## Install

```sh
cargo install --path .
```

Or from a checkout:

```sh
cargo build --release
./target/release/stitch --help
```

Requires Rust 2024 edition.

## Quick start

```sh
mkdir ~/dots && cd ~/dots
stitch init                                # create stitch.toml + .stitch/state.toml
$EDITOR stitch.toml                        # add behavior (when, hooks, ignore)
stitch add ~/.config/nvim                   # move existing content into the repo and link back (or create empty store if target doesn't exist)
stitch apply                               # link them into place
stitch status                              # verify
```

Config is split across two files by authorship:

- **`stitch.toml`** (repo root) — yours. `vars`, `when`, `hooks`, `ignore`.
  Written once at `init`; the tool never rewrites it, so your comments survive.
- **`.stitch/state.toml`** (hidden) — the tool's. `target`, `files`,
  `patterns`. `add`/`remove` are the only writers.

## Example config

`stitch.toml` (authored — behavior):

```toml
[vars]
editor = "nvim"

[stores.shells.when]
os = "linux"

[stores.git]
hooks = { post = "git config --global core.editor nvim" }
```

`.stitch/state.toml` (generated — link inventory):

```toml
[stores.nvim]
target = "~/.config/nvim"

[stores.shells]
target = "~"
files = [".bashrc", ".zshrc"]

[stores.git]
target = "~/.config/git"
```

## Commands

| Command | Purpose |
|---|---|
| `stitch init` | Create `stitch.toml` + `.stitch/state.toml` in the current dir |
| `stitch migrate` | Split a v0.2 `.stitch/config.toml` into the two-file layout |
| `stitch apply` | Reconcile filesystem to match config (the update loop) |
| `stitch status [name]` | Show symlink state per store |
| `stitch diff` | Preview what `apply` would do |
| `stitch list` | Print all configured stores and targets |
| `stitch add <path>` | Move existing content into the repo and link back, or create an empty store if the path doesn't exist |
| `stitch remove <name>` | Remove symlinks and the inventory entry |
| `stitch edit` | Open `stitch.toml` in `$EDITOR` |
| `stitch doctor` | Health check (orphaned behavior, broken links, conflicts) |

`apply`, `diff`, and `add` support `--only <name>` (repeatable), `--dry-run`, and
`--force` where applicable.

## Concepts

- **Store** — a top-level directory in the repo. One unit of config.
- **Target** — where the symlink(s) land on disk. Declared explicitly.
- **Whole-directory mode** — no `files`/`patterns` → the entire store dir is one
  symlink.
- **File mode** — `files` and/or `patterns` → individual files are symlinked into
  the target dir.
- **`when`** — platform filter (`os`, `arch`, `distro`, `hostname`, `shell`).
  All specified fields must match.
- **Hooks** — per-store `pre`/`post` shell commands, plus global
  `.stitch/hooks/pre-apply` / `post-apply` / `pre-remove` / `post-remove`
  executables.
- **Authored vs generated** — `stitch.toml` is yours and hand-editable;
  `.stitch/state.toml` is the tool's. The tool never rewrites the authored
  file after `init`, so comments and formatting survive every mutation.
- **Config is truth** — `apply` reconciles the filesystem to match the merged
  config. Change config, re-apply. That's the loop.

## Safety

`stitch` mutates `$HOME`. The contract: **never surprise the user with data
movement, exposure, or silent replacement.**

- No external upload by default. Backups stay local.
- Foreign symlinks (stow, chezmoi, Nix, Home-Manager) are always conflicts —
  never silently replaced, even under `--force`.
- `apply --force` renames a conflicting real file/dir to `{target}.bak` before
  linking. Refuses if `.bak` already exists.
- `add` moves a file *into* the repo to manage it; `--force` leaves the
  backup in the target dir.
- Path traversal is rejected: absolute and `..`-containing `files`/`patterns`
  entries are rejected at config load.

## Documentation

- **[SPEC.md](SPEC.md)** — full command/feature contract, config reference,
  hooks, ignore patterns, conflict handling, roadmap.
- **[CHANGELOG.md](CHANGELOG.md)** — release notes (trust review resolutions,
  per-version changes).
- **[AGENTS.md](AGENTS.md)** — contributor notes (architecture, red lines,
  quality bar).
- **[docs/reviews/2026-06-15-trust-review.md](docs/reviews/2026-06-15-trust-review.md)**
  — the trust review that established the safety contract.

## Development

```sh
cargo build
cargo test          # unit + CLI integration tests
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Zero warnings, `cargo fmt` and `cargo clippy -D warnings` clean — required for
a change to count as done.

## License

Personal-use project. See repository for license terms.
