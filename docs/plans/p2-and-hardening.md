# P2 resolution + hardening plan

Source: Oracle #1 + #2 trust reviews (`T-019ecb9c-abb9-7067-9581-4083dcff2431`).
All P0/P1 blockers resolved as of 2026-06-15 and re-verified 2026-06-17. This doc
covers the remaining P2 items (feature/spec gaps, not trust-blockers) and the
defense-in-depth hardening items Oracle #2 surfaced.

State: `cargo build`, `cargo fmt --check`, `cargo clippy -D warnings`, and
`cargo test` (106 tests) all pass.

---

## Recommended order

| # | Item | Size | Category | Status |
|---|---|---|---|---|
| 1 | H3: SPEC templates wording | S | Docs | ✅ 2026-06-17 |
| 2 | H4: `when.os` darwin/macos comment + dead Windows code | S | Code hygiene | ✅ 2026-06-17 |
| 3 | P2#11: Unknown store names error | S | UX correctness | ✅ 2026-06-17 |
| 4 | H2: `add` rejects existing unconfigured store dir | S | Safety | ✅ 2026-06-17 |
| 5 | H1: `points_into_repo` `..` normalization | S | Defense-in-depth | ✅ 2026-06-17 |
| 6 | P2#10: Atomic config writes | S | Data safety | ✅ 2026-06-17 |
| 7 | P2#9: Recursive glob resolution | S–M | Feature completeness | ✅ 2026-06-17 |

Rationale: easy docs/code-hygiene wins first (build momentum), then the
correctness+UX items, then the two feature changes. Atomic config writes before
recursive glob because config writes happen on every management command; glob
only matters for pattern-based file-mode stores.

---

## 1. H3: SPEC templates wording ✅ DONE (2026-06-17)

**Problem:** The "Templates & secrets (v0.3)" section used present tense
("files are rendered", "secrets stored encrypted") despite being an unchecked
v0.3 roadmap item. Readers would assume it works today.

**What to change:**
- `SPEC.md` §Templates & secrets: rewrite in future/planned tense, add an
  explicit "planned, not yet implemented" disclaimer.

**Test:** visual review of SPEC.md.

---

## 2. H4: `when.os` darwin/macos comment

**Problem:** `std::env::consts::OS` returns `macos` on macOS, not `darwin`.
A config using `when = { os = "darwin" }` would silently never match. (The
dead Windows distro branch flagged in the original review was already removed
in a prior commit — nothing to do there.)

**What to change:**
- `src/platform.rs`: add a doc comment on `Platform::detect()` noting the
  `macos` vs `darwin` trap.
- `SPEC.md` §Platform conditionals: note that `os` values mirror
  `std::env::consts::OS` (i.e. `linux`, `macos`, `windows`).

**Files touched:** `src/platform.rs`, `SPEC.md`.

**Test:** `cargo build` passes. No behavior change on Linux.

---

## 3. P2#11: Unknown store names should error

**Problem:** `stitch apply --only typo`, `stitch status typo`, `stitch diff
--only typo` silently succeed with no output — a typo does nothing with no
feedback.

**What to change:**
- In `cmd_apply`: after filtering stores by `--only`, check that every
  requested name matched at least one store. If not, return an error listing
  unknown names.
- In `cmd_status`: same logic for named status lookups.
- In `cmd_diff`: same for `--only` filtering.
- All three should error *before* any filesystem mutation (apply/diff filters
  are already pre-mutation; status has no mutation).

**Files touched:** `src/main.rs`.

**Tests:**
- `apply --only nonexistent` exits non-zero with "unknown store" message.
- `status nonexistent` exits non-zero.
- `diff --only nonexistent` exits non-zero.
- `apply --only foo --only bar` where both are unknown reports both names.
- `apply --only real --only fake` reports only "fake" and still applies "real"
  (or error policy — decide: partial is confusing; I'd error on any unknown
  and abort the whole apply).

---

## 4. H2: `add` rejects existing unconfigured store directory

**Problem:** `cmd_add` checks `config.stores.contains_key(name)` but not
whether `root.join(name)` already exists on disk. `create_dir_all` is a no-op
on existing directories, so no data loss — but if the directory has existing
content, the "add creates a fresh store" contract is fuzzy.

**What to change:**
- In `cmd_add`, before `create_dir_all`, check `root.join(name).symlink_metadata()`.
  If the path exists (file, dir, or symlink), return an error like
  `"store directory 'X' already exists"`.
- This should fire *after* the config-name check and *before* fragment
  validation (no point validating fragments for a store we won't create).

**Files touched:** `src/main.rs`.

**Tests:**
- `stitch add foo` where `foo/` already exists on disk → non-zero, "already exists".
- `stitch add foo` where `foo` is an existing file → non-zero.
- `stitch add foo` where `foo` is an existing symlink → non-zero.
- `stitch add foo` where `foo` is in config but dir doesn't exist → existing error
  path fires first ("already exists in config"), no regression.

---

## 5. H1: `points_into_repo` `..` normalization

**Problem:** `linker::points_into_repo` resolves the symlink target and checks
`starts_with(repo_root)` without normalizing `.` or `..` components. A crafted
symlink target like `/home/user/repo/../.ssh` would be misclassified as
repo-owned (it starts with the repo prefix lexically) when it actually points
outside the repo.

This is not a real-world threat for a personal tool where the user controls
both the repo and the symlinks, but it's a defense-in-depth gap worth closing.

**What to change:**
- In `points_into_repo`, after resolving the symlink target to an absolute path:
  - If the resolved path exists: canonicalize it, then `starts_with` the
    canonicalized `repo_root`.
  - If the resolved path does not exist (dangling symlink): lexically normalize
    `.` and `..` components using `Path::components()` iteration, then
    `starts_with` the similarly normalized `repo_root`.
- Add a helper `normalize_lexical(path: &Path) -> PathBuf` that collapses
  `.` and `..` without touching the filesystem.

**Files touched:** `src/linker.rs`.

**Tests (unit, in `linker::tests`):**
- `points_into_repo` with path containing `..` that escapes → false.
- `points_into_repo` with path containing `..` that stays inside → true.
- `points_into_repo` with `.` components → true/false as appropriate.
- `points_into_repo` with a dangling path containing `..` escape → false.
- `points_into_repo` with a real path containing a symlink chain (canonicalize
  resolves it correctly) → true/false based on resolved path.

---

## 6. P2#10: Atomic config writes

**Problem:** `Config::save()` serializes and writes directly with
`std::fs::write`. On interruption (crash, power loss), the config file can be
truncated or corrupted. Also, `toml::to_string_pretty` reorders stores
(`HashMap` iteration order is non-deterministic) and strips comments — annoying
for a hand-maintained config, but comment preservation is a larger effort.

**What to change (phase 1 — atomicity only):**
- In `Config::save()` in `src/config.rs`:
  1. Serialize to string as before.
  2. Write to a temp file `<path>.tmp` in the same directory.
  3. `std::fs::rename` the temp file to the final path (atomic on Linux
     for same-filesystem renames).
  4. On any error, attempt to remove the temp file.
- The temp file should be created with `tempfile::NamedTempFile::new_in()` or
  manually with a non-colliding name (e.g. `<path>.<pid>.tmp`).

**Phase 2 (future — comment preservation):** Replace `toml::to_string_pretty`
with `toml_edit` for in-place edits, or accept that management commands
normalize the TOML formatting. Lower priority for a personal tool.

**Files touched:** `src/config.rs`.

**Tests:**
- Normal save → config file unchanged in content, just written atomically.
- Kill test (optional, integration): write large config, kill process mid-write,
  verify original file intact (the temp-file+rename pattern makes this
  unnecessary to test — `rename(2)` is atomic by spec).

---

## 7. P2#9: Recursive glob resolution

**Problem:** `store::resolve_files()` uses `std::fs::read_dir(store_dir)` —
top-level only. Glob patterns and ignore rules cannot match nested paths.
This limits file-mode stores to flat directories and makes ignore patterns
like `scratch/` or `.git/**` syntactic only.

**What to change:**
- Replace `read_dir` in `resolve_files()` with `walkdir::WalkDir` over
  `store_dir`.
- For each entry, compute the path relative to `store_dir` and match it
  against the globset. The file name matched against patterns should be the
  relative path (e.g. `subdir/config.toml`), not just the leaf name.
- Exclude directories from results (only files get symlinked). If a glob
  pattern intentionally matches a directory, that's a user error — skip with
  a warning or treat as no-match.
- The ignore globset should also match against the relative path.
- Ensure `walkdir` skips `.git` and `.stitch` (already covered by global
  ignores, but `walkdir` has its own filtering — align them).
- The project already depends on `walkdir` (it's in `Cargo.toml`), so this is
  about using it, not adding a dep.

**Files touched:** `src/store.rs`.

**Tests:**
- File-mode store with nested directory structure → files matched by glob
  at any depth.
- Ignore pattern `scratch/` excludes all files under `scratch/`.
- Ignore pattern `*.bak` excludes `.bak` files at any depth.
- Pattern `**/*.toml` matches nested files.
- Empty directories produce no matches (no error).
- Whole-directory mode is unaffected (doesn't use `resolve_files`).

---

## Completion criteria

All items are done when:
- `cargo build` passes
- `cargo fmt --check` passes
- `cargo clippy --all-targets --all-features -- -D warnings` passes
- `cargo test` passes (existing + new tests)
- Trust review doc updated with resolutions
