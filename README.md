# stitch

Keep your Linux config files in one place — and keep them in sync.

`stitch` takes the scattered files in your home directory (`~/.bashrc`, `~/.config/nvim`, `~/.gitconfig`…) and links them back to a single folder you control (like `~/dots`). Edit the file in your home dir, and you're really editing the file in your repo. No copying, no re-adding, no drift.

If you've ever wanted to back up your dotfiles, move them to a new machine, or just stop losing them — this is the tool.

> **New here?** Dotfiles are just hidden config files — the ones starting with `.` in your home folder. Stitch puts them in a Git repo so you can track and restore them.

**Linux only.** Built on standard Linux links. Not tested on macOS, doesn't run on Windows.

---

## Get it

### Option 1 — Grab a ready-made binary (easiest)

No Rust needed. Go to the **[Releases page](https://github.com/bermudi/stitch/releases)** and download the file for your computer.

- Intel/AMD PC or laptop → `stitch-v0.10.0-x86_64-unknown-linux-gnu.tar.gz`
- Raspberry Pi / ARM server → `stitch-v0.10.0-aarch64-unknown-linux-gnu.tar.gz`

> Tip: Not sure which you need? Run `uname -m` — it prints `x86_64` for most desktops, `aarch64` for ARM.

Then install it:

```sh
# example for x86_64 — swap the filename if you grabbed the ARM one
# (check the Releases page for the latest version number)
curl -LO https://github.com/bermudi/stitch/releases/download/v0.10.0/stitch-v0.10.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf stitch-v0.10.0-x86_64-unknown-linux-gnu.tar.gz
sudo mv stitch /usr/local/bin/
stitch --help
```

Each download has a `.sha256` file next to it if you want to verify it. You can always find the newest version at **https://github.com/bermudi/stitch/releases/latest**.

### Option 2 — Build from source

Need Rust first (2024 edition). Then:

```sh
cargo install --path .
# or, to try it without installing:
cargo build --release
./target/release/stitch --help
```

---

## Your first 5 minutes

```sh
# 1. Make a folder for your dotfiles
mkdir ~/dots && cd ~/dots

# 2. Set it up — creates two small config files (see "How it works" below)
stitch init

# 3. Add something you already have — say your Neovim config
#    This moves ~/.config/nvim INTO ~/dots and leaves a link behind
stitch add ~/.config/nvim

# 4. Or create a new empty file-backed store for a file that doesn't exist yet
stitch add ~/.bashrc --file

# 5. Put the links in place (and re-run this any time you change config)
stitch apply

# 6. Check that everything looks good
stitch status
```

What just happened?

```
Before:  ~/.config/nvim  (real folder on disk)
After:   ~/.config/nvim  →  ~/dots/nvim  (a link pointing back to your repo)
```

You edit `~/.config/nvim/init.lua` like normal — your editor follows the link and saves straight into `~/dots/nvim/init.lua`. Commit and push `~/dots` and your setup is backed up.

To undo one thing: `stitch remove nvim` removes the links and the bookkeeping, but leaves the files in your repo alone. `remove` never deletes the store directory in the repo, so to re-`add` the same store you either keep the old directory (rename it first, then `add` the original path) or remove it yourself (`rm -rf ~/dots/nvim`) before running `add` again — `add` refuses to adopt into an existing store directory.

---

## How it works (the 30-second version)

- **Store** = a folder in your repo, like `~/dots/nvim` or `~/dots/shells`. One unit of stuff to manage.
- **Target** = where it should appear in your home, like `~/.config/nvim` or `~`.
- **Link** = stitch makes the target point back at the store. Edits at the target land in the repo — no second step.

Stitch keeps two config files so your comments never get wiped out:

- **`stitch.toml`** (in your repo root) — **yours.** You write it, you keep the comments. Things like `when` (only on certain machines) and `hooks`. Stitch creates it once at `init` and then never touches it again.
- **`.stitch/state.toml`** (hidden folder) — **stitch's.** The list of *what* goes *where* (`target`, `files`). `add` and `remove` update this one.

That's it. Change the config, run `stitch apply` — stitch makes the filesystem match.

<details>
<summary>Example of the two files</summary>

`stitch.toml` — your choices:

```toml
[vars]
editor = "nvim"

[stores.shells.when]
os = "linux"

[stores.git]
hooks = { post = "git config --global core.editor nvim" }
```

`.stitch/state.toml` — stitch's inventory:

```toml
[stores.nvim]
target = "~/.config/nvim"

[stores.shells]
target = "~"
files = [".bashrc", ".zshrc"]

[stores.git]
target = "~/.config/git"
```

</details>

---

## Common commands

**Everyday:**

| Command | What it does |
|---|---|
| `stitch add <path>` | Bring a file/folder into the repo and link it back. Use `--file` for a missing single file. Use `--to <store>` to add an existing file to a grouped store. Existing repo paths and unsafe target/destination parents are rejected; `--dry-run` checks them without changing anything. |
| `stitch apply` | Make your home match the config. Creates/fixes links, reports anything in the way. |
| `stitch status` | Is everything linked? Shows `linked`, `missing`, `conflict`, or `broken`. |
| `stitch diff` | Preview what `apply` would do, without changing anything. Add `--exit-code` for a scriptable exact-state check. |

**Occasionally:**

| Command | What it does |
|---|---|
| `stitch list` | Show every store and where it points. |
| `stitch remove <name>` | Unlink and forget a store (your files stay in the repo). |
| `stitch edit` | Open `stitch.toml` in your editor. `stitch edit nvim` opens that store's file. |
| `stitch doctor` | Health check — missing folders, broken links, leftover settings. |
| `stitch prune` | List links pointing into your repo that nothing uses anymore. Add `--yes` to actually remove them. |
| `stitch import` | Found old hand-made links pointing into your repo? Register them. |
| `stitch plan` | Save exactly what `apply` would do to a file, so you can review and run `apply --plan <file>` later. |

`apply`, `add`, and `remove` support `--dry-run` (just pretend); `diff` is always a preview. `apply` and `diff` take `--only <name>` to act on one store, and `apply` takes `--force` to back up a real file to `*.bak` before replacing it. Add `--json` to read/plan commands, or to supported dry-run/reporting commands such as `add --dry-run`, for machine-readable output.

> Want the full details? See **[SPEC.md](SPEC.md)** — every flag, edge case, and design choice is there.

---

## Templates (optional)

Need a config that changes per machine? Rename a file to end in `.tmpl` and use `{{ }}` inside:

```
# git/gitconfig.tmpl
[user]
    name = {{ hostname }}
    email = {{ vars.email }}
[core]
    editor = {{ env("EDITOR", "nvim") }}
```

On `apply`, stitch renders it to `.stitch/render/` (locked down, not tracked by Git) and links the target without the `.tmpl` ending. Edit the `.tmpl` source, not the rendered file.

Available inside templates: `{{ env("VAR") }}`, `{{ vars.key }}`, `{{ hostname }}`, `{{ os }}`, `{{ arch }}`, `{{ distro }}`, `{{ shell }}`.

See SPEC.md §Templates for the full list.

---

## Safety — how stitch treats your home

Stitch changes files in `$HOME`, so it's careful:

- **Never uploads anything.** Backups stay on your disk.
- **Never quietly replaces a link it didn't make.** If a link points at stow, chezmoi, Nix, etc., stitch calls it a conflict and stops — even with `--force`.
- **`--force` backs up, not deletes.** A real file in the way gets renamed to `file.bak` next to the original. If `.bak` already exists, it stops rather than pile up.
- **Won't follow trick paths.** Entries with `/` at the start or `..` inside are rejected — nothing escapes your store or target.
- **`prune` only lists by default.** It shows orphaned links; it only deletes with `--yes`.

---

## Keep learning

- **[SPEC.md](SPEC.md)** — the full contract: every command, config field, hook, and conflict rule.
- **[CHANGELOG.md](CHANGELOG.md)** — what's new in each release (including the trust-review fixes).
- **[Releases](https://github.com/bermudi/stitch/releases)** — download binaries and read release notes.
- **[AGENTS.md](AGENTS.md)** — contributor notes (architecture, safety rules).
- **[docs/reviews/2026-06-15-trust-review.md](docs/reviews/2026-06-15-trust-review.md)** — the deep safety review this project was built around.

---

## Building & testing (for contributors)

```sh
cargo build
cargo test          # unit + CLI integration tests
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

A change only counts as done when `cargo fmt` and `cargo clippy -D warnings` are clean.

## License

Personal-use project. See repository for license terms.
