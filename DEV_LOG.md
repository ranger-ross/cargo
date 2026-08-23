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

2. **Path patches of registry crates make the dependent ineligible for the
   cache.** A registry crate can depend on a *patched* (path) crate. Editing
   the patch changes no hash (mtimes only), so a content-only freshness check
   kept the dependent fresh (`freshness::bust_patched_dep`). The first fix
   routed mtime-based invalidation into cacheable units' freshness check —
   that conflicted with the design premise that cache units are immutable
   and keyed by content, and was **reverted in favor of an eligibility rule**:
   a unit that depends on a path-sourced package is not eligible for the
   build cache at all (`CompilationFiles::is_cacheable`, computed from the
   unit graph since `Unit` itself carries no dependency edges). Patched
   dependents therefore use upstream's normal mtime-based freshness logic,
   and no mtime machinery exists inside the cache path. The rmeta content
   checksums pinned by finding #17 remain as defense-in-depth for immutable-
   source dependencies whose artifacts can still diverge across workspaces.
   Regression test: `build_cache::path_patched_dependent_not_cached` (the
   patched dependent and the patch target get no cache entry while an
   unrelated registry crate does; editing the patch rebuilds through
   upstream's logic).

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

13. **Cached units depending on build-script crates were invalidated by
    `cargo clean`.** `foo` (serde/syn/serde_derive): after `cargo clean` of
    the workspace target only, `unicode-ident` stayed fresh but `syn` and
    `serde_derive` rebuilt with "info of dependency changed". Root cause: a
    `run-custom-build` unit's `local` fingerprint switches between the
    whole-crate `Precalculated` form (when the previous run's output is
    missing) and the `RerunIfChanged`/`RerunIfEnvChanged` form (when it is
    present). A cacheable unit's *stored* fingerprint (written after the
    build, with the `RerunIf*` state) never matched the *plan-time* fingerprint
    computed after a clean (which sees the `Precalculated` state) — a
    byte-identical dependency rebuild invalidated the entry.
    **Fixed**: `Fingerprint::deep_clone` + a normalization pass applied in
    `calculate()` to cacheable units' fingerprints: dependency `local`
    fingerprints of `run-custom-build` and local (path) units are replaced
    (recursively) by the content-based `Precalculated(pkg_fingerprint)` form.
    Registry/git build scripts are immutable, so the `rerun-if-*` bookkeeping
    adds no information; local deps' package fingerprints are mtime-based, so
    path patches still invalidate (verified by `freshness::bust_patched_dep`).
    The clone is private to the cacheable fingerprint, so the shared
    fingerprints used for the dependencies' own freshness are untouched. The
    fs-status check for cacheable units is relaxed to ignore dependency
    staleness (`StaleDependency`/`StaleDepFingerprint` — a dependency was
    rebuilt, which content matching already accounts for) while still
    requiring the unit's own outputs (`Stale`/`StaleItem` still rebuild).
    Result: after `cargo clean`, `serde_derive`/`syn`/`unicode-ident` all stay
    fresh and only the non-cacheable build-script crates rebuild (5.5s → 3s
    in the repro). Known limitation (documented): a build script's
    `rerun-if-env-changed` env vars are not part of the normalized content, so
    an env change that alters a build-script crate's output does not
    invalidate *cache hits* of units depending on it (the build-script crate
    itself still rebuilds correctly; only the cached dependent's reuse
    decision is affected).
    Consequence for the testsuite: cacheable units that stay fresh now emit
    the `build cache: \`pkg\` target is fresh (hit …)` diagnostic on stdout
    (feature requested by the user), and dependency-content changes are
    reported as "info of dependency `X` changed" instead of "the dependency
    `X` was rebuilt" (content-based instead of mtime-based). Affected
    snapshots updated: `freshness::bust_patched_dep`,
    `freshness_checksum::bust_patched_dep_checksum`, and the
    exact-stdout assertions in `features2` (3 tests), `offline`,
    `package::workspace_with_local_deps`, `replace::
    overriding_nonexistent_no_spurious`.

14. **Cached units' fs-status mtime chain dirtied dependents forever.** Same
    `foo` repro: two consecutive builds — `serde` (and `foo`) rebuilt every
    time with "the dependency `serde_derive` was rebuilt"
    (`StaleDependency`), even though `serde_derive` was a fresh cache hit.
    Root cause, two layers:
    (a) a cacheable unit's *own* fs-status still ran the mtime chain against
    its dependencies. The shared cache is written once and never refreshed,
    so a cacheable unit's artifacts are almost always *older* than its
    freshly-rebuilt non-cacheable deps (e.g. `serde_derive`'s cache .so vs
    workspace `quote`/`proc-macro2`) — its fs-status was permanently
    `StaleDependency`, which every dependent then inherited as
    `StaleDepFingerprint` and rebuilt against forever; and
    (b) even with (a) fixed, a dependent's own mtime comparison against a
    cacheable dep's artifacts fires whenever the shared cache entry was
    rewritten after the dependent last built.
    **Fixed** in `Fingerprint::check_filesystem`: cacheable units skip the
    dependency loop entirely (their normalized fingerprint content is the
    authoritative signal; `Stale`/`StaleItem` for their own outputs still
    rebuild), and non-cacheable units skip the mtime comparison for cacheable
    dependencies. Unit identity comes from `BuildRunner::
    cacheable_unit_indices` (populated in `prepare_units`). Regression tests:
    `build_cache::fresh_despite_cache_entry_rewrite` (cache artifacts bumped
    newer than the workspace) and `build_cache::
    cacheable_unit_fresh_despite_newer_noncacheable_deps` (cacheable unit's
    workspace dep bumped newer than the cache — reproduces the `serde` case
    exactly: fails with `Dirty foo: the dependency dep1 was rebuilt` without
    the fix, passes with it).

16. **`bat` failed with `E0463: can't find crate for indexmap` (in its build
    script) — poisoned cache entries from the buggy WIP binary, not a bug in
    the committed code.** The `.cargo-build-test.sh` run failed while
    compiling bat's build script (`build/main.rs`): E0463 for `indexmap` and
    `serde_with`, with `--extern` paths pointing at existing cache artifacts.
    Root cause chain: (1) the unit hash is cargo-binary-independent, so
    entries written by the intermediate WIP binary (the shared-dir routing
    bug during the sccache E0514 investigation, Aug 21 ~20:16) carry the same
    cache dir names (`indexmap/0ddd52494d3fa8e6`) the current binary uses;
    (2) the WIP binary routed the non-cacheable `serde_core` dependency
    against the wrong rlib, so the cached indexmap rlib embeds a
    `serde_core` metadata hash that does not match the current unit graph;
    (3) the freshness check (content-based fingerprints) correctly reports
    fresh, but the artifact-level reference is garbage → rustc reports E0463
    for the direct dep (or E0460 "possibly newer version of crate
    `serde_core`" when the extra `-L` dirs happen to resolve). The current
    binary itself never reproduces this: with a clean `~/.cargo/build-cache`,
    bat builds (25s cold, ~2s warm) and the second build is fully served from
    cache hits. **Remedy: wipe `~/.cargo/build-cache` once after switching
    dev-cargo binaries.** No code change: cargo artifacts are
    forward-compatible across correct cargo versions, so a commit-level cache
    marker would invalidate the cache on every cargo update for no
    correctness gain, and a binary-content marker costs ~100ms+ per
    invocation (prohibitive for the test-suite, which runs cargo thousands of
    times). The poison is only producible by a *buggy* intermediate binary;
    the standard dev remedy is a one-time cache wipe.

17. **Cache poisoning across workspaces (E0463/E0460 at link time) — fixed.**
    With all 20 repos enabled in `.cargo-build-test.sh`, `uv` and `tauri`
    failed with `error[E0463]: can't find crate for \`toml_datetime\`` (and a
    direct probe gave `E0460: found possibly newer version of crate
    \`serde_core\``). Root cause, pinned by byte-level inspection of the
    artifacts: a cacheable unit (`toml_datetime`) links a dependency
    (`serde_core`, non-cacheable — it has a build script) whose rmeta embeds
    the **absolute OUT_DIR path** of the build-script output (e.g.
    `.../{workspace}/target/debug/build/serde_core/4e0b53.../out/private.rs`).
    The same unit compiled in two workspaces therefore has a *different*
    crate identity even though the `-C metadata` is identical — rustc
    rejects the mismatch at link time (`E0460`), and the freshness check
    accepted the entry because the fingerprint (including the `-C metadata`,
    which is the same) cannot see the artifact-level divergence.
    **Fix**: cacheable units' normalized fingerprints now pin each
    dependency's rmeta **content checksum** (in addition to the existing
    `-C metadata` mix-in), so any dependency-artifact divergence — including
    a rebuild in another workspace — makes the entry dirty and rebuilds it
    against the local artifacts. Two follow-ups were needed:
    (1) the checksum must be read as **binary** (`cargo_util::paths::read`
    requires UTF-8 and always failed on rmeta files, silently degrading to
    the "missing" placeholder); (2) the persisted fingerprint must be
    **refreshed at write time** — a cold-cache build prepares the fingerprint
    before dependencies are built (checksum placeholder "missing") and a
    rebuild mid-build makes the prepared checksum stale, so the write closure
    recomputes every dependency checksum from the actual rmeta before
    persisting (and clears the memoized hashes). The `debug_assert_eq!` in
    `_compare_old_fingerprint` was removed: fingerprint format changes across
    cargo versions legitimately break the stored-short vs JSON-rehash
    invariant it checked. Regression coverage: the existing 10-test
    `build_cache` suite (including `cacheable_unit_fresh_despite_newer_
    noncacheable_deps`, which exercises the cold-cache refresh) and
    `freshness::bust_patched_dep` (mid-build dep rebuild). Full suite: 4377
    passed / 1 env-only failure (build-std) / 28 ignored. Script: 18/20
    repos pass; the only failures are environmental (`zellij`: upstream
    `include_bytes!` quirk; `tauri`: missing system `javascriptcore`/
    `webkit2gtk` dev libraries).

18. **Architecture correction: eligibility instead of mtime-based
    invalidation (follow-up to finding #2).** The freshness path for cacheable
    units no longer consults dependency mtimes in any form:
    * `CompilationFiles::is_cacheable` now also excludes any unit with a
      dependency on a path-sourced package (precomputed `path_dep_units` set
      from the unit graph; `Unit` does not carry dependency edges). Registry
      crates whose dependency is replaced by `[patch] ... { path = ... }`
      (or `[replace]`, or directory sources) keep upstream's mtime-based
      freshness and are never routed into `$CARGO_HOME/build-cache`.
    * `FsStatus::cache_compatible()` was deleted; `_compare_old_fingerprint`
      uses plain `up_to_date()`. For cacheable units this is behavior-
      preserving (the dependency mtime loop was already skipped per #14, so
      their fs-status could only be `UpToDate`/`Stale`/`StaleItem`), but the
      API no longer advertises an mtime-tolerance mode.
    * `CacheCompletionState::fs_up_to_date` is sourced from `up_to_date()`;
      its remaining role is catching partially written cache entries (own
      outputs missing), not dependency invalidation.
    Transitive note: for `A(reg) -> B(reg) -> C(path-patched)`, `B` becomes
    uncacheable by the direct rule; `A` stays cacheable and is protected by
    its pinned checksum of `B`'s rmeta (#17): if editing `C` changes `B`'s
    artifact bytes, `A` rebuilds; if `B`'s artifact is byte-identical, reuse
    is semantically correct.

## Full test-suite results
