# Blob storage GC design

## 1. Purpose and scope

The cache shares identical build outputs across builds and workspaces. Garbage collection removes outputs that no retained build snapshot needs.

The main rules are:

- Keep recently used snapshots.
- Delete older snapshots when necessary to meet a size limit.
- Keep shared files until their last retained reference disappears.
- Never delete build-directory files.
- Store everything needed for transport without a shared database.

Blob metadata no longer uses SQLite. Cargo's separate `.global-cache` database still handles unrelated cache tracking and GC scheduling.

The current capture path covers eligible non-local build units using the new build-directory layout. A snapshot describes their recorded outputs, rather than every output or input in the entire workspace.

## 2. What is stored

| Item | Meaning |
|---|---|
| Blob | One output file's contents, named by its BLAKE3 hash. |
| Unit inventory | A list of output paths, blob hashes, and sizes for one recorded build step. |
| Snapshot | A sorted, duplicate-free list of unit inventories used by a successful build. |
| Local receipt | Freshness information and last-used times for one build directory. |
| Import receipt | Temporary retention records for snapshots restored without local receipts. |

```text
$CARGO_HOME/blobs/
  <hash>                  Blob contents
  units-v1/<hash>          Unit inventory
  snapshots-v1/<hash>      Snapshot
  local-v1/<directory-id>  Local receipt
  local-v1/imports         Import retention records
```

Blobs, inventories, and snapshots are immutable: changing their contents changes their names. Receipts are mutable and replaced atomically.

Manifests use versioned binary formats with explicit path encodings. They contain no machine-specific build-directory identifiers or usage timestamps.

## 3. Recording a build

1. Read the build directory's receipt.
2. Reuse recorded inventory IDs when Cargo's freshness information still matches.
3. Hash and capture outputs for changed or previously untracked units.
4. Publish completed unit inventories.
5. After a successful build, publish its snapshot.
6. Replace the local receipt last.

Ordinary fresh builds avoid walking and hashing every output.

Identical recorded output sets produce the same snapshot ID. Different feature combinations can retain separate snapshots while sharing unchanged blobs.

Failed builds can save completed unit inventories, but they do not create or refresh a successful-build snapshot. Those inventories alone do not protect blobs from GC.

## 4. Retention rules

### Normal usage

A snapshot remains eligible for retention while at least one build directory has recorded using it within the last 30 days.

Usage updates are limited to once every four hours per build-directory/snapshot pair. One workspace's update does not suppress another workspace's update.

GC uses the most recent retained usage across all receipts.

This makes age approximate: recorded usage can lag actual usage by almost four hours. Wall-clock changes can also affect expiry.

### Restored snapshots

A complete snapshot with no usage record receives a 30-day grace period, starting when local GC first discovers it.

Ordinary GC does not renew that period. Expired records are considered before assigning new import periods, preventing automatic renewal during normal collection.

This is local retention. A new machine, deleted receipts, or some interrupted deletion sequences can result in a new grace period.

### Size pressure

An explicit size limit can evict snapshots before their 30 days expire.

GC counts each distinct blob once, then removes the oldest snapshots until retained blob contents fit the limit. Equal usage times are ordered by snapshot hash for consistent results.

The limit excludes manifest files, receipts, temporary files, and filesystem overhead. A zero-byte limit therefore does not guarantee an entirely empty cache directory.

## 5. Collection procedure

GC prepares the complete deletion plan before changing the blob store.

1. Find managed files. List blobs, inventories, snapshots, and receipts.
2. Validate inventories. Check their hashes and structure, and require referenced blobs to exist with the declared sizes.
3. Validate snapshots. Require every referenced inventory to be valid and complete.
4. Choose retained snapshots. Apply recorded usage, import grace periods, and the age cutoff.
5. Apply the size limit. Remove oldest snapshots while tracking which inventories and blobs remain shared.
6. Update receipts. Remove obsolete usage and lookup entries; delete empty receipts.
7. Delete unused objects. Remove snapshot files first, then inventories, then blobs. Remove recognized legacy database and timestamp files as well.

For example:

```text
Snapshot A -> blobs X, Y
Snapshot B -> blobs Y, Z
```

Evicting A removes X. Y remains because B still needs it.

Blobs without any retained snapshot are eligible for removal immediately; their file modification times do not grant retention.

## 6. Safety and failure handling

### Concurrent builds

Builds hold Cargo's shared package-cache lock. GC requires the exclusive mutation lock, so it cannot remove blobs while participating builds are using or publishing them.

Metadata publication is also serialized. These locks protect local Cargo processes; they do not coordinate remote machines.

### Corrupt or incomplete data

- Invalid manifests and missing or wrong-sized blobs make affected snapshots ineligible for retention.
- An unreadable or corrupt receipt stops blob GC before deletion. It may contain usage that would otherwise protect data.
- Unexpected non-regular metadata files also stop collection.
- Unrecognized files are left alone.

GC does not hash every blob's contents. Same-size corruption can remain until reuse.

Before cached bytes replace compiler outputs, their hash is checked. Incorrect contents and stored symlinks are repaired from the existing compiler output.

### Interrupted writes and deletion

Files are staged and renamed into place. Publication writes referenced objects before snapshots, then receipts last.

Collection updates receipts before deleting objects. An interruption can leave extra data, and a later run may retain some of it again.

This provides ordered, atomic file replacement, not a database transaction or a guarantee against every power-loss scenario.

### Existing build outputs

GC removes cache entries, not build-directory outputs. Removing a cache entry may free less disk space than its reported size because build outputs can still share the underlying data.

A blob-store dry run changes neither receipts nor objects.

## 7. When GC runs

Blob collection uses Cargo's existing GC scheduling:

- Automatic collection normally runs at most once a day.
- It is skipped offline.
- Automatic collection skips rather than waits when the required lock is busy.
- `cache.auto-clean-frequency` controls the schedule.
- Automatic blob collection applies age retention without a default blob-size cap.

Manual size-limited collection:

```sh
cargo clean gc -Zgc -Zshared-blob-storage --max-blob-size 10GiB
```

Preview the same operation:

```sh
cargo clean gc -Zgc -Zshared-blob-storage --max-blob-size 10GiB --dry-run
```

Ordinary `cargo clean` removes build outputs without clearing the shared blob cache.

## 8. Remote storage

Transfer blobs, inventories, and snapshots. Exclude `local-v1`.

A complete transfer includes each snapshot and every inventory and blob it references. Restore that complete set before running Cargo.

### S3

The layout maps directly to immutable object keys:

1. Upload blobs.
2. Upload unit inventories.
3. Upload snapshots last.

[S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) can avoid replacing existing objects.

Remote deletion needs separate coordination with all uploaders. Publishing snapshots last alone does not prevent a remote collector from deleting an upload's not-yet-referenced blobs. One machine's local retention records cannot determine what every other machine still needs.

### GitHub Actions cache

[Actions caches](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching) store immutable archives. Bundle complete snapshots and their files, using a new cache key when contents change.

GitHub retains or removes whole archives under its own policies. Cargo GC only manages the restored local copy. Separate archives may duplicate shared blobs.

### Boundaries

The implementation does not include cloud network clients or remote lookup that skips compilation. Compiled outputs can still depend on their toolchain, target, flags, and paths.

Hashes detect changed contents; they do not establish who produced them. Cache writers should be trusted.

## 9. Cost, migration, and verification

Fresh-build tracking stays small, but GC scans the stored metadata and keeps its reference counts in memory. GC work and memory grow with the cache. It avoids reading all blob contents.

Existing blob bytes are reused. Old SQLite metadata is not read; enabled builds recreate inventories and receipts. GC removes recognized obsolete database and timestamp files.

Verification completed for this implementation:

- 17 unit tests and 13 integration tests.
- Formatting, strict Clippy, and release build.
- Actual CLI archive restoration, retention and eviction.
- Cross-filesystem reuse between tmpfs and btrfs.
- Repair of a cached symlink without replacing a compiler output with that link.

Fresh Zed builds measured about 5.6 ms, or 1%, of tracking overhead across 30 randomized interleaved rounds. This measures fresh-build tracking, not large-cache GC throughput. Actual S3 and GitHub Actions transfers were not exercised.

Core implementation:

- `src/compiler/blob_storage/format.rs`: file formats.
- `src/compiler/blob_storage/snapshots.rs`: retention and deletion planning.
- `src/compiler/blob_storage/mod.rs`: capture, publication, and safe blob reuse.
- `src/workspace/gc.rs`: scheduling and GC entry points.
