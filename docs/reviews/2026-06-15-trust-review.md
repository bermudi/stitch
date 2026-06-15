# Trust review — 2026-06-15

Source: Oracle review (`T-019ecb9c-abb9-7067-9581-4083dcff2431`). Candid "would you use
this for real dotfiles?" assessment. Conclusion: **no**, not yet. ~1–2 days of focused
work to earn trust, not a rewrite. The core symlink idea and tempdir test suite are
solid foundations.

This doc records the *current-state* findings with file/function references. The durable
safety principles derived from it live in the root `AGENTS.md`. When you fix items below,
update or strike them here.

**Resolved 2026-06-15:** P0#1, P0#2, P0#4, P0#7 — gist machinery deleted, adopt made
atomic with rollback + collision pre-checks, `add`/`adopt` exit codes made honest
(`apply`/`diff` were already correct). See commit history. Remaining blockers: P0#3
(foreign symlinks clobbered in `apply_single_link`), plus the P1/P2 items below.

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

### 3. Non-owned symlinks are silently replaced
- `linker::check_link()` returns `Broken` for any symlink not resolving to the expected
  source; `store::apply_single_link()` removes it and relinks.
- A stow/chezmoi/Nix/Home-Manager symlink at the target gets clobbered with no
  backup/confirm — contradicting `LinkStatus::Conflict`'s own doc comment ("Something
  else (file, dir, different symlink) occupies the path").
- `store.rs` swallows the `remove_file` error when replacing broken links.
- **Fix:** only auto-replace links known to point into this repo; otherwise report
  conflict. Propagate remove errors. Effort: M. **← next blocker**

### 4. ~~`adopt` can overwrite / half-mutate state~~ ✅ RESOLVED
`cmd_adopt` now: (a) pre-checks collisions (store name in config, store dir exists)
  before any mutation; (b) reorders to move → link → record, rolling back on any
  failure via `rollback_adopt_move` (which handles file vs dir mode correctly); (c)
  returns non-zero if the link step reports conflict/error.

---

## P1 — must fix before trust

### 5. `apply --force` parsed but unused
- `src/main.rs` accepts `force`, passes `_force`, ignores it. SPEC says `--force`
  auto-creates `.bak` backups for conflicts. A no-op safety flag misleads scripted users.
- **Fix:** implement backups, or remove the flag/claim. Effort: S–M.

### 6. Path traversal unguarded in file mode
- `store_dir.join(file_name)` / `target_path.join(file_name)`. `../` or absolute entries
  escape the intended dirs. Config repo may be shared/malicious.
- **Fix:** reject absolute entries and any `..` component; allow nested paths only if
  they stay under store/target after normalization. Effort: S–M.

### 7. ~~`add`/`adopt` print errors but return success~~ ✅ RESOLVED
Both now return non-zero when `apply_store` reports `Conflict`/`Error`. (`apply`/`diff`
were already honest.)

### 8. SPEC ↔ implementation drift
Missing vs SPEC: `import`, `modify`, hook execution (per-store + global), full ignore
behavior, global ignores (`.git`/`.stitch`/`.DS_Store`), whole-dir→file-mode promotion
when ignored content exists, a distinct `diff` report
(`ok`/`create`/`conflict`/`replace`). `undo` exists but isn't in the SPEC command list.
Gist snapshot behavior isn't represented in the SPEC's `adopt` contract.

Additional, verified against source: the `add` flags are `--file`/`-f` and
`--target-flag`/`-t` in code but documented as `--files`/`--target` (`cli.rs`). The
code names look like the bug; SPEC is likely the intended contract.
- **Fix:** implement v0.2 behavior, or mark it clearly future/unsupported. Effort: L.

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

1. `apply --force` actually creates `.bak` and preserves conflict content.
2. Existing symlink → another manager is treated as conflict, not replaced.
3. Broken symlink outside the repo is not blindly clobbered (or behavior is documented + tested).
4. ✅ `adopt` rejects store/config collisions; does not overwrite existing repo files; fails
   if relinking fails. *(covered: `adopt_rejects_store_name_already_in_config`,
   `adopt_rejects_when_store_dir_already_exists`, `adopt_rolls_back_file_when_record_fails`)*
5. ✅ `add` returns failure on apply conflict/error.
6. `files = ["../x"]` / absolute file entries are rejected.
7. Recursive glob + ignore behavior.
8. Multi-target `apply`/`status`/`doctor` (not just `list`).
9. Unknown store names fail.
10. ✅ Snapshot code is mocked; tests never hit GitHub. *(moot — snapshot code removed)*

---

## Recommended fix order

1. Disable Gist snapshots by default (local backups or explicit opt-in).
2. Harden linker semantics: foreign symlink = conflict; only replace repo-owned broken links.
3. Honest exit codes from `add` / `adopt` / `apply` / `diff`.
4. Implement or remove `--force`.
5. Collision + path-traversal validation in `adopt` / `add` / file mode.
6. Mock snapshotting in tests.
7. Close or explicitly trim the SPEC gaps.
