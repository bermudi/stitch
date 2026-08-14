# 2026-08-13 — Module refactor

Source: architectural review of `src/` (23,605 lines across 14 files).
Reviewed against two external critiques (2026-08-13); this revision
incorporates the verified findings. **Status: plan only — no code has
moved.** This doc exists to be reviewed before execution.

## Why

The crate grew from "small Rust CLI" to 23k lines without a module
reorganization. The size itself isn't the problem; the problems are:

1. **No command layer.** `main.rs` (5,222 lines) is CLI dispatch + every
   `cmd_*` implementation + the entire add/remove rollback machinery
   (~2,400 lines) + JSON DTOs + plan rendering + 11 integration tests. A
   change to `add` requires navigating past 4,000 lines of unrelated
   commands.

2. **`store.rs` (3,827 lines) is a second god file** doing four unrelated
   jobs: apply + plan computation (tightly coupled — see Phase 2),
   status, doctor, plus glob/ignore resolution. These share types but
   not logic.

3. **A module cycle: `store` ↔ `plan_exec`.** `store` imports
   `TargetAncestorSnapshot`/`TargetAncestorRedirect` from `plan_exec`;
   `plan_exec` imports `compute_plan`/`resolve_link_source`/
   `collect_reconciliation_keeps` from `store`. Legal in Rust (same crate)
   but an architecture smell. Root cause: `TargetAncestorSnapshot` is a
   filesystem safety primitive parked in `plan_exec` because that's where
   it was first needed.

4. **FS identity primitives are scattered.** `main.rs` owns
   `inode_identity`/`filesystem_identity`; `plan_exec.rs` has
   `directory_identity` doing the same thing with a different error
   type. There is no single place for "how we identify filesystem
   objects." (Note: `StateLock`/`atomic_write` are *not* generic FS
   primitives — they return `ConfigError` and encode `.stitch/`-specific
   semantics. They stay in `config/`.)

5. **`plan_exec.rs` (3,632 lines) mixes two concerns:** plan file
   format/construction (serialize/deserialize, `build_plan_file`,
   `compute_config_hash`) and plan execution (replay ops, preflight,
   validate). The ancestor snapshot logic (concern #3 above) is a third
   concern that gets extracted first.

This is a personal-use project, so the bar is "navigable by one person
six months from now," not "enterprise clean architecture." The goal is:
no file over ~2,000 lines (down from 5,222), no module cycles, FS
identity primitives in one place, each command in one file. Two files
exceed ~1,500 lines after the refactor (`commands/add.rs` ~2,000,
`render.rs` ~1,994) — each is justified below as a cohesive unit that
can't be split further without fragmenting a single command's logic or
a non-mechanical staging refactor.

## What does NOT change

- **No logic changes.** Pure code movement + import path fixes. If a bug
  surfaces during the move, it gets a separate commit with a separate
  test, not an inline fix.
- **No public API changes.** Every type keeps its name and visibility.
  Internal `pub(crate)` items may move between modules but keep their
  names.
- **No test assertion changes.** The 375 integration tests in `tests/cli/`
  are the safety net. The 273 inline unit tests move with their code.
  Phase 0 *adds* new characterization tests but does not modify existing
  test assertions.
- **No SPEC.md, AGENTS.md, or docs changes** (except this plan file).
- **No dependency changes.** `Cargo.toml` is untouched.

## Precondition

**Land or stash the uncommitted working-tree changes before starting.**
`git status` shows modifications to `main.rs`, `render.rs`, and
`store.rs` (an in-progress fix for skipped stores still running hooks).
The plan's core invariant — "each phase is a clean commit with a
balanced move-only diff" — requires a committed baseline. Re-count the
test baseline after landing; the current count is 648 (273 unit + 375
integration) but may shift.

## Target structure

```
src/
  main.rs                 ~150 lines: parse CLI, dispatch, error reporting
  cli.rs                  unchanged (484 lines)
  error.rs                unchanged (811 lines)
  platform.rs             unchanged (142 lines)
  hooks.rs                unchanged (219 lines)
  scan.rs                 unchanged (439 lines)
  safety.rs               unchanged (781 lines)
  plan.rs                 unchanged (714 lines)

  config/
    mod.rs                explicit re-export facade (see "Re-exports" below)
    types.rs              ~250 lines: Config, Store, TargetEntry, AuthoredConfig,
                          AuthoredStore, AuthoredTarget, GeneratedState,
                          GeneratedStore, GeneratedTarget, WhenClause, Hooks,
                          Loaded
    load.rs               ~400 lines: ConfigSnapshot, impl ConfigSnapshot,
                          path_exists, open_and_read_validated,
                          parse_authored_bytes, parse_state_bytes,
                          impl Config, impl Store, impl AuthoredConfig,
                          impl GeneratedState, validate_merged,
                          merge_store, merge_targets, validate_store_has_target,
                          validate_non_overlapping_targets, validate_store_names,
                          validate_fragments, validate_globs, validate_target,
                          normalize_fragment, normalize_fragment_lists,
                          normalize_ignores, skip_if_default
    state.rs              ~250 lines: atomic_write, validate_atomic_write_target,
                          StateLock, impl StateLock, impl Drop for StateLock,
                          validate_regular_file
                          (stays here — returns ConfigError, encodes .stitch/
                          semantics, not a generic FS primitive)
    legacy.rs             ~200 lines: LegacyConfig, LegacyStore,
                          LegacyTargetEntry, split_legacy, split_legacy_targets
    paths.rs              ~150 lines: find_root, expand_home, home_dir,
                          is_safe_fragment, is_store_name,
                          test_home_guard, TestHomeGuard, set_test_home
    error.rs              ~120 lines: ConfigError enum + impls

  fsutil.rs               ~150 lines: InodeIdentity, inode_identity,
                          ensure_inode_identity, filesystem_identity,
                          ensure_filesystem_identity, CreatedDirectory,
                          directory_identity, require_directory_identity
                          (unified from main.rs + plan_exec.rs — genuinely
                          domain-neutral identity helpers. Named `fsutil` not
                          `fs` to avoid shadowing `std::fs` in 15 files that
                          import it.)
    linker.rs             unchanged (1,319 lines) — stays at src/linker.rs,
                          not moved into fsutil. It's already its own
                          well-scoped module.

  ancestor.rs             ~300 lines: TargetAncestorEntry,
                          TargetAncestorRedirect, impl Display,
                          TargetAncestorSnapshot, impl TargetAncestorSnapshot,
                          target_ancestor_entry, has_parent_dir
                          (from plan_exec.rs — breaks the store↔plan_exec cycle.
                          Top-level module, not under fsutil — it's a safety
                          primitive specific to stitch's apply race model.)

  plan.rs                 unchanged (714 lines) — in-memory plan types

  plan_file.rs            ~800 lines: PlanFile, PlanFileOp, PlanFileRequires,
                          PlanConflict, PlanError, PlanExecReport,
                          PlanExecError, PlatformFingerprint, build_plan_file,
                          convert_store_ops, compute_config_hash,
                          sha256_hex, target_state_id/value/from,
                          plan_source_root, check_source_exists_for_preflight,
                          maybe_keep_staged, stage_render_for_op,
                          verify_stage_render, base_report, sync_ops_remaining
                          (plan file format + construction — split from
                          plan_exec.rs)

  plan_exec.rs            ~1,987 lines (with tests): execute_plan,
                          preflight_op, execute_op, check_target_state,
                          check_remove_link_ownership, remove_link_for_store,
                          replace_link_real_entry, create_link_for_plan,
                          plan_exec_error, conflict_class, op_description,
                          target_paths_for_store, is_under_any_target,
                          check_ancestors_writable, check_physical_ancestor,
                          symlink_ancestor (plan_exec's own), is_dir_empty,
                          link_error, PreflightState, RenderPin,
                          the test seam + execution inline tests
                          (execution + preflight — split from plan_exec.rs.
                          Calls validate_op from plan_validate.rs.)

  plan_validate.rs        ~562 lines: validate_op, validate_link_op,
                          validate_remove_link_op, validate_cleanup_dependencies,
                          validate_fresh_link_write, validate_backup_path,
                          ValidationContext, CurrentRemovals, current_removals
                          (preflight validation — split from plan_exec.rs.
                          Called by execute_plan/preflight_op; does not call
                          back into execution. One-directional.)

  store/
    mod.rs                explicit re-export facade
    apply.rs              ~1,600 lines: ApplyAction, ApplyResult, ApplyOpts,
                          apply_all, apply_store, apply_target,
                          prepare_file_mode_target, preflight_file_mode_promotion,
                          preview_file_entry_after_root_removal,
                          apply_whole_dir, target_would_be_empty_after_removals,
                          remove_empty_target_dir, target_is_confined,
                          symlink_ancestor (store's own), apply_file_entry,
                          source_is_symlink, create_link_for, apply_single_link,
                          force_backup_link, backup_path, has_active_template_sources,
                          target_has_template_source, is_regular_template_source,
                          redirect_to_apply_action, internal_error,
                          config_revalidation_error, compute_plan (3 lines,
                          calls apply_all — stays here to avoid a cycle)
    plan_compute.rs       ~250 lines: to_plan, action_to_plan_op,
                          unresolved_source_op, whole_dir_link_target,
                          remove_requires
                          (plan conversion — apply_all calls to_plan, but
                          to_plan/action_to_plan_op don't call back into apply.
                          One-directional: apply.rs → plan_compute.rs.)
    resolve.rs            ~700 lines: merge_ignores, build_globset,
                          build_directory_globset, is_under_directory_pattern,
                          is_ignored_path, has_ignored_entry,
                          resolve_link_source, resolve_link_source_for_target,
                          resolve_remove_source, LinkTargets,
                          collect_reconciliation_keeps, resolve_target_names,
                          collect_link_targets_for_target, resolve_files,
                          resolve_targets
                          (shared resolution layer — used by apply, status,
                          doctor, safety, plan_exec, and prune. Leaf module,
                          no imports from other store submodules.)
    status.rs             ~250 lines: StatusEntry, status_all, collect_statuses
    doctor.rs             ~600 lines: Severity, DoctorFinding, DoctorResult,
                          doctor, duplicate_target_message

  render.rs               ~1,994 lines (unchanged): template detection, render
                          context, render_string, render_file, staging paths,
                          stage_template, staged_differs, reconcile_store_links,
                          reconcile_store_staging, remove_staged,
                          remove_store_staging, stale_store_staging,
                          resolve_edit_source, verify_edit_target,
                          match_target_to_source, ensure_render_root,
                          checked_render_dir(s), staged_leaf_metadata,
                          read_staged_file, atomic_write_secure, StageOutcome,
                          gitignore helpers, secure temp helpers
                          (nothing moves out — gitignore/temp helpers are
                          render-specific, not generic FS primitives. Over the
                          ~1,500 goal but under the ~2,000 hard limit. A future
                          pass could extract the staging layer, but that's a
                          logic refactor, not a mechanical move.)

  commands/
    mod.rs                ~50 lines: run() dispatch + command_name only
    common.rs             ~180 lines: shared helpers (resolve_root,
                          resolve_override, print_warnings, filter_config,
                          check_unknown_names, apply_error_from_actions,
                          add_error_from_action, plan_error,
                          global_redirect_to_error)
    init.rs               ~110 lines: cmd_init
    apply.rs              ~400 lines: cmd_apply, cmd_apply_plan, apply_json,
                          render_plan, pending_change_count, test seam
                          (TEST_PAUSE_AFTER_SNAPSHOT) + 11 inline tests
    plan.rs               ~60 lines: cmd_plan
    status.rs             ~100 lines: cmd_status
    diff.rs               ~90 lines: cmd_diff
    list.rs               ~40 lines: cmd_list
    add.rs                ~2,000 lines: cmd_add, cmd_add_to_store, cmd_add_json,
                          rollback_adopt_move, cleanup_uncommitted_add,
                          discard_uncommitted_empty_file, discard_uncommitted_add,
                          add_cleanup_error, rollback_add_to_store,
                          remove_created_parents (only called by add helpers,
                          not by remove — verified),
                          validate_store_destination_parent,
                          prepare_store_destination_parent, target_parent_candidates,
                          prepare_target_parents, revalidate_add_boundaries,
                          paths_equal, target_dir_for_file_link,
                          lexically_normalize, collapse_home
    remove.rs             ~430 lines: cmd_remove
    edit.rs               ~60 lines: cmd_edit, resolve_editor
    doctor.rs             ~60 lines: cmd_doctor
    import.rs             ~230 lines: cmd_import
    migrate.rs            ~200 lines: cmd_migrate
    prune.rs              ~170 lines: cmd_prune, prune_roots
    render.rs             ~90 lines: cmd_render, validate_render_spec

  report.rs               ~1,300 lines (absorbs DTOs from main.rs): Envelope,
                          write, write_error, write_data_error, run_json,
                          StatusRow, status, ListStore, ListTarget, list,
                          DoctorData, DoctorSummary, DoctorRow, doctor,
                          PruneData, PruneRow, prune, prune_with_status,
                          RenderData, render, AddData, RemoveData,
                          ImportedStore, ImportData, MigrateData
```

**Note on duplicate leaf filenames:** `store/apply.rs` +
`commands/apply.rs`, `store/status.rs` + `commands/status.rs`,
`store/doctor.rs` + `commands/doctor.rs`, `render.rs` +
`commands/render.rs` — four collisions. This is standard Rust layering
(store = domain logic, commands = CLI layer) and there's no clearly
better naming scheme. Accepted deliberately; editor tabs will show two
`apply.rs` files but the path prefix disambiguates.

**Result by the numbers:**

| File | Before | After |
|---|---|---|
| main.rs | 5,222 | ~150 |
| store.rs | 3,827 | split into 5 files, largest ~1,600 (apply.rs) |
| plan_exec.rs | 3,632 | split into 3 files: plan_file.rs ~800, plan_validate.rs ~562, plan_exec.rs ~1,987 |
| config.rs | 2,795 | split into 6 files, largest ~400 |
| render.rs | 1,994 | ~1,994 (unchanged — nothing moves out) |
| report.rs | 1,226 | ~1,300 |
| linker.rs | 1,319 | unchanged |
| safety.rs | 781 | unchanged |
| error.rs | 811 | unchanged |
| scan.rs | 439 | unchanged |
| cli.rs | 484 | unchanged |
| plan.rs | 714 | unchanged |
| hooks.rs | 219 | unchanged |
| platform.rs | 142 | unchanged |
| **new: fsutil.rs** | — | ~150 |
| **new: ancestor.rs** | — | ~300 |
| **new: plan_file.rs** | — | ~800 |
| **new: plan_validate.rs** | — | ~562 |
| **new: commands/** | — | ~4,300 across 15 files |

Two files exceed ~1,500 lines: `commands/add.rs` (~2,000, one command
+ rollback) and `render.rs` (~1,994, unchanged). `store/apply.rs` at
~1,600 is close — it's the apply subsystem proper (`apply_all` alone is
435 lines). `plan_exec.rs` at ~1,987 is just under the ~2,000 hard
limit. Each is a cohesive unit that can't be split without creating a
cycle or fragmenting a single command's logic.

## Re-exports

Use **explicit re-export facades**, not wildcard `pub use *`. List the
items callers previously accessed by name. This prevents accidental
exports and name collisions while avoiding call-site churn:

```rust
// store/mod.rs
mod apply;
mod status;
mod doctor;
mod resolve;

pub use apply::{ApplyAction, ApplyResult, ApplyOpts, apply_all, apply_store, compute_plan, ...};
pub use status::{StatusEntry, status_all};
pub use doctor::{Severity, DoctorFinding, DoctorResult, doctor};
pub use resolve::{merge_ignores, build_globset, ...};
```

Newly cross-module private helpers use `pub(super)` rather than widening
to `pub(crate)` where possible. Tightening to direct paths (removing the
facade) is a follow-up pass, not part of this refactor.

## Execution order

Six phases. Phase 0 hardens the test suite; phases 1–5 do the
mechanical moves. Each phase is a standalone commit that ends with
`cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
&& cargo test` clean. If a phase can't be completed cleanly, stop and
reassess — don't carry broken state into the next phase.

### Phase 0: Coverage hardening (characterization tests)

**Why first:** The refactor is mechanical (code movement), but the
existing test suite has a gap in exactly the code path being moved that
is both high-risk and undertested: `add`'s rollback machinery.
Characterization tests pin current behavior before the move so a silent
breakage is caught immediately. New tests land in `tests/cli/` so they
don't move during phases 1–5 — zero double-work.

**Coverage audit findings (2026-08-13, revised after external review):**

| Area | Existing tests | Verdict |
|---|---|---|
| `add` rollback (adopt file/dir, config save) | 3 integration | Covered |
| `add --to` rollback after mid-move failure | 0 | **Gap — `rollback_add_to_store` has 8 error branches, only pre-move rejections tested** |
| `add` cleanup/discard branches | 0 direct | **Gap — `discard_uncommitted_*`/`cleanup_uncommitted_add` in ~15 error branches, only 2 scenarios exercised** |
| `add` boundary revalidation | 0 | **Gap — `revalidate_add_boundaries` untested** |
| `render` command (CLI) | 7 in `template.rs` (`render_text_prints_content`, `render_rejects_non_template`, `render_undefined_var_errors`, `render_defined_var_works`, `render_builtin_hostname_works`, `render_env_with_default_works`, `render_expression_works`) | Adequate — no new tests needed |
| `edit` command (core path) | 6 in `template.rs`/`security.rs` (`edit_works_when_editor_set`, `edit_linked_target_opens_source`, `template_edit_opens_source_not_staging`, `edit_rejects_foreign_symlink`, `edit_fails_nonzero_when_editor_unset`, `edit_uses_vi_fallback_when_editor_unset`) | Adequate — no new tests needed |
| `import` command (state writing) | 3 (`import_registers_existing_links`, `import_registers_nested_file_links`, `import_leaves_stitch_toml_byte_stable`) | Adequate — no new tests needed |
| `TargetAncestorSnapshot` (direct) | 8 inline in `plan_exec.rs` (`target_ancestor_snapshot_includes_home_and_deduplicates`, `_allows_absent_to_real_dir`, `_rejects_absent_to_symlink`, `_rejects_real_dir_identity_change`, `_rejects_real_dir_to_symlink`, `_allows_symlink_identity_preservation`, `_rejects_symlink_repointing`, `_rejects_existing_dir_removal`) | Already covered — no new tests needed |

**Tests to write (all in `tests/cli/add.rs`):**

1. **`--to` rollback paths (deterministic only):**
   - `add_to_rolls_back_when_state_save_fails` — make `.stitch/`
     read-only after preflight via `cmd` env setup, verify the moved
     file is restored and no partial state remains
   - `add_to_rolls_back_when_link_creation_fails` — make the target
     parent read-only after preflight, verify the moved file is restored

2. **Cleanup/discard branches (deterministic only):**
   - `add_rolls_back_when_link_creation_fails` — adopt dir, make target
     parent read-only after preflight, verify store dir is removed
   - `add_rolls_back_when_state_save_fails` — create-empty path, make
     `.stitch/` read-only after preflight, verify store dir is removed
   - `add_file_rolls_back_when_link_creation_fails` — `--file` path,
     link creation fails, verify empty file + store dir are removed

3. **Boundary revalidation:**
   - `add_rejects_config_change_during_operation` — swap `state.toml`
     between preflight and mutation using the existing
     `TEST_PAUSE_AFTER_SNAPSHOT` test seam (if accessible from inline
     tests) or a deterministic filesystem setup, verify the operation
     aborts

**Non-deterministic tests excluded:** Tests requiring "permission change
after preflight" via a filesystem race are excluded — they're flaky
without a failpoint. Adding `#[cfg(test)]` thread-local failpoints to
`cmd_add`'s 8 rollback branches is possible (the pattern exists via
`TEST_PAUSE_AFTER_SNAPSHOT`) but not worth the instrumentation cost for
a mechanical move. If Phase 0 turns up something scary during execution,
failpoints can be added then.

**Estimated new tests:** ~5–6
**Files touched:** 1 (`tests/cli/add.rs` only — no new test files, so
no `tests/cli.rs` registration needed)
**Estimated diff:** ~200–300 lines of new test code

**Verification:** `cargo test` count goes from 648 to ~653–654. All
new tests must pass against the *current* (pre-refactor) codebase —
they're characterizing existing behavior, not specifying new behavior.
If a test fails during Phase 0, the test is wrong (or a bug was found —
flag separately, don't fix inline).

### Phase 1: Extract `ancestor.rs` + `fsutil.rs` (breaks the cycle)

**Why first:** This is the phase that breaks the `store` ↔ `plan_exec`
cycle. It's the smallest phase and touches the fewest call sites.

**Steps:**

1. Create `src/ancestor.rs` — move from `plan_exec.rs`:
   - `TargetAncestorEntry` (line 60)
   - `TargetAncestorRedirect` + `impl Display` (lines 66–162)
   - `TargetAncestorSnapshot` + `impl` (lines 163–263)
   - `target_ancestor_entry` (line 66)
   - `has_parent_dir` (line 287)
   - Move the 8 `target_ancestor_snapshot_*` inline tests with them
2. Create `src/fsutil.rs` — unify identity helpers:
   - From `main.rs`: `InodeIdentity` (93), `inode_identity` (105),
     `ensure_inode_identity` (115), `filesystem_identity` (50),
     `ensure_filesystem_identity` (64), `CreatedDirectory` (99)
   - From `plan_exec.rs`: `directory_identity` (25),
     `require_directory_identity` (39) — collocate, keep both error
     types for now (unifying is a logic change, out of scope)
3. Update `mod` declarations in `main.rs`: add `mod ancestor; mod fsutil;`
4. Update `use crate::plan_exec::{TargetAncestorRedirect, TargetAncestorSnapshot}`
   in `store.rs` → `use crate::ancestor::{...}`
5. Update `use crate::plan_exec::{directory_identity, ...}` in
   `plan_exec.rs` → `use crate::fsutil::{...}` (internal references)
6. Update `main.rs` identity helper references → `use fsutil::{...}`

**What stays in `config.rs`:** `atomic_write`, `StateLock`,
`validate_atomic_write_target`, `validate_regular_file`. These return
`ConfigError` and encode `.stitch/`-specific semantics (symlinked state
parent rejection, lock file creation). They're state-persistence
infrastructure, not generic FS primitives. Moving them to a generic FS
module would create a worse dependency direction (`fsutil` →
`config::ConfigError`).

**Cycle verification:** After this phase, `store` imports from
`ancestor`, not `plan_exec`. `plan_exec` still imports from `store`
(one-directional). The cycle is broken.

**Files touched:** ~6 (create 2 new, modify 4)
**Estimated diff:** ~450 lines moved

### Phase 2: Split `store.rs` into `store/`

**Why second:** Now that the cycle is broken, `store.rs` can be split
without import tangles. The split separates apply execution, plan
conversion, shared resolution, status, and doctor into 5 files with
one-directional dependencies: `apply.rs` → `plan_compute.rs`,
`apply.rs` → `resolve.rs`, `plan_compute.rs` → `resolve.rs`,
`status.rs` → `resolve.rs`, `doctor.rs` → `resolve.rs`. `resolve.rs`
is a leaf module with no imports from other store submodules.

**Steps:**

1. Create `src/store/mod.rs` with an explicit re-export facade listing
   all currently `pub`/`pub(crate)` items by name
2. Create `src/store/apply.rs` — move apply execution + `compute_plan`:
   - `ApplyAction`, `ApplyResult`, `ApplyOpts` (lines 16–129)
   - `apply_all` (131), `apply_store` (630), `apply_target` (819),
     `prepare_file_mode_target` (884), `preflight_file_mode_promotion` (992),
     `preview_file_entry_after_root_removal` (1056), `apply_whole_dir` (1069),
     `target_would_be_empty_after_removals` (1153),
     `remove_empty_target_dir` (1192), `target_is_confined` (1218),
     `symlink_ancestor` (1299), `apply_file_entry` (1426),
     `source_is_symlink` (1507), `create_link_for` (1513),
     `apply_single_link` (1521), `force_backup_link` (1618),
     `backup_path` (1671), `has_active_template_sources` (586),
     `target_has_template_source` (608), `is_regular_template_source` (623),
     `redirect_to_apply_action` (81), `internal_error` (53),
     `config_revalidation_error` (61), `compute_plan` (567, 3 lines,
     calls `apply_all` — stays here to avoid a cycle)
   - Imports from `plan_compute.rs` (`to_plan`) and `resolve.rs`
     (`resolve_targets`, `resolve_link_source`, `collect_reconciliation_keeps`,
     `LinkTargets`, `resolve_target_names`)
3. Create `src/store/plan_compute.rs` — move plan conversion:
   - `to_plan` (1680), `action_to_plan_op` (1708),
     `unresolved_source_op` (1815), `whole_dir_link_target` (1822),
     `remove_requires` (1861)
   - Imports from `resolve.rs` (`resolve_link_source`, `resolve_remove_source`)
   - Does NOT import from `apply.rs` — `to_plan`/`action_to_plan_op` are
     pure conversions from `ApplyResult` to `PlanOp`, no callback into apply
4. Create `src/store/resolve.rs` — move all shared resolution helpers:
   - `merge_ignores` (2779), `build_globset` (2788),
     `build_directory_globset` (2875), `is_under_directory_pattern` (2884),
     `is_ignored_path` (2901), `has_ignored_entry` (2913),
     `resolve_link_source` (1892), `resolve_link_source_for_target` (1939),
     `resolve_remove_source` (1828), `LinkTargets` (2773),
     `collect_reconciliation_keeps` (2816), `resolve_target_names` (2947),
     `collect_link_targets_for_target` (3026), `resolve_files` (3067),
     `resolve_targets` (2854)
   - Leaf module — no imports from other store submodules
   - Used by: `apply.rs`, `plan_compute.rs`, `status.rs`, `doctor.rs`,
     `safety.rs`, `plan_exec.rs`, `main.rs` (prune)
5. Create `src/store/status.rs` — move:
   - `StatusEntry` (1977), `status_all` (2000), `collect_statuses` (2076)
   - Imports from `resolve.rs` (`resolve_targets`, `LinkTargets`)
6. Create `src/store/doctor.rs` — move:
   - `Severity` (2194), `DoctorFinding` (2203), `DoctorResult` (2212),
     `duplicate_target_message` (2218), `doctor` (2257)
   - Imports from `resolve.rs` (`resolve_target_names`, `LinkTargets`)
7. Move the 22 inline tests from `store.rs` into the relevant submodules
   (apply tests → `store/apply.rs`, resolve tests → `store/resolve.rs`, etc.)
8. Update `mod store;` → resolves to `store/mod.rs`
9. Update `use crate::store::` paths — stay the same via `mod.rs`
   facade. Internal references between submodules use `super::` or
   `crate::store::resolve::`.

**Files touched:** ~8 (create 6 new, delete 1, modify main.rs + plan_exec.rs imports)
**Estimated diff:** ~3,800 lines moved

### Phase 3: Split `config.rs` into `config/`

**Steps:**

1. Create `src/config/mod.rs` with an explicit re-export facade
2. Create `src/config/types.rs` — move all struct/enum definitions:
   `AuthoredConfig`, `AuthoredStore`, `AuthoredTarget`, `GeneratedState`,
   `GeneratedStore`, `GeneratedTarget`, `Config`, `Store`, `TargetEntry`,
   `Loaded`, `WhenClause`, `Hooks` (lines 32–168, 434–500)
3. Create `src/config/load.rs` — move:
   - `ConfigSnapshot` + impl (168–267), `path_exists` (268),
     `open_and_read_validated` (287), `parse_authored_bytes` (342),
     `parse_state_bytes` (352), `impl Store` (501), `impl Config` (507),
     `validate_merged` (659), `impl AuthoredConfig` (669),
     `impl GeneratedState` (675), `merge_store` (1051), `merge_targets` (1081),
     `validate_store_has_target` (1557), `validate_non_overlapping_targets` (1451),
     `validate_store_names` (1491), `validate_fragments` (1510),
     `validate_globs` (1536), `validate_target` (1367),
     `normalize_fragment` (1299), `normalize_fragment_lists` (1312),
     `normalize_ignores` (1323), `skip_if_default` (1291)
4. Create `src/config/state.rs` — move state-persistence infrastructure:
   - `atomic_write` (839), `validate_atomic_write_target` (813),
     `StateLock` + `impl StateLock` + `impl Drop for StateLock` (918–1050),
     `validate_regular_file` (775)
5. Create `src/config/legacy.rs` — move:
   - `LegacyConfig` (1129), `LegacyStore` (1146), `LegacyTargetEntry` (1165),
     `split_legacy` (1183), `split_legacy_targets` (1236)
6. Create `src/config/paths.rs` — move:
   - `find_root` (1574), `expand_home` (1654), `home_dir` (1622),
     `is_safe_fragment` (1338), `is_store_name` (1359),
     `set_test_home` (1600), `TestHomeGuard` (1605), `impl Drop` (1608),
     `test_home_guard` (1617)
7. Create `src/config/error.rs` — move:
   - `ConfigError` (1675), `impl ConfigError` (1700)
8. Move the 59 inline tests into relevant submodules
9. Update `mod config;` → resolves to `config/mod.rs`
10. Update `use crate::config::` paths — stay the same via facade

**Files touched:** ~9 (create 7 new, delete 1, modify callers)
**Estimated diff:** ~2,800 lines moved

### Phase 4: Split `plan_exec.rs` into `plan_file.rs` + `plan_validate.rs` + `plan_exec.rs`

**Why now (not in Phase 1):** Phase 1 only extracted the ancestor
snapshot (~263 lines). The remaining 3,369 lines are three concerns
that can now be separated cleanly: plan file format/construction,
preflight validation, and execution.

**Steps:**

1. Create `src/plan_file.rs` — move plan file format + construction:
   - `PlanFile` (295), `PlatformFingerprint` (314) + impls,
     `PlanFileRequires` (350), `PlanFileOp` (364) + impl,
     `PlanConflict` (422), `PlanError` (431), `PlanExecReport` (439),
     `PlanExecError` (451) + impl, `build_plan_file` (471),
     `convert_store_ops` (635), `maybe_keep_staged` (831),
     `stage_render_for_op` (849), `target_state_id/value/from` (928–944),
     `compute_config_hash` (957), `read_bytes_or_none` (976),
     `sha256_hex`/`sha256_hex_bytes` (987–993), `base_report` (997),
     `sync_ops_remaining` (1008), `verify_stage_render` (1017),
     `plan_source_root` (1048), `check_source_exists_for_preflight` (1061),
     `symlinked_ancestor` (570), `link_target` (593),
     `plan_link_targets` (606), `conflict_kind` (622),
     `impl From<LinkRequires> for PlanFileRequires` (917)
2. Create `src/plan_validate.rs` — move preflight validation:
   - `validate_op` (2749), `validate_link_op` (2893),
     `validate_remove_link_op` (3023), `validate_cleanup_dependencies` (2707),
     `validate_fresh_link_write` (2691), `validate_backup_path` (3074),
     `ValidationContext` (2547), `CurrentRemovals` (2562),
     `current_removals` (2574)
   - Called by `execute_plan`/`preflight_op` in `plan_exec.rs`; does not
     call back into execution. One-directional: `plan_exec.rs` → `plan_validate.rs`.
3. Slim `src/plan_exec.rs` to execution + helpers:
   - `execute_plan` (1384), `run_store_pre_hook` (1966),
     `run_store_post_hook` (1987), `plan_exec_error` (2008),
     `conflict_class` (2029), `op_description` (2037),
     `source_store` (2052), `staged_store` (2067),
     `symlink_ancestor_error` (2082), `check_physical_ancestor` (2097),
     `check_ancestors_writable` (2116), `check_target_state` (2124),
     `check_remove_link_ownership` (2163), `remove_link_for_store` (2192),
     `preflight_op` (2197), `is_dir_empty` (2308),
     `replace_link_real_entry` (2315), `is_symlink_source` (2395),
     `create_link_for_plan` (2401), `execute_op` (2413),
     `link_error` (2539), `PreflightState` (1084), `RenderPin` (1095),
     `target_paths_for_store` (2670), `is_under_any_target` (2681)
   - The `test_pause_after_global_hash` seam + execution tests stay here
   - Imports `validate_op` etc. from `plan_validate.rs`
4. Update `mod` declarations and `use` paths
5. Move inline tests to their respective files (construction tests →
   `plan_file.rs`, validation tests → `plan_validate.rs`, execution
   tests → `plan_exec.rs`)

**Files touched:** ~5 (create 2 new, modify plan_exec.rs + callers)
**Estimated diff:** ~1,350 lines moved

### Phase 5: Extract `commands/` from `main.rs` (incremental)

**Incremental, not all-at-once.** The shared helpers are extracted
first, then commands move in batches ordered by risk: trivial commands
first, `add` last with its own dedicated commit. Each batch is a
separate commit that ends with a clean build + test.

The test seam (`TEST_PAUSE_AFTER_SNAPSHOT`) + its 11 tests move *with*
`cmd_apply` in the apply batch — no separate Phase 6.

#### Phase 5a: Extract `commands/mod.rs` + `commands/common.rs`

1. Create `src/commands/mod.rs` with:
   - `pub fn run(cli: cli::Cli) -> Result<(), StitchError>` (moved from main.rs:207)
   - `command_name` (main.rs:187)
2. Create `src/commands/common.rs` with shared helpers:
   - `resolve_root` (436), `resolve_override` (454),
     `print_warnings` (346), `filter_config` (355), `check_unknown_names` (365),
     `apply_error_from_actions` (383), `add_error_from_action` (414),
     `plan_error` (505), `global_redirect_to_error` (27)
3. **Intermediate state:** `run()` in `commands/mod.rs` calls `cmd_*`
   functions that are still in `main.rs`. Since `main.rs` is the crate
   root, items declared there are accessible via `crate::cmd_init()` etc.
   — but they're currently private `fn`. Widen them to `pub(crate)` for
   the duration of phases 5b–5e. Each phase removes one `cmd_*` from
   `main.rs` and the corresponding `pub(crate)` visibility is no longer
   needed. By the end of Phase 5e, no `cmd_*` functions remain in
   `main.rs`.
4. Reduce `main.rs` to: `mod` declarations + `fn main()` calling
   `commands::run`

#### Phase 5b: Move trivial commands (one commit)

Move low-coupling commands in a single batch:
- `commands/init.rs` ← `cmd_init` (534)
- `commands/list.rs` ← `cmd_list` (1231)
- `commands/plan.rs` ← `cmd_plan` (779)
- `commands/status.rs` ← `cmd_status` (1039)
- `commands/diff.rs` ← `cmd_diff` (1148)
- `commands/doctor.rs` ← `cmd_doctor` (4029)
- `commands/edit.rs` ← `cmd_edit` (3679), `resolve_editor` (3720)
- `commands/render.rs` ← `cmd_render` (4485), `validate_render_spec` (4460)
- `commands/prune.rs` ← `cmd_prune` (4283), `prune_roots` (4449)
- `commands/migrate.rs` ← `cmd_migrate` (4089)
- `commands/import.rs` ← `cmd_import` (3735)

These are independent and low-risk. A single commit keeps the import
churn in one place.

#### Phase 5c: Move `remove` (one commit)

- `commands/remove.rs` ← `cmd_remove` (3256)

#### Phase 5d: Move `apply` + DTOs (one commit)

- `commands/apply.rs` ← `cmd_apply` (642), `cmd_apply_plan` (833),
  `apply_json` (937), `render_plan` (462), `pending_change_count` (1139)
- Move the test seam (`TEST_PAUSE_AFTER_SNAPSHOT`,
  `set_test_pause_after_snapshot`, `TEST_PAUSE_AFTER_SNAPSHOT`
  thread_local) + 11 inline tests from `main.rs` to `commands/apply.rs`
- Move DTOs from `main.rs` to `report.rs`:
  `AddData` (80), `RemoveData` (130), `ImportedStore` (141),
  `ImportData` (150), `MigrateData` (158)

#### Phase 5e: Move `add` (one commit — dedicated, highest risk)

- `commands/add.rs` ← `cmd_add` (2321), `cmd_add_to_store` (1883),
  `cmd_add_json` (2284), `rollback_adopt_move` (1270),
  `cleanup_uncommitted_add` (1362), `discard_uncommitted_empty_file` (1423),
  `discard_uncommitted_add` (1463), `add_cleanup_error` (1497),
  `rollback_add_to_store` (1509), `remove_created_parents` (1614),
  `validate_store_destination_parent` (1650),
  `prepare_store_destination_parent` (1693),
  `target_parent_candidates` (1791), `prepare_target_parents` (1807),
  `revalidate_add_boundaries` (1866), `paths_equal` (3962),
  `target_dir_for_file_link` (3975), `lexically_normalize` (3997),
  `collapse_home` (4018)
- This is the riskiest move (trust review flagged `add`'s rollback
  machinery as the most safety-critical code in the crate). A dedicated
  commit keeps the "balanced diff, no logic change" check meaningful
  where it matters most.
- Verify the Phase 0 characterization tests pass unchanged.

**Final `main.rs`:**
```rust
mod cli;
mod commands;
mod config;
mod error;
mod ancestor;
mod fsutil;
mod hooks;
mod plan;
mod plan_file;
mod plan_validate;
mod plan_exec;
mod platform;
mod render;
mod report;
mod safety;
mod scan;
mod store;

use clap::Parser;
use error::StitchError;

fn main() {
    let cli = cli::Cli::parse();
    let json = cli.json;
    let command_name = commands::command_name(&cli.command);
    if let Err(e) = commands::run(cli) {
        if json {
            report::write_error(command_name, &e, Vec::new());
        } else {
            eprintln!("error: {e}");
            if let Some(hint) = e.hint() {
                eprintln!("hint: {hint}");
            }
        }
        std::process::exit(e.exit_code());
    }
}
```

**Files touched (all of Phase 5):** ~20 (create 15 new, heavily modify main.rs + report.rs)
**Estimated diff:** ~5,000 lines moved

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Import path breakage causes cascading compile errors | Each phase ends with full compile + test. Phases are ordered so the cycle-breaking extraction (Phase 1) comes first when the codebase is smallest. |
| `pub(crate)` visibility needs widening for cross-module access | Use `pub(super)` for cross-module private helpers where possible. Acceptable — visibility widens, never narrows. No public API change. |
| `store/mod.rs` re-exports create a "wall of `pub use`" | Explicit facades listing items by name, not wildcards. Tightening to direct paths is a follow-up pass. |
| Test seam (`TEST_PAUSE_AFTER_SNAPSHOT`) is `#[cfg(test)]` + `thread_local!` — moving it might break the test that installs the callback | The seam and its 11 tests move together to `commands/apply.rs` in Phase 5d. The `#[cfg(test)]` attribute travels with them. No separate phase. |
| `store/apply.rs` at ~1,600 lines exceeds the ~1,500 goal | `apply_all` alone is 435 lines; the apply subsystem is inherently large. Plan conversion (`to_plan`/`action_to_plan_op`) is separated into `plan_compute.rs`, and shared resolution helpers are in `resolve.rs`. Further splitting would fragment a single execution pipeline. |
| `commands/add.rs` at ~2,000 lines is still large | It's one command with its rollback machinery. Further splitting (e.g., `commands/add/` with `rollback.rs`, `prepare.rs`) is a future option, but premature now — the helpers are tightly coupled to `cmd_add`'s flow. |
| `render.rs` at ~1,994 lines is unchanged | Nothing moves out — gitignore/temp helpers are render-specific, not generic FS primitives. Splitting would require extracting the staging layer, which is a logic refactor. Out of scope. |
| The `symlink_ancestor` function exists in both `store.rs` (1299) and `plan_exec.rs` (570) | Both stay in their respective modules — they have different signatures and error types. Unifying is a logic change, out of scope. |
| Silent behavior change during a mechanical move | Phase 0 adds ~5–6 characterization tests for the `add` rollback gaps before any code moves. The 375 integration tests + 273 inline tests cover the rest. |
| Phase 0 tests encode wrong behavior | All Phase 0 tests must pass against the *current* codebase before proceeding. If a test fails during Phase 0, the test is wrong (or a bug was found — flag separately, don't fix inline). |
| `tests/cli.rs` doesn't pick up new test files | Phase 0 adds tests to the *existing* `tests/cli/add.rs` — no new files, no registration needed. If new test files are added later, they must be registered in `tests/cli.rs` via `#[path = "..."] mod ...`. |
| Git history breaks at the refactor | Splitting `store.rs` 5 ways and `main.rs` 15 ways means `git log --follow` dies at the refactor for most of the crate. `git blame` gets shallow after this lands. Nothing to do about it, but future archeology will need to cross-reference this plan doc. |

## What's explicitly out of scope

- Splitting `commands/add.rs` further into a subdirectory
- Splitting `store/apply.rs` further (the apply execution pipeline is inherently large)
- Splitting `render.rs` (would require extracting the staging layer — a logic refactor, not a mechanical move)
- Unifying the duplicate `symlink_ancestor` / `directory_identity` functions (logic change)
- Unifying `StateLock`/`atomic_write` into a generic FS module (they're state-specific)
- Tightening re-export facades to direct paths
- Any change to `SPEC.md`, `AGENTS.md`, or test assertions
- Any change to `Cargo.toml` or dependencies

## Verification

Every phase ends with:

```sh
cargo build
cargo test          # 273 unit + 375 integration = 648 tests (pre-Phase 0)
                     # ~653–654 tests (post-Phase 0, for phases 1–5)
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Per AGENTS.md: zero warnings, fmt clean, clippy clean. This tool mutates
`$HOME`.

The final commit on each phase should be reviewed with
`git diff --color-moved` to confirm only moves occurred. Diff statistics
(additions ≈ deletions) are a necessary but not sufficient check — body
changes must be reviewed manually. Diff statistics alone cannot prove a
mechanical move.
