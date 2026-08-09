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

**Config/state split (v0.3, shipped, breaking):** human-authored config (`stitch.toml`, repo root) is separated from tool-generated desired state (`.stitch/state.toml`). The tool never rewrites the authored file after `init`; `add`/`remove` write `state.toml` only. Motivation: v0.2's single-file `.stitch/config.toml` was reserialized (`to_string_pretty`) on every mutation, silently destroying comments. Load merges the two by store name. Multi-target `targets` is a name-keyed `BTreeMap` — **the name is the map key**, not a field on `TargetEntry` (a redundant field would desync). See SPEC.md §Config.

## Platform
**Linux only.** Built on POSIX symlinks; does not compile on Windows. macOS is not
officially supported or tested. `when.os` mirrors `std::env::consts::OS`.

## ⚠️ Trust status
**All P0, P1, and P2 issues from the 2026-06 trust review are resolved.** Each red
line below is now upheld by the code. Full findings — severity-ranked, with file
references, test gaps, and a fix order — are in `docs/reviews/2026-06-15-trust-review.md`.
Read it before touching `add`, the linker, or `apply`.

Oracle #2 re-review (2026-06-17) confirmed all blockers resolved and would
personally use stitch for real dotfiles. Four hardening items were also
resolved in the same pass; see `docs/plans/p2-and-hardening.md`.

No known trust blockers remain.

## Constraints & red lines
`stitch` mutates `$HOME`. The core safety contract: **never surprise the user with data
movement, data exposure, or silent replacement.** New code must uphold:
- **No external upload by default.** Backups/snapshots stay local unless an explicit
  opt-in flag is set.
- **Foreign symlinks are conflicts, not replacements.** A symlink not pointing into this
  repo is never silently clobbered. Ownership is two-tier: the broad `points_into_repo`
  follows the symlink chain *canonically* (so a link pointing *through* a repo gateway
  symlink to an external path is foreign, not repo-owned); the exact-entry `points_at_source`
  handles the special case where the configured source is itself a symlink resolving outside
  the repo (a stitch-created link pointing directly at that source entry is still
  stitch-owned). Use `points_into_repo` for broad decisions (apply Broken arm, scan/prune,
  wildcard `remove_link`) and `points_at_source`/`remove_link_to` where the configured
  source is known. (2026-08-08, shipped.)
- **Exit codes are honest.** `add`/`apply`/`diff`/`prune` return non-zero on real errors
  and conflicts (including partial removal failure in `prune --yes`).
- **Validate path fragments.** Reject absolute and `..`-containing file entries; nothing
  escapes the store/target dirs.
- **No destructive overwrite of existing repo content.** Collisions are rejected.
- **Filesystem race boundary:** stitch rejects hostile state already present when an operation starts and revalidates after hooks immediately before mutation. A malicious same-UID process racing the final filesystem syscall is out of scope (it can already mutate the same files directly). Do not claim stronger guarantees in docs or tests.
- **No hidden quarantine mechanism.** Successful operations must not leave `.quarantine.*` links/files or retain old rendered secrets. User-data replacement uses the explicit, visible `.bak` behavior; tool-owned staging uses exclusive temporary files and atomic rename.
- **Authored config is read-only to the tool.** After `init`, stitch never rewrites `stitch.toml`. Mutations touch `.stitch/state.toml` only — never silently destroy the user's comments or formatting. (v0.3 split, shipped.)
- **`prune` is list-only by default.** It walks `$HOME` for repo-pointing symlinks no store references; removal requires explicit `--yes`. Removal routes through the `points_into_repo`-guarded `remove_link`, so foreign symlinks are never touched and a link repointed between scan and unlink is skipped. (v0.3.1, shipped.)

Note: as of 2026-06-15 every red-line violation flagged in the review is resolved.
When touching `add`, the linker, `apply`, or config parsing, keep the code in line
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

## Releases
A release is a commit titled `Release vX.Y.Z` that bumps `version` in `Cargo.toml`.
**Every release commit must also be tagged** with an annotated tag of the same name
(`vX.Y.Z`); a version bump without a tag is incomplete. The tag is created on the
release commit itself, not on a later follow-up.

```sh
# after the Release vX.Y.Z commit lands on main:
git tag -a vX.Y.Z <release-commit> -m "vX.Y.Z: <one-line summary>"
git push origin main && git push origin vX.Y.Z
```

Pushing the `v*` tag triggers `.github/workflows/release.yml`, which builds the
release binaries, creates the GitHub release (title + CHANGELOG notes), and
attaches the artifacts. Supported build targets: `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu` (Linux only — macOS is not officially supported).
Artifacts: `stitch-vX.Y.Z-<target>.tar.gz` plus a `.sha256` sidecar.

`scripts/gh-release.sh` is now a manual fallback for re-publishing notes; it is
idempotent (edits an existing release instead of failing). The workflow reads
the one-line summary from the annotated tag's subject, so the tag message
format above matters.

Existing tags: `v0.2.0` (d35496a), `v0.3.0` (76fc01f), `v0.3.1` (6d10de3),
`v0.4.0`, `v0.4.1`, `v0.5.0`, `v0.6.0` (23f2dbd), `v0.7.0`.
