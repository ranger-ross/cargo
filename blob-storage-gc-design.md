# Blob storage / cache design

## Scope

Blob storage deduplicates non-local compiler outputs and supplies the content-addressed data for a local build cache. Tracking uses unit outputs, build snapshots, and workspace history. Remote storage, remote cache lookup, and remote garbage collection are not implemented.

Enable the implementation with `-Zshared-blob-storage` and the new build-directory layout. Local packages are not stored. Build-cache restoration has stricter eligibility than blob deduplication.

## Storage graph

```text
Workspace history -> Build snapshot -> Unit output -> Blob
                       Cache entry -> Unit output
```

```text
$CARGO_HOME/blobs/
  <blob hash>
$CARGO_HOME/shared-storage/
  unit-output/<unit-output hash>
  snapshots/<snapshot hash>
  workspace-history/<workspace-id>/<snapshot hash>
  cache-entries/<unit-hash>
```

A blob contains one file's bytes and is named by its lowercase hexadecimal BLAKE3 hash. Blobs remain in a flat directory.

A unit output contains sorted, unique relative output paths, blob hashes, and sizes. Its name hashes its canonical serialized contents. It contains no Cargo unit identity, workspace identity, permissions, or timestamps. Compiler dep-info and cached diagnostics are output files and can be referenced like other blobs.

A build snapshot contains the sorted, unique unit-output hashes used by one successful build or check. It covers the non-local outputs participating in blob storage. Fresh units, newly compiled units, and cache hits all contribute. Identical sets produce identical snapshot hashes.

Workspace history has a separate file for each workspace/snapshot pair. The file contains decimal Unix seconds followed by a newline. Every successful invocation updates its snapshot's timestamp. No per-blob timestamp or workspace-wide history file is rewritten.

The local workspace ID is the BLAKE3 hash of the canonical workspace-root path's native bytes. It is independent of the build directory. Separate build directories and targets can contribute different snapshots to the same history. This is a local identity, not a remote workspace identity.

## Fingerprints and build recording

An eligible unit's short fingerprint may contain:

```text
<16-digit fingerprint>
unit-output-v1 <64-digit unit-output hash>
```

The unit-output hash is excluded from Cargo's fingerprint hash and diagnostic JSON. It does not propagate to dependent fingerprints or implement early cutoff.

1. Load fingerprints while building the unit graph.
2. Reuse a fresh unit's unit-output hash after validating its metadata and referenced blob sizes. This does not rehash output files.
3. For a rebuilt or previously untracked unit, hash and deduplicate its output files, then publish its unit output.
4. Attach the unit-output hash to the fingerprint only after publication.
5. After a successful invocation, publish the build snapshot and update its workspace-history file.

Dirty units clear their optional hash. A successful rebuild without tracking writes a plain fingerprint, so re-enabling tracking cannot reuse an obsolete unit output. Missing or damaged metadata is recaptured from still-fresh compiler outputs.

A failed invocation may leave completed unit outputs and cache entries, but it does not create or refresh a build snapshot. These objects alone do not retain blobs during garbage collection.

## Local build cache

A cache entry is keyed by Cargo's unit hash and references a unit output. It also contains a guard derived from Cargo's fingerprint and the dependency artifacts supplied to the compiler. The guard protects against changed compiler inputs, including changed transitive environment-dependent outputs. Dependency artifact hashing is needed for cache lookup/publication, not ordinary fresh-build usage tracking.

Initial eligibility is immutable registry or Git library units in build or non-test check mode. Local units, build scripts, proc-macros, and their transitive consumers are excluded. Artifact dependencies, per-unit extra arguments, forced rebuilds, custom compiler commands, and compiler wrappers are excluded. Custom executors are excluded unless they explicitly opt into local caching.

On a cache hit:

1. Read the cache entry and check its input guard.
2. Read the referenced unit output and validate native relative output paths.
3. Stage private reflinks or copies and verify every blob's BLAKE3 hash before replacing the output tree.
4. Check rustc's recorded environment dependencies and reject source inputs outside the immutable package.
5. Regenerate Cargo's translated dep-info, replay cached diagnostics, and notify the scheduler when metadata is available.
6. Persist the fingerprint and include the unit output in the successful build snapshot.

Missing, corrupt, or incompatible entries fall back to compilation. A clean build directory can restore dependencies without invoking rustc, while local packages still compile.

Permissions and modification times are stored in the cache entry, separate from the unit-output identity. Restored compiler outputs receive the current invocation timestamp to preserve subsequent freshness. Restoration therefore uses private reflinks or copies rather than hardlinks.

Cache entries are not independent garbage-collection roots. An entry remains available while a retained build snapshot references its unit output. A successful cache hit refreshes workspace history. This resolves cache-entry retention without adding a timestamp to each entry or changing build-snapshot identity.

## Deduplication and hardlink safety

Capture prefers reflinks, then hardlinks where permitted, then copies. Reuse through a hardlink requires compatible permissions and modification times. A private replacement preserves the original compiler output's metadata. Cached bytes are verified before replacing good compiler output.

Before any dirty non-local rustc invocation, Cargo removes the per-unit output tree. This detaches artifacts, raw dep-info, and auxiliary outputs from blobs before a compiler or linker can truncate them. It also applies when tracking is disabled for that invocation.

Rustdoc capture does not use hardlinks because its output-writing lifecycle differs. Cache restoration remains private because the compiler updates restored timestamps.

Hardlinked output files and cache contents must not be modified by external tools in place. Cache writers must be trusted. BLAKE3 verifies contents, not provenance.

## Locking and publication

The implementation reuses Cargo's package-cache locking system for local coordination:

- Builds hold its shared lock for the storage lifetime, excluding garbage collection.
- Immutable object writes use staged atomic publication.
- Snapshot and workspace-history commits also hold the download/append lock.
- Garbage collection holds the exclusive mutation lock before traversing or deleting shared storage.

Publication orders blobs before unit outputs, unit outputs before fingerprint hashes and cache entries, and build snapshots before workspace history. A crash can leave unreferenced objects. Garbage collection removes them later. Unrecognized files, including interrupted staging files, are left alone.

There is no remote locking protocol.

## Garbage collection

Workspace history is the retention root. Automatic collection uses the existing Cargo GC schedule and a 30-day maximum age. It is skipped offline and does not wait for a busy automatic-GC lock.

Under the exclusive mutation lock:

1. Read workspace histories and identify recent build snapshots.
2. Validate build snapshots and their unit outputs.
3. Walk unit outputs to discover referenced blobs, checking regular-file status and sizes.
4. If a size limit is requested, evict the oldest snapshots until retained blobs fit. Count each distinct blob once.
5. Remove expired history files and cache entries whose unit outputs are no longer retained.
6. Remove unretained build snapshots, unit outputs, and blobs.

The collector prepares the complete plan before deletion. Unreadable or malformed recognized history files stop collection because they may hide a live root. Incomplete snapshots do not retain blobs. Blob contents are not rehashed during collection.

Snapshots without recent workspace history do not receive an import grace period. There are no cache revision tokens, workspace receipts, or throttled history updates.

Manual size-limited collection:

```sh
cargo clean gc -Zgc -Zshared-blob-storage --max-blob-size 10GiB
```

Add `--dry-run` to preview without changing metadata or blobs. Logical size excludes metadata and filesystem overhead. Collection does not delete build-directory outputs. A blob's data blocks may remain allocated while build-directory hardlinks exist. Ordinary `cargo clean` leaves shared storage available for restoration.

## Metadata formats

Formats are versioned binary records, except workspace-history timestamps. They are unstable implementation details, not a public compatibility promise.

- Unit output: `cargo-shared-storage-unit-output-v1\0`, path-encoding byte, u64 count, then repeated u64 path length, path bytes, 32-byte blob hash, and u64 size.
- Build snapshot: `cargo-shared-storage-snapshot-v1\0`, u64 count, and sorted unique 32-byte unit-output hashes.
- Cache entry: `cargo-shared-storage-cache-entry-v1\0`, 32-byte unit-output hash, u64 input guard, u64 count, then output metadata in canonical path order: u32 permission bits, i64 modification-time seconds, and u32 nanoseconds.
- Workspace history: decimal Unix seconds and a newline.

Integers are little-endian. Path encodings distinguish Unix bytes, Windows UTF-16 code units, and UTF-8 fallback. Paths are relative to the unit directory. Restoration accepts only paths within its output tree. Metadata traversal does not require native path decoding, while restoration requires the native encoding.

## Remaining design boundaries

- Remote workspace identity and protocols are unresolved and not implemented.
- Remote garbage collection needs its own retention and publication coordination.
- Unit outputs and cache entries remain separate because they have different keys and responsibilities.
- Local packages, build-script opt-in, proc-macro opt-in, and rebuild early cutoff remain outside this implementation.
- Sharding blob paths is deferred. The blob directory is flat as specified.
