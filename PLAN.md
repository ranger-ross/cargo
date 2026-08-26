# Cargo build-cache

Add a new cross workspace build cache to Cargo the Rust package manager.


## Design

1. Stored in `$CARGO_HOME/build-cache` as a flat list of build-units.
2. `-Zbuild-dir-new-layout` is required for the cache. It is stabilized (always on) in this tree (1.100); if the new layout is not in effect — or `$CARGO_HOME/build-cache` is not writable — the cache silently disables itself and the build proceeds normally. Never crash.
3. Not all build units are eligible to be cached. Only immutable build units: not workspace-local, no build scripts, not bin/doc/test/bench/example/artifact units, and with no direct dependency on a path-sourced package (`[patch]`, `[replace]`, or directory source). Units that stay cacheable despite an indirect path-patched transitive dependency are kept correct by the rmeta checksum pinned in the normalized fingerprint.
4. Build units that are eligible are built inside of the build-cache. (we cannot move them while the build is in progress and we don't want to lose out on pipelined builds)
5. The build-cache should support per-build unit locking.
   * We must allow concurrent reads while taking into account pipelined builds (e.g. rmeta ready but not rlib)
   * My idea is the have lock files in each build unit that can be taken shared/exlcusively to represent different states. (though consider other designs)


## Requirements

1. The build-cache should be concurrent with multiple cargos. This includes pipelined builds across workspaces with different cargo processes. 
2. Build units in the cache are immutable.
3. Build units are shared across builds in different workspaces.
4. Workspaces are able to reuse already compiled build units.


