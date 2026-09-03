# Cross-workspace build cache

Cargo builds eligible dependencies once in `$CARGO_HOME/build-cache` and reuses the artifacts in every workspace, even when several cargo processes run at the same time. A dependency that previously rebuilt in each workspace now builds once per identity, and a second workspace can skip rustc for it entirely.

The cache is part of the new build-dir layout and is enabled when `-Zbuild-dir-new-layout` is active. If `$CARGO_HOME` is read-only, the cache disables itself and the build falls back to the workspace build directory.

## High level design

### What gets cached

A unit is cacheable when it is not workspace-local, has no build script, and is not a bin, doc, test, or artifact unit. A unit with a direct dependency on a path-sourced package (a `[patch]`, `[replace]` or directory source) is also excluded. Path inputs are tracked by mtime and the cache is keyed by content only. The exclusion applies to that direct edge, units further upstream can still be cacheable because the normalized fingerprint pins the rmeta checksum of each dependency (see implementation details). `CompilationFiles::is_cacheable` computes this once from the unit graph.

### Where entries live

```text
$CARGO_HOME/build-cache/
  content/
    <sha256>          deduplicated file blobs (rlib, rmeta, fingerprint, etc.)
  entries/
    $pkgname/
      $hash           manifest JSON: { fingerprint_hash, files: { "out/foo.rlib": "<sha256>", ... } }
  # Legacy entries from earlier staging builds (if present) are ignored;
  # CAS entries are authoritative.
```

Cacheable units are compiled directly in the workspace `build-dir` (e.g. `target/debug/build/$pkg/$hash/{out,fingerprint}`) so that pipelined builds and `rmeta`/`rlib` ordering work naturally. After the unit finishes, `CompilationFiles::publish_to_cas` hashes each output and fingerprint file with SHA-256, hardlinks (or copies on `EXDEV`) them into `content/<sha256>`, and atomically writes a manifest at `entries/$pkg/$hash` containing the expected fingerprint hash and the `rel -> sha256` map. `AlreadyExists` is treated as success (another process published first); empty or incomplete manifests are treated as poisoned and retried. No per-unit `_staging/<pid>` directory is used.

On a cache hit, `BuildCacheLayout::restore_from_cas` is called during `fingerprint::prepare_init` before the job is enqueued: it hardlinks the manifest's files from `content` back into the workspace `build-dir` locations and `touch`es the manifest (throttled to once per day). The subsequent `rustc` invocation is then skipped via the `CacheCompletionState::is_complete` probe.

### Freshness

Cached units use the normal fingerprint check. The stored fingerprint must match the freshly computed one and the unit's own outputs must exist. The stored fingerprint is normalized so it stays stable after `cargo clean` (see implementation details). An entry is only rebuilt when its identity changes or the entry is incomplete.

For cacheable units the filesystem check does not compare dependency mtimes. Instead it checks that the manifest exists, all `content` blobs exist, and the stored `fingerprint_hash` matches the freshly computed expected hash (with pinned rmeta checksums refreshed). Dependency mtimes are ignored for cacheable units, content matching through pinned rmeta checksums is the check that matters. The stored fingerprint is not truncated when a cacheable unit is planned as dirty, because that file lives inside the CAS manifest's fingerprint set.

Because artifacts are content-addressed, absolute output paths do not affect the cache key. `normalize_cache_deps` replaces path-dependent parts of the fingerprint with content-based `rmeta` checksums. `refresh_cache_dep_checksums` runs at publish time (after deps are built) and again lazily when `expected_hash` is checked.

### Concurrency

The CAS design does not use per-unit locks. Multiple cargo processes may race to build the same unit, each in its own workspace `build-dir`. The first `write_manifest_atomic` to succeed publishes; the others discard their hardlinked content (already deduplicated) and reuse the winner on the next build. Inside a single cargo, dependencies build before dependents, so the fingerprint's `is_complete` probe can see the manifest become complete after dependencies are published. That choice is guarded by `is_cacheable` so non-cacheable path deps do not hit the `debug_assert`.

Pipelined builds (where `rmeta` is available before `rlib`) still work because the whole dependency graph for that cargo lives in the same `build-dir` until publish, and `rmeta` is published as part of the manifest. No cross-process pipelining is needed; a waiter in another process just sees the entry as incomplete until the manifest is written.

Crash recovery is implicit. A builder killed mid-compile never publishes, so no manifest is written and another builder will publish on the next run. Partial `content` blobs are harmless (they are unreferenced until GC).

## Modules affected

- `src/compiler/layout.rs`. `BuildCacheLayout` CAS helpers (`content_dir`, `entries_dir`, `entry_manifest_path`, `content_path`, `hash_file`, `insert_into_content`, `write_manifest_atomic`, `read_manifest`, `manifest_content_exists`, `touch_manifest`, `publish_unit_to_cas`, `restore_from_cas`, `gc`).
- `src/compiler/build_runner/compilation_files.rs`. Always routes cacheable units to the workspace `build-dir`; `is_cacheable` gating and `publish_to_cas` batch.
- `src/compiler/fingerprint/mod.rs`. Fingerprint normalization for cacheable units, pinned dependency rmeta checksums, lazy completion hashing, and the CAS manifest+content filesystem check.
- `src/compiler/mod.rs`. `CacheCompletionState` probe and `build cache: ... is fresh (hit ...)` diagnostics; `link_targets` no longer remaps through staging.
- `src/compiler/unit.rs`. `Unit::is_cacheable`, the per-unit predicate.
- `src/compiler/job_queue/`. Cache-hit probe so the queue does not print `Compiling` for a unit that will be reused from cache.
- `tests/testsuite/build_cache.rs`. 14 integration tests covering reuse across workspaces and after `cargo clean`, concurrent builders, git revision bumps, exclusions and output checks; helpers now resolve via `entries`/`content`.

### Fingerprint normalization

A stored cache fingerprint has to match a freshly computed one after `cargo clean` or after a non-cacheable dependency rebuilds, and after the entry is published to CAS with different absolute paths. Three steps make this work:

1. Build-script and path dependencies in the fingerprint tree are replaced recursively with a content-based form: package fingerprint, `-C metadata`, and the checksum of the dependency's rmeta bytes. The rmeta checksum matters because a dependency's rmeta contains workspace-absolute paths like a build script's `OUT_DIR`, so the same unit built in two workspaces has different crate identity. Pinning the bytes lets the entry rebuild against local artifacts instead of failing at link time with `E0460` or `E0463`.
2. Checksums are refreshed at publish time. On a cold build the fingerprint is prepared before its dependencies exist, so the code falls back to the workspace `rmeta` when needed. They are refreshed again lazily at completion-check time, when a dirty unit runs and its dependencies have been built.
3. The filesystem check for cacheable units ignores dependency mtimes. Cache artifacts are written once and never updated, so mtime comparisons against them do not tell you anything, content matching does. When a cacheable unit is planned as dirty its stored fingerprint is not truncated, because that file is inside the CAS manifest.

### Observability

Every reuse prints `build cache: `pkg` target is fresh (hit $CARGO_HOME/build-cache/entries/...)` or `build cache: `pkg` target is fresh (hit $CARGO_HOME/build-cache/content/...)`-like. On the first build after a clean, the message comes from the `rustc` Work when `is_complete` becomes true after dependencies are built. Planner hits also print.

### Known limitations

- No automatic garbage collection by default. `BuildCacheLayout::gc` can be invoked to remove manifests older than a threshold and unreferenced `content` blobs. Git revision bumps and rustc upgrades otherwise leave old entries orphaned.
- Entries written by a buggy cargo can poison the cache. Empty or incomplete poisoned manifests are detected (`files.is_empty()` or missing `content`) and removed on the next publish retry. Otherwise a one-time `rm -rf ~/.cargo/build-cache/{content,entries}` fixes it.
