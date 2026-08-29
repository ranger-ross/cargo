# DEV_LOG_V2: Staging build cache

Working log for the `_staging/<pid>` redesign. Notes what broke and how it was fixed.

## Status

- [x] Baseline: understand existing POC (`caching` branch) and `cache.rs` locking protocol
- [x] Add `BuildCacheLayout` staging helpers (`staging_root`, `staging_pid_root`, `staging_build_unit`, `staging_fingerprint`, `staging_out`, `staging_incremental`, `publish_staged_unit`, `publish_all_staged`, `cleanup_staging_pid`)
- [x] Wire `CompilationFiles` staging helpers (`staging_fingerprint_dir`, `staging_fingerprint_file_path`, `staging_deps_dir`, `staging_output_dir`, `staging_message_cache_path`, `cache_build_unit`, `staging_build_unit`, `staging_incremental_dir`, `cleanup_staging`, `publish_all_staged`)
- [x] Wire `BuildRunner` to publish staged units atomically after `queue.execute` and clean up `_staging/<pid>` on success, failure or cancel (`publish_all_staged` handles `AlreadyExists`, `DirectoryNotEmpty` and `EXDEV` races; empty poisoned entries are removed and retried)
- [x] Wire `fingerprint` to write cacheable fingerprints and outputs to staging (`staging_fingerprint_file_path`, `staging_deps_dir`, `prepare_init` creates staging dirs)
- [x] Wire `compiler/mod.rs` `rustc` to write to `physical_outputs` (staging-mapped) for cacheable units
- [ ] Fix cache-hit recognition after `cargo clean` for `cache_entry_reused_after_workspace_clean` (mid to script_dep)
- [ ] Fix `extern` resolution for `foo` to `mid` when `mid` is a hit from cache (currently `extern location for mid does not exist: .../out/libmid-...rlib`)
- [x] Fix `layout.rs` corruption (`__omp_shell` placeholder) and duplicate poison checks
- [x] Restore lost wiring after `git stash pop` (re-applied `layout.rs`, `compilation_files.rs`, `build_runner/mod.rs`, `mod.rs`, `fingerprint/mod.rs` from `stash@{0}`)
- [ ] Remove `/tmp` and `println!` debug artifacts before final commit
- [ ] Full `build_cache` suite green (currently 13/14, `cache_entry_reused_after_workspace_clean` fails on extern)

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

Fix in progress:

* `extern_args` now checks `staging_deps_dir(&dep.unit).exists()` and, when true, maps `cache_outputs` to staging by rewriting `cache_root` to `staging_root` (the same rewrite that `link_targets` and `lib_search_paths` should use). When staging does not exist it uses cache.
* `lib_search_paths` and `add_dep_arg` still used `staging_deps_dir` for every cacheable unit, which is wrong for hits. They need the same `use_staging` check as `extern_args`.
* `link_targets` now uses staging only when `!fresh && is_cacheable` (dirty and built in staging this invocation), otherwise cache.
* `rustc` `physical_outputs` mapping and `on_rmeta` (`downgrade_rmeta` only when `!is_cacheable`) fixed to avoid the `rmeta downgrade outside builder` panic.

## Next steps

1. Make `lib_search_paths` and `add_dep_arg` use staging only when `staging_deps_dir` exists (like `extern_args`), otherwise cache, so `foo` gets `-L dependency=...` from cache on a `mid` hit.
2. Check that `publish_staged_unit` correctly moves `libmid-...rlib` and `rmeta` from `staging/mid/<hash>/out` to `cache/mid/<hash>/out` (look at `staging out` contents at publish time; may need to confirm `fingerprint::write_fingerprint` writes to `staging_fingerprint` and `physical_outputs` to `staging_out` before publish).
3. Make sure `fingerprint::prepare_init` and `fingerprint::write_fingerprint` for staging consistently use `staging_fingerprint_dir` and `staging_deps_dir`, and that `cache_completion_state` `rmeta_paths` point to the right place (staging at build time versus cache at check time).
4. Remove all `/tmp/mid_staging.log` and `println!`/`eprintln!` debug, check `grep -rn "/tmp\|mid debug\|__omp"` is clean, run `cargo check`, then `cargo test --test testsuite build_cache` 14/14.
5. Update `FEATURE.md` and `PLAN.md` if needed and clean up `DEV_LOG.md` and `DEV_LOG_V2.md` before final.

## Notes

* Staging layout is `$CARGO_HOME/build-cache/_staging/<pid>/<pkg>/<hash>` with `fingerprint`, `out` and `incremental`. `publish` does an atomic `rename` to `$CARGO_HOME/build-cache/<pkg>/<hash>` on success. `AlreadyExists`, `DirectoryNotEmpty` and `EXDEV` mean another process won the race, so staging is discarded. `cleanup_staging_pid` removes `_staging/<pid>` when the build ends or is canceled (best effort, ignores `NotFound`).
* Fingerprint paths change after `rename`: `fingerprint` `outputs` move from staging to cache. `normalize_cache_deps` pins `rmeta` checksums, and `refresh_cache_dep_checksums` at write time and at `expected_hash` time handles that transition.
