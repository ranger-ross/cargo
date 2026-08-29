# Cross-workspace build cache

Cargo builds eligible dependencies once in `$CARGO_HOME/build-cache` and reuses the artifacts in every workspace, even when several cargo processes run at the same time. A dependency that previously rebuilt in each workspace now builds once per identity, and a second workspace can skip rustc for it entirely.

The cache is part of the new build-dir layout and is enabled when `-Zbuild-dir-new-layout` is active. If `$CARGO_HOME` is read-only, the cache disables itself and the build falls back to the workspace build directory.

## High level design

### What gets cached

A unit is cacheable when it is not workspace-local, has no build script, and is not a bin, doc, test, or artifact unit. A unit with a direct dependency on a path-sourced package (a `[patch]`, `[replace]` or directory source) is also excluded. Path inputs are tracked by mtime and the cache is keyed by content only. The exclusion applies to that direct edge, units further upstream can still be cacheable because the normalized fingerprint pins the rmeta checksum of each dependency (see implementation details). `CompilationFiles::is_cacheable` computes this once from the unit graph.

### Where entries live

```text
$CARGO_HOME/build-cache/
  $pkgname/
    $hash/
      out/          rustc artifacts
      fingerprint/  fingerprint, dep-info, message cache
      incremental/  reserved (never used; incremental is off for non-local packages)
  _staging/
    <pid>/
      $pkgname/
        $hash/
          out/          same layout as above, but per-process
          fingerprint/
          incremental/
```

Cacheable units build in the per-process staging directory `_staging/<pid>/<pkg>/<hash>` so the final entry is never seen half-written. When the unit finishes, `BuildCacheLayout::publish_staged_unit` moves the directory to its final location with an atomic `rename`. If the destination already exists, another process published first and the staging copy is discarded. `AlreadyExists`, `DirectoryNotEmpty` and `EXDEV` are all treated as already published. An empty or incomplete existing entry left by an older buggy cargo is treated as poisoned, removed, and the rename is retried.

The staging area for the current PID is removed when the build finishes or is canceled (`cleanup_staging_pid` is best effort and ignores `NotFound`). If cargo is killed before cleanup, an orphaned `_staging/<pid>` directory remains. It is harmless and will be reused or ignored the next time that PID is used.

### Freshness

Cached units use the normal fingerprint check. The stored fingerprint must match the freshly computed one and the unit's own outputs must exist. The stored fingerprint is normalized so it stays stable after `cargo clean` (see implementation details). An entry is only rebuilt when its identity changes or the entry is incomplete.

For cacheable units the filesystem check does not use the per-PID staging `fs_status`, which is `Stale` on a cold build because the staging directory starts empty. Instead it checks the final location directly, that `out` and `fingerprint` exist and `out` contains files. Dependency mtimes are ignored for cacheable units, content matching through pinned rmeta checksums is the check that matters. The stored fingerprint is not truncated when a cacheable unit is planned as dirty, because that file lives inside the final cache entry.

Because the entry moves from staging to cache, absolute output paths change. `normalize_cache_deps` replaces path-dependent parts of the fingerprint with content-based `rmeta` checksums. `refresh_cache_dep_checksums` runs at write time (after deps are built, falling back to `_staging/<pid>` when the cache `rmeta` is not yet published) and again lazily when `expected_hash` is checked.

### Concurrency

The staging design does not use per-unit locks. Multiple cargo processes may race to build the same unit, each in its own `_staging/<pid>` directory. The first `rename` to succeed publishes, the others discard their copy and reuse the winner on the next build. Inside a single cargo, dependencies build before dependents, so a dependent can point its `extern` and `-L` at staging when the cache entry is not yet complete, or at cache when it is a hit. That choice is made at plan time from cache completeness (`has_out && has_fp && out_has_files`) and is guarded by `is_cacheable` so non-cacheable path deps do not hit the `debug_assert`.

Pipelined builds (where `rmeta` is available before `rlib`) still work inside one cargo because the whole dependency graph for that cargo lives in the same staging tree until publish. No cross-process pipelining is needed. A waiter in another process just sees the entry as incomplete until publish and will rebuild or wait for the next invocation.

Crash recovery is implicit. A builder killed mid-compile never publishes, so its staging is discarded on the next run and another builder will publish.

## Modules affected

- `src/compiler/layout.rs`. `BuildCacheLayout` staging helpers (`staging_root`, `staging_pid_root`, `staging_build_unit`, `staging_out`, `staging_fingerprint`, `staging_incremental`), `publish_staged_unit`/`publish_all_staged` and `cleanup_staging_pid`.
- `src/compiler/build_runner/compilation_files.rs`. Path routing between `deps_dir`/`output_dir`/`fingerprint_dir` and `staging_deps_dir`/`staging_output_dir`/`staging_fingerprint_dir`, plus `cache_build_unit`/`staging_build_unit` and `is_cacheable`.
- `src/compiler/fingerprint/mod.rs`. Fingerprint normalization for cacheable units, pinned dependency rmeta checksums with staging fallback, lazy completion hashing, and the cache-direct filesystem check.
- `src/compiler/mod.rs`. `rustc` `physical_outputs` and `--out-dir` staging mapping, `extern_args`/`add_dep_arg`/`lib_search_paths` choice between staging and cache, and the `build cache: ... is fresh (hit ...)` diagnostics.
- `src/compiler/unit.rs`. `Unit::is_cacheable`, the per-unit predicate.
- `src/compiler/job_queue/`. Cache-hit probe so the queue does not print `Compiling` for a unit that will be reused from staging or cache.
- `tests/testsuite/build_cache.rs`. 14 integration tests covering reuse across workspaces and after `cargo clean`, concurrent builders, git revision bumps, exclusions and output checks.

### Fingerprint normalization

A stored cache fingerprint has to match a freshly computed one after `cargo clean` or after a non-cacheable dependency rebuilds, and after the entry moves from staging to cache with different absolute paths. Three steps make this work:

1. Build-script and path dependencies in the fingerprint tree are replaced recursively with a content-based form: package fingerprint, `-C metadata`, and the checksum of the dependency's rmeta bytes. The rmeta checksum matters because a dependency's rmeta contains workspace-absolute paths like a build script's `OUT_DIR`, so the same unit built in two workspaces has different crate identity. Pinning the bytes lets the entry rebuild against local artifacts instead of failing at link time with `E0460` or `E0463`.
2. Checksums are refreshed at write time. On a cold build the fingerprint is prepared before its dependencies exist, so the code falls back from the cache `rmeta` to the `_staging/<pid>` `rmeta` when needed. They are refreshed again lazily at completion-check time, when a dirty unit runs and its dependencies have been built.
3. The filesystem check for cacheable units ignores dependency mtimes. Cache artifacts are written once and never updated, so mtime comparisons against them do not tell you anything, content matching does. When a cacheable unit is planned as dirty its stored fingerprint is not truncated, because that file is inside the final cache entry and clearing it would delete a valid entry.

### Observability

Every reuse prints `build cache: `pkg` target is fresh (hit $CARGO_HOME/build-cache/...)`. On the first build after a clean, the message comes from the `rustc` Work when `is_complete` becomes true after dependencies are built. Planner hits also print.

### Known limitations

- No garbage collection. Git revision bumps and rustc upgrades leave old entries orphaned.
- Crash-orphaned `_staging/<pid>` directories are not cleaned automatically. They are ignored and will be reused or overwritten when that PID is used again.
- Entries written by a buggy cargo can poison the cache. The fingerprint may be content-correct while the artifact references are not. Empty or incomplete poisoned entries are detected (`!has_out || !has_fp || !out_has_files`) and removed on the next publish retry. Otherwise a one-time wipe of `~/.cargo/build-cache` fixes it.
