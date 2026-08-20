# DEV_LOG — Cross-workspace build cache

Working log of design decisions, discovered issues, and fixes while building
the `$CARGO_HOME/build-cache` feature. This is a PoC to learn potential design
issues, so observations are recorded as they are found.

## Status

- [x] Baseline: understand existing POC (`caching poc` commit) and Cargo internals
- [x] Design locking protocol
- [x] Path routing for cacheable units
- [x] Fingerprint/freshness rework
- [x] Locking implementation
- [x] Cross-workspace / concurrency tests (manual)
- [x] Crash recovery test (manual)
- [x] Full test-suite pass (4375/4376 runnable; 1 environment-only failure)
- [x] Tests in the cargo testsuite

## Existing POC (commit 247c74dc1) — findings

The prior commit added a first cut. Key observations:

1. `Unit::is_cacheable()` was `!is_local && !custom_build && !is_bin`. Problems:
   - Registry crates **with build scripts** were marked cacheable, but their
     `lib` unit is compiled with workspace-local env vars / `OUT_DIR` baked in.
     Now excluded via `pkg.has_custom_build()`.
   - Doc units were cacheable per the filter but their `output_dir` is the
     workspace `doc/` dir. Now excluded.
   - Test/bench/artifact-dep units are now excluded too.

2. The POC's freshness shortcut (`fingerprint file exists → fresh`) was broken
   in a subtle way: **the fingerprint's *content* encodes identity that the
   unit hash does not.** In particular the git revision is *not* part of the
   unit hash (upstream `SourceId::stable_hash` deliberately excludes
   `precise`), and cargo detects git rev changes via the checkout directory
   path, which shows up as `DirtyReason::PathToSourceChanged` in the
   fingerprint. A pure existence check therefore silently reused the stale
   artifact after a git rev bump — the binary kept printing the old value.
   **Fixed**: cacheable units are fresh only when the stored fingerprint hash
   matches the computed one (content check). The mtime/`fs_status` part of the
   normal comparison is intentionally skipped: cache artifacts are immutable,
   so mtime changes alone (e.g. the same unit rebuilt byte-identically by
   another workspace) must not invalidate them; every *real* change (rev,
   version, features, flags) shows up in the hash.

3. The POC filtered cacheable dependencies out of `calculate_normal`'s
   fingerprint dep list. That broke **dependency-change propagation to
   non-cacheable dependents**: a binary depending on a git crate was not
   relinked after a rev bump (the dep fingerprint was dropped, so the bin's
   fingerprint hash never changed). **Fixed**: the filter is removed. The
   original reason for it (the `dep_info.strip_prefix(build_root).unwrap()`
   panic for cacheable dep-infos, which live outside the workspace build
   root) is fixed properly: cacheable units now use
   `LocalFingerprint::Precalculated(pkg_fingerprint)` instead of
   `CheckDepInfo`. `calculate_run_custom_build` had the same filter issue and
   the same fix.

4. The abandoned move-based approach (FIXME in `compile()`): correct to
   abandon — with pipelining, consumers read `.rmeta` from the workspace path
   mid-build; moving the directory breaks those reads. Artifacts are born in
   the cache (PLAN.md requirement 4).

5. Debug `println!`s in `fingerprint/dep_info.rs` removed.

6. `-Zbuild-dir-new-layout` is **stabilized** in this tree (1.100) and on by
   default; the legacy layout is only reachable through the temporary
   `__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT` opt-out. The cache is
   gated on the effective new-layout setting (`CompilationFiles::is_cacheable`
   requires `cli_unstable().build_dir_new_layout`), matching PLAN.md's "always
   assume `-Zbuild-dir-new-layout`": legacy-layout invocations never use the
   cache and keep the old workspace behavior (verified by the
   `clean_legacy_layout` / `build_dir_legacy` test modules passing unchanged).

## Design decisions

### Where cacheable units live

`$CARGO_HOME/build-cache/$pkgname/$METADATA/` mirroring the new layout:

```
$CARGO_HOME/build-cache/
  $pkgname/
    $hash/
      out/          # rustc artifacts (replaces deps/)
      fingerprint/  # fingerprint files (hash + .json + dep-info + output cache)
      incremental/  # per-unit incremental data (see "incremental" finding)
      .rmeta.lock   # state lock: rmeta availability
      .rlib.lock    # state lock: full completion
      .lock         # fine-grain locking compatibility (unused by cache protocol)
```

Path routing funnels through `CompilationFiles`: `deps_dir`, `output_dir`,
`fingerprint_dir`, `host_deps`, `build_unit_lock`, and `incremental_dir`
switch on `unit.is_cacheable()`. `pkg_dir` (`$pkgname/$hash`) is stable across
workspaces because the unit hash covers pkg id (registry/git sources hash
independent of workspace root), features, profile, mode, compile kind, rustc
version, and rustflags (minus remap-path-prefix).

### Freshness

Cacheable units use the **normal fingerprint comparison** (hash match plus
filesystem status), like any other unit. The hash match catches identity
changes that are not part of the unit hash; the filesystem-status check
catches changes to non-cacheable dependencies (path patches) and to
dep-info-tracked inputs (env vars under `-Zrustdoc-depinfo`). The fingerprint
file is written after artifacts complete; a unit is reusable when the stored
fingerprint matches the freshly computed one. The artifacts stay effectively
immutable: nothing rebuilds them unless one of these conditions fires.

### Locking protocol (per build unit, two state locks)

flock primitives; two lock files per cacheable unit encode state:

- `.rmeta.lock`: exclusive while compiling until rustc reports the `.rmeta`
  artifact (rustc writes it fully before the artifact notification), then
  shared.
- `.rlib.lock`: exclusive while compiling; shared only after the fingerprint
  is persisted.

Roles (see `src/compiler/cache.rs`):

- **Builder**: shared→exclusive both locks (after releasing shared and
  re-acquiring, with a fingerprint double-check), compiles, downgrades
  `.rmeta.lock` to shared on the rmeta notification (unblocking remote
  pipelined dependents), then downgrades `.rlib.lock` to shared after the
  fingerprint write.
- **Waiter**: takes shared `.rmeta.lock` (blocks until rmeta ready / complete
  / builder crash), then detects an active builder via a non-blocking shared
  `.rlib.lock` attempt. If a builder is active and the unit was never built
  (cold cache), it signals `rmeta_produced` early so its own dependents
  pipeline against the metadata; it then blocks on shared `.rlib.lock`.
  Missing fingerprint after that → the builder crashed → the waiter takes over.

Key ordering invariants:
- fingerprint write happens before `.rlib.lock` is released to shared;
- all lock acquisition is in a fixed order (`.rmeta.lock` before
  `.rlib.lock`), and contenders **release their shared locks before trying
  the exclusive upgrade**, so the multi-lock protocol cannot deadlock (a
  blocking SH→EX conversion drops the old lock while waiting, which would
  otherwise allow lock-order cycles between contenders);
- locks are held (shared) until the end of the build, matching the existing
  fine-grain-locking policy, which also makes crash recovery safe.

### `rmeta_produced` idempotence

`JobState::rmeta_produced` now only pushes `Message::Finish(Metadata)` when it
flips `rmeta_required` from true to false. The dependency queue's `finish`
asserts the edge is still present, so a second Metadata finish would panic;
the waiter-then-builder crash path can reach `rmeta_produced` twice.

## Findings during development

1. **Git rev changes keep the same unit hash.** `SourceId::stable_hash`
   excludes the git `precise` by design. For the cache this is not just an
   efficiency problem: two Cargo processes building **different revisions** of
   the same git URL concurrently collide in one cache directory and race
   (cargo deletes the stale rlib before recompiling, so a concurrent linker
   can observe a missing rlib — reproduced by
   `concurrent::git_same_branch_different_revs`). **Fixed**: `compute_metadata`
   now hashes the git precise into the unit hash, so each revision maps to its
   own cache entry and the "units are immutable" premise of the design is
   actually true. Tradeoff: revision changes accumulate cache entries (old
   ones orphaned); there is no cache GC in the PoC.

2. **Path patches of registry crates need mtime-based invalidation.** A
   cacheable registry crate can depend on a *patched* (path) crate. Editing
   the patch changes no hash (mtimes only), so a content-only freshness check
   kept the dependent fresh (`freshness::bust_patched_dep`). **Fixed**: the
   freshness check for cacheable units is the normal fingerprint comparison
   (hash match *and* filesystem-status up-to-date), exactly like upstream.
   The filesystem-status check may cause a conservative rebuild when a cache
   entry is rebuilt byte-identically by another workspace (newer mtimes) —
   correct, rare, and it converges.

3. **Dep-info based env/source tracking must be preserved.** The initial
   `Precalculated` local fingerprint for cacheable units dropped the
   dep-info's `# env-dep:` tracking, so `-Zrustdoc-depinfo` (and
   `-Zbinary-dep-depinfo`) no longer invalidated cacheable check units when
   the environment changed (`doc::rebuild_tracks_env_in_dep`). **Fixed**:
   cacheable units keep `LocalFingerprint::CheckDepInfo`, storing the
   **absolute** dep-info path (the cache is not under the workspace build
   root; `build_root.join(abs)` is a no-op for absolute paths, and cache
   fingerprints are machine-local anyway).

4. **`rmeta_produced` must stay observable.** Guarding the
   `Message::Finish(Metadata)` send on `rmeta_required` suppressed the
   `unit-rmeta-finished` timing event recorded by `-Zbuild-analysis` when no
   dependency needed the metadata edge. **Fixed**: a dedicated `rmeta_sent`
   cell makes the method idempotent (protecting the dependency queue from a
   double `finish`) without suppressing the first message.

5. **The fingerprint directory must exist during compilation.** The POC
   skipped creating it for cacheable units, but the message-cache file is
   created while rustc runs, before the fingerprint is written
   (`build::rustc_wrapper_relative`). **Fixed**: `prepare_init` creates it for
   all units.

6. **Incremental compilation is never used for cacheable units.** Cargo forces
   `profile.incremental = false` for non-local packages, so the per-unit
   `incremental/` directory in the cache is inert today. Kept as defensive
   routing.

7. **Worker threads cannot print "Blocking" lock messages.** `Work` closures
   are `'static`, so the cache locks are acquired silently. The existing
   fine-grain locking avoids this by acquiring locks on the main thread.

8. **flock SH→EX conversion drops the old lock while blocked.** Fixed by
   releasing all shared locks before the exclusive acquisition.

9. **Torn-read race on crash recovery after rmeta-ready (accepted).** A
   builder crash after rmeta-ready can let a waiter pipeline against the
   orphaned rmeta that the waiter's own takeover rebuild rewrites. Rewrites
   are byte-identical in practice; documented in `cache.rs`. Pipelining is
   suppressed when the fingerprint *exists* but mismatches (identity rebuild).

10. **Test-harness footgun**: `(cmd1) (cmd2)` runs subshells sequentially;
    concurrent builds need `&` + `wait`.

## Verification performed (manual)

- Cold build of a git dependency compiles into `$CARGO_HOME/build-cache` and a
  second workspace reuses it without invoking rustc; its binary links against
  the cached rlib and runs.
- `cargo check` units get their own cache entries (rmeta-only) and are reused
  across workspaces.
- Two concurrent cargos, cold cache: exactly one rustc invocation for the
  shared unit (counted in `-vv` logs), the waiter's lib-dependent started
  ~3.4s before the builder's unit completed (cross-process pipelining).
- Warm concurrent rerun: both builds fully fresh in ~40ms.
- Builder killed (SIGKILL) mid-compile leaves rmeta + lock files but no rlib
  and no fingerprint; the next build detects the missing/mismatched
  fingerprint, takes over, completes the unit; the other workspace then
  reuses it.
- Git rev bump (`cargo update`): the new revision gets its own cache entry and
  is compiled; dependents relink (binary output changed); the other workspace
  reuses the new entry.
- Packages with `build.rs` are excluded: built into the workspace, no
  build-cache dir created, build-script env vars work.
- `cargo doc` and `cargo test` unaffected.

11. **Read-only `$CARGO_HOME` must disable the cache, not fail the build.**
    `registry::readonly_registry_still_works_*` chmods `$CARGO_HOME` read-only
    and expects `cargo check` to succeed (deps already fetched). The cache's
    first write (lock file creation) failed with EACCES. **Fixed**: `Layout`
    probes `$CARGO_HOME/build-cache` writability at layout time; when
    un-writable, the cache is disabled for the whole invocation and cacheable
    units fall back to the workspace build directory (`CompilationFiles`
    routes through `is_cacheable() = cache_enabled && unit.is_cacheable()`).

12. **LockManager must not block on `flock` while holding its internal lock
    table.** `concurrent::multiple_registry_fetches` deadlocked
    intermittently: a job thread blocked on a cache `flock` while holding the
    LockManager's `RwLock`, serializing every other lock operation in the
    process — including the operations the lock it was waiting on depended on
    (cross-process cycle). **Fixed**: lock handles are stored as
    `Arc<FileLock>` and every potentially-blocking `flock` call happens after
    dropping the table lock. The test now passes 8/8 consecutive runs (it
    previously hung at a measurable rate).

## Full test-suite results

Definitive run with the final binary: **4404 tests — 4375 passed, 1 failed,
28 ignored**. The single failure is `standard_lib::
build_std_with_no_arg_for_core_only_target`, which requires an uninstalled
`aarch64-unknown-none` rustup target (an environment prerequisite unrelated to
the cache; the test instructs `rustup target add aarch64-unknown-none`).

Bugs found by the suite and fixed (each reproduced by a failing test, then
verified fixed):

- `freshness::bust_patched_dep` — cacheable units must use the full normal
  fingerprint comparison (hash *and* filesystem status), not content-only, so
  path-patch changes invalidate dependents.
- `concurrent::git_same_branch_different_revs` — the git revision is now part
  of the unit hash, so different revisions never share a cache entry.
- `doc::rebuild_tracks_env_in_dep` — cacheable units keep
  `LocalFingerprint::CheckDepInfo` (absolute cache path) so dep-info
  environment tracking (`-Zrustdoc-depinfo`) still works.
- `build_analysis::log_msg_timing_info*` — `rmeta_produced` stays observable
  (a dedicated `rmeta_sent` cell makes it idempotent without suppressing the
  first message).
- `build::rustc_wrapper_relative` / `cache_messages::very_verbose` — the
  fingerprint directory is created for cacheable units during compilation.
- `registry::readonly_registry_still_works_*` /
  `global_cache_tracker::read_only_locking_auto_gc` — a read-only
  `$CARGO_HOME` disables the cache instead of failing the build.
- `concurrent::multiple_registry_fetches` — LockManager no longer blocks on
  `flock` while holding its internal lock table (8/8 consecutive passes after
  the fix; hung intermittently before).

Design-change consequences (tests updated): `cargo clean` no longer removes
cached non-local dependency artifacts, and cacheable units' artifacts,
fingerprints, and dep-info files live in `$CARGO_HOME/build-cache` — affected
snapshots in `clean`, `clean_legacy_layout` (reverted: legacy layout disables
the cache), `build_dir*`, `registry`, `offline`, `features2`, `dep_info`,
`freshness_checksum`, `build`, `weak_dep_features`, `rename_deps`,
`alt_registry`, `global_cache_tracker`.
