//! Unit outputs and cache entries rooted only by per-snapshot usage timestamps.
//!
//! Builds hold a package-cache Shared lock for the storage lifetime. Snapshot
//! commits also hold DownloadExclusive; collection requires MutateExclusive.
//! Object publication uses complete atomic replacements, never in-place writes.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

use super::format::{
    CacheEntry, Digest, Output, Snapshot, UnitOutput, decode_native_unit, decode_unit,
    digest_filename, valid_unit_hash,
};
use crate::CargoResult;
use crate::ops::CleanContext;
use crate::util::data_structures::HashMap;

const RETENTION: u64 = 30 * 24 * 60 * 60;
const BLOBS: &str = "blobs";
const UNITS: &str = "unit-output";
const SNAPSHOTS: &str = "snapshots";
const USAGE: &str = "usage";
const CACHE: &str = "cache-entries";

pub(super) struct SnapshotStore<'a> {
    root: &'a Path,
}

impl<'a> SnapshotStore<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub fn publish_unit(&self, output: &UnitOutput) -> CargoResult<()> {
        self.check_directories()?;
        publish_object(&self.unit_path(&output.id), &output.bytes)
    }

    pub fn read_unit(&self, id: &Digest) -> CargoResult<Option<Vec<Output>>> {
        self.check_directories()?;
        let Some(bytes) = read_regular(&self.unit_path(id))? else {
            return Ok(None);
        };
        Ok(decode_native_unit(&bytes, id).ok())
    }

    pub fn unit_path(&self, id: &Digest) -> PathBuf {
        self.root.join(UNITS).join(hex(id))
    }

    /// The unit output's files when it and every referenced blob are present.
    /// Blob sizes are validated without rehashing immutable bytes.
    pub fn complete_unit(&self, id: &Digest) -> CargoResult<Option<Vec<Output>>> {
        let Some(outputs) = self.read_unit(id)? else {
            return Ok(None);
        };
        for output in &outputs {
            if regular_size(&blob_path(self.root, &output.hash))? != Some(output.size) {
                return Ok(None);
            }
        }
        Ok(Some(outputs))
    }

    /// Returns the entry with the BLAKE3 digest of its encoded bytes.
    pub fn read_cache_entry(&self, unit_hash: &str) -> CargoResult<Option<(Digest, CacheEntry)>> {
        self.check_directories()?;
        let path = self.cache_path(unit_hash)?;
        let Some(bytes) = read_regular(&path)? else {
            self.evict_cache_entry(unit_hash)?;
            return Ok(None);
        };
        match CacheEntry::decode_native(&bytes) {
            Ok(entry) => Ok(Some((*blake3::hash(&bytes).as_bytes(), entry))),
            Err(_) => {
                self.evict_cache_entry(unit_hash)?;
                Ok(None)
            }
        }
    }

    /// The entry when it decodes natively and every referenced blob is present.
    /// Blob sizes are validated without rehashing blob contents.
    pub fn complete_cache_entry(&self, unit_hash: &str) -> CargoResult<Option<CacheEntry>> {
        self.check_directories()?;
        let Some(bytes) = read_regular(&self.cache_path(unit_hash)?)? else {
            return Ok(None);
        };
        let Ok(entry) = CacheEntry::decode_native(&bytes) else {
            return Ok(None);
        };
        for output in &entry.outputs {
            if regular_size(&blob_path(self.root, &output.hash))? != Some(output.size) {
                return Ok(None);
            }
        }
        Ok(Some(entry))
    }

    /// Returns the BLAKE3 digest of the encoded entry.
    pub fn publish_cache_entry(&self, unit_hash: &str, entry: &CacheEntry) -> CargoResult<Digest> {
        self.check_directories()?;
        let bytes = entry.encode();
        atomic_replace(&self.cache_path(unit_hash)?, &bytes)?;
        Ok(*blake3::hash(&bytes).as_bytes())
    }

    pub fn evict_cache_entry(&self, unit_hash: &str) -> CargoResult<()> {
        let path = self.cache_path(unit_hash)?;
        let removal = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        match removal {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("failed to evict {}", path.display())),
        }
    }

    fn cache_path(&self, unit_hash: &str) -> CargoResult<PathBuf> {
        ensure!(valid_unit_hash(unit_hash), "invalid local cache unit hash");
        Ok(self.root.join(CACHE).join(unit_hash))
    }

    /// Refresh every successful use, even when the graph has not changed.
    pub fn save(
        &self,
        unit_outputs: Vec<Digest>,
        cache_entries: Vec<String>,
        now: u64,
    ) -> CargoResult<()> {
        self.check_directories()?;
        let bytes = Snapshot::new(unit_outputs, cache_entries).encode();
        let id = *blake3::hash(&bytes).as_bytes();
        let manifest = self.root.join(SNAPSHOTS).join(hex(&id));
        if !read_regular(&manifest)?.is_some_and(|existing| existing == bytes) {
            atomic_replace(&manifest, &bytes)?;
        }
        atomic_replace(&self.usage_path(&id), format!("{now}\n").as_bytes())
    }

    fn usage_path(&self, id: &Digest) -> PathBuf {
        self.root.join(SNAPSHOTS).join(USAGE).join(hex(id))
    }

    pub fn clean(
        &self,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
        now: u64,
    ) -> CargoResult<()> {
        let plan = self.plan_collection(max_size, now)?;
        for path in plan.removals {
            clean_ctx.rm_rf(&path)?;
        }
        Ok(())
    }

    fn check_directories(&self) -> CargoResult<()> {
        check_directory(self.root)?;
        for directory in [BLOBS, UNITS, SNAPSHOTS, CACHE] {
            check_directory(&self.root.join(directory))?;
        }
        check_directory(&self.root.join(SNAPSHOTS).join(USAGE))?;
        Ok(())
    }

    /// Read and plan the complete graph before mutation. Unreadable or malformed
    /// usage can hide a live root, so it aborts collection rather than pruning.
    fn plan_collection(&self, max_size: Option<u64>, now: u64) -> CargoResult<Collection> {
        self.check_directories()?;
        let mut blobs = HashMap::default();
        let mut invalid = Vec::new();
        for (id, path) in blob_files(&self.root.join(BLOBS))? {
            if let Some(size) = regular_size(&path)? {
                blobs.insert(id, (path, size));
            } else {
                invalid.push(path);
            }
        }
        // Every complete unit output and cache entry, with its distinct blobs.
        let references = |outputs: Vec<(Digest, u64)>| -> Option<Vec<Digest>> {
            let mut references = Vec::with_capacity(outputs.len());
            for (hash, size) in outputs {
                if blobs.get(&hash).is_none_or(|(_, stored)| *stored != size) {
                    return None;
                }
                references.push(hash);
            }
            references.sort_unstable();
            references.dedup();
            Some(references)
        };
        let mut members = HashMap::default();
        let unit_files = object_files(&self.root.join(UNITS))?;
        for (id, path) in &unit_files {
            let Some(bytes) = read_regular(path)? else {
                continue;
            };
            let Ok(outputs) = decode_unit(&bytes, id) else {
                continue;
            };
            let outputs = outputs.iter().map(|output| (output.hash, output.size));
            if let Some(references) = references(outputs.collect()) {
                members.insert(Member::UnitOutput(*id), references);
            }
        }
        let mut cache_files = Vec::new();
        for entry in entries(&self.root.join(CACHE))? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_unit_hash(&name) {
                continue;
            }
            let path = entry.path();
            // Entries are decoded in any path encoding. Invalid ones are disposable.
            if let Some(entry) =
                read_regular(&path)?.and_then(|bytes| CacheEntry::decode(&bytes).ok())
                && let Some(references) = references(
                    entry
                        .outputs
                        .iter()
                        .map(|output| (output.hash, output.size))
                        .collect(),
                )
            {
                members.insert(Member::CacheEntry(name.clone()), references);
            }
            cache_files.push((Member::CacheEntry(name), path));
        }
        let snapshot_files = object_files(&self.root.join(SNAPSHOTS))?;
        let mut snapshots = HashMap::default();
        for (id, path) in &snapshot_files {
            let Some(bytes) = read_regular(path)? else {
                continue;
            };
            let Ok(snapshot) = Snapshot::decode(&bytes, id) else {
                continue;
            };
            let snapshot_members: Vec<_> = snapshot
                .unit_outputs
                .into_iter()
                .map(Member::UnitOutput)
                .chain(snapshot.cache_entries.into_iter().map(Member::CacheEntry))
                .collect();
            if snapshot_members
                .iter()
                .all(|member| members.contains_key(member))
            {
                snapshots.insert(*id, snapshot_members);
            }
        }

        let cutoff = now.saturating_sub(RETENTION);
        let mut usages = Vec::new();
        let mut recency = HashMap::<Digest, u64>::default();
        for entry in entries(&self.root.join(SNAPSHOTS).join(USAGE))? {
            let path = entry.path();
            let Some(id) = entry.file_name().to_str().and_then(digest_filename) else {
                continue;
            };
            let bytes = read_regular(&path)?
                .with_context(|| format!("unreadable snapshot usage: {}", path.display()))?;
            let timestamp = std::str::from_utf8(&bytes)?;
            let timestamp = timestamp.strip_suffix('\n').unwrap_or(timestamp);
            ensure!(
                !timestamp.is_empty() && timestamp.bytes().all(|b| b.is_ascii_digit()),
                "invalid snapshot usage timestamp: {}",
                path.display()
            );
            let time: u64 = timestamp
                .parse()
                .with_context(|| format!("invalid snapshot usage timestamp: {}", path.display()))?;
            if time >= cutoff && snapshots.contains_key(&id) {
                recency.insert(id, time);
            }
            usages.push((path, id, time));
        }

        // Reference counts count unique physical blobs across all retained roots.
        let mut member_refs = HashMap::<&Member, usize>::default();
        let mut blob_refs = HashMap::<Digest, usize>::default();
        let mut size = 0_u128;
        for id in recency.keys() {
            for member in &snapshots[id] {
                let count = member_refs.entry(member).or_default();
                *count += 1;
                if *count == 1 {
                    for blob in &members[member] {
                        let count = blob_refs.entry(*blob).or_default();
                        *count += 1;
                        if *count == 1 {
                            size += u128::from(blobs[blob].1);
                        }
                    }
                }
            }
        }
        if let Some(limit) = max_size {
            let mut oldest: Vec<_> = recency.iter().map(|(id, time)| (*time, *id)).collect();
            oldest.sort_unstable();
            for (_, id) in oldest {
                if size <= u128::from(limit) {
                    break;
                }
                recency.remove(&id);
                for member in &snapshots[&id] {
                    let count = member_refs.get_mut(member).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        for blob in &members[member] {
                            let count = blob_refs.get_mut(blob).unwrap();
                            *count -= 1;
                            if *count == 0 {
                                size -= u128::from(blobs[blob].1);
                            }
                        }
                    }
                }
            }
        }

        let retained = |member: &Member| member_refs.get(member).is_some_and(|count| *count > 0);
        let mut plan = Collection::default();
        for (path, id, _) in usages {
            if !recency.contains_key(&id) {
                plan.removals.push(path);
            }
        }
        for (member, path) in cache_files {
            if !retained(&member) {
                plan.removals.push(path);
            }
        }
        for (id, path) in snapshot_files {
            if !recency.contains_key(&id) {
                plan.removals.push(path);
            }
        }
        for (id, path) in unit_files {
            if !retained(&Member::UnitOutput(id)) {
                plan.removals.push(path);
            }
        }
        for (id, (path, _)) in blobs {
            if !blob_refs.get(&id).is_some_and(|count| *count > 0) {
                plan.removals.push(path);
            }
        }
        plan.removals.extend(invalid);
        Ok(plan)
    }
}

/// A snapshot member that references blobs.
#[derive(PartialEq, Eq, Hash)]
enum Member {
    UnitOutput(Digest),
    CacheEntry(String),
}

#[derive(Default)]
struct Collection {
    removals: Vec<PathBuf>,
}

pub(super) fn hex(id: &Digest) -> String {
    blake3::Hash::from_bytes(*id).to_hex().to_string()
}

/// Blobs are sharded as `blobs/<hash[..2]>/<hash[2..]>`.
pub(super) fn blob_path(root: &Path, id: &Digest) -> PathBuf {
    let hex = hex(id);
    root.join(BLOBS).join(&hex[..2]).join(&hex[2..])
}

/// Move a verified staged file into its blob path, creating the shard directory.
pub(super) fn publish_blob(staged: &Path, blob: &Path) -> CargoResult<()> {
    let shard = blob.parent().expect("blob has a shard directory");
    check_directory(shard.parent().expect("shard has a blob directory"))?;
    check_directory(shard)?;
    fs::create_dir_all(shard)?;
    fs::rename(staged, blob)?;
    Ok(())
}

fn entries(path: &Path) -> CargoResult<Vec<fs::DirEntry>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(entries.collect::<Result<_, _>>()?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to list {}", path.display())),
    }
}

fn object_files(path: &Path) -> CargoResult<HashMap<Digest, PathBuf>> {
    let mut files = HashMap::default();
    for entry in entries(path)? {
        if let Some(id) = entry.file_name().to_str().and_then(digest_filename) {
            files.insert(id, entry.path());
        }
    }
    Ok(files)
}

fn blob_files(path: &Path) -> CargoResult<HashMap<Digest, PathBuf>> {
    let mut files = HashMap::default();
    for shard in entries(path)? {
        let Some(prefix) = shard.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if prefix.len() != 2 || !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in entries(&shard.path())? {
            let Some(rest) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Some(id) = digest_filename(&format!("{prefix}{rest}")) {
                files.insert(id, entry.path());
            }
        }
    }
    Ok(files)
}

pub(super) fn check_directory(path: &Path) -> CargoResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "shared storage path is not a directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) fn regular_size(path: &Path) -> CargoResult<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file().then_some(metadata.len())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn read_regular(path: &Path) -> CargoResult<Option<Vec<u8>>> {
    if regular_size(path)?.is_none() {
        return Ok(None);
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn publish_object(path: &Path, bytes: &[u8]) -> CargoResult<()> {
    if read_regular(path)?.is_some_and(|existing| existing == bytes) {
        return Ok(());
    }
    atomic_replace(path, bytes)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> CargoResult<()> {
    let parent = path.parent().expect("metadata file has a parent");
    check_directory(parent)?;
    fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged
        .persist(path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::format::{CachedOutput, encode_output_path};
    use super::*;

    fn blob(store: &SnapshotStore<'_>, contents: &[u8]) -> (Digest, u64) {
        let hash = *blake3::hash(contents).as_bytes();
        let blob = blob_path(store.root, &hash);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(blob, contents).unwrap();
        (hash, contents.len() as u64)
    }

    fn output(store: &SnapshotStore<'_>, contents: &[u8]) -> UnitOutput {
        let (hash, size) = blob(store, contents);
        UnitOutput::new(vec![Output {
            path: encode_output_path(Path::new("out/artifact")).unwrap(),
            hash,
            size,
        }])
    }

    fn entry(store: &SnapshotStore<'_>, contents: &[u8]) -> CacheEntry {
        let (hash, size) = blob(store, contents);
        CacheEntry::new(
            42,
            vec![CachedOutput {
                path: encode_output_path(Path::new("out/artifact")).unwrap(),
                hash,
                size,
                mode: 0o644,
                mtime_seconds: 0,
                mtime_nanos: 0,
            }],
        )
    }

    fn snapshot_id(unit_outputs: Vec<Digest>, cache_entries: &[&str]) -> Digest {
        let cache_entries = cache_entries
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        *blake3::hash(&Snapshot::new(unit_outputs, cache_entries).encode()).as_bytes()
    }

    fn save(store: &SnapshotStore<'_>, output: &UnitOutput, now: u64) -> Digest {
        store.publish_unit(output).unwrap();
        store.save(vec![output.id], Vec::new(), now).unwrap();
        snapshot_id(vec![output.id], &[])
    }

    fn clean(
        store: &SnapshotStore<'_>,
        max_size: Option<u64>,
        now: u64,
        dry_run: bool,
    ) -> CargoResult<()> {
        let gctx = crate::util::GlobalContext::new(
            cargo_util_terminal::Shell::from_write(Box::new(Vec::new())),
            store.root.to_path_buf(),
            store.root.join("cargo-home"),
        );
        let mut clean_ctx = CleanContext::new(&gctx);
        clean_ctx.dry_run = dry_run;
        store.clean(&mut clean_ctx, max_size, now)
    }

    #[test]
    fn usage_refreshes_every_use_and_shares_snapshot_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let output = output(&store, b"shared");
        let snapshot = save(&store, &output, 10);
        save(&store, &output, 11);
        let usage = store.usage_path(&snapshot);
        assert_eq!(fs::read(&usage).unwrap(), b"11\n");
        save(&store, &output, RETENTION + 10);
        clean(&store, None, RETENTION + 12, false).unwrap();
        assert!(usage.exists());
        assert!(store.complete_unit(&output.id).unwrap().is_some());
        clean(&store, None, 2 * RETENTION + 11, false).unwrap();
        assert!(!usage.exists());
        assert!(store.complete_unit(&output.id).unwrap().is_none());
    }

    #[test]
    fn unreferenced_snapshots_never_gain_retention() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let output = output(&store, b"orphan");
        let snapshot = save(&store, &output, 1);
        fs::remove_dir_all(store.root.join(SNAPSHOTS).join(USAGE)).unwrap();
        clean(&store, None, 2, false).unwrap();
        assert!(!store.root.join(SNAPSHOTS).join(hex(&snapshot)).exists());
        assert!(store.complete_unit(&output.id).unwrap().is_none());
        assert!(blob_files(&store.root.join(BLOBS)).unwrap().is_empty());
    }

    #[test]
    fn size_pressure_preserves_blobs_shared_across_member_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let shared = output(&store, b"shared");
        store.publish_unit(&shared).unwrap();
        store
            .publish_cache_entry("aa", &entry(&store, b"shared"))
            .unwrap();
        store
            .publish_cache_entry("bb", &entry(&store, b"old"))
            .unwrap();
        store
            .save(vec![shared.id], vec!["bb".to_owned()], 1)
            .unwrap();
        store.save(Vec::new(), vec!["aa".to_owned()], 2).unwrap();
        let shared_blob = blob_path(store.root, blake3::hash(b"shared").as_bytes());
        let old_blob = blob_path(store.root, blake3::hash(b"old").as_bytes());
        clean(&store, Some(6), 3, true).unwrap();
        assert!(store.read_cache_entry("bb").unwrap().is_some());
        assert!(old_blob.exists());
        // Evicting the oldest snapshot drops its members, but not a blob that the
        // newer snapshot reaches through a cache entry.
        clean(&store, Some(6), 3, false).unwrap();
        assert!(store.read_cache_entry("aa").unwrap().is_some());
        assert!(store.read_cache_entry("bb").unwrap().is_none());
        assert!(!store.unit_path(&shared.id).exists());
        assert!(shared_blob.exists());
        assert!(!old_blob.exists());
    }

    #[test]
    fn cache_entries_are_retained_only_through_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        store
            .publish_cache_entry("aa", &entry(&store, b"unreferenced"))
            .unwrap();
        store
            .publish_cache_entry("bb", &entry(&store, b"referenced"))
            .unwrap();
        store.save(Vec::new(), vec!["bb".to_owned()], 1).unwrap();
        clean(&store, None, 2, false).unwrap();
        assert!(store.read_cache_entry("aa").unwrap().is_none());
        assert!(!blob_path(store.root, blake3::hash(b"unreferenced").as_bytes()).exists());
        assert!(store.complete_cache_entry("bb").unwrap().is_some());
        fs::remove_file(blob_path(
            store.root,
            blake3::hash(b"referenced").as_bytes(),
        ))
        .unwrap();
        assert!(store.complete_cache_entry("bb").unwrap().is_none());
    }

    #[test]
    fn invalid_usage_prevents_any_collection_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let output = output(&store, b"protected");
        let snapshot = save(&store, &output, 0);
        let usage = store.usage_path(&snapshot);
        fs::write(&usage, b"corrupt").unwrap();
        assert!(clean(&store, Some(0), RETENTION + 1, false).is_err());
        assert!(store.complete_unit(&output.id).unwrap().is_some());
        assert_eq!(fs::read(usage).unwrap(), b"corrupt");
    }

    #[test]
    fn invalid_cache_entries_are_disposable_not_roots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let output = output(&store, b"live");
        save(&store, &output, 1);
        fs::create_dir_all(store.root.join(CACHE)).unwrap();
        let corrupt = store.root.join(CACHE).join("aa");
        fs::write(&corrupt, b"invalid").unwrap();
        clean(&store, None, 2, true).unwrap();
        assert!(corrupt.exists());
        clean(&store, None, 2, false).unwrap();
        assert!(!corrupt.exists());
        assert!(store.complete_unit(&output.id).unwrap().is_some());
    }

    #[test]
    fn unrelated_files_and_interrupted_publications_do_not_block_collection() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let output = output(&store, b"expired");
        save(&store, &output, 0);
        let usage = store.root.join(SNAPSHOTS).join(USAGE);
        let leftovers = [
            usage.join("unrelated"),
            usage.join(".tmp-interrupted"),
            store.root.join(CACHE).join("unrelated"),
            store.root.join(BLOBS).join("unrelated"),
            store.root.join(BLOBS).join("zz").join("unrelated"),
            blob_path(store.root, &[0xab; 32]).with_file_name("unrelated"),
        ];
        for path in &leftovers {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"leave alone").unwrap();
        }
        clean(&store, None, RETENTION + 1, false).unwrap();
        assert!(store.complete_unit(&output.id).unwrap().is_none());
        for path in leftovers {
            assert_eq!(fs::read(path).unwrap(), b"leave alone");
        }
    }
}
