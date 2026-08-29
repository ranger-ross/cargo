# DEV_LOG_V2: Staging build cache

Working log for the `_staging/<pid>` redesign. Notes what broke and how it was fixed.

## Status

- [x] Baseline: understand existing POC (`caching` branch) and `cache.rs` locking protocol
- [x] Add `BuildCacheLayout` staging helpers (`staging_root`, `staging_pid_root`, `staging_build_unit`, `staging_fingerprint`, `staging_out`, `staging_incremental`, `publish_staged_unit`, `publish_all_staged`, `cleanup_staging_pid`)
- [x] Wire `CompilationFiles` staging helpers (`staging_fingerprint_dir`, `staging_fingerprint_file_path`, `staging_deps_dir`, `staging_output_dir`, `staging_message_cache_path`, `cache_build_unit`, `staging_build_unit`, `staging_incremental_dir`, `cleanup_staging`, `publish_all_staged`)
- [x] Wire `BuildRunner` to publish staged units atomically after `queue.execute` and clean up `_staging/<pid>` on success, failure or cancel (`publish_all_staged` handles `AlreadyExists`, `DirectoryNotEmpty` and `EXDEV` races; empty poisoned entries are removed and retried)
- [x] Wire `fingerprint` to write cacheable fingerprints and outputs to staging (`staging_fingerprint_file_path`, `staging_deps_dir`, `prepare_init` creates staging dirs)
- [x] Wire `compiler/mod.rs` `rustc` to write to `physical_outputs` (staging-mapped) for cacheable units
- [x] Fix cache-hit recognition after `cargo clean` for `cache_entry_reused_after_workspace_clean` (mid to script_dep)
- [x] Fix `extern` and `-L` resolution for `foo` to `mid` when `mid` is a hit from cache
- [x] Fix `rmeta` checksum pinning for first-build staging (fallback to `_staging/<pid>` when cache rmeta missing)
- [x] Fix `rustc --out-dir` for cacheable units to use `staging_output_dir`
- [x] Fix `layout.rs` corruption (`__omp_shell` placeholder) and duplicate poison checks
- [x] Restore lost wiring after `git stash pop` (re-applied `layout.rs`, `compilation_files.rs`, `build_runner/mod.rs`, `mod.rs`, `fingerprint/mod.rs` from `stash@{0}`)
- [x] Remove `/tmp` and `println!` debug artifacts
- [x] Full `build_cache` suite green (14/14)

## Issues and resolutions

### 1. layout.rs python patch corruption: __omp_shell

`cargo check` failed with `__omp_shell("has_out || !has_fp || !out_has_files")` at `layout.rs:640,665`. The incremental `eval(python)` replace had corrupted `!has_out || !has_fp || !out_has_files` and duplicated the `AlreadyExists` and `DirectoryNotEmpty` arms. The `raw_os_error == 17` arm also used a weaker check (`read_dir(...).next().is_some() == false`).

Fixed with a single atomic `edit` using a fresh tag, restoring `!has_out || !has_fp || !out_has_files` in both arms and the full `is_poisoned` check (`has_out`, `has_fp`, `out_has_files`) for `raw_os_error == 17`. Checked with `grep -rn __omp_shell` and `cargo check`.

### 2. Staging helpers looked dead (grep only found layout.rs)

`staging_*`, `publish_*` and `cleanup_staging_pid` existed in `layout.rs` but had no callers in `src/compiler/mod.rs` or `build_runner`. `git diff HEAD` showed the `mod.rs` and `compilation_files.rs` changes were missing after a `stash pop`.

The earlier `stash pop` had dropped the wiring for `src/compiler/mod.rs` and `compilation_files.rs` while leaving `layout.rs`. Restored the full set from `stash@{0}` with `git show stash@{0}:src/... > src/...` for `layout.rs`, `compilation_files.rs`, `build_runner/mod.rs`, `mod.rs`, `fingerprint/mod.rs` and `tests/testsuite/build_cache.rs`, then removed the temporary `/tmp` blocks for `mid` and `script_dep`.

### 3. cache_entry_reused_after_workspace_clean: mid not fresh after cargo clean

After `rm -rf foo/target` and rebuild, the test expected `build cache: mid mid is fresh` but got `UnitDependencyInfoChanged { unit: UnitIndex(2) }` for `mid` (deps hash 348 vs 764) and no fresh message. Two things were wrong.

First, `prepare_target` truncated the fingerprint for every dirty unit. For cacheable units the fingerprint lives in the immutable cache entry, so this wiped the valid entry. This was already fixed earlier with a guard `if loc.exists() && !is_cacheable` (commit `fc3d9f8`).

Second, with staging the fingerprint was written to `staging_fingerprint_file_path` but `CacheCompletionState` still used `fingerprint.fs_status` from the per-PID staging dir. On a cold build that dir is empty, so `fs_status` is `Stale` and `fs_up` is false, which made `is_complete` (`content_matches && (fs_up || builder_active)`) false even when `expected_hash` matched the stored `a6bae...`. Also `rmeta` checksums at plan time are placeholders (`cd044...`) and only become real (`a6bae...`) after dependencies are built.

Kept the `if loc.exists() && !is_cacheable` guard. Changed `CacheCompletionState::fs_up_to_date` for cacheable units to check the cache directly (`cache_unit.join("out").exists() && cache_unit.join("fingerprint").exists()`) instead of `fingerprint.fs_status`. Added lazy checksum refresh in `expected_hash` (`refresh_cache_dep_checksums` at check time, after deps are built, so `cd044...` becomes `a6bae...` and matches the stored value). Added a `cache_hit_probe` for dirty cacheable units and checked `is_complete` at execution time in the `rustc` Work (after deps are built). If it hits, skip `rustc` and replay the output, printing `build cache: mid is fresh (hit ...)`. This restores the behavior from `fc3d9f8` for staging.

### 4. extern location for mid does not exist after a hit

After the hit fix, the second build printed `build cache: mid mid is fresh (hit ...)` but `foo` failed to link: `extern location for mid does not exist: .../build-cache/mid/.../out/libmid-...rlib`.

`extern_args` (which calls `build_runner.outputs(&dep.unit)` for `dep=mid`) always returned the cache `out` (`deps_dir` is `build-cache/.../out`). For a dirty `mid` built in this invocation the `out` is in staging (`_staging/<pid>/.../out`) until `publish_all_staged` runs after the queue. For the hit case on the second build, `mid` was not built in this pid, so `staging_deps_dir` does not exist, but `extern_args` still returned the cache `out` which existed as a directory but contained no `libmid-...rlib` (logs showed `cache_out .../out exists=true` but `EXTERN ... exists=false`; `cache_unit` had `out` and `fingerprint` entries but `out` was empty). The same problem affected the first build: `mid` writes were staging-mapped but `publish` for `mid` had not yet run.

Added `PUBLISH` logging in `layout.rs::publish_staged_unit` (staging, cache, existence, and `staging out` contents) and `EXTERN` logging in `mod.rs::extern_args` (cache and staging existence and listings). First build `PUBLISH` for `mid` showed `staging_exists=true cache_exists=true` with `fingerprint` and `out` entries but no files in `staging out`; second build `EXTERN` for `foo` showed cache `out` exists but `libmid-...rlib` missing.

Fixed by making `extern_args`, `add_dep_arg` (which provides `-L dependency=`) and `collect_dep_rmeta_paths` decide based on whether the cache entry is complete. At prepare time `staging_deps_dir.exists()` is false before deps are built, so that check would always pick cache on the first build and miss. Now the code checks cache completeness (`has_out && has_fp && out_has_files`). If the cache is incomplete, the dep will be built in staging this run, so the caller uses `staging_deps_dir`. If the cache is complete, it uses `deps_dir` (cache). Both `extern_args` and `add_dep_arg` guard the `staging_deps_dir()` call with `is_cacheable(&dep.unit)` to avoid the `debug_assert` panic on non-cacheable path deps. `link_targets` was updated to use staging only when `!fresh && is_cacheable`, otherwise cache. `rustc` `physical_outputs` mapping and `on_rmeta` (`downgrade_rmeta` only when `!is_cacheable`) were fixed to avoid the `rmeta downgrade outside builder` panic.

### 5. rustc --out-dir still pointed at cache for cacheable units

Even with `physical_outputs` mapped to staging, `build_base_args` passed `--out-dir` as `build_runner.files().output_dir(unit)`, which for cacheable units is `build-cache/.../out` (cache). `rustc` therefore wrote `rlib` and `rmeta` to cache while `fingerprint/dep-*` went to staging via `staging_fingerprint_file_path`. `publish` then moved staging (which had an empty `out`) over the cache `out` that already contained the `rlib`, or discarded staging on `AlreadyExists` and left the cache without the `dep-*` file. The next build saw `fingerprint/dep-lib-dep1` missing and reported `DIRTY` for `dep1`.

Changed `build_base_args` to use `staging_output_dir` when `is_cacheable(unit)` is true, so `rustc` writes all outputs to `_staging/<pid>/.../out`. `publish` now moves both `out` and `fingerprint` together. This fixed `concurrent_cold_builds_share_the_unit` which previously left `cache/.../out` empty and `fingerprint` without `dep-lib-*`.

### 6. rmeta checksum still pinned to :missing on first build

`rmeta_checksum_paths` for a cacheable unit are collected at plan time via `collect_dep_rmeta_paths`, which calls `build_runner.outputs(&dep.unit)` and pushes cache `out` paths. On the first build the cache is empty, so those paths do not exist yet. `refresh_cache_dep_checksums` at write time (inside the `Work` at `fingerprint/mod.rs:616`, after deps have been built) read the cache paths, got `NotFound`, and kept the `:missing` placeholder. The stored hash stayed `cd044...` while second-build `expected_hash` read the now-published cache `rmeta` and became `a6bae...`, so `is_complete` never matched.

Kept collection as cache paths, but made `refresh_dep_checksums` fall back to staging when the cache read fails. If `std::fs::read(cache_path)` fails and the path contains `build-cache` and not already `_staging`, it builds the staging path as `<root>/build-cache/_staging/<pid>/<rest>` using `std::process::id()` and retries the read. This covers both the write-time refresh and the `expected_hash` refresh. Verified `concurrent_cold_builds_share_the_unit` and `cache_entry_reused_after_workspace_clean` now get the same `a6bae...` hash on first and second builds.

### 7. Verification

Removed all `/tmp/mid_staging.log` and `PUBLISH`/`EXTERN` debug, confirmed `grep -rn "/tmp\|mid debug\|__omp"` clean. `cargo check` passes (8 warnings about unused `before`, `fingerprint_dir`, etc.). Full suite:

```
cargo test --test testsuite build_cache -- --nocapture  -> 14 passed, 0 failed
  cache_entry_reused_after_workspace_clean
  concurrent_cold_builds_share_the_unit
  concurrent_builders_resolve_to_a_single_builder
  check_units_shared_across_workspaces
  clean_does_not_touch_cache
  fresh_despite_cache_entry_rewrite
  git_dep_built_into_cache_and_reused
  ... +7
```

Staging is cleaned on both success and failure via `cleanup_staging_pid` (ignores `NotFound`).

* Fingerprint paths change after `rename`: `fingerprint` `outputs` move from staging to cache. `normalize_cache_deps` pins `rmeta` checksums, and `refresh_cache_dep_checksums` at write time and at `expected_hash` time handles that transition, with the staging fallback for the first build.

### 8. PR 31 CodeRabbit review batch 1 (minor quick-wins)

Fixed 5 actionable comments verified against current code:

* `crates/cargo-util/src/paths.rs:913` partial dst leak on cross-device copy — `move_directory` slow path now removes `dst` on `copy_directory` failure before returning the error, so a later reader cannot see a partial cache unit.
* `src/compiler/mod.rs:665` `unwrap()` panic on read-only/full FS — `paths::create_dir_all` on `dep_info_loc` parent now uses `if let Some(parent)` and propagates `?`, preserving `CargoResult` flow.
* `src/compiler/build_runner/compilation_files.rs:964` precise hash changes `-C metadata` for every git dep even when cache inactive — gated the `precise.hash` on `build_dir_new_layout` (the flag that enables the cache), preserving existing hashes in legacy layout.
* `src/compiler/locking.rs:508,700,710` debug-assertion tests `unlock_below_zero_is_a_bug`, `assert_locked_rejects_*` fail in `--release` — added `#[cfg(debug_assertions)]` to all three. Also added `debug_assert!(!exclusively_held)` before blocking `lock_shared`/`lock_shared_path` to match `try_lock_shared_path`, preventing silent shared→exclusive downgrade. Fixed missing `entry.count += 1` regression introduced during the guard insertion.

### 9. PR 31 CodeRabbit review batch 2

* `src/compiler/layout.rs:701` raw `EEXIST` (`errno 17`) branch used weak `read_dir(...).next().is_some()==false` check, while `AlreadyExists`/`DirectoryNotEmpty` used full `!has_out || !has_fp || !out_has_files`. Unified to full `is_poisoned` predicate (`has_out`, `has_fp`, `out_has_files`) so a nonempty incomplete destination is correctly treated as poisoned and retried, preserving complete staging.
* `src/compiler/fingerprint/mod.rs:1788` staging fallback derived via `path.to_string_lossy().find("build-cache")` substring — if `CARGO_HOME` parent contains `build-cache` as substring the fallback could construct a path outside `$CARGO_HOME/build-cache/_staging/<pid>`. Rewrote to component-wise `Path::components()` search for exact `build-cache` component, then inserts `_staging/<pid>` after it, preserving `cargo-util` handling and keeping `CacheCompletionState::expected_hash` consistent. Added `ok_or` for missing component so non-cache paths correctly return `NotFound`.
