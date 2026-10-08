# Blob storage / cache design

## Scope

Blob storage deduplicates non-local compiler outputs and supplies the content-addressed data for local and optional remote build caches. Tracking uses unit outputs, cache entries, build snapshots, and snapshot usage. Remote caching uses REAPI ActionCache and CAS/ByteStream. Remote garbage collection remains the server's responsibility.

Enable the implementation with `-Zshared-blob-storage` and the new build-directory layout. Local packages are not stored. Build-cache restoration has stricter eligibility than blob deduplication.

## Storage graph

```text
Snapshot usage -> Build snapshot -> Unit output -> Blob
                                 -> Cache entry -> Blob
```

```text
$CARGO_HOME/shared-storage/
  blobs/<hash[..2]>/<hash[2..]>
  unit-output/<unit-output hash>
  snapshots/<snapshot hash>
  snapshots/usage/<snapshot hash>
  cache-entries/<unit-hash>
```

A blob contains one file's bytes and is named by its lowercase hexadecimal BLAKE3 hash. Blob paths are sharded by the first two hex digits to keep directories small.

A unit output contains sorted, unique relative output paths, blob hashes, and sizes. Its name hashes its canonical serialized contents. It contains no Cargo unit identity, workspace identity, permissions, or timestamps. Compiler dep-info and cached diagnostics are output files and can be referenced like other blobs. Only non-local units that are not cacheable are tracked by unit output.

A cache entry is named by Cargo's unit hash. It duplicates the unit-output path and blob list so a lookup reads one file, locally or remotely. Cacheable units are tracked only by their cache entry.

A build snapshot contains the sorted, unique unit-output hashes and cache-entry names used by one successful build or check. It covers the non-local outputs participating in blob storage. Fresh units, newly compiled units, and cache hits all contribute. Identical sets produce identical snapshot hashes.

Snapshot usage has one file per build snapshot. The file contains decimal Unix seconds followed by a newline. Every successful invocation updates its snapshot's timestamp, regardless of workspace or build directory. No per-blob timestamp or per-workspace file is rewritten.

## Fingerprints and build recording

A tracked unit's short fingerprint contains one of:

```text
<16-digit fingerprint>
unit-output-v1 <64-digit unit-output hash>
```

```text
<16-digit fingerprint>
cache-entry-v1
```

The pointer is excluded from Cargo's fingerprint hash and diagnostic JSON. It does not propagate to dependent fingerprints or implement early cutoff. A cache-entry pointer carries no digest because the entry is named by the unit hash.

1. Load fingerprints while building the unit graph.
2. Reuse a fresh unit's pointer after validating the referenced metadata and blob sizes. This does not rehash output files. A pointer of the wrong kind for the unit's current cacheability is not reused.
3. For a rebuilt or previously untracked unit, hash and deduplicate its output files, then publish its cache entry or unit output.
4. Attach the pointer to the fingerprint only after publication.
5. After a successful invocation, publish the build snapshot and update its usage file.

Dirty units clear their pointer. A successful rebuild without tracking writes a plain fingerprint, so re-enabling tracking cannot reuse an obsolete pointer. Missing or damaged metadata is recaptured from still-fresh compiler outputs.

A failed invocation may leave completed unit outputs and cache entries, but it does not create or refresh a build snapshot. These objects alone do not retain blobs during garbage collection.

## Local build cache

A cache entry also contains a guard derived from Cargo's fingerprint and the dependency artifacts supplied to the compiler, plus each output's permissions and modification time. The guard protects against changed compiler inputs, including changed transitive environment-dependent outputs. Dependency artifact hashing is needed for cache lookup/publication, not ordinary fresh-build usage tracking. For a package with a build script, the guard also covers the parsed script output that reaches rustc (cfgs, check-cfgs, environment, link directives, and search paths relative to `OUT_DIR`) and the contents of its `OUT_DIR`. A clean build reruns the script without changing any fingerprint, so only the guard notices changed output.

Workspaces that share a unit hash share one entry, and the last publisher wins. A fresh unit keeps using whatever entry is present instead of comparing contents. Comparing contents would make two workspaces with different guards or timestamps rewrite the entry on every alternating build.

Eligibility is immutable registry or Git library units in build or non-test check mode, whose dependencies are eligible, the package's own build-script run, or immutable proc-macros. Build scripts and proc-macros themselves still compile. The compiled proc-macro is part of its consumers' guard. Like Cargo's freshness checks, the guard does not cover files or environment variables that a proc-macro reads without declaring them. Local units, artifact dependencies, per-unit extra arguments, forced rebuilds, custom compiler commands, and compiler wrappers are excluded. Custom executors are excluded unless they explicitly opt into local caching.

On a cache hit:

1. Read the cache entry and check its input guard.
2. Validate the entry's native relative output paths.
3. Stage private reflinks or copies and verify every blob's BLAKE3 hash before replacing the output tree.
4. Check rustc's recorded environment dependencies and reject source inputs outside the immutable package and its `OUT_DIR`. A recorded `OUT_DIR` path from another build directory maps to this build's `OUT_DIR` through the run unit's `<package>/<hash>/out` suffix.
5. Regenerate Cargo's translated dep-info, replay cached diagnostics, and notify the scheduler when metadata is available.
6. Persist the fingerprint and include the cache entry in the successful build snapshot.

Missing, corrupt, or incompatible entries fall back to compilation. A clean build directory can restore dependencies without invoking rustc, while local packages still compile.

Restored compiler outputs receive the current invocation timestamp to preserve subsequent freshness. Restoration therefore uses private reflinks or copies rather than hardlinks.

Cache entries are retained only while a retained build snapshot names them. A successful cache hit refreshes snapshot usage. This resolves cache-entry retention without adding a timestamp to each entry.

## Remote build cache

The optional `[cache.remote]` configuration connects to a BuildBuddy-compatible REAPI cache. It supports TLS, instance names, environment-supplied API keys, read-only access, RPC deadlines, and streaming inactivity timeouts. Configuration details are in the [unstable feature reference](doc/book/src/reference/unstable.md#remote-build-cache).

Local restoration is attempted first. Remote ActionCache keys hash a versioned Cargo namespace, host OS, unit hash, and input guard with SHA256. Each ActionResult lists the cache entry and every referenced blob as an output file. This exposes the complete graph to server-side retention and missing-blob checks.

Uploads hash files incrementally, query FindMissingBlobs, transfer missing contents, and publish the action result last. A server can finish a streamed upload early when another writer has already stored the blob. The client checks the complete committed size before accepting that response.

Blobs up to 1 MiB are packed into BatchUpdateBlobs and BatchReadBlobs requests of at most 3 MiB, which stays under the default 4 MiB gRPC message limit. Larger blobs use ByteStream. A unit runs up to four transfers concurrently, which also bounds memory use.

The timeout bounds each unary RPC and each wait for streaming progress or the final upload acknowledgment. It does not cap local hashing, a progressing blob transfer, or the combined uploads for a unit. Timeout warnings identify the operation.

Lookups ask the server to inline the cache entry in the ActionResult. Inlined contents are checked against their SHA256 digest. Servers may ignore the hint, in which case Cargo downloads the entry like other blobs.

Downloads stage files privately and validate REAPI SHA256 digests, BLAKE3 identities, native relative paths, metadata counts, and blob sizes. Graph metadata is published locally only after the referenced blobs have been verified. The existing output restoration and rustc input checks still decide whether a hit is usable. Absolute compiler paths are not relocated.

Fresh builds make no remote requests. Only newly compiled eligible units can populate the remote cache. Local and remote hits are not republished. Restored input guards and output identities are tracked so that rejecting a hit does not suppress publication of changed artifacts after recompilation. Offline and frozen builds make no remote requests. Remote failures warn and disable further remote operations for that invocation without disabling local storage or compilation.

Publication runs on four background upload workers, so compilation and dependent units do not wait on the network. The build waits for queued uploads before it finishes and releases its storage lock, so collection cannot remove blobs mid-upload. Cargo prints an `Uploading` status while it waits.

Lookups and downloads start ahead of the job queue. An input guard hashes dependency artifacts, and those hashes equal the blob hashes in each dependency's cache entry or unit output. Cargo records them when a dependency is prefetched, accepted after restoration, captured after compilation, or retained as fresh. Sixteen prefetch threads take dirty cacheable units whose dependency hashes and build-script outputs are known, compute the guard from metadata, and fetch the entry into local storage. They hold no jobserver tokens. The job queue skips a job while its unit's fetch is in flight and starts other ready work instead. The finished fetch wakes the queue, and the queue keeps its acquired tokens while deferred jobs are pending. Only an in-flight fetch defers a job, and every fetch ends in a hit, a miss, or a remote failure, so deferral cannot stall the build. A job claims its unit before restoring. It removes a unit that prefetching has not started, and skips the remote lookup after a prefetch miss for the same guard. The job still computes its guard from files on disk, so a dependency that was rejected and recompiled produces a different guard and an ordinary lookup. The build stops prefetching before it waits for uploads.

Lookups and other small RPCs use their own HTTP/2 connection, and blob transfers use two more, so a lookup does not wait behind a multi-hundred-megabyte transfer. Unavailable servers, timeouts, and similar transient failures are retried up to three times with backoff before the remote cache is disabled for the invocation. Integrity failures are not retried. When `GetCapabilities` advertises zstd, transfers use `compressed-blobs/zstd` ByteStream resources and zstd batch encodings. Cargo still verifies the uncompressed size, SHA256, and BLAKE3 identity.

Remote cache writers must be trusted. Compiler artifacts can contain diagnostics, source paths, and recorded environment values. Snapshot usage and build snapshots are never uploaded, and local garbage collection does not delete remote data.

## Deduplication and hardlink safety

Capture prefers reflinks, then hardlinks where permitted, then copies. Reuse through a hardlink requires compatible permissions and modification times. A private replacement preserves the original compiler output's metadata. Cached bytes are verified before replacing good compiler output.

Before any dirty non-local rustc invocation, Cargo removes the per-unit output tree. This detaches artifacts, raw dep-info, and auxiliary outputs from blobs before a compiler or linker can truncate them. It also applies when tracking is disabled for that invocation.

Rustdoc capture does not use hardlinks because its output-writing lifecycle differs. Cache restoration remains private because the compiler updates restored timestamps.

Hardlinked output files and cache contents must not be modified by external tools in place. Cache writers must be trusted. BLAKE3 verifies contents, not provenance.

## Locking and publication

The implementation reuses Cargo's package-cache locking system for local coordination:

- Builds hold its shared lock for the storage lifetime, excluding garbage collection.
- Immutable object writes use staged atomic publication.
- Snapshot and usage commits also hold the download/append lock.
- Garbage collection holds the exclusive mutation lock before traversing or deleting shared storage.

Publication orders blobs before unit outputs and cache entries, those before fingerprint pointers, and build snapshots before their usage files. A crash can leave unreferenced objects. Garbage collection removes them later. Unrecognized files, including interrupted staging files, are left alone.

There is no remote locking protocol.

## Garbage collection

Snapshot usage is the retention root. Automatic collection uses the existing Cargo GC schedule and a 30-day maximum age. It is skipped offline and does not wait for a busy automatic-GC lock.

Under the exclusive mutation lock:

1. Read snapshot usage files and identify recent build snapshots.
2. Validate build snapshots, their unit outputs, and their cache entries.
3. Walk unit outputs and cache entries to discover referenced blobs, checking regular-file status and sizes.
4. If a size limit is requested, evict the oldest snapshots until retained blobs fit. Count each distinct blob once.
5. Remove expired usage files.
6. Remove unretained build snapshots, unit outputs, cache entries, and blobs.

The collector prepares the complete plan before deletion. Unreadable or malformed recognized usage files stop collection because they may hide a live root. Incomplete snapshots do not retain blobs. Blob contents are not rehashed during collection.

Snapshots without recent usage do not receive an import grace period. There are no cache revision tokens, workspace receipts, or throttled usage updates.

Manual size-limited collection:

```sh
cargo clean gc -Zgc -Zshared-blob-storage --max-blob-size 10GiB
```

Add `--dry-run` to preview without changing metadata or blobs. Logical size excludes metadata and filesystem overhead. Collection does not delete build-directory outputs. A blob's data blocks may remain allocated while build-directory hardlinks exist. Ordinary `cargo clean` leaves shared storage available for restoration.

## Metadata formats

Formats are versioned binary records, except snapshot usage timestamps. They are unstable implementation details, not a public compatibility promise.

- Unit output: `cargo-shared-storage-unit-output-v1\0`, path-encoding byte, u64 count, then repeated u64 path length, path bytes, 32-byte blob hash, and u64 size.
- Build snapshot: `cargo-shared-storage-snapshot-v2\0`, u64 count and sorted unique 32-byte unit-output hashes, then u64 count and sorted unique length-prefixed cache-entry names.
- Cache entry: `cargo-shared-storage-cache-entry-v2\0`, path-encoding byte, u64 input guard, u64 count, then in canonical path order: u64 path length, path bytes, 32-byte blob hash, u64 size, u32 permission bits, i64 modification-time seconds, and u32 nanoseconds.
- Snapshot usage: decimal Unix seconds and a newline.

Integers are little-endian. Path encodings distinguish Unix bytes, Windows UTF-16 code units, and UTF-8 fallback. Paths are relative to the unit directory. Restoration accepts only paths within its output tree. Metadata traversal does not require native path decoding, while restoration requires the native encoding.

## Remaining design boundaries

- Remote protocols are unresolved and not implemented.
- Remote garbage collection needs its own retention and publication coordination.
- Cache entries duplicate unit-output metadata rather than referencing it, trading a few bytes per entry for one fewer lookup.
- Local packages, build-script opt-in, proc-macro opt-in, and rebuild early cutoff remain outside this implementation.
