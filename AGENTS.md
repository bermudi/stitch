# AGENTS.md — stitch

## Project
A small Rust CLI dotfile manager. Config files live in one repo; `stitch` reads
`.stitch/config.toml` and symlinks stores into place (target → repo). "Config is truth":
`stitch apply` reconciles the filesystem to match the config. Personal-use project.
See `SPEC.md` for the full command/feature contract.

## Stack
Rust 2024 edition. `clap` (CLI), `serde`/`toml` (config), `walkdir` + `globset`
(file resolution), `dirs`, `thiserror`. `tempfile`/`assert_cmd`/`predicates` for tests.

## Architecture
A store is a top-level repo dir symlinked to an explicit target — whole-directory mode
(one symlink) or file mode (`files`/`patterns` link individual files in). `when` clauses
filter by platform/hostname/shell. Defining choice: symlinks point target→repo, so edits
always land in the repo directly — no source/target drift, no re-add step.

## Platform
**Linux only.** Built on POSIX symlinks; does not compile on Windows. macOS is not
officially supported or tested. `when.os` mirrors `std::env::consts::OS`.

## ⚠️ Trust status
**All P0 and P1 issues from the 2026-06 trust review are resolved.** Each red line below
is now upheld by the code. Full findings — severity-ranked, with file references, test
gaps, and a fix order — are in `docs/reviews/2026-06-15-trust-review.md`. Read it
before touching `adopt`, the linker, or `apply`.

The remaining open items are the P2s (recursive glob, atomic config writes,
unknown-store-name errors) — feature/spec gaps, not red-line breaches.

## Constraints & red lines
`stitch` mutates `$HOME`. The core safety contract: **never surprise the user with data
movement, data exposure, or silent replacement.** New code must uphold:
- **No external upload by default.** Backups/snapshots stay local unless an explicit
  opt-in flag is set.
- **Foreign symlinks are conflicts, not replacements.** A symlink not pointing into this
  repo is never silently clobbered.
- **Exit codes are honest.** `add`/`adopt`/`apply`/`diff` return non-zero on real errors
  and conflicts.
- **Validate path fragments.** Reject absolute and `..`-containing file entries; nothing
  escapes the store/target dirs.
- **No destructive overwrite of existing repo content.** Collisions are rejected.

Note: as of 2026-06-15 every red-line violation flagged in the review is resolved.
When touching `adopt`, the linker, `apply`, or config parsing, keep the code in line
with these red lines — don't reintroduce a violation.

## Quality bar
Zero warnings. `cargo fmt` and `cargo clippy` (`-D warnings`) must be clean for a
change to count as "done" — this tool mutates `$HOME`.

## Workflow
```sh
cargo build
cargo test          # unit + CLI integration tests
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```
