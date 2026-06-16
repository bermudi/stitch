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
rollback + collision pre-checks (P0#4); `add`/`adopt` exit codes made honest (P0#7);
foreign symlinks treated as conflicts, not clobbers (P0#3); path fragments validated
at config load — absolute and `..`-containing `files`/`patterns` entries are rejected
(P1#6); `apply --force` implemented as `{target}.bak` backup for real-file/dir
conflicts, foreign symlinks still hard conflicts, fails loudly if `.bak` exists (P1#5).
All P0 blockers and the resolved P1 items are clear; every red line below is now upheld
by the code. **Still open:** P1#8 SPEC/impl reconciliation, and the P2 items
(recursive glob P2#9, atomic config writes P2#10, unknown-store-name errors P2#11)
in the review doc.

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

Note: as of 2026-06-15 every red-line violation flagged in the review is resolved —
gist-upload, adopt-collision, dishonest-exit-code, foreign-symlink-clobbering, and
path-fragment-validation (P1#6) are all fixed. `apply --force` (P1#5) is now
implemented as a local `.bak` backup (never external upload, foreign symlinks stay
conflicts). The remaining open items (P1#8 SPEC reconciliation, the P2s) are
feature/spec gaps, not red-line breaches. When touching `adopt`, the linker, `apply`,
or config parsing, keep the code in line with these red lines — don't reintroduce a
violation.

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
