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
**Not yet safe for real dotfiles.** A candid review flagged data-exposure,
non-destructive, and exit-code problems that disqualify it from real `$HOME` use until
fixed. Full findings — severity-ranked, with file references, test gaps, and a fix
order — are in `docs/reviews/2026-06-15-trust-review.md`. Read it before touching
`adopt`, the linker, or `apply`.

**Resolved (2026-06-15):** gist uploads deleted (P0#1/#2); `adopt` made atomic with
rollback + collision pre-checks (P0#4); `add`/`adopt` exit codes made honest (P0#7).
**Still open:** P0#3 (foreign symlinks clobbered in `apply_single_link`) is the next
blocker, plus the P1/P2 items in the review doc.

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

Note: some current code still violates these (see the review doc). As of 2026-06-15 the
gist-upload, adopt-collision, and dishonest-exit-code violations are fixed; the remaining
violations are **foreign-symlink clobbering (P0#3)** in `apply_single_link`, and
**path-fragment validation (P1#6)** is not yet implemented. When fixing, bring the code
into line — don't preserve the violation.

## Quality bar
Target: zero warnings. For a change to count as "done", `cargo fmt` and `cargo clippy`
(`-D warnings`) must be clean — this tool mutates `$HOME`. (As of the review, both fail
on existing code; clearing that debt is itself an open task, not a reason to skip the
bar on new code.)

## Workflow
```sh
cargo build
cargo test          # unit + CLI integration tests
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```
