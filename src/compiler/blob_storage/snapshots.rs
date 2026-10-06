//! Immutable unit outputs rooted only by local, per-snapshot workspace history.
//!
//! Builds hold a package-cache Shared lock for the storage lifetime. Snapshot
//! commits also hold DownloadExclusive; collection requires MutateExclusive.
//! Object publication uses complete atomic replacements, never in-place writes.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

use super::format::{
    CacheEntry, Digest, Output, UnitOutput, decode_native_unit, decode_snapshot, decode_unit,
    digest_filename, encode_snapshot, snapshot_id,
};
use crate::CargoResult;
use crate::ops::CleanContext;
use crate::util::data_structures::HashMap;

const RETENTION: u64 = 30 * 24 * 60 * 60;
const UNITS: &str = "unit-output";
const SNAPSHOTS: &str = "snapshots";
const HISTORY: &str = "workspace-history";
const CACHE: &str = "cache-entries";

pub(super) struct SnapshotStore<'a> {
    blobs: &'a Path,
    root: PathBuf,
}

impl<'a> SnapshotStore<'a> {
    pub fn new(blobs: &'a Path) -> Self {
        Self {
            blobs,
            root: blobs
                .parent()
                .expect("blob directory has a parent")
                .join("shared-storage"),
        }
    }

    pub fn publish_unit(&self, output: &UnitOutput) -> CargoResult<()> {
        self.check_directories()?;
        publish_object(&self.root.join(UNITS).join(hex(&output.id)), &output.bytes)
    }

    pub fn read_unit(&self, id: &Digest) -> CargoResult<Option<Vec<Output>>> {
        self.check_directories()?;
        let Some(bytes) = read_regular(&self.root.join(UNITS).join(hex(id)))? else {
            return Ok(None);
        };
        Ok(decode_native_unit(&bytes, id).ok())
    }

    /// Validate the unit output and blob sizes, without rehashing immutable bytes.
    pub fn unit_is_complete(&self, id: &Digest) -> CargoResult<bool> {
        let Some(outputs) = self.read_unit(id)? else {
            return Ok(false);
        };
        for output in outputs {
            if regular_size(&self.blobs.join(hex(&output.hash)))? != Some(output.size) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn read_cache_entry(&self, unit_hash: &str) -> CargoResult<Option<CacheEntry>> {
        self.check_directories()?;
        let path = self.cache_path(unit_hash)?;
        let Some(bytes) = read_regular(&path)? else {
            self.evict_cache_entry(unit_hash)?;
            return Ok(None);
        };
        match CacheEntry::decode(&bytes) {
            Ok(entry) => Ok(Some(entry)),
            Err(_) => {
                self.evict_cache_entry(unit_hash)?;
                Ok(None)
            }
        }
    }

    pub fn publish_cache_entry(&self, unit_hash: &str, entry: &CacheEntry) -> CargoResult<()> {
        self.check_directories()?;
        atomic_replace(&self.cache_path(unit_hash)?, &entry.encode())
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
    pub fn save(&self, workspace: &Digest, mut outputs: Vec<Digest>, now: u64) -> CargoResult<()> {
        self.check_directories()?;
        let id = snapshot_id(&mut outputs);
        let manifest = self.root.join(SNAPSHOTS).join(hex(&id));
        if !read_regular(&manifest)?.is_some_and(|bytes| blake3::hash(&bytes).as_bytes() == &id) {
            atomic_replace(&manifest, &encode_snapshot(&outputs))?;
        }
        let workspace_dir = self.root.join(HISTORY).join(hex(workspace));
        check_directory(&workspace_dir)?;
        atomic_replace(&workspace_dir.join(hex(&id)), format!("{now}\n").as_bytes())
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
        check_directory(self.blobs)?;
        check_directory(&self.root)?;
        for directory in [UNITS, SNAPSHOTS, HISTORY, CACHE] {
            check_directory(&self.root.join(directory))?;
        }
        Ok(())
    }

    /// Read and plan the complete graph before mutation. Unreadable or malformed
    /// history can hide a live root, so it aborts collection rather than pruning.
    fn plan_collection(&self, max_size: Option<u64>, now: u64) -> CargoResult<Collection> {
        self.check_directories()?;
        let mut blobs = HashMap::default();
        let mut invalid = Vec::new();
        for (id, path) in object_files(self.blobs)? {
            if let Some(size) = regular_size(&path)? {
                blobs.insert(id, (path, size));
            } else {
                invalid.push(path);
            }
        }
        let unit_files = object_files(&self.root.join(UNITS))?;
        let snapshot_files = object_files(&self.root.join(SNAPSHOTS))?;
        let mut units = HashMap::default();
        let mut output_counts = HashMap::default();
        for (id, path) in &unit_files {
            let Some(bytes) = read_regular(path)? else {
                continue;
            };
            let Ok(outputs) = decode_unit(&bytes, id) else {
                continue;
            };
            if outputs.iter().all(|output| {
                blobs
                    .get(&output.hash)
                    .is_some_and(|(_, size)| *size == output.size)
            }) {
                output_counts.insert(*id, outputs.len());
                let mut references: Vec<_> =
                    outputs.into_iter().map(|output| output.hash).collect();
                references.sort_unstable();
                references.dedup();
                units.insert(*id, references);
            }
        }
        let mut snapshots = HashMap::default();
        for (id, path) in &snapshot_files {
            let Some(bytes) = read_regular(path)? else {
                continue;
            };
            let Ok(outputs) = decode_snapshot(&bytes, id) else {
                continue;
            };
            if outputs.iter().all(|output| units.contains_key(output)) {
                snapshots.insert(*id, outputs);
            }
        }

        let cutoff = now.saturating_sub(RETENTION);
        let mut histories = Vec::new();
        let mut recency = HashMap::<Digest, u64>::default();
        for workspace in entries(&self.root.join(HISTORY))? {
            if workspace
                .file_name()
                .to_str()
                .and_then(digest_filename)
                .is_none()
            {
                continue;
            }
            let path = workspace.path();
            ensure!(
                workspace.file_type()?.is_dir(),
                "invalid workspace history directory: {}",
                path.display()
            );
            for entry in entries(&path)? {
                let path = entry.path();
                let Some(id) = entry.file_name().to_str().and_then(digest_filename) else {
                    continue;
                };
                let bytes = read_regular(&path)?
                    .with_context(|| format!("unreadable workspace history: {}", path.display()))?;
                let timestamp = std::str::from_utf8(&bytes)?;
                let timestamp = timestamp.strip_suffix('\n').unwrap_or(timestamp);
                ensure!(
                    !timestamp.is_empty() && timestamp.bytes().all(|b| b.is_ascii_digit()),
                    "invalid workspace history timestamp: {}",
                    path.display()
                );
                let time: u64 = timestamp.parse().with_context(|| {
                    format!("invalid workspace history timestamp: {}", path.display())
                })?;
                if time >= cutoff && snapshots.contains_key(&id) {
                    recency
                        .entry(id)
                        .and_modify(|last| *last = (*last).max(time))
                        .or_insert(time);
                }
                histories.push((path, id, time));
            }
        }

        // Reference counts count unique physical blobs across all retained roots.
        let mut unit_refs = HashMap::<Digest, usize>::default();
        let mut blob_refs = HashMap::<Digest, usize>::default();
        let mut size = 0_u128;
        for id in recency.keys() {
            for unit in &snapshots[id] {
                let count = unit_refs.entry(*unit).or_default();
                *count += 1;
                if *count == 1 {
                    for blob in &units[unit] {
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
                for unit in &snapshots[&id] {
                    let count = unit_refs.get_mut(unit).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        for blob in &units[unit] {
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

        let mut plan = Collection::default();
        for (path, id, time) in histories {
            if time < cutoff || !recency.contains_key(&id) {
                plan.removals.push(path);
            }
        }
        // Cache entries are edges, never roots. Invalid entries are disposable.
        for entry in entries(&self.root.join(CACHE))? {
            let path = entry.path();
            if !entry.file_name().to_str().is_some_and(valid_unit_hash) {
                continue;
            }
            let valid = read_regular(&path)?
                .and_then(|bytes| CacheEntry::decode(&bytes).ok())
                .is_some_and(|entry| {
                    unit_refs
                        .get(&entry.unit_output)
                        .is_some_and(|count| *count > 0)
                        && output_counts.get(&entry.unit_output) == Some(&entry.outputs.len())
                });
            if !valid {
                plan.removals.push(path);
            }
        }
        for (id, path) in snapshot_files {
            if !recency.contains_key(&id) {
                plan.removals.push(path);
            }
        }
        for (id, path) in unit_files {
            if !unit_refs.get(&id).is_some_and(|count| *count > 0) {
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

#[derive(Default)]
struct Collection {
    removals: Vec<PathBuf>,
}

pub(super) fn hex(id: &Digest) -> String {
    blake3::Hash::from_bytes(*id).to_hex().to_string()
}

fn valid_unit_hash(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
    use super::super::format::{OutputMetadata, encode_output_path};
    use super::*;

    fn output(store: &SnapshotStore<'_>, contents: &[u8]) -> UnitOutput {
        fs::create_dir_all(store.blobs).unwrap();
        let hash = *blake3::hash(contents).as_bytes();
        fs::write(store.blobs.join(hex(&hash)), contents).unwrap();
        UnitOutput::new(vec![Output {
            path: encode_output_path(Path::new("out/artifact")).unwrap(),
            hash,
            size: contents.len() as u64,
        }])
    }

    fn save(
        store: &SnapshotStore<'_>,
        workspace: &Digest,
        output: &UnitOutput,
        now: u64,
    ) -> Digest {
        store.publish_unit(output).unwrap();
        store.save(workspace, vec![output.id], now).unwrap();
        snapshot_id(&mut vec![output.id])
    }

    fn clean(
        store: &SnapshotStore<'_>,
        max_size: Option<u64>,
        now: u64,
        dry_run: bool,
    ) -> CargoResult<()> {
        let gctx = crate::util::GlobalContext::new(
            cargo_util_terminal::Shell::from_write(Box::new(Vec::new())),
            store.blobs.to_path_buf(),
            store.blobs.join("cargo-home"),
        );
        let mut clean_ctx = CleanContext::new(&gctx);
        clean_ctx.dry_run = dry_run;
        store.clean(&mut clean_ctx, max_size, now)
    }

    #[test]
    fn history_refreshes_every_use_and_shares_snapshot_identity() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let store = SnapshotStore::new(&blobs);
        let output = output(&store, b"shared");
        let snapshot = save(&store, &[1; 32], &output, 10);
        save(&store, &[1; 32], &output, 11);
        save(&store, &[2; 32], &output, RETENTION + 10);
        let first = store
            .root
            .join(HISTORY)
            .join(hex(&[1; 32]))
            .join(hex(&snapshot));
        assert_eq!(fs::read(&first).unwrap(), b"11\n");
        clean(&store, None, RETENTION + 12, false).unwrap();
        assert!(!first.exists());
        assert!(store.unit_is_complete(&output.id).unwrap());
        clean(&store, None, 2 * RETENTION + 11, false).unwrap();
        assert!(!store.unit_is_complete(&output.id).unwrap());
    }

    #[test]
    fn unreferenced_snapshots_never_gain_retention() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let store = SnapshotStore::new(&blobs);
        let output = output(&store, b"orphan");
        let snapshot = save(&store, &[1; 32], &output, 1);
        fs::remove_dir_all(store.root.join(HISTORY)).unwrap();
        clean(&store, None, 2, false).unwrap();
        assert!(!store.root.join(SNAPSHOTS).join(hex(&snapshot)).exists());
        assert!(!store.unit_is_complete(&output.id).unwrap());
        assert!(entries(&blobs).unwrap().is_empty());
    }

    #[test]
    fn size_pressure_preserves_shared_blobs_and_evicts_cache_edges() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let store = SnapshotStore::new(&blobs);
        let shared = output(&store, b"shared");
        let old = output(&store, b"old");
        store.publish_unit(&shared).unwrap();
        store.publish_unit(&old).unwrap();
        store.save(&[1; 32], vec![shared.id, old.id], 1).unwrap();
        store.save(&[2; 32], vec![shared.id], 2).unwrap();
        for (key, output) in [("aa", &shared), ("bb", &old)] {
            store
                .publish_cache_entry(
                    key,
                    &CacheEntry {
                        unit_output: output.id,
                        fingerprint: 42,
                        outputs: vec![OutputMetadata {
                            mode: 0o644,
                            mtime_seconds: 0,
                            mtime_nanos: 0,
                        }],
                    },
                )
                .unwrap();
        }
        clean(&store, Some(6), 3, true).unwrap();
        assert!(store.unit_is_complete(&old.id).unwrap());
        clean(&store, Some(6), 3, false).unwrap();
        assert!(store.unit_is_complete(&shared.id).unwrap());
        assert!(!store.unit_is_complete(&old.id).unwrap());
        assert!(store.read_cache_entry("aa").unwrap().is_some());
        assert!(store.read_cache_entry("bb").unwrap().is_none());
    }

    #[test]
    fn invalid_history_prevents_any_collection_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let store = SnapshotStore::new(&blobs);
        let output = output(&store, b"protected");
        let snapshot = save(&store, &[1; 32], &output, 0);
        let history = store
            .root
            .join(HISTORY)
            .join(hex(&[1; 32]))
            .join(hex(&snapshot));
        fs::write(&history, b"corrupt").unwrap();
        assert!(clean(&store, Some(0), RETENTION + 1, false).is_err());
        assert!(store.unit_is_complete(&output.id).unwrap());
        assert_eq!(fs::read(history).unwrap(), b"corrupt");
    }

    #[test]
    fn invalid_cache_entries_are_disposable_not_roots() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let store = SnapshotStore::new(&blobs);
        let output = output(&store, b"live");
        save(&store, &[1; 32], &output, 1);
        fs::create_dir_all(store.root.join(CACHE)).unwrap();
        let corrupt = store.root.join(CACHE).join("aa");
        fs::write(&corrupt, b"invalid").unwrap();
        clean(&store, None, 2, true).unwrap();
        assert!(corrupt.exists());
        clean(&store, None, 2, false).unwrap();
        assert!(!corrupt.exists());
        assert!(store.unit_is_complete(&output.id).unwrap());
    }

    #[test]
    fn unrelated_files_and_interrupted_publications_do_not_block_collection() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let store = SnapshotStore::new(&blobs);
        let output = output(&store, b"expired");
        save(&store, &[1; 32], &output, 0);
        fs::create_dir_all(store.root.join(CACHE)).unwrap();
        let leftovers = [
            store.root.join(HISTORY).join("unrelated"),
            store
                .root
                .join(HISTORY)
                .join(hex(&[1; 32]))
                .join(".tmp-interrupted"),
            store.root.join(CACHE).join("unrelated"),
        ];
        for path in &leftovers {
            fs::write(path, b"leave alone").unwrap();
        }
        clean(&store, None, RETENTION + 1, false).unwrap();
        assert!(!store.unit_is_complete(&output.id).unwrap());
        for path in leftovers {
            assert_eq!(fs::read(path).unwrap(), b"leave alone");
        }
    }
}
