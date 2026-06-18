# Trust review — 2026-06-15

Source: Oracle review (`T-019ecb9c-abb9-7067-9581-4083dcff2431`). Candid "would you use
this for real dotfiles?" assessment. Conclusion: **no**, not yet. ~1–2 days of focused
work to earn trust, not a rewrite. The core symlink idea and tempdir test suite are
solid foundations.

This doc records the *current-state* findings with file/function references. The durable
safety principles derived from it live in the root `AGENTS.md`. When you fix items below,
update or strike them here.

**Resolved 2026-06-15:** P0#1, P0#2, P0#3, P0#4, P0#7, and P1#6 — gist machinery deleted, adopt
made atomic with rollback + collision pre-checks, `add`/`adopt` exit codes made honest
(`apply`/`diff` were already correct), foreign symlinks are now conflicts rather
than silent clobbers, and path fragments are validated at config load (rejecting
absolute and `..`-containing `files`/`patterns` entries). All P0 blockers and the
P1 path-traversal gap are clear; see commit history. Remaining: the P1/P2 items below.

State at review time: 17 unit + 35 CLI tests pass. `cargo fmt --check` fails.
`cargo clippy --all-targets --all-features -- -D warnings` fails (style nits + one
`too_many_arguments`).

---

## P0 — block real use

### 1. ~~`adopt` uploads to GitHub Gist by default~~ ✅ RESOLVED
Gist machinery (`src/snapshot.rs`, the `undo` command) was deleted. `adopt` no longer
makes any network call. Git (the repo the file is moved into) is the historical record.

### 2. ~~Tests have external side effects~~ ✅ RESOLVED
Removed with the gist code. `cargo test` is now local-only.

### 3. ~~Non-owned symlinks are silently replaced~~ ✅ RESOLVED
- `apply_single_link` now checks `linker::points_into_repo(target, repo_root)` on the
  `Broken` arm. A broken/mismatched symlink that points into this repo (stale stitch
  state — store moved or a file renamed) is self-healed by relinking; one pointing
  elsewhere (stow/chezmoi/Nix/Home-Manager/hand-managed, or a dangling user link) is
  reported as `Conflict` and left untouched. The `remove_file` error is now propagated
  as `ApplyAction::Error` instead of being swallowed. The `dry_run` path mirrors this
  (foreign → `Conflict`, not `Replaced`), so `stitch diff` no longer misleads.
- `check_link` is deliberately left returning `Broken` for any non-matching symlink:
  `status`/`doctor` report the honest user-facing state ("broken") regardless of
  ownership; only `apply`'s *action* (replace vs conflict) is ownership-aware. Covered
  by `apply_replaces_repo_owned_broken_symlink`, `apply_conflicts_on_foreign_symlink`,
  and `apply_conflicts_on_dangling_foreign_symlink`.

### 4. ~~`adopt` can overwrite / half-mutate state~~ ✅ RESOLVED
`cmd_adopt` now: (a) pre-checks collisions (store name in config, store dir exists)
  before any mutation; (b) reorders to move → link → record, rolling back on any
  failure via `rollback_adopt_move` (which handles file vs dir mode correctly); (c)
  returns non-zero if the link step reports conflict/error.

---

## P1 — must fix before trust

### 5. ~~`apply --force` parsed but unused~~ ✅ RESOLVED
- `--force` now backs up real-file/dir conflicts to `{target}.bak` and links
  in place (`force_backup_link` in `src/store.rs`). Threaded through the apply
  chain via an `ApplyOpts { dry_run, force }` struct (avoids `too_many_arguments`
  on `apply_target`). A new `ApplyAction::BackedUp { target, backup }` variant
  is counted in the summary. Foreign symlinks remain hard conflicts even under
  `--force` (P0#3 guarantee untouched — they surface as `Broken`, never
  `Conflict`). `diff --force` previews the backup. If `{target}.bak` already
  exists, `--force` fails loudly (`symlink_metadata` catches dangling
  symlinks too, since `rename(2)` would atomically replace them). On a
  link-step failure after the rename, the backup is restored. Covered by
  `apply_force_backs_up_real_file_and_links`,
  `apply_force_backs_up_real_directory`, `apply_force_fails_when_bak_already_exists`,
  `apply_force_does_not_clobber_foreign_symlink`, `diff_force_reports_backup_without_changing`.

### 6. ~~Path traversal unguarded in file mode~~ ✅ RESOLVED
- `Config::load` now calls `Config::validate`, which rejects any `files`/`patterns`
  entry (on a `Store` **or** a `TargetEntry`) that is absolute or contains a `..`
  component. Validation is lexical via `Path::components()` (TOCTOU-free, works for
  not-yet-existing entries); nested relative paths like `config/app.conf` remain
  valid. `cmd_add` validates its `--file`/`--pattern` args before creating the store
  dir, so a bad fragment can't escape during apply or leave an orphan dir. Covered by
  unit tests (`is_safe_fragment` truth table, per-store/per-target reject cases) and
  CLI tests (`apply_rejects_traversal_in_files`, `apply_rejects_absolute_in_files`,
  `apply_allows_nested_file_entries`, `add_rejects_traversal_in_files`).

### 7. ~~`add`/`adopt` print errors but return success~~ ✅ RESOLVED
Both now return non-zero when `apply_store` reports `Conflict`/`Error`. (`apply`/`diff`
were already honest.)

### 8. ~~SPEC ↔ implementation drift~~ ✅ RESOLVED
Missing vs SPEC: `import`, `modify`, hook execution (per-store + global), full ignore
behavior, global ignores (`.git`/`.stitch`/`.DS_Store`), whole-dir→file-mode promotion
when ignored content exists, a distinct `diff` report
(`ok`/`create`/`conflict`/`replace`). Gist snapshot behavior isn't represented in
the SPEC's `adopt` contract.

Additional, verified against source: the `add` flags are `--file`/`-f` and
`--target-flag`/`-t` in code but documented as `--files`/`--target` (`cli.rs`). The
code names look like the bug; SPEC is likely the intended contract.
- **Fix:** implement v0.2 behavior, or mark it clearly future/unsupported. Effort: L.

**Resolved by (2026-06-15):** Implemented — hook execution (per-store `pre`/`post`
+ global `.stitch/hooks/` executables, with `STITCH_*` env vars; new `src/hooks.rs`);
global ignores (`.git`, `.stitch`, `.gitignore`, `.DS_Store`) now always active with
whole-dir→file-mode promotion when ignored content is present; `add` flag naming
aligned to SPEC (`--files`/`-f`, `--target`/`-t` via a clap `ArgGroup`); dangling
`stitch import` reference removed from `cmd_adopt`; review-doc `undo` contradiction
struck. Deferred items (`import`, `modify`, distinct `diff` report format) are now
explicitly marked future on the v0.2 roadmap. Commits `fac97d2` (A/H), `105acd3` (G),
`a781530` (D/E), `118d834` (C).

---

## P2 — important, not existential

### 9. Glob resolution is non-recursive
- `resolve_files()` uses `read_dir(store_dir)` — top-level only. Clashes with ignore
  examples like `scratch/` and `.git/**`.
- **Fix:** `walkdir` + match on paths relative to `store_dir`. Effort: S–M.

### 10. Config writes are non-atomic
- `Config::save()` → `toml::to_string_pretty`: reorders stores (`HashMap`), strips
  comments, corruption risk if interrupted.
- **Fix:** atomic temp-file + rename now (S); comment-preserving edits later (L).

### 11. Unknown store names are silent no-ops
- `status <name>` / `apply --only <name>` don't error on unknown names — a typo does
  nothing.
- **Fix:** hard error. Effort: S.

### 12. (Was Windows portability in the original review — now resolved by scope.)
Project is scoped Linux-only (see root `AGENTS.md` / `SPEC.md`). The `std::os::unix::fs::symlink`
usage and Unix-only tests are correct for scope, not a defect.

---

## Known bug (outside the review)

- **`when.os` value mismatch.** `std::env::consts::OS` returns `macos` on macOS, not
  `darwin`. A `when = { os = "darwin" }` store would never match. Out of scope while
  Linux-only, but flag if macOS support is ever claimed — use `macos`. Also the
  unreachable `"windows" => Some("windows".into())` distro branch in `src/platform.rs`
  is dead code under the Linux-only scope.

---

## Test gaps (high-value)

1. ✅ `apply --force` actually creates `.bak` and preserves conflict content. *(covered: `apply_force_backs_up_real_file_and_links`, `apply_force_backs_up_real_directory`, `apply_force_fails_when_bak_already_exists`, `apply_force_does_not_clobber_foreign_symlink`, `diff_force_reports_backup_without_changing`)*
2. ✅ Existing symlink → another manager is treated as conflict, not replaced. *(covered: `apply_conflicts_on_foreign_symlink`)*
3. ✅ Broken/dangling symlink outside the repo is not blindly clobbered. *(covered: `apply_conflicts_on_dangling_foreign_symlink`; repo-owned self-heal covered by `apply_replaces_repo_owned_broken_symlink`)*
4. ✅ `adopt` rejects store/config collisions; does not overwrite existing repo files; fails
   if relinking fails. *(covered: `adopt_rejects_store_name_already_in_config`,
   `adopt_rejects_when_store_dir_already_exists`, `adopt_rolls_back_file_when_record_fails`)*
5. ✅ `add` returns failure on apply conflict/error.
6. ✅ `files = ["../x"]` / absolute file entries are rejected. *(covered: `apply_rejects_traversal_in_files`, `apply_rejects_absolute_in_files`, `add_rejects_traversal_in_files`; unit `test_is_safe_fragment` / `test_validate_*`)*
7. Recursive glob + ignore behavior.
8. Multi-target `apply`/`status`/`doctor` (not just `list`).
9. Unknown store names fail.
10. ✅ Snapshot code is mocked; tests never hit GitHub. *(moot — snapshot code removed)*

---

## Recommended fix order (all completed as of 2026-06-15)

1. ~~Disable Gist snapshots by default~~ ✅ (`387488f`)
2. ~~Harden linker semantics~~ ✅ (`de07218`)
3. ~~Honest exit codes~~ ✅ (`387488f`, `7a3eeb1`)
4. ~~Implement or remove `--force`~~ ✅ (`b28bed8`)
5. ~~Collision + path-traversal validation~~ ✅ (`387488f`, `a941c1a`)
6. ~~Mock snapshotting in tests~~ ✅ (moot — snapshot code deleted)
7. ~~Close or explicitly trim SPEC gaps~~ ✅ (`fac97d2`, `105acd3`, `a781530`, `118d834`, `5cd715a`, `2196237`)
